//! opencode.
//!
//! opencode has no command hooks, only in-process JS plugins. argus ships one
//! (`opencode-plugin.js`, compiled into argus and written next to Claude's
//! shared settings at startup) that forwards the relevant bus events through
//! `argus-hook opencode`, already flattened into the events below.
//!
//! Both the plugin and the self-label instructions are injected per agent
//! through `OPENCODE_CONFIG_CONTENT`, a config layer opencode merges over the
//! user's own, appending to their `plugin` and `instructions` lists. No user
//! file is touched and nothing needs trusting. A user's own
//! `OPENCODE_CONFIG_CONTENT` is kept: ours is merged into it.
//!
//! opencode creates a session only with the first prompt, so a freshly
//! started agent reports nothing until then. Nor is it `idle` right away: its
//! TUI discards keys typed during its first seconds, until it shows the
//! cursor in its prompt.

use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result};
use serde_json::{Value, json};

use super::{Context, Driver, DriverReport, Launch, SELF_LABEL_INSTRUCTIONS, plugin_report};

pub struct Opencode;

const PLUGIN_JS: &str = include_str!("opencode-plugin.js");
const PLUGIN_FILE: &str = "argus-opencode.js";
const INSTRUCTIONS_FILE: &str = "argus-instructions.md";
const CONFIG_ENV: &str = "OPENCODE_CONFIG_CONTENT";
/// How long a permission request may take to resolve without a person
/// before it counts as waiting on one.
const CONFIRM_AFTER: Duration = Duration::from_secs(3);
/// The plugin's event format; see the `v` field in `opencode-plugin.js`.
const EVENT_VERSION: u64 = 1;

impl Driver for Opencode {
    fn kind(&self) -> &'static str {
        "opencode"
    }

    fn has_hooks(&self) -> bool {
        true
    }

    fn ready_on_cursor(&self) -> Option<Duration> {
        Some(Duration::ZERO)
    }

    fn prepare(&self, launch: &mut Launch, ctx: &Context) -> Result<Option<String>> {
        let existing = launch.env.iter().position(|(k, _)| k == CONFIG_ENV);
        let mut config = match existing {
            Some(i) => serde_json::from_str(&launch.env[i].1).with_context(|| format!("parsing your {CONFIG_ENV}"))?,
            None => json!({}),
        };
        if !config.is_object() {
            anyhow::bail!("your {CONFIG_ENV} is not a JSON object");
        }
        // Independent of the plugin, like the other drivers' instructions.
        let instructions = ctx.dir.join(INSTRUCTIONS_FILE);
        push(&mut config, "instructions", json!(instructions.to_string_lossy()));
        let warning = match &ctx.hook_exe {
            Some(hook_exe) => {
                let plugin = file_url(&ctx.dir.join(PLUGIN_FILE));
                push(&mut config, "plugin", json!([plugin, { "hook": hook_exe.to_string_lossy() }]));
                None
            }
            None => Some("argus-hook is not installed next to argus; activity will not be tracked".into()),
        };
        let value = serde_json::to_string(&config)?;
        match existing {
            Some(i) => launch.env[i].1 = value,
            None => launch.env.push((CONFIG_ENV.into(), value)),
        }
        Ok(warning)
    }

    fn translate(&self, event: &Value) -> DriverReport {
        plugin_report(event, EVENT_VERSION, Some(CONFIRM_AFTER))
    }

    /// Writes the plugin and the instructions it points opencode at.
    fn write_shared_files(&self, ctx: &Context) -> Result<()> {
        for (name, text) in [(PLUGIN_FILE, PLUGIN_JS), (INSTRUCTIONS_FILE, SELF_LABEL_INSTRUCTIONS)] {
            let path = ctx.dir.join(name);
            fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        }
        Ok(())
    }
}

/// Appends `item` to the list at `config[key]`, making it a list if needed.
fn push(config: &mut Value, key: &str, item: Value) {
    let list = config.as_object_mut().expect("checked by the caller").entry(key).or_insert_with(|| json!([]));
    match list.as_array_mut() {
        Some(list) => list.push(item),
        None => *list = json!([item]),
    }
}

