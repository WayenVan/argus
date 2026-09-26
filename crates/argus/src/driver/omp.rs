//! omp (oh-my-pi), a fork of pi.
//!
//! omp loads extensions and takes options the way pi does, so it is wired in
//! the same way (see `pi.rs`), with its own extension (`omp-extension.js`):
//! omp's lifecycle events differ from pi's (no `agent_settled`, real tool
//! approvals, in-place session switches).

use std::fs;

use anyhow::{Context as _, Result};
use serde_json::Value;

use super::{Context, Driver, DriverReport, Launch, pi, plugin_report};

pub struct Omp;

const EXTENSION_JS: &str = include_str!("omp-extension.js");
const EXTENSION_FILE: &str = "argus-omp.js";
/// Tells the extension where argus-hook is.
const HOOK_ENV: &str = "ARGUS_OMP_HOOK";
/// The extension's event format; see the `v` field in `omp-extension.js`.
const EVENT_VERSION: u64 = 1;

impl Driver for Omp {
    fn kind(&self) -> &'static str {
        "omp"
    }

    fn has_hooks(&self) -> bool {
        true
    }

    fn prepare(&self, launch: &mut Launch, ctx: &Context) -> Result<Option<String>> {
        // No subcommand is left alone: omp finds a subcommand behind leading
        // options and drops the launch options for it, `-e` and
        // `--append-system-prompt` included.
        Ok(pi::inject(launch, ctx, &[], EXTENSION_FILE, HOOK_ENV))
    }

    fn translate(&self, event: &Value) -> DriverReport {
        plugin_report(event, EVENT_VERSION, None)
    }

    /// Writes the extension the agents load.
    fn write_shared_files(&self, ctx: &Context) -> Result<()> {
        let path = ctx.dir.join(EXTENSION_FILE);
        fs::write(&path, EXTENSION_JS).with_context(|| format!("writing {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::SELF_LABEL_INSTRUCTIONS;
    use std::path::PathBuf;

    fn ctx() -> Context {
        Context { hook_exe: Some(PathBuf::from("/opt/argus-hook")), dir: PathBuf::from("/s/drivers") }
    }

    fn launch(command: &[&str]) -> Launch {
        Launch { command: command.iter().map(|s| s.to_string()).collect(), env: vec![], agent_dir: PathBuf::new() }
    }

    #[test]
    fn injects_its_own_extension() {
        let mut l = launch(&["omp", "fix the build"]);
        assert_eq!(Omp.prepare(&mut l, &ctx()).unwrap(), None);
        assert_eq!(l.command[1..3], ["--append-system-prompt", SELF_LABEL_INSTRUCTIONS]);
        assert_eq!(l.command[3..], ["-e", "/s/drivers/argus-omp.js", "fix the build"]);
        assert_eq!(l.env, [(HOOK_ENV.to_string(), "/opt/argus-hook".to_string())]);
    }

    #[test]
    fn subcommands_get_the_options_too() {
        let mut l = launch(&["omp", "commit"]);
        Omp.prepare(&mut l, &ctx()).unwrap();
        assert_eq!(l.command[3..], ["-e", "/s/drivers/argus-omp.js", "commit"]);
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
        assert!(status.success(), "omp-extension.js has a syntax error");
    }
}
