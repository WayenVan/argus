//! The output convention every data-printing command follows: text for
//! people by default, one JSON object per line with `--json`.
//!
//! - Every JSON object carries `"schema": 1`. Fields are only ever added.
//! - Agents always appear as an [`AgentView`].
//! - Streams print one object per line (JSON Lines); a one-shot command
//!   prints exactly one line.
//! - On failure stdout stays empty and stderr gets
//!   `{"schema":1,"error":{"code":...,"message":...}}`.

use std::io::Write;

use argus_proto::msg::{AgentInfo, Availability, now_secs};
use serde::Serialize;

use crate::errors;

pub const SCHEMA: u32 = 1;

#[derive(clap::Args, Clone, Copy, Debug, Default)]
pub struct OutputArgs {
    /// Print JSON (one object per line) instead of text
    #[arg(long)]
    pub json: bool,
}

/// An agent as JSON output shows it: every `AgentInfo` field plus values
/// derived from them, so callers do not have to.
#[derive(Serialize)]
pub struct AgentView<'a> {
    #[serde(flatten)]
    pub info: &'a AgentInfo,
    /// The name's group path; `null` at the top level.
    pub group: Option<&'a str>,
    pub availability: Availability,
    /// Seconds since `activity` last changed.
    pub activity_age_secs: Option<u64>,
}

impl<'a> AgentView<'a> {
    pub fn new(info: &'a AgentInfo) -> Self {
        AgentView {
            info,
            group: info.name.rsplit_once('/').map(|(group, _)| group),
            availability: info.availability(),
            activity_age_secs: info.activity_since.map(|since| now_secs().saturating_sub(since)),
        }
    }
}

/// `{"agents": [...]}`.
#[derive(Serialize)]
pub struct AgentList<'a> {
    pub agents: Vec<AgentView<'a>>,
}

impl<'a> AgentList<'a> {
    pub fn new(agents: &'a [AgentInfo]) -> Self {
        AgentList { agents: agents.iter().map(AgentView::new).collect() }
    }
}

/// `{"agent": {...}}`.
#[derive(Serialize)]
pub struct OneAgent<'a> {
    pub agent: AgentView<'a>,
}

impl<'a> OneAgent<'a> {
    pub fn new(agent: &'a AgentInfo) -> Self {
        OneAgent { agent: AgentView::new(agent) }
    }
}

/// `argus run --json`: the new agent and any setup warnings.
#[derive(Serialize)]
pub struct AgentWithWarnings<'a> {
    pub agent: AgentView<'a>,
    pub warnings: &'a [String],
}

// Bodies are structs, not `json!` maps: a map would sort the agent's fields.
#[derive(Serialize)]
struct Envelope<T> {
    schema: u32,
    #[serde(flatten)]
    body: T,
}

/// One JSON line, `body` (a struct or map) with `schema` added.
pub fn to_line(body: impl Serialize) -> String {
    serde_json::to_string(&Envelope { schema: SCHEMA, body }).expect("output values serialize")
}

/// Prints one JSON line to stdout. A closed stdout is not an error.
pub fn print(body: impl Serialize) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{}", to_line(body));
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: ErrorDetail<'a>,
}

#[derive(Serialize)]
struct ErrorDetail<'a> {
    code: &'a str,
    message: String,
}

/// Prints a failed command's error to stderr.
pub fn report(e: &anyhow::Error, json: bool) {
    if json {
        eprintln!(
            "{}",
            to_line(ErrorBody { error: ErrorDetail { code: errors::code_of(e), message: format!("{e:#}") } })
        );
    } else {
        eprintln!("argus: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use argus_proto::msg::AgentStatus;
    use serde_json::{Value, json};

    fn agent(name: &str) -> AgentInfo {
        serde_json::from_value(json!({
            "id": 7, "name": name, "kind": "claude", "command": ["claude"], "cwd": "/w",
            "created_at": 0, "status": "running", "activity": "tool:Bash"
        }))
        .unwrap()
    }

    #[test]
    fn agent_view_adds_derived_fields() {
        let info = agent("team/api/claude-1");
        let view: Value = serde_json::from_str(&to_line(json!({ "agent": AgentView::new(&info) }))).unwrap();
        assert_eq!(view["schema"], 1);
        assert_eq!(view["agent"]["id"], 7);
        assert_eq!(view["agent"]["activity"], "tool:Bash");
        assert_eq!(view["agent"]["group"], "team/api");
        assert_eq!(view["agent"]["availability"], "active");

        let mut top = agent("claude-1");
        top.status = AgentStatus::Exited;
        let view = serde_json::to_value(AgentView::new(&top)).unwrap();
        assert_eq!(view["group"], Value::Null);
        assert_eq!(view["availability"], "exited");
    }

    #[test]
    fn errors_keep_their_code_and_exit_status() {
        let e = errors::coded(errors::NOT_READY, "claude-1 is working").context("sending");
        assert_eq!(errors::code_of(&e), errors::NOT_READY);
        assert_eq!(errors::exit_status(&e), 75);
        assert_eq!(errors::exit_status(&errors::coded(errors::TIMEOUT, "")), 124);
        let plain = anyhow::anyhow!("boom");
        assert_eq!((errors::code_of(&plain), errors::exit_status(&plain)), (errors::FAILED, 1));
    }
}
