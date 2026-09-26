//! `argus inspect`, `argus pending`, and `argus status`: read-only views meant as much for
//! other agents deciding what to do as for people.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use argus_proto::msg::{Activity, AgentInfo, Availability, InteractionPhase, Request, Response, now_secs};
use serde::Serialize;

use crate::client::{self, Conn};
use crate::manager::driver;
use crate::naming::self_id;
use crate::output::{self, AgentView};
use crate::turns::{self, Turn};

// ---------------------------------------------------------------------------
// inspect
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct Inspection<'a> {
    agent: AgentView<'a>,
    /// The current screen as plain text; `null` when not running.
    #[serde(skip_serializing_if = "Option::is_none")]
    screen: Option<Option<String>>,
    /// The last finished turns, oldest first (`--last`).
    #[serde(skip_serializing_if = "Option::is_none")]
    turns: Option<Vec<Turn>>,
}

pub fn inspect(target: String, screen: bool, last: Option<u64>, json: bool) -> Result<()> {
    let mut conn = Conn::connect()?;
    let agent = conn.find(&target)?;
    let screen = if screen && agent.status.is_live() {
        Some(Some(screen_text(&mut conn, agent.id)?))
    } else {
        screen.then_some(None)
    };
    let turns = match last {
        Some(n) => Some(turns::read(agent.id, n as usize).context("reading the turn log")?),
        None => None,
    };
    if json {
        output::print(Inspection { agent: AgentView::new(&agent), screen, turns });
        return Ok(());
    }
    print!("{}", describe(&agent));
    if let Some(turns) = turns {
        print!("{}", describe_turns(&turns));
    }
    if let Some(screen) = screen {
        println!("\nscreen:");
        println!("{}", screen.as_deref().unwrap_or("(not running)"));
    }
    Ok(())
}

fn describe_turns(turns: &[Turn]) -> String {
    if turns.is_empty() {
        return "\nturns: (none recorded)\n".into();
    }
    let now = now_secs();
    let mut out = String::new();
    for t in turns {
        out += &format!("\nturn {} ({}, {} ago)\n", t.turn, t.ended, client::age(now.saturating_sub(t.at)));
        if let Some(prompt) = &t.prompt {
            for line in printable(prompt).lines() {
                out += &format!("> {line}\n");
            }
        }
        match &t.reply {
            Some(reply) => out += &format!("{}\n", printable(reply)),
            None => out += "(no reply recorded)\n",
        }
    }
    out
}

/// Drops control characters other than newlines and tabs, so text an agent
/// wrote cannot drive the reader's terminal.
fn printable(text: &str) -> String {
    text.chars().filter(|&c| !c.is_control() || matches!(c, '\n' | '\t')).collect()
}

/// The screen as plain text: styles dropped, trailing blanks trimmed.
pub fn screen_text(conn: &mut Conn, id: u64) -> Result<String> {
    let request = Request::ScreenPreview { target: id.to_string(), rows: u16::MAX, cols: u16::MAX };
    let Response::ScreenPreview { lines } = conn.request(&request)? else {
        bail!("unexpected reply to ScreenPreview");
    };
    let mut text: Vec<String> = lines
        .iter()
        .map(|line| line.iter().map(|span| span.text.as_str()).collect::<String>().trim_end().into())
        .collect();
    while text.last().is_some_and(String::is_empty) {
        text.pop();
    }
    Ok(text.join("\n"))
}

