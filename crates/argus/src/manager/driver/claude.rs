//! Claude Code.
//!
//! Hooks are registered per session with `--settings <file>`, merged with the
//! user's own hooks by Claude. Claude only honours the last `--settings`
//! flag, so when the user passes one we merge ours into a copy of theirs.

use std::fs;
use std::path::Path;

use anyhow::{Context as _, Result};
use argus_proto::msg::InteractionPhase;
use serde_json::{Value, json};

use super::{
    Context, Driver, Hint, InteractionChange, Launch, SELF_LABEL_INSTRUCTIONS, field, hook_command, interaction_request,
};

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
        // Independent of hooks: ARGUS_AGENT_ID is set on the process
        // environment regardless, so self-labeling works even without them.
        launch.command.splice(1..1, ["--append-system-prompt".to_string(), SELF_LABEL_INSTRUCTIONS.to_string()]);

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
                merge_settings(&mut settings, &settings_json(hook_exe));
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
            // Other permission hooks (or auto mode) may answer this without a
            // person. The later permission_prompt notification confirms wait.
            "PermissionRequest" => Hint::Ignore,
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

    fn interaction(&self, event: &Value) -> Option<InteractionChange> {
        if event.get("agent_id").is_some() {
            return None;
        }
        match field(event, "hook_event_name") {
            Some("PermissionRequest") => Some(InteractionChange::Opened {
                request: interaction_request(event),
                confirm_after: None,
                on_screen: None,
            }),
            Some("Notification") if field(event, "notification_type") == Some("permission_prompt") => {
                let mut fallback = interaction_request(event);
                fallback.id = format!("{}:permission_prompt", fallback.session_id);
                fallback.phase = InteractionPhase::NeedsUser;
                Some(InteractionChange::NeedsUser { fallback })
            }
            _ => None,
        }
    }

    fn hold_stop(&self, event: &Value, reason: &str) -> Option<String> {
        // Already continuing because of a Stop hook; holding again could loop.
        if event.get("stop_hook_active").and_then(Value::as_bool) == Some(true) {
            return None;
        }
        Some(json!({ "decision": "block", "reason": reason }).to_string())
    }
}

/// Writes the settings file shared by every Claude agent started by argus.
pub fn write_shared_settings(ctx: &Context) -> Result<()> {
    let Some(hook_exe) = &ctx.hook_exe else { return Ok(()) };
    let path = ctx.dir.join(SHARED_FILE);
    fs::write(&path, serde_json::to_vec_pretty(&settings_json(hook_exe))?)
        .with_context(|| format!("writing {}", path.display()))
}

/// Lets the agent set its own labels without a permission prompt; the label
/// instructions and `hold_stop` both ask it to.
const LABEL_RULE: &str = "Bash(argus label self:*)";

/// Our hooks and the label permission. Every hook runs in the background
/// except `Stop`, whose answer Claude waits for so the manager can hold the
/// turn open (see `hold_stop`).
fn settings_json(hook_exe: &Path) -> Value {
    let command = hook_command(hook_exe, "claude");
    let hooks: serde_json::Map<String, Value> = HOOK_EVENTS
        .iter()
        .map(|&e| {
            let handler = json!({ "type": "command", "command": command, "async": e != "Stop" });
            (e.to_string(), json!([{ "hooks": [handler] }]))
        })
        .collect();
    json!({ "hooks": hooks, "permissions": { "allow": [LABEL_RULE] } })
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

/// Appends each of our matcher groups to the user's list for that event, and
/// our permission rules to the user's `permissions.allow`.
fn merge_settings(settings: &mut Value, ours: &Value) {
    let hooks = object_at(settings, &["hooks"]);
    for (event, groups) in ours["hooks"].as_object().unwrap() {
        let list = hooks.entry(event.clone()).or_insert_with(|| json!([]));
        if let (Some(list), Some(groups)) = (list.as_array_mut(), groups.as_array()) {
            list.extend(groups.iter().cloned());
        }
    }
    let permissions = object_at(settings, &["permissions"]);
    let allow = permissions.entry("allow").or_insert_with(|| json!([]));
    if !allow.is_array() {
        *allow = json!([]);
    }
    let allow = allow.as_array_mut().unwrap();
    for rule in ours["permissions"]["allow"].as_array().unwrap() {
        if !allow.contains(rule) {
            allow.push(rule.clone());
        }
    }
}

/// The object at `path` inside `value`, replacing anything in the way that
/// is not an object.
fn object_at<'a>(value: &'a mut Value, path: &[&str]) -> &'a mut serde_json::Map<String, Value> {
    if !value.is_object() {
        *value = json!({});
    }
    let mut map = value.as_object_mut().unwrap();
    for key in path {
        let next = map.entry(key.to_string()).or_insert_with(|| json!({}));
        if !next.is_object() {
            *next = json!({});
        }
        map = next.as_object_mut().unwrap();
    }
    map
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

        let mut launch = Launch { command: vec!["claude".into(), "-c".into()], env: vec![], agent_dir: dir.clone() };
        Claude.prepare(&mut launch, &ctx).unwrap();
        // Each `splice(1..1, ..)` lands right after the program name, so the
        // one that runs second (the settings injection) ends up first.
        assert_eq!(launch.command[1], "--settings");
        assert_eq!(launch.command[3], "--append-system-prompt");
        assert_eq!(launch.command[4], SELF_LABEL_INSTRUCTIONS);
        assert_eq!(launch.command[5], "-c", "the user's own trailing arg is kept, just pushed further out");

        let user = r#"{"model":"opus","hooks":{"Stop":[{"hooks":[{"type":"command","command":"mine"}]}]},"permissions":{"allow":["Bash(ls:*)"]}}"#;
        let mut launch = Launch {
            command: vec!["claude".into(), "--settings".into(), user.into()],
            env: vec![],
            agent_dir: dir.clone(),
        };
        Claude.prepare(&mut launch, &ctx).unwrap();
        let merged: Value = serde_json::from_str(&fs::read_to_string(&launch.command[4]).unwrap()).unwrap();
        assert_eq!(merged["model"], "opus");
        let stop = merged["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2, "user hook kept, ours appended");
        assert_eq!(stop[1]["hooks"][0]["command"], "'/opt/argus hook' claude");
        assert_eq!(stop[1]["hooks"][0]["async"], false, "Claude waits for our Stop answer");
        assert_eq!(merged["hooks"]["PreToolUse"][0]["hooks"][0]["async"], true);
        assert_eq!(merged["permissions"]["allow"], json!(["Bash(ls:*)", LABEL_RULE]), "user rules kept, ours added");
        fs::remove_dir_all(dir).unwrap();
    }
}
