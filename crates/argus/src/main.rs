//! argus: run agents under per-agent holders, managed by one manager.
//!
//! This binary is both the client (every user-facing subcommand) and the
//! manager (`argus manager run`, started automatically by the first client).

mod attach;
mod client;
mod manager;
mod naming;
mod stream;
mod view;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "argus", version, about = "Lightweight manager for long-running terminal agents")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start an agent in the background
    Run {
        /// Agent name (last path segment, or a full `group/name` path)
        #[arg(long)]
        name: Option<String>,
        /// Group to create the agent in (default: $ARGUS_GROUP)
        #[arg(long = "in", value_name = "GROUP")]
        group: Option<String>,
        /// Working directory (default: current directory)
        #[arg(long)]
        cwd: Option<std::path::PathBuf>,
        /// Attach right after starting
        #[arg(short, long)]
        attach: bool,
        /// Label as key=value (repeatable)
        #[arg(short, long = "label", value_name = "KEY=VALUE", value_parser = naming::parse_label)]
        label: Vec<(String, String)>,
        /// Treat the program as this kind of agent (e.g. a wrapper script for claude)
        #[arg(long)]
        kind: Option<String>,
        /// Program to run; its name selects the kind unless --kind is given
        program: String,
        /// Arguments passed to the program
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// List agents
    Ps {
        /// Only agents whose name starts with this group prefix
        prefix: Option<String>,
        /// Include exited agents
        #[arg(short, long)]
        all: bool,
        /// Only agents with this label (repeatable; all must match)
        #[arg(short, long = "label", value_name = "KEY=VALUE", value_parser = naming::parse_label)]
        label: Vec<(String, String)>,
        /// Keep the list on screen and update it as agents change
        #[arg(short, long, conflicts_with = "json")]
        watch: bool,
        #[arg(long)]
        json: bool,
    },
    /// Full-screen dashboard: a live thumbnail grid of every agent's screen
    View {
        /// Only agents whose name starts with this group prefix
        prefix: Option<String>,
        /// Only agents with this label (repeatable; all must match)
        #[arg(short, long = "label", value_name = "KEY=VALUE", value_parser = naming::parse_label)]
        label: Vec<(String, String)>,
    },
    /// Print an agent's recent output
    Logs {
        target: String,
        /// Only the last N bytes
        #[arg(short = 'n', long, value_name = "BYTES")]
        bytes: Option<u64>,
        /// Keep printing new output until the agent exits
        #[arg(short, long)]
        follow: bool,
    },
    /// Type text into an agent without attaching (Enter is pressed after it)
    Send {
        target: String,
        text: String,
        /// Do not press Enter after the text
        #[arg(short = 'n', long)]
        no_enter: bool,
    },
    /// Rename an agent: a new last segment, a full path, or `group/`
    Rename { target: String, name: String },
    /// Move an agent into another group, keeping its last segment
    Mv {
        target: String,
        /// Destination group; `/` for the top level
        group: String,
    },
    /// Set (key=value) or remove (key-) labels
    Label {
        /// ID, name, or `group/**`
        target: String,
        #[arg(required = true, value_name = "KEY=VALUE|KEY-")]
        changes: Vec<String>,
    },
    /// Mark a finished agent as seen (done → waiting_input)
    Ack { target: String },
    /// Print agent events as they happen
    Events {
        /// One JSON object per line
        #[arg(long)]
        json: bool,
    },
    /// Block until an agent exits (exiting with its code) or reaches an activity
    Wait {
        target: String,
        /// `exited`, or an activity such as `waiting`
        #[arg(long, default_value = "exited")]
        until: String,
        /// Give up after this many seconds (exit code 124)
        #[arg(long, value_name = "SECS")]
        timeout: Option<u64>,
    },
    /// Take over an agent's terminal (detach with Ctrl-\)
    Attach {
        /// ID or name
        target: String,
        /// Watch only: send no input and never resize the agent
        #[arg(long)]
        ro: bool,
        /// Disconnect every other attached terminal first
        #[arg(long)]
        steal: bool,
        /// Print recent output before live output
        #[arg(long)]
        replay: bool,
    },
    /// Stop an agent (SIGTERM, then SIGKILL after 5s)
    Kill {
        /// ID, name, or `group/**`
        target: String,
        /// Signal number to send instead of SIGTERM
        #[arg(short, long)]
        signal: Option<i32>,
    },
    /// Remove an exited agent
    Rm { target: String },
    /// Remove every exited agent
    Prune {
        /// Only agents under this group prefix
        prefix: Option<String>,
        /// Only agents that ended longer ago than this, e.g. 30m, 24h, 7d
        #[arg(long, value_name = "AGE", value_parser = parse_age)]
        older_than: Option<u64>,
    },
    /// Manage the manager process
    Manager {
        #[command(subcommand)]
        action: ManagerAction,
    },
}

#[derive(Subcommand)]
enum ManagerAction {
    /// Start the manager if it is not running
    Start,
    /// Restart the manager (e.g. after upgrading); running agents keep running
    Restart,
    /// Stop the manager; running agents keep running
    Stop {
        /// Also kill every running agent
        #[arg(long)]
        kill_agents: bool,
    },
    /// Show whether the manager is running
    Status,
    /// Run the manager in the foreground (used by auto-start)
    #[command(hide = true)]
    Run,
}

/// Parses `90s`, `30m`, `24h`, `7d` (or bare seconds) into seconds.
fn parse_age(s: &str) -> Result<u64, String> {
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = num.parse().map_err(|_| format!("expected a number with s/m/h/d, got {s:?}"))?;
    let scale = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => return Err(format!("unknown unit {unit:?}; use s, m, h or d")),
    };
    Ok(n * scale)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Run { name, group, cwd, attach, label, kind, program, args } => {
            client::run(client::RunOptions { name, group, cwd, labels: label, kind, attach }, program, args)
        }
        Command::Attach { target, ro, steal, replay } => {
            client::attach(target, attach::Options { readonly: ro, steal, replay })
        }
        Command::Ps { prefix, all, label, watch, json } => {
            client::ps(client::PsOptions { prefix, all, labels: label, json, watch })
        }
        Command::View { prefix, label } => view::run(prefix, label),
        Command::Logs { target, bytes, follow } => stream::logs(target, bytes, follow),
        Command::Send { target, text, no_enter } => client::send(target, text, !no_enter),
        Command::Rename { target, name } => client::rename(target, name),
        Command::Mv { target, group } => client::mv(target, group),
        Command::Label { target, changes } => client::label(target, changes),
        Command::Events { json } => stream::events(json),
        Command::Ack { target } => client::ack(target),
        Command::Wait { target, until, timeout } => stream::wait(target, until, timeout),
        Command::Kill { target, signal } => client::kill(target, signal),
        Command::Rm { target } => client::rm(target),
        Command::Prune { prefix, older_than } => client::prune(prefix, older_than),
        Command::Manager { action } => match action {
            ManagerAction::Start => client::manager_start(),
            ManagerAction::Stop { kill_agents } => client::manager_stop(kill_agents),
            ManagerAction::Restart => client::manager_restart(),
            ManagerAction::Status => client::manager_status(),
            ManagerAction::Run => manager::run(),
        },
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("argus: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_age;

    #[test]
    fn ages() {
        assert_eq!(parse_age("90"), Ok(90));
        assert_eq!(parse_age("30m"), Ok(1800));
        assert_eq!(parse_age("24h"), Ok(86400));
        assert_eq!(parse_age("7d"), Ok(604800));
        assert!(parse_age("3w").is_err());
        assert!(parse_age("h").is_err());
    }
}
