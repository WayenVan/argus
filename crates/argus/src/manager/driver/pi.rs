//! pi (the pi coding agent).
//!
//! pi has no command hooks, only in-process extensions. argus ships one
//! (`pi-extension.js`, compiled into argus and written next to Claude's
//! shared settings at startup) that forwards the relevant lifecycle events
//! through `argus-hook pi`, already flattened into the same events as
//! opencode's plugin.
//!
//! Both the extension (`-e`) and the self-label instructions
//! (`--append-system-prompt`) go on the command line, where pi takes any
//! number of each: a user's own `-e` and `--no-extensions` leave ours alone,
//! and nothing in `~/.pi` is touched. They go right after the program name,
//! ahead of any `--` that ends option parsing.

use std::fs;

use anyhow::{Context as _, Result};
use serde_json::Value;

use super::{
    Context, Driver, Hint, InteractionChange, Launch, SELF_LABEL_INSTRUCTIONS, plugin_hint, plugin_interaction,
};

pub struct Pi;

const EXTENSION_JS: &str = include_str!("pi-extension.js");
const EXTENSION_FILE: &str = "argus-pi.js";
/// Tells the extension where argus-hook is.
const HOOK_ENV: &str = "ARGUS_PI_HOOK";
/// The extension's event format; see the `v` field in `pi-extension.js`.
const EVENT_VERSION: u64 = 1;
/// pi's package-management commands, which take no options before them and
/// start no session.
const SUBCOMMANDS: &[&str] = &["install", "remove", "uninstall", "update", "list", "config", "auth"];

impl Driver for Pi {
    fn kind(&self) -> &'static str {
        "pi"
    }

    fn has_hooks(&self) -> bool {
        true
    }

    fn prepare(&self, launch: &mut Launch, ctx: &Context) -> Result<Option<String>> {
        Ok(inject(launch, ctx, SUBCOMMANDS, EXTENSION_FILE, HOOK_ENV))
    }

    fn interpret(&self, event: &Value) -> Hint {
        plugin_hint(event, EVENT_VERSION)
    }

    fn interaction(&self, event: &Value) -> Option<InteractionChange> {
        plugin_interaction(event, EVENT_VERSION, None)
    }
}

/// Puts the label instructions and, with argus-hook installed, the extension
/// right after the program name, for pi and its forks, which take any number
/// of both. Leaves `subcommands` alone. Returns the warning for `prepare`.
pub(super) fn inject(
    launch: &mut Launch,
    ctx: &Context,
    subcommands: &[&str],
    extension_file: &str,
    hook_env: &str,
) -> Option<String> {
    if launch.command.get(1).is_some_and(|arg| subcommands.contains(&arg.as_str())) {
        return None;
    }
    // Independent of the extension, like the other drivers' instructions.
    let mut args = vec!["--append-system-prompt".to_string(), SELF_LABEL_INSTRUCTIONS.to_string()];
    let warning = match &ctx.hook_exe {
        Some(hook_exe) => {
            args.extend(["-e".to_string(), ctx.dir.join(extension_file).to_string_lossy().into_owned()]);
            launch.env.retain(|(k, _)| k != hook_env);
            launch.env.push((hook_env.into(), hook_exe.to_string_lossy().into_owned()));
            None
        }
        None => Some("argus-hook is not installed next to argus; activity will not be tracked".into()),
    };
    launch.command.splice(1..1, args);
    warning
}

/// Writes the extension the agents load.
pub fn write_shared_files(ctx: &Context) -> Result<()> {
    let path = ctx.dir.join(EXTENSION_FILE);
    fs::write(&path, EXTENSION_JS).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn ctx(hook: bool) -> Context {
        Context { hook_exe: hook.then(|| PathBuf::from("/opt/argus-hook")), dir: PathBuf::from("/s/drivers") }
    }

    fn launch(command: &[&str]) -> Launch {
        Launch { command: command.iter().map(|s| s.to_string()).collect(), env: vec![], agent_dir: PathBuf::new() }
    }

    #[test]
    fn interprets_events() {
        let e = |name: &str| json!({"v": 1, "hook_event_name": name, "session_id": "s"});
        assert_eq!(Pi.interpret(&e("SessionStart")), Hint::SessionStart);
        assert_eq!(Pi.interpret(&e("UserPromptSubmit")), Hint::Working);
        assert_eq!(
            Pi.interpret(&json!({"v": 1, "hook_event_name": "PreToolUse", "tool_name": "bash"})),
            Hint::Tool("bash".into())
        );
        assert_eq!(Pi.interpret(&e("PermissionRequest")), Hint::WaitingApproval);
        assert_eq!(Pi.interpret(&e("Stop")), Hint::Done);
        assert_eq!(Pi.interpret(&e("StopFailure")), Hint::Error);
        assert_eq!(Pi.interpret(&e("Interrupt")), Hint::Interrupted);
        assert_eq!(Pi.interpret(&json!({"v": 2, "hook_event_name": "Stop"})), Hint::Ignore);
    }

    #[test]
    fn injects_ahead_of_the_users_arguments() {
        let mut l = launch(&["pi", "-e", "mine.ts", "--", "-not-an-option"]);
        assert_eq!(Pi.prepare(&mut l, &ctx(true)).unwrap(), None);
        assert_eq!(l.command[1], "--append-system-prompt");
        assert_eq!(l.command[2], SELF_LABEL_INSTRUCTIONS);
        assert_eq!(l.command[3..], ["-e", "/s/drivers/argus-pi.js", "-e", "mine.ts", "--", "-not-an-option"]);
        assert_eq!(l.env, [(HOOK_ENV.to_string(), "/opt/argus-hook".to_string())]);
    }

    #[test]
    fn without_argus_hook_only_the_instructions_go_in() {
        let mut l = launch(&["pi"]);
        assert!(Pi.prepare(&mut l, &ctx(false)).unwrap().is_some());
        assert_eq!(l.command.len(), 3);
        assert!(l.env.is_empty());
    }

    #[test]
    fn subcommands_are_left_alone() {
        let mut l = launch(&["pi", "install", "npm:foo"]);
        assert_eq!(Pi.prepare(&mut l, &ctx(true)).unwrap(), None);
        assert_eq!(l.command, ["pi", "install", "npm:foo"]);
        assert!(l.env.is_empty());
    }

    #[test]
    fn extension_is_valid_javascript() {
        let Ok(status) = std::process::Command::new("node")
            .args(["--input-type=module", "--check"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                child.stdin.take().unwrap().write_all(EXTENSION_JS.as_bytes())?;
                child.wait()
            })
        else {
            eprintln!("node not found; skipping the syntax check");
            return;
        };
        assert!(status.success(), "pi-extension.js has a syntax error");
    }
}
