//! argus: run agents under per-agent holders, managed by one manager.
//!
//! This binary is both the client (every user-facing subcommand) and the
//! manager (`argus manager run`, started automatically by the first client).

mod client;
mod manager;
mod naming;

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
        /// Program to run; its name becomes the agent kind
        kind: String,
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
        #[arg(long)]
        json: bool,
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

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Run { name, group, cwd, kind, args } => client::run(name, group, cwd, kind, args),
        Command::Ps { prefix, all, json } => client::ps(prefix, all, json),
        Command::Kill { target, signal } => client::kill(target, signal),
        Command::Rm { target } => client::rm(target),
        Command::Manager { action } => match action {
            ManagerAction::Start => client::manager_start(),
            ManagerAction::Stop { kill_agents } => client::manager_stop(kill_agents),
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