fn describe(a: &AgentInfo) -> String {
    let now = now_secs();
    let ago = |t: u64| format!("{} ago", client::age(now.saturating_sub(t)));
    let status = match a.exit_code {
        Some(code) => format!("{} (code {code})", a.status.as_str()),
        None => a.status.as_str().to_string(),
    };
    let mut rows = vec![
        ("id", a.id.to_string()),
        ("name", a.name.clone()),
        ("group", a.name.rsplit_once('/').map_or("-", |(g, _)| g).to_string()),
        ("kind", a.kind.clone()),
        ("status", status),
        (
            "activity",
            match a.activity_since {
                Some(since) => format!("{} (since {})", a.activity, ago(since)),
                None => a.activity.to_string(),
            },
        ),
        ("availability", a.availability().to_string()),
    ];
    if driver::for_kind(&a.kind).has_hooks() {
        rows.push(("turns", a.turns.to_string()));
    }
    rows.extend([("cwd", a.cwd.clone()), ("command", a.command.join(" ")), ("created", ago(a.created_at))]);
    if let Some(t) = a.exited_at {
        rows.push(("exited", ago(t)));
    }
    if a.status.is_live() {
        rows.push(("attached", a.attached.to_string()));
    }
    if let (Some(holder), Some(agent)) = (a.holder_pid, a.agent_pid) {
        rows.push(("pids", format!("holder {holder}, agent {agent}")));
    }
    for (k, v) in &a.labels {
        rows.push(("label", format!("{k}={v}")));
    }
    for p in &a.pending_interactions {
        let detail = p.summary.as_deref().unwrap_or(&p.kind);
        let options = if p.choices.is_empty() { String::new() } else { format!("; options: {}", p.choices.join(", ")) };
        rows.push(("interaction", format!("{} ({}, id {}){options}", detail, p.phase, p.id)));
    }
    if a.pending_interactions.iter().any(|p| p.phase == InteractionPhase::NeedsUser) || a.activity == Activity::Blocked
    {
        rows.push((
            "action",
            format!("argus attach {} (or argus inspect {} --screen for the exact prompt)", a.id, a.id),
        ));
    }
    let width = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0) + 1;
    rows.iter().map(|(k, v)| format!("{:<width$} {v}\n", format!("{k}:"), width = width)).collect()
}

// ---------------------------------------------------------------------------
// pending
// ---------------------------------------------------------------------------

pub fn pending(target: Option<String>, json: bool) -> Result<()> {
    let mut conn = Conn::connect()?;
    let agents: Vec<AgentInfo> = match target {
        Some(target) => vec![conn.find(&target)?],
        None => {
            let place = Place::new(None, Scope::Under)?;
            conn.list(true)?.into_iter().filter(|a| a.status.is_live() && place.contains(a) && has_pending(a)).collect()
        }
    };
    if json {
        output::print(output::AgentList::new(&agents));
        return Ok(());
    }
    if agents.is_empty() {
        println!("No pending interactions in this directory.");
        return Ok(());
    }
    for agent in &agents {
        if agent.pending_interactions.is_empty() {
            if agent.activity == Activity::Blocked {
                println!("{} ({}): blocked; prompt details unavailable", agent.name, agent.id);
                println!("  inspect: argus inspect {} --screen", agent.id);
                println!("  answer:  argus attach {}", agent.id);
            } else {
                println!("{} ({}): no pending interactions", agent.name, agent.id);
            }
            continue;
        }
        println!("{} ({}):", agent.name, agent.id);
        for p in &agent.pending_interactions {
            let detail = p.summary.as_deref().unwrap_or(&p.kind);
            println!("  {}: {} [id {}]", p.phase, one_line(detail), one_line(&p.id));
            if !p.choices.is_empty() {
                println!("    options: {}", p.choices.iter().map(|c| one_line(c)).collect::<Vec<_>>().join(", "));
            }
        }
        if agent.pending_interactions.iter().any(|p| p.phase == InteractionPhase::NeedsUser)
            || agent.activity == Activity::Blocked
        {
            println!("  answer: argus attach {}", agent.id);
        } else if agent.pending_interactions.iter().any(|p| p.phase == InteractionPhase::Observed) {
            println!("  check: argus inspect {} --screen (request may resolve automatically)", agent.id);
        }
    }
    Ok(())
}

fn has_pending(agent: &AgentInfo) -> bool {
    !agent.pending_interactions.is_empty() || agent.activity == Activity::Blocked
}

