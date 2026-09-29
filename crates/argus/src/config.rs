//! The user's config file: `$ARGUS_CONFIG`, else
//! `$XDG_CONFIG_HOME/argus/config.toml`, else `~/.config/argus/config.toml`.
//! A missing file means every default.
//!
//! Read once per process. Clients read it on every command; the manager at
//! start, so its `[manager]` settings take effect on `argus manager restart`,
//! and the ones holders use only for agents started after that.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use argus_proto::msg::HolderLimits;
use serde::{Deserialize, Deserializer};

use crate::naming;

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Group for `argus run` when neither `--in` nor `$ARGUS_GROUP` gives one.
    pub default_group: Option<String>,
    pub detach_key: DetachKey,
    /// Launch presets, by the name given to `argus run`.
    pub profiles: BTreeMap<String, Profile>,
    pub dashboard: DashboardConfig,
    pub manager: ManagerConfig,
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct DashboardConfig {
    /// Prefill `a`'s `--in` with the dashboard's tmux session name instead of
    /// the group under the cursor.
    pub group_from_tmux: bool,
}

/// What `argus run <name>` expands to when `<name>` is a profile.
#[derive(Deserialize, Default, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct Profile {
    /// Program to run instead of the profile's name.
    pub program: Option<String>,
    /// Driver to use, as `--kind` would pick it.
    pub kind: Option<String>,
    /// Put before the arguments given on the command line.
    pub args: Vec<String>,
    /// Used when neither `--in` nor `$ARGUS_GROUP` gives a group.
    pub group: Option<String>,
    /// `--label`s given on the command line override these.
    pub labels: BTreeMap<String, String>,
}

#[derive(Deserialize, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct ManagerConfig {
    /// How long `working` may go without hooks or output before it is `unknown`.
    #[serde(deserialize_with = "duration")]
    pub silence: Duration,
    /// How long after someone types in an attached terminal `send` refuses.
    #[serde(deserialize_with = "duration")]
    pub typing_grace: Duration,
    /// Finished turns kept per agent for `inspect --last`.
    pub turn_history: usize,
    /// From SIGTERM to SIGKILL on `argus kill`.
    #[serde(deserialize_with = "duration")]
    pub kill_grace: Duration,
    /// Recent output each agent keeps for `attach --replay` and `logs`.
    #[serde(deserialize_with = "size")]
    pub replay_buffer: usize,
}

impl Default for ManagerConfig {
    fn default() -> ManagerConfig {
        let holder = HolderLimits::default();
        ManagerConfig {
            silence: Duration::from_secs(15),
            typing_grace: Duration::from_secs(10),
            turn_history: 50,
            kill_grace: Duration::from_millis(holder.kill_grace_ms),
            replay_buffer: holder.replay_buffer,
        }
    }
}

impl ManagerConfig {
    pub fn holder_limits(&self) -> HolderLimits {
        HolderLimits { kill_grace_ms: self.kill_grace.as_millis() as u64, replay_buffer: self.replay_buffer }
    }
}

/// The key that ends an attach: Ctrl plus a letter, `\` or `]`. Ctrl-] by
/// default: `]` sits in the same place on US and UK keyboards, and it sends
/// a control byte (GS) in every terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetachKey {
    /// The character pressed with Ctrl, e.g. `]` or `a`.
    pub key: char,
}

impl Default for DetachKey {
    fn default() -> DetachKey {
        DetachKey { key: ']' }
    }
}

