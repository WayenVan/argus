//! Codex.
//!
//! Hooks are injected per session with `-c hooks.<Event>=[…]` config
//! overrides, so no user file is touched. Codex runs a hook only after the
//! user has trusted its exact definition; for `-c` hooks the trust is stored
//! in `config.toml` under `hooks.state."/<session-flags>/config.toml:<event>:0:0"`,
//! independent of the working directory. The definitions below are therefore
//! frozen: changing any of them makes every user review the hooks again.
//!
//! Codex only fires `SessionStart` with the first prompt, so a freshly started
//! agent reports nothing until then.

use std::path::PathBuf;

use anyhow::Result;
use serde_json::Value;

use super::{Context, Driver, Hint, Launch, SELF_LABEL_INSTRUCTIONS, field, hook_command};

pub struct Codex;

/// (hook event, the name Codex uses for it in trust keys).
const HOOK_EVENTS: &[(&str, &str)] = &[
    ("SessionStart", "session_start"),
    ("UserPromptSubmit", "user_prompt_submit"),
    ("PreToolUse", "pre_tool_use"),
    ("PostToolUse", "post_tool_use"),
    ("PermissionRequest", "permission_request"),
    ("Stop", "stop"),
    ("Interrupt", "interrupt"),
];

impl Driver for Codex {
    fn kind(&self) -> &'static str {
        "codex"
    }

    fn has_hooks(&self) -> bool {
        true
    }

    fn initial_activity(&self) -> Option<&'static str> {
        // At its prompt; the first hook only arrives with the first prompt.
        Some("idle")
    }

    fn prepare(&self, launch: &mut Launch, ctx: &Context) -> Result<Option<String>> {
        // Independent of hooks: ARGUS_AGENT_ID is set on the process
        // environment regardless, so self-labeling works even without them.
        // A user's own `-c developer_instructions=` comes later in argv and
        // wins, same as any other config override; that's a soft loss (the
        // agent just won't know the label convention), not worth a warning.
        launch.command.splice(1..1, developer_instructions_override());

        let Some(hook_exe) = &ctx.hook_exe else {
            return Ok(Some("argus-hook is not installed next to argus; activity will not be tracked".into()));
        };
        // Checked on the user's own arguments, before ours are added.
        let overridden = launch.command[1..].iter().any(|a| a.starts_with("hooks.") || a.starts_with("-chooks."));
        let overrides = hook_overrides(&hook_command(hook_exe, "codex"));
        launch.command.splice(1..1, overrides);

        if overridden {
            return Ok(Some("your own -c hooks.* overrides may replace argus's hooks for those events".into()));
        }
        if !trusted() {
            return Ok(Some(
                "Codex will ask you to review argus's hooks once: attach to this agent and trust them".into(),
            ));
        }
        Ok(None)
    }

    fn interpret(&self, event: &Value) -> Hint {
        match field(event, "hook_event_name").unwrap_or_default() {
            "SessionStart" => match field(event, "source") {
                Some("compact") => Hint::Ignore,
                _ => Hint::SessionStart,
            },
            "UserPromptSubmit" | "PostToolUse" => Hint::Working,
            "PreToolUse" => Hint::Tool(field(event, "tool_name").unwrap_or("tool").to_string()),
            "PermissionRequest" => Hint::WaitingApproval,
            "Stop" => Hint::Done,
            // The user interrupted the turn and is presumably about to type.
            "Interrupt" => Hint::WaitingInput,
            _ => Hint::Ignore,
        }
    }
}

/// `-c hooks.<Event>=[…]` pairs. Frozen: see the module docs. The timeout is
/// 3s because Codex caps `Interrupt` hooks at 3s and warns on every start
/// otherwise; argus-hook itself gives up after 0.5s.
fn hook_overrides(command: &str) -> Vec<String> {
    let command = command.replace('\\', r"\\").replace('"', r#"\""#);
    HOOK_EVENTS
        .iter()
        .flat_map(|(event, _)| {
            let value =
                format!(r#"hooks.{event}=[{{hooks=[{{type="command",command="{command}",async=true,timeout=3}}]}}]"#);
            ["-c".to_string(), value]
        })
        .collect()
}

/// `-c developer_instructions="…"`, escaped as a basic TOML string. Plain
/// config, not a hook: never asks the user to trust anything.
fn developer_instructions_override() -> Vec<String> {
    let text = SELF_LABEL_INSTRUCTIONS.replace('\\', r"\\").replace('"', r#"\""#).replace('\n', r"\n");
    vec!["-c".to_string(), format!(r#"developer_instructions="{text}""#)]
}

/// `$CODEX_HOME`, defaulting to `~/.codex`.
fn codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".codex"))
}

/// Whether the user has trusted argus's hooks before. Only presence can be
/// checked: Codex's hash is its own business, so a stale trust (e.g. after
/// argus-hook moved) is caught by Codex itself, which asks again.
fn trusted() -> bool {
    let Ok(config) = std::fs::read_to_string(codex_home().join("config.toml")) else { return false };
    HOOK_EVENTS.iter().all(|(_, key)| config.contains(&format!("/<session-flags>/config.toml:{key}:0:0")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn interprets_events() {
        let hint = |v: Value| Codex.interpret(&v);
        assert_eq!(hint(json!({"hook_event_name":"UserPromptSubmit"})), Hint::Working);
        assert_eq!(hint(json!({"hook_event_name":"PreToolUse","tool_name":"shell"})), Hint::Tool("shell".into()));
        assert_eq!(hint(json!({"hook_event_name":"PermissionRequest"})), Hint::WaitingApproval);
        assert_eq!(hint(json!({"hook_event_name":"Stop"})), Hint::Done);
        assert_eq!(hint(json!({"hook_event_name":"Interrupt"})), Hint::WaitingInput);
    }

    #[test]
    fn overrides_are_stable_toml() {
        let args = hook_overrides(r#"'/opt/argus hook' codex"#);
        assert_eq!(args.len(), HOOK_EVENTS.len() * 2);
        assert_eq!(args[0], "-c");
        // The exact bytes matter: Codex trusts this definition by hash.
        assert_eq!(
            args[1],
            r#"hooks.SessionStart=[{hooks=[{type="command",command="'/opt/argus hook' codex",async=true,timeout=3}]}]"#
        );
    }

    #[test]
    fn developer_instructions_are_escaped_for_a_basic_toml_string() {
        let args = developer_instructions_override();
        assert_eq!(args[0], "-c");
        assert!(args[1].starts_with(r#"developer_instructions=""#));
        assert!(args[1].ends_with('"'));
        // Real newlines break a basic (non-triple-quoted) TOML string; the
        // instructions are multi-line, so they must come out escaped.
        let inner = &args[1][r#"developer_instructions=""#.len()..args[1].len() - 1];
        assert!(!inner.contains('\n'));
        assert!(inner.contains(r"\n"));
        assert!(SELF_LABEL_INSTRUCTIONS.contains('"'), "sanity check: exercises quote-escaping too");
        assert!(inner.contains(r#"\""#));
    }
}
