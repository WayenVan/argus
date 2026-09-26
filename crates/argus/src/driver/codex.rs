//! Codex.
//!
//! Hooks are injected per session with `-c hooks.<Event>=[…]` config
//! overrides, so no user file is touched. Codex runs a hook only after the
//! user has trusted its exact definition; for `-c` hooks the trust is stored
//! in `config.toml` under `hooks.state."/<session-flags>/config.toml:<event>:0:0"`,
//! independent of the working directory. The definitions below are therefore
//! frozen: changing any of them makes every user review the hooks again.
//! (`Stop` changed once, from `async=true` to `async=false`, so that Codex
//! reads its answer; see `hold_stop`.)
//!
//! Codex only fires `SessionStart` with the first prompt, so a freshly started
//! agent reports nothing until then. It draws its prompt first and a startup
//! dialog (trust this folder, review hooks) over it about a second later, so
//! it counts as ready only once its cursor has stayed up for `STEADY_CURSOR`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde_json::Value;

use super::{
    Context, Driver, DriverReport, Hint, InteractionChange, Launch, SELF_LABEL_INSTRUCTIONS, ScreenCheck, block_stop,
    field, hook_command, interaction_request,
};

pub struct Codex;

/// Measured: startup dialogs replace the prompt 1.0–1.2 s after it appears.
const STEADY_CURSOR: Duration = Duration::from_secs(3);

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

    fn ready_on_cursor(&self) -> Option<Duration> {
        Some(STEADY_CURSOR)
    }

    fn prepare(&self, launch: &mut Launch, ctx: &Context) -> Result<Option<String>> {
        // Independent of hooks: ARGUS_AGENT_ID is set on the process
        // environment regardless, so self-labeling works even without them.
        // A user's own `-c developer_instructions=` comes later in argv and
        // wins, same as any other config override; that's a soft loss (the
        // agent just won't know the label convention), not worth a warning.
        // Same soft loss on `codex resume` of a session started outside
        // argus: Codex replays the instructions stored with the session and
        // ignores this override (see docs/src/agents/codex.md).
        launch.command.splice(1..1, developer_instructions_override());

        let Some(hook_exe) = &ctx.hook_exe else {
            return Ok(Some("argus-hook is not installed next to argus; activity will not be tracked".into()));
        };
        // Checked on the user's own arguments, before ours are added.
        let overridden = launch.command[1..].iter().any(|a| a.starts_with("hooks.") || a.starts_with("-chooks."));
        let overrides = hook_overrides(&hook_command(hook_exe, "codex"));
        launch.command.splice(1..1, overrides);

        let mut warnings = Vec::new();
        if overridden {
            warnings.push("your own -c hooks.* overrides may replace argus's hooks for those events");
        } else if !trusted() {
            warnings.push("Codex will ask you to review argus's hooks once: attach to this agent and trust them");
        }
        if !rules_installed() {
            warnings.push("Codex's sandbox blocks `argus label` and `argus ps`/`status`/`inspect`/`wait`, so they may need your approval: run `argus setup codex` once to allow them");
        }
        Ok((!warnings.is_empty()).then(|| warnings.join("; ")))
    }

    fn translate(&self, event: &Value) -> DriverReport {
        DriverReport { hint: hint(event), interaction: interaction(event) }
    }

    /// Codex takes Claude's answer: a block with a reason continues the turn
    /// with the reason as the next input.
    fn hold_stop(&self, event: &Value, reason: &str) -> Option<String> {
        block_stop(event, reason)
    }
}

fn hint(event: &Value) -> Hint {
    match field(event, "hook_event_name").unwrap_or_default() {
        "SessionStart" => match field(event, "source") {
            Some("compact") => Hint::Ignore,
            _ => Hint::SessionStart,
        },
        "UserPromptSubmit" | "PostToolUse" => Hint::Working,
        "PreToolUse" => Hint::Tool(field(event, "tool_name").unwrap_or("tool").to_string()),
        "Stop" => Hint::Done,
        // The user interrupted the turn and is presumably about to type.
        "Interrupt" => Hint::Interrupted,
        // `PermissionRequest` is only an interaction; see `interaction`.
        _ => Hint::Ignore,
    }
}

fn interaction(event: &Value) -> Option<InteractionChange> {
    (field(event, "hook_event_name") == Some("PermissionRequest")).then(|| InteractionChange::Opened {
        request: interaction_request(event),
        // Codex may route the request to its automatic reviewer. This fires
        // before the reviewer decides whether a person must answer, and no
        // later hook says it reached one: only the approval prompt on screen
        // does.
        confirm_after: None,
        on_screen: Some(approval_check(event)),
    })
}

/// How many of the screen's last non-blank rows the approval prompt is
/// looked for in. Codex draws it in place of the composer, at the bottom.
const PROMPT_ROWS: usize = 16;
/// How much of the requested command must appear on screen. A prefix, since
/// Codex may shorten a long command.
const COMMAND_PREFIX: usize = 40;