impl DetachKey {
    pub fn parse(spec: &str) -> Result<DetachKey> {
        let lower = spec.to_ascii_lowercase();
        let Some(rest) = lower.strip_prefix("ctrl-").or_else(|| lower.strip_prefix("ctrl+")) else {
            bail!("detach_key {spec:?}: expected ctrl-<key>, e.g. \"ctrl-]\" or \"ctrl-b\"");
        };
        let mut chars = rest.chars();
        let (Some(key), None) = (chars.next(), chars.next()) else {
            bail!("detach_key {spec:?}: expected one key after ctrl-");
        };
        match key {
            // Ctrl-H is Backspace in many terminals, and I, J, M are Tab and Enter.
            'h' | 'i' | 'j' | 'm' => bail!("detach_key {spec:?} is also Backspace, Tab or Enter"),
            'a'..='z' | '\\' | ']' => Ok(DetachKey { key }),
            _ => bail!("detach_key {spec:?}: use ctrl- with a letter, \\ or ]"),
        }
    }

    /// The byte the key sends in the terminal's legacy encoding.
    pub fn byte(self) -> u8 {
        self.key as u8 & 0x1f
    }

    /// The key's code in the kitty keyboard protocol and modifyOtherKeys.
    pub fn codepoint(self) -> u32 {
        self.key as u32
    }
}

impl<'de> Deserialize<'de> for DetachKey {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<DetachKey, D::Error> {
        DetachKey::parse(&String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

/// The config this process uses. Tests always get the defaults.
pub fn get() -> Result<&'static Config> {
    static CONFIG: OnceLock<Result<Config, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| if cfg!(test) { Ok(Config::default()) } else { load(&path()).map_err(|e| format!("{e:#}")) })
        .as_ref()
        .map_err(|e| anyhow!("{e}"))
}

/// The manager's settings. Only for the manager, which checked the file at
/// start.
pub fn manager() -> &'static ManagerConfig {
    &get().expect("config checked at manager start").manager
}

pub fn path() -> PathBuf {
    if let Some(path) = std::env::var_os("ARGUS_CONFIG") {
        return path.into();
    }
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"),
    };
    base.join("argus/config.toml")
}

fn load(path: &Path) -> Result<Config> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    parse(&text).with_context(|| format!("in {}", path.display()))
}

pub(crate) fn parse(text: &str) -> Result<Config> {
    let config: Config = toml::from_str(text).map_err(|e| anyhow!("{}", e.to_string().trim_end()))?;
    config.validate()?;
    Ok(config)
}

impl Config {
    fn validate(&self) -> Result<()> {
        if let Some(group) = &self.default_group {
            naming::normalize_group(group).context("default_group")?;
        }
        for (name, profile) in &self.profiles {
            let what = || format!("profiles.{name}");
            if let Some(group) = &profile.group {
                naming::normalize_group(group).with_context(what)?;
            }
            for (k, v) in &profile.labels {
                naming::validate_label(k, v).with_context(what)?;
            }
            if profile.program.as_deref() == Some("") {
                bail!("{}: program must not be empty", what());
            }
        }
        let m = &self.manager;
        if m.silence.is_zero() || m.kill_grace.is_zero() {
            bail!("manager.silence and manager.kill_grace must be more than 0");
        }
        if m.turn_history == 0 {
            bail!("manager.turn_history must be at least 1");
        }
        if !(64 * 1024..=64 * 1024 * 1024).contains(&m.replay_buffer) {
            bail!("manager.replay_buffer must be between 64K and 64M");
        }
        Ok(())
    }

    /// The profile `argus run <program>` names, if any.
    pub fn profile(&self, program: &str) -> Option<&Profile> {
        self.profiles.get(program)
    }
}

/// `500ms`, `15s`, `2m`, `1h`.
fn duration<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Value {
        Text(String),
        Number(u64),
    }
    match Value::deserialize(d)? {
        Value::Text(s) => parse_duration(&s),
        Value::Number(n) => Err(format!("give a unit, as in \"{n}s\" or \"{n}ms\"")),
    }
    .map_err(serde::de::Error::custom)
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = num.parse().map_err(|_| format!("expected a duration like 500ms, 15s or 2m, got {s:?}"))?;
    Ok(match unit {
        "ms" => Duration::from_millis(n),
        "s" => Duration::from_secs(n),
        "m" => Duration::from_secs(n * 60),
        "h" => Duration::from_secs(n * 3600),
        _ => return Err(format!("expected a duration like 500ms, 15s or 2m, got {s:?}")),
    })
}

