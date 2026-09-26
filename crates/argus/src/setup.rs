//! `argus setup <agent>`: one-time changes to an agent's own configuration
//! that argus will not make behind the user's back.
//!
//! Every change that widens what an agent may do (permissions, sandbox
//! escapes, trust) and outlives one session goes through here, one
//! subcommand per agent: show exactly what will be written, write only after
//! the user agrees (or `--yes`), keep it in a file of argus's own, and undo
//! with `--remove`. Settings argus can pass for a single session (Claude's
//! `--settings`) need no setup.

use std::io::{self, BufRead, IsTerminal, Write};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::manager::driver::codex;
use crate::output;

#[derive(Serialize)]
struct CodexSetup {
    path: String,
    rule: &'static str,
    /// Whether the rule is in place when the command finishes.
    installed: bool,
    /// Whether this run changed the file.
    changed: bool,
}

/// Shows the Codex rule that lets agents set their labels, then writes it to
/// argus's own rules file once the user agrees (or with `--yes`). `--remove`
/// deletes that file again.
pub fn codex(yes: bool, remove: bool, json: bool) -> Result<()> {
    let path = codex::rules_file();
    let report = |installed: bool, changed: bool| {
        let shown = path.display().to_string();
        if json {
            output::print(CodexSetup { path: shown, rule: codex::LABEL_RULE, installed, changed });
        } else if !changed {
            println!("{} {shown}", if installed { "already set up:" } else { "nothing to remove:" });
        } else if installed {
            println!("wrote {shown}; delete it, or run `argus setup codex --remove`, to undo");
        } else {
            println!("removed {shown}");
        }
    };

    if remove {
        let existed = path.exists();
        if existed {
            std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
        report(false, existed);
        return Ok(());
    }
    if codex::label_rule_installed() {
        report(true, false);
        return Ok(());
    }
    if !yes {
        if json || !io::stdin().is_terminal() {
            bail!("pass --yes to write {} without asking", path.display());
        }
        println!("Codex's sandbox blocks the manager's socket, so an agent cannot run");
        println!("`argus label self ...` without your approval. This rule lets exactly");
        println!("that command run outside the sandbox, with no prompt, in every Codex");
        println!("session (a chained command such as `argus label self ... && other` still");
        println!("runs sandboxed):\n");
        println!("    {}\n", codex::LABEL_RULE);
        print!("Write it to {}? [y/N] ", path.display());
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().lock().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            println!("nothing written");
            return Ok(());
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let text = format!("# Written by `argus setup codex`; delete this file to undo.\n{}\n", codex::LABEL_RULE);
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    report(true, true);
    Ok(())
}