/// Recognizes Codex's approval prompt for the request in `event`:
///
/// ```text
///   Would you like to run the following command?
///
///   $ open -a Calculator
///
/// › 1. Yes, proceed (y)
///   2. Yes, and don't ask again for commands that start with `open -a Calculator` (p)
///   3. No, and tell Codex what to do differently (esc)
///
///   Press enter to confirm or esc to cancel
/// ```
///
/// It checks the prompt's structure rather than its wording, which changes
/// between Codex versions: a numbered list of choices with one selected, key
/// hints after the choices, and an enter/esc footer. Two of them are enough,
/// or one along with the requested command from the hook, which does not
/// depend on Codex's wording at all.
fn approval_check(event: &Value) -> ScreenCheck {
    let command = event
        .get("tool_input")
        .and_then(|input| input.get("command"))
        .and_then(Value::as_str)
        .and_then(|c| c.lines().map(str::trim).find(|l| !l.is_empty()))
        .map(|line| squeeze(line).chars().take(COMMAND_PREFIX).collect::<String>())
        .filter(|c| c.chars().count() >= 3);
    Arc::new(move |screen| shows_approval(screen, command.as_deref()))
}

fn shows_approval(screen: &vt100::Screen, command: Option<&str>) -> bool {
    let (_, cols) = screen.size();
    let mut rows: Vec<String> = screen.rows(0, cols).collect();
    while rows.last().is_some_and(|r| r.trim().is_empty()) {
        rows.pop();
    }
    let rows: Vec<&str> = rows.iter().map(String::as_str).filter(|r| !r.trim().is_empty()).collect();
    let rows = &rows[rows.len().saturating_sub(PROMPT_ROWS)..];

    let choices = choice_rows(rows);
    let listed = choices.len() >= 2 && choices.iter().any(|&(_, selected)| selected);
    let hinted = choices.iter().filter(|&&(i, _)| ends_with_key_hint(rows[i])).count() >= 2;
    let footer = choices.last().is_some_and(|&(last, _)| {
        rows[last + 1..].iter().any(|r| {
            let r = r.to_lowercase();
            r.contains("enter") && r.contains("esc")
        })
    });
    let structure = [listed, hinted, footer].iter().filter(|&&f| f).count();
    let shows_command = command.is_some_and(|c| squeeze(&rows.concat()).contains(c));
    structure >= 2 || (structure >= 1 && shows_command)
}

/// The longest run of rows numbered `1.`, `2.`, … in order, as (row index,
/// whether Codex marks it selected with `›`). Rows between numbered ones,
/// such as a wrapped choice, do not break the run.
fn choice_rows(rows: &[&str]) -> Vec<(usize, bool)> {
    let mut best = Vec::new();
    let mut run: Vec<(usize, bool)> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let mut text = row.trim_start();
        let selected = text.starts_with('›');
        if selected {
            text = text['›'.len_utf8()..].trim_start();
        }
        let digits = text.chars().take_while(char::is_ascii_digit).count();
        let Some(n) = text[..digits].parse::<usize>().ok().filter(|_| text[digits..].starts_with(". ")) else {
            continue;
        };
        if n == 1 {
            run.clear();
        } else if n != run.len() + 1 {
            continue;
        }
        run.push((i, selected));
        if run.len() > best.len() {
            best = run.clone();
        }
    }
    best
}

/// Whether a row ends with a short key hint such as `(y)` or `(esc)`.
fn ends_with_key_hint(row: &str) -> bool {
    let row = row.trim_end();
    let Some(inner) = row.strip_suffix(')').and_then(|r| r.rsplit_once('(')).map(|(_, k)| k) else {
        return false;
    };
    (1..=5).contains(&inner.len()) && inner.chars().all(|c| c.is_ascii_lowercase())
}