fn one_line(text: &str) -> String {
    printable(text).replace(['\n', '\t'], " ")
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Working directory is PATH or below it.
    Under,
    /// Working directory is exactly PATH.
    Exact,
    /// Working directory is in the same git repository as PATH, including
    /// its other worktrees.
    Repo,
}

/// A directory and which agents' working directories count as in it.
pub struct Place {
    pub path: PathBuf,
    pub scope: Scope,
    /// The repository's common `.git` for `Scope::Repo`.
    repo: Option<PathBuf>,
}

impl Place {
    /// `path` defaults to the current directory.
    pub fn new(path: Option<PathBuf>, scope: Scope) -> Result<Place> {
        let raw = match path {
            Some(p) => p,
            None => std::env::current_dir()?,
        };
        let path = raw.canonicalize().with_context(|| format!("no such directory: {}", raw.display()))?;
        let repo = match scope {
            Scope::Repo => {
                Some(git_common_dir(&path).with_context(|| format!("{} is not in a git repository", path.display()))?)
            }
            _ => None,
        };
        Ok(Place { path, scope, repo })
    }

    /// Touches the file system: callers checking often should cache it.
    pub fn contains(&self, agent: &AgentInfo) -> bool {
        let cwd = Path::new(&agent.cwd).canonicalize().unwrap_or_else(|_| agent.cwd.clone().into());
        match self.scope {
            Scope::Under => cwd.starts_with(&self.path),
            Scope::Exact => cwd == self.path,
            Scope::Repo => git_common_dir(&cwd) == self.repo,
        }
    }
}

pub struct StatusOptions {
    pub path: Option<PathBuf>,
    pub scope: Scope,
    pub all: bool,
    pub include_self: bool,
    pub labels: Vec<(String, String)>,
    pub json: bool,
}

#[derive(Serialize, Default)]
struct Summary {
    /// No running agent is doing anything or waiting on someone. True when
    /// no agent runs here at all.
    all_free: bool,
    active: usize,
    free: usize,
    attention: usize,
    unknown: usize,
    exited: usize,
}

#[derive(Serialize)]
struct Status<'a> {
    path: String,
    scope: Scope,
    /// The agent running this command, left out of `agents` unless
    /// `--include-self`; `null` outside argus.
    #[serde(rename = "self")]
    self_id: Option<u64>,
    summary: Summary,
    agents: Vec<AgentView<'a>>,
}

pub fn status(opts: StatusOptions) -> Result<()> {
    let place = Place::new(opts.path, opts.scope)?;
    let path = &place.path;
    let me = self_id();
    let agents: Vec<AgentInfo> = Conn::connect()?
        .list(true)?
        .into_iter()
        .filter(|a| opts.all || a.status.is_live())
        .filter(|a| opts.include_self || Some(a.id) != me)
        .filter(|a| opts.labels.iter().all(|(k, v)| a.labels.get(k) == Some(v)))
        .filter(|a| place.contains(a))
        .collect();
    let summary = summarize(&agents);
    if opts.json {
        output::print(Status {
            path: path.display().to_string(),
            scope: opts.scope,
            self_id: me,
            summary,
            agents: agents.iter().map(AgentView::new).collect(),
        });
        return Ok(());
    }
    let counts: Vec<String> = [
        ("active", summary.active),
        ("free", summary.free),
        ("attention", summary.attention),
        ("unknown", summary.unknown),
        ("exited", summary.exited),
    ]
    .iter()
    .filter(|(_, n)| *n > 0)
    .map(|(name, n)| format!("{n} {name}"))
    .collect();
    let scope = match opts.scope {
        Scope::Under => "and below",
        Scope::Exact => "exactly",
        Scope::Repo => "whole repository",
    };
    println!("{} ({scope})", path.display());
    let listed = if counts.is_empty() { "no agents".to_string() } else { counts.join(", ") };
    let excluded = match me {
        Some(id) if !opts.include_self => format!("; not counting this agent ({id})"),
        _ => String::new(),
    };
    println!("{listed}; all free: {}{excluded}", if summary.all_free { "yes" } else { "no" });
    if !agents.is_empty() {
        println!();
        print!("{}", client::format_table(&agents));
        if agents.iter().any(|a| a.activity == Activity::Blocked && a.status.is_live()) {
            println!("Blocked agent: run argus pending <id> for details; argus attach <id> to answer.");
        }
    }
    Ok(())
}

