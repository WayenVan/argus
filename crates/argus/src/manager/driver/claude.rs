//! Claude Code.
//!
//! Hooks are registered per session with `--settings <file>`, merged with the
//! user's own hooks by Claude. Claude only honours the last `--settings`
//! flag, so when the user passes one we merge ours into a copy of theirs.

use std::fs;
use std::path::Path;

use anyhow::{Context as _, Result};
use serde_json::{Value, json};

use super::{Context, Driver, Hint, Launch, field, hook_command};

pub struct Claude;

/// Hook events that affect activity.
const HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionRequest",
    "Notification",
    "Stop",
    "StopFailure",
];

const SHARED_FILE: &str = "claude-hooks.json";

impl Driver for Claude {
    fn kind(&self) -> &'static str {
        "claude"
    }

    fn has_hooks(&self) -> bool {
        true
    }

    fn prepare(&self, launch: &mut Launch, ctx: &Context) -> Result<Option<String>> {
        let Some(hook_exe) = &ctx.hook_exe else {
            return Ok(Some("argus-hook is not installed next to argus; activity will not be tracked".into()));
        };
        let shared = ctx.dir.join(SHARED_FILE);
        match find_settings_arg(&launch.command) {
            None => {
                launch.command.splice(1..1, ["--settings".to_string(), shared.to_string_lossy().into_owned()]);
            }
            Some(idx) => {
                // The user brought their own settings: add our hooks to a copy.
                let (value_idx, user) = settings_value(&launch.command, idx);
                let mut settings = load_user_settings(&user)?;
                merge_hooks(&mut settings, &hooks_json(hook_exe));
                let merged = launch.agent_dir.join("claude-settings.json");
                fs::write(&merged, serde_json::to_vec_pretty(&settings)?)?;
                let merged = merged.to_string_lossy().into_owned();
                match value_idx {
                    Some(v) => launch.command[v] = merged,
                    None => launch.command[idx] = format!("--settings={merged}"),
                }
            }
        }
        Ok(None)
    }

    fn interpret(&self, event: &Value) -> Hint {
        // Events from subagents do not describe the main agent.
        if event.get("agent_id").is_some() {
            return Hint::Ignore;
        }
        match field(event, "hook_event_name").unwrap_or_default() {
            "SessionStart" => match field(event, "source") {
                Some("compact") => Hint::Ignore, // Happens mid-turn.
                _ => Hint::SessionStart,
            },
            "UserPromptSubmit" | "PostToolUse" | "PostToolUseFailure" => Hint::Working,
            "PreToolUse" => Hint::Tool(field(event, "tool_name").unwrap_or("tool").to_string()),
            "PermissionRequest" => Hint::WaitingApproval,
            "Notification" => match field(event, "notification_type") {
                Some("permission_prompt") => Hint::WaitingApproval,
                Some("idle_prompt" | "agent_needs_input") => Hint::WaitingInput,
                _ => Hint::Ignore,
            },
            "Stop" => Hint::Done,
            "StopFailure" => Hint::Error,
            _ => Hint::Ignore,
        }
    }
}

/// Writes the settings file shared by every Claude agent started by argus.
pub fn write_shared_settings(ctx: &Context) -> Result<()> {
    let Some(hook_exe) = &ctx.hook_exe else { return Ok(()) };
    let path = ctx.dir.join(SHARED_FILE);
    fs::write(&path, serde_json::to_vec_pretty(&hooks_json(hook_exe))?)
        .with_context(|| format!("writing {}", path.display()))
}

fn hooks_json(hook_exe: &Path) -> Value {
    let handler = json!({ "type": "command", "command": hook_command(hook_exe, "claude"), "async": true });
    let hooks: serde_json::Map<String, Value> =
        HOOK_EVENTS.iter().map(|e| (e.to_string(), json!([{ "hooks": [handler.clone()] }]))).collect();
    json!({ "hooks": hooks })
}

/// Index of a `--settings` or `--settings=…` argument, if any.
fn find_settings_arg(command: &[String]) -> Option<usize> {
    command.iter().position(|a| a == "--settings" || a.starts_with("--settings="))
}

/// Returns (index of the value argument, if separate) and the value.
fn settings_value(command: &[String], idx: usize) -> (Option<usize>, String) {
    match command[idx].strip_prefix("--settings=") {
        Some(v) => (None, v.to_string()),
        None => (Some(idx + 1), command.get(idx + 1).cloned().unwrap_or_default()),
    }
}

/// `--settings` takes a JSON file path or inline JSON.
fn load_user_settings(value: &str) -> Result<Value> {
    let text = if value.trim_start().starts_with('{') {
        value.to_string()
    } else {
        fs::read_to_string(value).with_context(|| format!("reading --settings file {value}"))?
    };
    serde_json::from_str(&text).with_context(|| format!("parsing --settings {value}"))
}

/// Appends each of our matcher groups to the user's list for that event.
fn merge_hooks(settings: &mut Value, ours: &Value) {
    if !settings.is_object() {
        *settings = json!({});
    }
    let hooks = settings.as_object_mut().unwrap().entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let hooks = hooks.as_object_mut().unwrap();
    for (event, groups) in ours["hooks"].as_object().unwrap() {
        let list = hooks.entry(event.clone()).or_insert_with(|| json!([]));
        if let (Some(list), Some(groups)) = (list.as_array_mut(), groups.as_array()) {
            list.extend(groups.iter().cloned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ev(v: Value) -> Hint {
        Claude.interpret(&v)
    }

    #[test]
    fn interprets_events() {
        assert_eq!(ev(json!({"hook_event_name":"UserPromptSubmit"})), Hint::Working);
        assert_eq!(ev(json!({"hook_event_name":"PreToolUse","tool_name":"Bash"})), Hint::Tool("Bash".into()));
        assert_eq!(ev(json!({"hook_event_name":"Stop"})), Hint::Done);
        assert_eq!(
            ev(json!({"hook_event_name":"Notification","notification_type":"permission_prompt"})),
            Hint::WaitingApproval
        );
        assert_eq!(ev(json!({"hook_event_name":"SessionStart","source":"compact"})), Hint::Ignore);
        assert_eq!(ev(json!({"hook_event_name":"Stop","agent_id":"sub"})), Hint::Ignore);
    }

    #[test]
    fn injects_or_merges_settings() {
        let dir = std::env::temp_dir().join(format!("argus-claude-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let ctx = Context { hook_exe: Some(PathBuf::from("/opt/argus hook")), dir: dir.clone() };

        let mut launch = Launch { command: vec!["claude".into(), "-c".into()], agent_dir: dir.clone() };
        Claude.prepare(&mut launch, &ctx).unwrap();
        assert_eq!(launch.command[1], "--settings");
        assert_eq!(launch.command[3], "-c");

        let user = r#"{"model":"opus","hooks":{"Stop":[{"hooks":[{"type":"command","command":"mine"}]}]}}"#;
        let mut launch =
            Launch { command: vec!["claude".into(), "--settings".into(), user.into()], agent_dir: dir.clone() };
        Claude.prepare(&mut launch, &ctx).unwrap();
        let merged: Value = serde_json::from_str(&fs::read_to_string(&launch.command[2]).unwrap()).unwrap();
        assert_eq!(merged["model"], "opus");
        let stop = merged["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2, "user hook kept, ours appended");
        assert_eq!(stop[1]["hooks"][0]["command"], "'/opt/argus hook' claude");
        fs::remove_dir_all(dir).unwrap();
    }
}
