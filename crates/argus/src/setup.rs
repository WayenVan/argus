//! `argus setup <agent>`: one-time changes to an agent's own configuration
//! that argus will not make behind the user's back.
//!
//! Every change that widens what an agent may do (permissions, sandbox
//! escapes, trust) and outlives one session goes through here, one
//! subcommand per agent: show exactly what will be written, write only after
//! the user agrees (or `--yes`), keep it in a file of argus's own, and undo
//! with `--remove`. Settings argus can pass for a single session (Claude's
//! `--settings`) need no setup. Plain `argus setup` does every agent it finds
//! on `PATH`.

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::manager::driver::codex;
use crate::output;

/// The agents argus has a driver for, in the order `argus setup` goes
/// through them, with whether each has a setup step.
const AGENTS: &[(&str, bool)] =
    &[("claude", false), ("codex", true), ("opencode", false), ("pi", false), ("omp", false)];

#[derive(Serialize)]
struct CodexSetup {
    path: String,
    rules: &'static [&'static str],
    /// Whether the rules are in place when the command finishes.
    installed: bool,
    /// Whether this run changed the file.
    changed: bool,
    /// Whether Codex has been told to trust argus's hooks. Only Codex can
    /// record that, when you attach to a Codex agent and accept its review.
    hooks_trusted: bool,
    /// The user said no when asked.
    #[serde(skip)]
    declined: bool,
}

impl CodexSetup {
    /// One line per finding, for people.
    fn describe(&self) -> Vec<String> {
        let mut lines = vec![self.describe_rules()];
        if !self.hooks_trusted {
            lines.push("hooks not trusted yet: attach to a Codex agent argus started and trust them once".into());
        }
        lines
    }

    fn describe_rules(&self) -> String {
        match (self.changed, self.installed) {
            _ if self.declined => "nothing written".into(),
            (false, true) => format!("already set up: {}", self.path),
            (false, false) => format!("nothing to remove: {}", self.path),
            (true, true) => format!("wrote {}; delete it, or run `argus setup codex --remove`, to undo", self.path),
            (true, false) => format!("removed {}", self.path),
        }
    }
}

#[derive(Serialize)]
struct AgentSetup {
    agent: &'static str,
    /// Where the program was found on `PATH`; absent when it was not.
    program: Option<String>,
    /// What its setup step did; absent when the agent needs none or was not found.
    setup: Option<CodexSetup>,
}

#[derive(Serialize)]
struct AllSetup {
    agents: Vec<AgentSetup>,
}

/// `argus setup codex`.
pub fn codex(yes: bool, remove: bool, json: bool) -> Result<()> {
    let setup = codex_step(yes, remove, json)?;
    if json {
        output::print(setup);
    } else {
        setup.describe().iter().for_each(|line| println!("{line}"));
    }
    Ok(())
}

/// `argus setup`: runs the setup step of every agent found on `PATH`, asking
/// before each change as its own subcommand would.
pub fn all(yes: bool, remove: bool, json: bool) -> Result<()> {
    let found: Vec<_> = AGENTS.iter().map(|&(agent, step)| (agent, step, find_program(agent))).collect();
    // Fail before doing anything rather than halfway through.
    let codex_pending =
        found.iter().any(|(agent, _, program)| *agent == "codex" && program.is_some()) && !codex::rules_installed();
    if codex_pending && !yes && !remove && (json || !io::stdin().is_terminal()) {
        bail!("pass --yes to set up without asking");
    }
    let mut results = Vec::new();
    for (agent, step, program) in found {
        let setup = match (&program, step) {
            (Some(_), true) => Some(codex_step(yes, remove, json)?),
            _ => None,
        };
        if !json {
            let lines = match (&program, &setup) {
                (None, _) => vec!["not found".to_string()],
                (Some(_), Some(setup)) => setup.describe(),
                (Some(_), None) => vec!["nothing to set up".to_string()],
            };
            for (i, line) in lines.iter().enumerate() {
                println!("{:<9} {line}", if i == 0 { agent } else { "" });
            }
        }
        results.push(AgentSetup { agent, program: program.map(|p| p.display().to_string()), setup });
    }
    if json {
        output::print(AllSetup { agents: results });
    }
    Ok(())
}

/// Shows the Codex rules that let agents set their labels and look at other
/// agents, then writes them to argus's own rules file once the user agrees (or with `--yes`). `remove`
/// deletes that file again.
fn codex_step(yes: bool, remove: bool, json: bool) -> Result<CodexSetup> {
    let path = codex::rules_file();
    let shown = path.display().to_string();
    let done = |installed: bool, changed: bool| CodexSetup {
        path: shown.clone(),
        rules: codex::RULES,
        installed,
        changed,
        hooks_trusted: codex::trusted(),
        declined: false,
    };

    if remove {
        let existed = path.exists();
        if existed {
            std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
        return Ok(done(false, existed));
    }
    if codex::rules_installed() {
        return Ok(done(true, false));
    }
    if !yes {
        if json || !io::stdin().is_terminal() {
            bail!("pass --yes to write {} without asking", path.display());
        }
        println!("Codex's sandbox blocks the manager's socket, so an agent cannot run");
        println!("`argus label self ...` or argus's read-only commands without your");
        println!("approval. These rules let exactly those commands run outside the");
        println!("sandbox, with no prompt, in every Codex session (a chained command such");
        println!("as `argus ps && other` still runs sandboxed):\n");
        for rule in codex::RULES {
            println!("    {rule}");
        }
        println!();
        print!("Write them to {}? [y/N] ", path.display());
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().lock().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            return Ok(CodexSetup { declined: true, ..done(false, false) });
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let text = format!("# Written by `argus setup codex`; delete this file to undo.\n{}\n", codex::RULES.join("\n"));
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(done(true, true))
}

/// The first executable `name` on `PATH`.
fn find_program(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|dir| dir.join(name)).find(|p| is_executable(p))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_agent_has_a_driver() {
        for &(agent, _) in AGENTS {
            assert_eq!(crate::manager::driver::for_kind(agent).kind(), agent);
        }
    }

    #[test]
    fn finds_programs_on_path() {
        assert!(find_program("sh").is_some());
        assert!(find_program("argus-no-such-program").is_none());
    }
}