fn summarize(agents: &[AgentInfo]) -> Summary {
    let mut s = Summary::default();
    for a in agents {
        match a.availability() {
            Availability::Active => s.active += 1,
            Availability::Free => s.free += 1,
            Availability::Attention => s.attention += 1,
            Availability::Unknown => s.unknown += 1,
            Availability::Exited => s.exited += 1,
        }
    }
    s.all_free = s.active + s.attention + s.unknown == 0;
    s
}

/// The repository's shared `.git` directory, the same for all of its
/// worktrees. Found by walking up from `start`, without running git.
fn git_common_dir(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        let dotgit = dir.join(".git");
        if dotgit.is_dir() {
            return dotgit.canonicalize().ok();
        }
        if dotgit.is_file() {
            // A worktree (or submodule): `gitdir: <path>`.
            let text = std::fs::read_to_string(&dotgit).ok()?;
            let gitdir = dir.join(text.strip_prefix("gitdir:")?.trim());
            // A worktree's gitdir names the main one in `commondir`.
            return match std::fs::read_to_string(gitdir.join("commondir")) {
                Ok(common) => gitdir.join(common.trim()).canonicalize().ok(),
                Err(_) => gitdir.canonicalize().ok(),
            };
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(activity: &str, live: bool) -> AgentInfo {
        serde_json::from_value(serde_json::json!({
            "id": 1, "name": "a", "kind": "claude", "command": [], "cwd": "/",
            "created_at": 0, "status": if live { "running" } else { "exited" }, "activity": activity
        }))
        .unwrap()
    }

    #[test]
    fn pending_view_includes_reported_requests_and_blocked_fallback() {
        let mut a = agent("working", true);
        assert!(!has_pending(&a));
        a.pending_interactions = serde_json::from_value(serde_json::json!([{
            "id": "question-1", "kind": "question", "phase": "observed",
            "session_id": "session-1", "created_at": 1
        }]))
        .unwrap();
        assert!(has_pending(&a));
        a.pending_interactions.clear();
        a.activity = Activity::Blocked;
        assert!(has_pending(&a));
        assert_eq!(one_line("approve\n\u{1b}[31m yes\tno"), "approve [31m yes no");
    }

    #[test]
    fn all_free_ignores_exited_agents_and_holds_for_none() {
        assert!(summarize(&[]).all_free);
        assert!(summarize(&[agent("idle", true), agent("done", true), agent("working", false)]).all_free);
        for busy in ["working", "tool:Bash", "blocked", "error", "unknown"] {
            assert!(!summarize(&[agent("idle", true), agent(busy, true)]).all_free, "{busy}");
        }
    }

    #[test]
    fn worktrees_share_the_main_repository() {
        let root = std::env::temp_dir().join(format!("argus-git-{}", std::process::id()));
        let main = root.join("main");
        let tree = root.join("tree");
        std::fs::create_dir_all(main.join(".git/worktrees/tree")).unwrap();
        std::fs::create_dir_all(tree.join("src")).unwrap();
        std::fs::write(main.join(".git/worktrees/tree/commondir"), "../..\n").unwrap();
        std::fs::write(tree.join(".git"), format!("gitdir: {}\n", main.join(".git/worktrees/tree").display())).unwrap();
        let common = git_common_dir(&main).unwrap();
        assert_eq!(git_common_dir(&tree.join("src")), Some(common));
        assert_eq!(git_common_dir(Path::new("/")), None);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