/// Bytes, or a string with a `K` or `M` suffix (1024-based).
fn size<'de, D: Deserializer<'de>>(d: D) -> Result<usize, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Size {
        Bytes(usize),
        Text(String),
    }
    match Size::deserialize(d)? {
        Size::Bytes(n) => Ok(n),
        Size::Text(s) => parse_size(&s).map_err(serde::de::Error::custom),
    }
}

fn parse_size(s: &str) -> Result<usize, String> {
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: usize = num.parse().map_err(|_| format!("expected a size like 512K or 4M, got {s:?}"))?;
    let scale = match unit.to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "K" | "KB" | "KIB" => 1024,
        "M" | "MB" | "MIB" => 1024 * 1024,
        _ => return Err(format!("expected a size like 512K or 4M, got {s:?}")),
    };
    Ok(n * scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_is_all_defaults() {
        let config = parse("").unwrap();
        assert_eq!(config.detach_key, DetachKey::default());
        assert_eq!(config.manager.silence, Duration::from_secs(15));
        assert_eq!(config.manager.replay_buffer, 1024 * 1024);
        assert!(config.profiles.is_empty());
        assert!(!config.dashboard.group_from_tmux);
    }

    #[test]
    fn full_file_parses() {
        let config = parse(
            r#"
            default_group = "work"
            detach_key = "ctrl-b"

            [profiles.review]
            program = "claude"
            args = ["--permission-mode", "plan"]
            group = "review"
            labels = { role = "reviewer" }

            [profiles.cc]
            kind = "claude"

            [dashboard]
            group_from_tmux = true

            [manager]
            silence = "30s"
            typing_grace = "5s"
            turn_history = 200
            kill_grace = "500ms"
            replay_buffer = "4M"
            "#,
        )
        .unwrap();
        assert_eq!(config.default_group.as_deref(), Some("work"));
        assert_eq!(config.detach_key.byte(), 0x02);
        assert!(config.dashboard.group_from_tmux);
        let review = config.profile("review").unwrap();
        assert_eq!(review.program.as_deref(), Some("claude"));
        assert_eq!(review.labels["role"], "reviewer");
        assert_eq!(config.profile("cc").unwrap().kind.as_deref(), Some("claude"));
        let limits = config.manager.holder_limits();
        assert_eq!((limits.kill_grace_ms, limits.replay_buffer), (500, 4 << 20));
        assert_eq!(config.manager.turn_history, 200);
    }

    #[test]
    fn mistakes_are_errors() {
        assert!(parse("detach_kye = \"ctrl-b\"").is_err(), "unknown key");
        assert!(parse("[manager]\nsilence = \"15\"").is_err(), "no unit");
        assert!(parse("[manager]\nsilence = 15").unwrap_err().to_string().contains("\"15s\""));
        assert!(parse("[manager]\nreplay_buffer = \"1K\"").is_err(), "too small");
        assert!(parse("default_group = \"Work\"").is_err(), "bad group");
        assert!(parse("[profiles.x]\nlabels = { \"a b\" = \"c\" }").is_err(), "bad label");
        assert!(parse("[profiles.x]\nargs = \"--flag\"").is_err(), "args is a list");
    }

    #[test]
    fn detach_keys() {
        let key = |s| DetachKey::parse(s).map(|k| (k.byte(), k.codepoint()));
        assert_eq!(key("ctrl-\\").unwrap(), (0x1c, 92));
        assert_eq!(key("Ctrl-]").unwrap(), (0x1d, 93));
        assert_eq!(key("ctrl+a").unwrap(), (0x01, 97));
        for bad in ["ctrl-m", "ctrl-i", "ctrl-[", "ctrl-", "ctrl-ab", "alt-a", "b"] {
            assert!(DetachKey::parse(bad).is_err(), "{bad}");
        }
    }
}
