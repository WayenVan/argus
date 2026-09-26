//! argus: run agents under per-agent holders, managed by one manager.
//!
//! This binary is both the client (every user-facing subcommand) and the
//! manager (`argus manager run`, started automatically by the first client).

mod attach;
mod client;
mod errors;
mod manager;
mod naming;
mod output;
mod query;
mod setup;
mod stream;
mod term;
mod theme;
mod tmux;
mod tui;
mod wait;

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use output::OutputArgs;

#[derive(Parser)]
#[command(name = "argus", version = argus_proto::BUILD, about = "Lightweight manager for long-running terminal agents")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start an agent and attach to it (use -d to leave it running in the background)
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
        /// Start in the background instead of attaching right away
        #[arg(short, long)]
        detach: bool,
        /// Label as key=value (repeatable)
        #[arg(short, long = "label", value_name = "KEY=VALUE", value_parser = naming::parse_label)]
        label: Vec<(String, String)>,
        /// Treat the program as this kind of agent (e.g. a wrapper script for claude)
        #[arg(long)]
        kind: Option<String>,
        #[command(flatten)]
        output: OutputArgs,
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
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Show one agent in full
    Inspect {
        /// ID, name, or `self` for the agent running this command
        target: String,
        /// Also print its current screen as plain text
        #[arg(long)]
        screen: bool,
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Agents working in a directory, and whether they are all free.
    /// Leaves out the agent running this command
    Status {
        /// Directory (default: the current one)
        path: Option<std::path::PathBuf>,
        /// Which working directories count as in PATH
        #[arg(long, value_enum, default_value = "under")]
        scope: query::Scope,
        /// Include exited agents
        #[arg(short, long)]
        all: bool,
        /// Include the agent running this command
        #[arg(long)]
        include_self: bool,
        /// Only agents with this label (repeatable; all must match)
        #[arg(short, long = "label", value_name = "KEY=VALUE", value_parser = naming::parse_label)]
        label: Vec<(String, String)>,
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Full-screen dashboard: a live thumbnail grid of every agent's screen
    Grid {
        /// Only agents whose name starts with this group prefix
        prefix: Option<String>,
        /// Only agents with this label (repeatable; all must match)
        #[arg(short, long = "label", value_name = "KEY=VALUE", value_parser = naming::parse_label)]
        label: Vec<(String, String)>,
    },
    /// Full-screen dashboard: a group-path tree with a live detail pane for
    /// the selected agent (same app as `argus grid`, opened on the tree mode)
    Tree {
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
        #[arg(short = 'n', long, value_name = "BYTES", conflicts_with = "screen")]
        bytes: Option<u64>,
        /// Keep printing new output until the agent exits
        #[arg(short, long, conflicts_with = "screen")]
        follow: bool,
        /// Write control sequences verbatim, including OSC 52 clipboard writes
        #[arg(long, conflicts_with = "screen")]
        raw: bool,
        /// Print the manager's virtual-terminal rendering of the agent's
        /// current screen instead of the output stream (a point-in-time
        /// snapshot; requires a running agent)
        #[arg(long)]
        screen: bool,
    },
    /// Type a prompt into an agent and press Enter, only while it is idle
    /// (or done); otherwise exit 75
    Send {
        target: String,
        /// The prompt; `-` reads it from stdin. Multi-line text is pasted
        text: String,
        /// Do not press Enter after the text
        #[arg(short = 'n', long)]
        no_enter: bool,
        /// Send even if the agent is not idle (never while it is blocked on
        /// a permission prompt)
        #[arg(long)]
        force: bool,
        /// Wait until the agent is idle instead of failing
        #[arg(short, long)]
        wait: bool,
        /// After sending, block until the agent finishes the turn this prompt
        /// started (like `wait --after`); prints the activity it ends in.
        /// Keeps waiting while it is blocked; fails with code `stuck` if the
        /// turn ends in an error or the agent goes unknown
        #[arg(long, conflicts_with = "no_enter")]
        then_wait: bool,
        /// Give up after this many seconds, counting both waits (exit 124)
        #[arg(long, value_name = "SECS")]
        timeout: Option<u64>,
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Rename an agent: a new last segment, a full path, or `group/`
    Rename {
        target: String,
        name: String,
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Move an agent into another group, keeping its last segment
    Mv {
        target: String,
        /// Destination group; `/` for the top level
        group: String,
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Set (key=value) or remove (key-) labels
    Label {
        /// ID, name, or `group/**`
        target: String,
        #[arg(required = true, value_name = "KEY=VALUE|KEY-")]
        changes: Vec<String>,
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Mark a finished agent as seen (done → idle)
    Ack {
        target: String,
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Print agent events as they happen
    Events {
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Block until agents are free (their turn is over) or reach another
    /// state; with several, until all have. One agent waited on to exit
    /// passes on its exit code
    Wait {
        /// IDs, names or `group/**`
        #[arg(required_unless_present = "dir")]
        targets: Vec<String>,
        /// Wait for every running agent working in this directory instead,
        /// as `argus status` lists them (not the one running this command)
        #[arg(long, value_name = "PATH", conflicts_with = "targets")]
        dir: Option<std::path::PathBuf>,
        /// With --dir: which working directories count as in PATH [default: under]
        #[arg(long, value_enum)]
        scope: Option<query::Scope>,
        /// With --dir: only agents with this label (repeatable)
        #[arg(short, long = "label", value_name = "KEY=VALUE", value_parser = naming::parse_label)]
        label: Vec<(String, String)>,
        /// The availability to wait for: `free`, `active`, `attention`,
        /// `unknown`, or `exited` (the process ended)
        #[arg(long, value_name = "AVAILABILITY", default_value = "free", value_parser = wait::parse_availability)]
        until: argus_proto::msg::Availability,
        /// Wait for one exact activity instead, such as `done` or `tool:Bash`
        #[arg(long, value_name = "ACTIVITY", value_parser = wait::parse_activity, conflicts_with = "until")]
        until_activity: Option<argus_proto::msg::Activity>,
        /// Only once the agent has finished a turn after turn N (its `turns`
        /// is past N), e.g. the `turn` `send --json` printed. One agent; keeps
        /// waiting while it is blocked; fails with code `stuck` if the turn
        /// ends in an error or the agent goes unknown first
        #[arg(long, value_name = "N", conflicts_with = "dir")]
        after: Option<u64>,
        /// Give up after this many seconds (exit code 124)
        #[arg(long, value_name = "SECS")]
        timeout: Option<u64>,
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Take over an agent's terminal (detach with Ctrl-\)
    Attach {
        /// ID, full group/name, or a globally unique final name
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
        /// Allow historical OSC 52 sequences to overwrite the clipboard
        #[arg(long, requires = "replay")]
        allow_clipboard_replay: bool,
    },
    /// Stop an agent (SIGTERM, then SIGKILL after 5s)
    Kill {
        /// ID, name, or `group/**`
        target: String,
        /// Signal number to send instead of SIGTERM
        #[arg(short, long)]
        signal: Option<i32>,
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Remove an exited agent
    Rm {
        target: String,
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Remove every exited agent
    Prune {
        /// Only agents under this group prefix
        prefix: Option<String>,
        /// Only agents that ended longer ago than this, e.g. 30m, 24h, 7d
        #[arg(long, value_name = "AGE", value_parser = parse_age)]
        older_than: Option<u64>,
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Manage the manager process
    Manager {
        #[command(subcommand)]
        action: ManagerAction,
    },
    /// One-time changes to an agent's own configuration that widen what it may do; shows them and asks first
    Setup {
        #[command(subcommand)]
        agent: SetupAgent,
    },
}

/// One subcommand per agent that needs a lasting change argus will not make
/// on its own (see `setup.rs`).
#[derive(Subcommand)]
enum SetupAgent {
    /// Let Codex agents run `argus label self` outside the sandbox, via a rule
    /// in $CODEX_HOME/rules/argus.rules
    Codex {
        /// Write the rule without asking
        #[arg(long, short)]
        yes: bool,
        /// Delete argus's rules file instead
        #[arg(long, conflicts_with = "yes")]
        remove: bool,
        #[command(flatten)]
        output: OutputArgs,
    },
}

#[derive(Subcommand)]
enum ManagerAction {
    /// Start the manager if it is not running
    Start {
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Restart the manager (e.g. after upgrading); running agents keep running
    Restart {
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Stop the manager; running agents keep running
    Stop {
        /// Also kill every running agent
        #[arg(long)]
        kill_agents: bool,
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Show whether the manager is running
    Status {
        #[command(flatten)]
        output: OutputArgs,
    },
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

impl Command {
    /// The output options of a command that prints data; `None` for the
    /// interactive ones.
    fn output(&self) -> Option<OutputArgs> {
        match self {
            Command::Run { output, .. }
            | Command::Ps { output, .. }
            | Command::Inspect { output, .. }
            | Command::Status { output, .. }
            | Command::Send { output, .. }
            | Command::Rename { output, .. }
            | Command::Mv { output, .. }
            | Command::Label { output, .. }
            | Command::Ack { output, .. }
            | Command::Events { output }
            | Command::Wait { output, .. }
            | Command::Kill { output, .. }
            | Command::Rm { output, .. }
            | Command::Prune { output, .. }
            | Command::Setup { agent: SetupAgent::Codex { output, .. } }
            | Command::Manager {
                action:
                    ManagerAction::Start { output }
                    | ManagerAction::Restart { output }
                    | ManagerAction::Stop { output, .. }
                    | ManagerAction::Status { output },
            } => Some(*output),
            Command::Attach { .. }
            | Command::Grid { .. }
            | Command::Tree { .. }
            | Command::Logs { .. }
            | Command::Manager { action: ManagerAction::Run } => None,
        }
    }
}

impl Command {
    /// Every agent target this command takes, for expanding `self`.
    fn targets(&mut self) -> Vec<&mut String> {
        match self {
            Command::Attach { target, .. }
            | Command::Inspect { target, .. }
            | Command::Logs { target, .. }
            | Command::Send { target, .. }
            | Command::Rename { target, .. }
            | Command::Mv { target, .. }
            | Command::Label { target, .. }
            | Command::Ack { target, .. }
            | Command::Kill { target, .. }
            | Command::Rm { target, .. } => vec![target],
            Command::Wait { targets, .. } => targets.iter_mut().collect(),
            Command::Run { .. }
            | Command::Ps { .. }
            | Command::Status { .. }
            | Command::Grid { .. }
            | Command::Tree { .. }
            | Command::Events { .. }
            | Command::Prune { .. }
            | Command::Setup { .. }
            | Command::Manager { .. } => vec![],
        }
    }
}

fn main() -> ExitCode {
    let mut cli = Cli::parse();
    let json = cli.command.output().is_some_and(|o| o.json);
    if let Err(e) = cli.command.targets().into_iter().try_for_each(naming::expand_self) {
        output::report(&e, json);
        return ExitCode::from(errors::exit_status(&e));
    }
    let result = match cli.command {
        Command::Run { name, group, cwd, detach, label, kind, output, program, args } => client::run(
            client::RunOptions { name, group, cwd, labels: label, kind, attach: !detach, json: output.json },
            program,
            args,
        ),
        Command::Attach { target, ro, steal, replay, allow_clipboard_replay } => client::attach(
            target,
            attach::Options { readonly: ro, steal, replay, allow_clipboard_replay, shared_screen: false },
        ),
        Command::Ps { prefix, all, label, watch, output } => {
            client::ps(client::PsOptions { prefix, all, labels: label, json: output.json, watch })
        }
        Command::Inspect { target, screen, output } => query::inspect(target, screen, output.json),
        Command::Status { path, scope, all, include_self, label, output } => {
            query::status(query::StatusOptions { path, scope, all, include_self, labels: label, json: output.json })
        }
        Command::Grid { prefix, label } => tui::run(tui::Mode::Grid, prefix, label),
        Command::Tree { prefix, label } => tui::run(tui::Mode::Tree, prefix, label),
        Command::Logs { target, bytes, follow, raw, screen } => stream::logs(target, bytes, follow, raw, screen),
        Command::Send { target, text, no_enter, force, wait, then_wait, timeout, output } => stream::send(
            target,
            stream::SendOptions { text, enter: !no_enter, force, wait, then_wait, timeout, json: output.json },
        ),
        Command::Rename { target, name, output } => client::rename(target, name, output.json),
        Command::Mv { target, group, output } => client::mv(target, group, output.json),
        Command::Label { target, changes, output } => client::label(target, changes, output.json),
        Command::Events { output } => stream::events(output.json),
        Command::Ack { target, output } => client::ack(target, output.json),
        Command::Wait { targets, dir, scope, label, until, until_activity, after, timeout, output } => {
            let waited = match dir {
                Some(path) => Ok(wait::Waited::Dir {
                    path: Some(path),
                    scope: scope.unwrap_or(query::Scope::Under),
                    labels: label,
                }),
                None if scope.is_some() || !label.is_empty() => Err(anyhow::anyhow!("--scope and --label need --dir")),
                None => Ok(wait::Waited::Targets(targets)),
            };
            let goal = until_activity.map_or(wait::Goal::Availability(until), wait::Goal::Activity);
            waited.and_then(|waited| wait::wait(waited, goal, after, timeout, output.json))
        }
        Command::Kill { target, signal, output } => client::kill(target, signal, output.json),
        Command::Rm { target, output } => client::rm(target, output.json),
        Command::Prune { prefix, older_than, output } => client::prune(prefix, older_than, output.json),
        Command::Manager { action } => match action {
            ManagerAction::Start { output } => client::manager_start(output.json),
            ManagerAction::Stop { kill_agents, output } => client::manager_stop(kill_agents, output.json),
            ManagerAction::Restart { output } => client::manager_restart(output.json),
            ManagerAction::Status { output } => client::manager_status(output.json),
            ManagerAction::Run => manager::run(),
        },
        Command::Setup { agent: SetupAgent::Codex { yes, remove, output } } => setup::codex(yes, remove, output.json),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            output::report(&e, json);
            ExitCode::from(errors::exit_status(&e))
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

    /// Commands that print nothing a script would parse: interactive or
    /// raw byte streams.
    const NO_JSON: &[&str] = &["attach", "grid", "tree", "logs", "manager run"];

    fn leaf_commands(cmd: &clap::Command, path: &str, out: &mut Vec<(String, clap::Command)>) {
        for sub in cmd.get_subcommands() {
            let name = if path.is_empty() { sub.get_name().to_string() } else { format!("{path} {}", sub.get_name()) };
            if sub.has_subcommands() {
                leaf_commands(sub, &name, out);
            } else {
                out.push((name, sub.clone()));
            }
        }
    }

    /// Every command either takes `--json` and honors it, or is listed as
    /// not printing data, so a new command cannot forget the convention.
    #[test]
    fn every_command_follows_the_output_convention() {
        use clap::{CommandFactory, Parser};
        let mut commands = Vec::new();
        leaf_commands(&super::Cli::command(), "", &mut commands);
        for (name, cmd) in commands {
            let has_json = cmd.get_arguments().any(|a| a.get_long() == Some("json"));
            let listed = NO_JSON.contains(&name.as_str());
            assert!(has_json != listed, "{name}: takes --json: {has_json}, listed as without: {listed}");
            if !has_json {
                continue;
            }
            let mut argv: Vec<String> = vec!["argus".into()];
            argv.extend(name.split(' ').map(String::from));
            // Every positional that parses without `--`, required or not.
            for arg in cmd.get_positionals().filter(|a| !a.is_last_set()) {
                argv.push(format!("<{}>", arg.get_id()));
            }
            argv.push("--json".into());
            let cli = super::Cli::try_parse_from(&argv).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(cli.command.output().is_some_and(|o| o.json), "{name}: --json is not wired to Command::output");
        }
    }

    /// Every command the launch instructions show still parses, so they
    /// cannot drift from the CLI.
    #[test]
    fn instruction_commands_parse() {
        use clap::Parser;
        let text = [
            include_str!("instructions/core.md"),
            include_str!("instructions/labels.md"),
            include_str!("instructions/coordination.md"),
        ]
        .concat();
        let mut checked = 0;
        for line in text.lines() {
            let line = line.trim_start().trim_start_matches('`');
            if !line.starts_with("argus ") || line.contains('\'') {
                continue;
            }
            let command = line.split("  ").next().unwrap().split('`').next().unwrap();
            let command = command
                .replace("<id>...", "1 2")
                .replace("<id>", "1")
                .replace("<secs>", "5")
                .replace("<activity>", "done")
                .replace("<n>", "3")
                .replace("<group>", "claude-1")
                .replace(['[', ']'], "");
            let args: Vec<&str> = command.split_whitespace().collect();
            if let Err(e) = super::Cli::try_parse_from(&args) {
                panic!("{command}: {e}");
            }
            checked += 1;
        }
        assert!(checked >= 7, "only {checked} commands found");
    }

    /// `docs/src/reference/cli.md` is generated from the clap definitions.
    /// Regenerate with `ARGUS_UPDATE_DOCS=1 cargo test -p argus cli_reference`.
    #[test]
    fn cli_reference_is_current() {
        let options =
            clap_markdown::MarkdownOptions::new().title("CLI".into()).show_footer(false).show_table_of_contents(false);
        let generated = clap_markdown::help_markdown_custom::<super::Cli>(&options);
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/src/reference/cli.md");
        if std::env::var_os("ARGUS_UPDATE_DOCS").is_some() {
            std::fs::write(path, &generated).unwrap();
            return;
        }
        let current = std::fs::read_to_string(path).unwrap_or_default();
        assert!(
            current == generated,
            "docs/src/reference/cli.md is stale; run `ARGUS_UPDATE_DOCS=1 cargo test -p argus cli_reference`"
        );
    }
}