/// `s` without whitespace, so text matches however the terminal wrapped it.
fn squeeze(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// `-c hooks.<Event>=[…]` pairs. Frozen: see the module docs. The timeout is
/// 3s because Codex caps `Interrupt` hooks at 3s and warns on every start
/// otherwise; argus-hook itself gives up after 0.5s. Only `Stop` is
/// synchronous: Codex ignores what an async hook prints.
fn hook_overrides(command: &str) -> Vec<String> {
    let command = command.replace('\\', r"\\").replace('"', r#"\""#);
    HOOK_EVENTS
        .iter()
        .flat_map(|&(event, _)| {
            let is_async = event != "Stop";
            let value = format!(
                r#"hooks.{event}=[{{hooks=[{{type="command",command="{command}",async={is_async},timeout=3}}]}}]"#
            );
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

/// The exec-policy rules that let the agent run `argus label self …` and
/// argus's read-only commands outside Codex's sandbox, which blocks the
/// manager's socket. Codex reads rules only from files, so `argus setup codex`
/// writes them, after asking.
pub(crate) const RULES: &[&str] = &[
    r#"prefix_rule(pattern=["argus", "label", "self"], decision="allow")"#,
    r#"prefix_rule(pattern=["argus", "ps"], decision="allow")"#,
    r#"prefix_rule(pattern=["argus", "status"], decision="allow")"#,
    r#"prefix_rule(pattern=["argus", "inspect"], decision="allow")"#,
    r#"prefix_rule(pattern=["argus", "wait"], decision="allow")"#,
];

/// argus's own rules file, next to the user's.
pub(crate) fn rules_file() -> PathBuf {
    codex_home().join("rules").join("argus.rules")
}

/// Whether argus's rules file holds every rule in `RULES`.
pub(crate) fn rules_installed() -> bool {
    std::fs::read_to_string(rules_file()).is_ok_and(|text| RULES.iter().all(|r| text.lines().any(|l| l.trim() == *r)))
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
pub(crate) fn trusted() -> bool {
    let Ok(config) = std::fs::read_to_string(codex_home().join("config.toml")) else { return false };
    HOOK_EVENTS.iter().all(|(_, key)| config.contains(&format!("/<session-flags>/config.toml:{key}:0:0")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn interprets_events() {
        let hint = |v: Value| Codex.translate(&v).hint;
        assert_eq!(hint(json!({"hook_event_name":"UserPromptSubmit"})), Hint::Working);
        assert_eq!(hint(json!({"hook_event_name":"PreToolUse","tool_name":"shell"})), Hint::Tool("shell".into()));
        assert_eq!(hint(json!({"hook_event_name":"PermissionRequest"})), Hint::Ignore);
        assert_eq!(hint(json!({"hook_event_name":"Stop"})), Hint::Done);
        assert_eq!(hint(json!({"hook_event_name":"Interrupt"})), Hint::Interrupted);
    }

    fn screen(text: &str) -> vt100::Screen {
        let mut parser = vt100::Parser::new(30, 100, 0);
        parser.process(text.replace('\n', "\r\n").as_bytes());
        parser.screen().clone()
    }

    fn check(event: Value, text: &str) -> bool {
        approval_check(&event)(&screen(text))
    }

    fn request(command: &str) -> Value {
        json!({"hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":command}})
    }

    /// Captured from Codex 0.157.0.
    const APPROVAL: &str = "\
• Running open -a Calculator


  Would you like to run the following command?

  Environment: local

  Reason: 你要批准打开“计算器”来测试审批窗口吗？

  $ open -a Calculator


› 1. Yes, proceed (y)
  2. Yes, and don't ask again for commands that start with `open -a Calculator` (p)
  3. No, and tell Codex what to do differently (esc)

  Press enter to confirm or esc to cancel
";

    #[test]
    fn recognizes_the_approval_prompt() {
        assert!(check(request("open -a Calculator"), APPROVAL));
        // Without the command, the structure alone is enough.
        assert!(check(json!({"hook_event_name":"PermissionRequest"}), APPROVAL));
    }

    #[test]
    fn a_reworded_prompt_still_matches_with_the_command() {
        let text = "\
  $ cargo test --workspace

› 1. Oui
  2. Non

";
        assert!(check(request("cargo test --workspace"), text));
        assert!(!check(request("rm -rf build"), text));
    }

    #[test]
    fn a_wrapped_command_still_matches() {
        let long = format!("echo {}", "x".repeat(120));
        let text = format!("  $ {}\n    {}\n\n› 1. Yes\n  2. No\n", &long[..8], &long[8..]);
        assert!(check(request(&long), &text));
    }

    #[test]
    fn a_working_screen_does_not_match() {
        let text = "\
• Running open -a Calculator

  Steps:
  1. Open it
  2. Check it

◦ Working (4s • esc to interrupt)

› Ask Codex to do anything
";
        assert!(!check(request("open -a Calculator"), text));
    }

    #[test]
    fn only_the_bottom_of_the_screen_counts() {
        let filler = "  output\n".repeat(PROMPT_ROWS);
        let text = format!("{APPROVAL}{filler}› Ask Codex to do anything\n");
        assert!(!check(request("open -a Calculator"), &text));
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
        let stop = args.iter().find(|a| a.starts_with("hooks.Stop=")).unwrap();
        assert_eq!(
            stop,
            r#"hooks.Stop=[{hooks=[{type="command",command="'/opt/argus hook' codex",async=false,timeout=3}]}]"#
        );
    }

    #[test]
    fn holds_stop_once() {
        let held = Codex.hold_stop(&json!({"hook_event_name":"Stop","stop_hook_active":false}), "argus: x").unwrap();
        assert_eq!(serde_json::from_str::<Value>(&held).unwrap(), json!({"decision":"block","reason":"argus: x"}));
        assert_eq!(Codex.hold_stop(&json!({"hook_event_name":"Stop","stop_hook_active":true}), "argus: x"), None);
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