/// `file://` URL for an absolute path, percent-encoding all but the
/// characters that need no escaping in a path.
fn file_url(path: &Path) -> String {
    let mut url = String::from("file://");
    for &b in path.to_string_lossy().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            url.push(b as char);
        } else {
            url.push_str(&format!("%{b:02X}"));
        }
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::Hint;
    use std::path::PathBuf;

    fn ev(v: Value) -> Hint {
        Opencode.translate(&v).hint
    }

    #[test]
    fn interprets_events() {
        let e = |name: &str| json!({"v": 1, "hook_event_name": name, "session_id": "s"});
        assert_eq!(ev(e("SessionStart")), Hint::SessionStart);
        assert_eq!(ev(e("UserPromptSubmit")), Hint::Working);
        assert_eq!(ev(json!({"v":1,"hook_event_name":"PreToolUse","tool_name":"bash"})), Hint::Tool("bash".into()));
        assert_eq!(ev(e("PermissionRequest")), Hint::Ignore);
        assert_eq!(
            ev(json!({"v":1,"hook_event_name":"PermissionReplied","tool_name":"task"})),
            Hint::Tool("task".into())
        );
        assert_eq!(ev(e("PermissionReplied")), Hint::Working);
        assert_eq!(ev(e("Stop")), Hint::Done);
        assert_eq!(ev(e("StopFailure")), Hint::Error);
        assert_eq!(ev(e("Interrupt")), Hint::Interrupted);
        assert_eq!(ev(json!({"v": 2, "hook_event_name": "Stop"})), Hint::Ignore, "unknown format version");
    }

    #[test]
    fn injects_or_merges_config() {
        let ctx = Context { hook_exe: Some(PathBuf::from("/opt/argus hook")), dir: PathBuf::from("/s d/drivers") };
        let config = |launch: &Launch| -> Value {
            let (_, v) = launch.env.iter().find(|(k, _)| k == CONFIG_ENV).expect("config set");
            serde_json::from_str(v).unwrap()
        };

        let mut launch = Launch { command: vec!["opencode".into()], env: vec![], agent_dir: PathBuf::new() };
        assert_eq!(Opencode.prepare(&mut launch, &ctx).unwrap(), None);
        assert_eq!(launch.command, vec!["opencode"], "the command line is left alone");
        let c = config(&launch);
        assert_eq!(c["instructions"], json!(["/s d/drivers/argus-instructions.md"]));
        assert_eq!(c["plugin"], json!([["file:///s%20d/drivers/argus-opencode.js", {"hook": "/opt/argus hook"}]]));

        let user = r#"{"model":"x/y","plugin":["mine"],"instructions":"not-a-list"}"#;
        let mut launch = Launch {
            command: vec!["opencode".into()],
            env: vec![("PATH".into(), "/bin".into()), (CONFIG_ENV.into(), user.into())],
            agent_dir: PathBuf::new(),
        };
        Opencode.prepare(&mut launch, &ctx).unwrap();
        assert_eq!(launch.env.len(), 2, "merged in place");
        let c = config(&launch);
        assert_eq!(c["model"], "x/y");
        assert_eq!(c["plugin"][0], "mine");
        assert_eq!(c["plugin"].as_array().unwrap().len(), 2);
        assert_eq!(c["instructions"], json!(["/s d/drivers/argus-instructions.md"]));

        let mut launch = Launch {
            command: vec!["opencode".into()],
            env: vec![(CONFIG_ENV.into(), "nope".into())],
            agent_dir: PathBuf::new(),
        };
        assert!(Opencode.prepare(&mut launch, &ctx).is_err());
        assert_eq!(launch.env[0].1, "nope", "left unchanged when it cannot be merged");
    }

    #[test]
    fn plugin_is_valid_javascript() {
        let Ok(status) = std::process::Command::new("node")
            .args(["--input-type=module", "--check"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                child.stdin.take().unwrap().write_all(PLUGIN_JS.as_bytes())?;
                child.wait()
            })
        else {
            eprintln!("node not found; skipping the syntax check");
            return;
        };
        assert!(status.success(), "opencode-plugin.js has a syntax error");
    }
}
