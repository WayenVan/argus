//! Agent names are paths (`research/claude-1`); a group is a name prefix.
//!
//! Each segment matches `[a-z0-9._-]+` and must not be all digits, so a bare
//! number on the command line always means an ID.

use anyhow::{Result, bail};
use argus_proto::msg::AgentInfo;

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("name must not be empty");
    }
    for segment in name.split('/') {
        validate_segment(segment).map_err(|e| anyhow::anyhow!("invalid name {name:?}: {e}"))?;
    }
    Ok(())
}

fn validate_segment(segment: &str) -> Result<()> {
    if segment.is_empty() {
        bail!("empty path segment");
    }
    if segment == "." || segment == ".." {
        bail!("segment {segment:?} is reserved");
    }
    if let Some(c) = segment.chars().find(|c| !matches!(c, 'a'..='z' | '0'..='9' | '.' | '_' | '-')) {
        bail!("character {c:?} not allowed (use a-z 0-9 . _ -)");
    }
    if segment.chars().all(|c| c.is_ascii_digit()) {
        bail!("segment {segment:?} is all digits and would look like an ID");
    }
    Ok(())
}

/// Normalizes a group given by `--in` or `ARGUS_GROUP`: no surrounding slashes.
pub fn normalize_group(group: &str) -> Result<Option<String>> {
    let group = group.trim_matches('/');
    if group.is_empty() {
        return Ok(None);
    }
    validate_name(group)?;
    Ok(Some(group.to_string()))
}

/// Derives the kind from the program name: `/usr/bin/Claude` → `claude`.
pub fn kind_of(program: &str) -> String {
    let base = program.rsplit('/').next().unwrap_or(program).to_ascii_lowercase();
    let kind: String =
        base.chars().map(|c| if matches!(c, 'a'..='z' | '0'..='9' | '.' | '_' | '-') { c } else { '-' }).collect();
    if validate_segment(&kind).is_ok() { kind } else { "agent".into() }
}

/// Picks `<group>/<kind>-<n>` with the smallest free `n` in that group.
pub fn default_name(group: Option<&str>, kind: &str, taken: impl Fn(&str) -> bool) -> String {
    (1..).map(|n| join(group, &format!("{kind}-{n}"))).find(|name| !taken(name)).expect("unbounded range")
}

pub fn join(group: Option<&str>, leaf: &str) -> String {
    match group {
        Some(g) => format!("{g}/{leaf}"),
        None => leaf.to_string(),
    }
}

/// Resolves a target to agent IDs.
///
/// - all digits → that ID;
/// - `group/**` or `group/` → every agent under the group;
/// - a full name → that agent;
/// - otherwise the last segment, if it is unique.
pub fn resolve<'a>(target: &str, agents: impl Iterator<Item = &'a AgentInfo> + Clone) -> Result<Vec<u64>> {
    if !target.is_empty() && target.chars().all(|c| c.is_ascii_digit()) {
        let id: u64 = target.parse()?;
        if agents.clone().any(|a| a.id == id) {
            return Ok(vec![id]);
        }
        bail!("no agent with ID {id}");
    }
    if let Some(group) = target.strip_suffix("/**").or_else(|| target.strip_suffix('/')) {
        let prefix = format!("{group}/");
        let ids: Vec<u64> = agents.filter(|a| a.name.starts_with(&prefix)).map(|a| a.id).collect();
        if ids.is_empty() {
            bail!("no agents in group {group}");
        }
        return Ok(ids);
    }
    let exact: Vec<&AgentInfo> = agents.clone().filter(|a| a.name == target).collect();
    if !exact.is_empty() {
        return match pick_one(&exact) {
            Some(id) => Ok(vec![id]),
            None => {
                let ids: Vec<String> = exact.iter().map(|a| a.id.to_string()).collect();
                bail!("{target} matches {} agents (name reused after exit); use an ID: {}", exact.len(), ids.join(", "))
            }
        };
    }
    let matches: Vec<&AgentInfo> = agents.filter(|a| a.name.rsplit('/').next() == Some(target)).collect();
    match matches.as_slice() {
        [] => bail!("no agent named {target}"),
        _ => match pick_one(&matches) {
            Some(id) => Ok(vec![id]),
            None => {
                let names: Vec<&str> = matches.iter().map(|a| a.name.as_str()).collect();
                bail!("{target} is ambiguous: {}", names.join(", "))
            }
        },
    }
}

/// Picks the one agent meant among several that share a name — this can
/// legitimately happen once an exited agent's name is reused by a new one.
/// The live agent wins when there's exactly one; a real tie (e.g. two exited
/// agents still un-removed under the same name) can't be guessed and has to
/// be broken by ID instead.
fn pick_one(matches: &[&AgentInfo]) -> Option<u64> {
    if let [one] = matches {
        return Some(one.id);
    }
    match matches.iter().filter(|a| a.status.is_live()).collect::<Vec<_>>().as_slice() {
        [one] => Some(one.id),
        _ => None,
    }
}

/// Label keys follow the name alphabet (plus `/` for namespacing, e.g.
/// `team/owner`); values are free text on one line.
pub fn validate_label(key: &str, value: &str) -> Result<()> {
    if key.is_empty() || !key.chars().all(|c| matches!(c, 'a'..='z' | '0'..='9' | '.' | '_' | '-' | '/')) {
        bail!("invalid label key {key:?} (use a-z 0-9 . _ - /)");
    }
    if value.is_empty() || value.contains(['\n', '\r']) {
        bail!("invalid value for label {key}: must be one non-empty line");
    }
    Ok(())
}

/// Parses `k=v` into a pair; used for `-l` flags.
pub fn parse_label(arg: &str) -> Result<(String, String)> {
    let Some((k, v)) = arg.split_once('=') else { bail!("expected key=value, got {arg:?}") };
    validate_label(k, v)?;
    Ok((k.to_string(), v.to_string()))
}

pub fn in_prefix(name: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_end_matches("/**").trim_end_matches('/');
    name == prefix || name.starts_with(&format!("{prefix}/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use argus_proto::msg::AgentStatus;

    fn agent(id: u64, name: &str) -> AgentInfo {
        AgentInfo {
            id,
            name: name.into(),
            kind: "claude".into(),
            command: vec!["claude".into()],
            cwd: "/".into(),
            created_at: 0,
            exited_at: None,
            holder_pid: None,
            agent_pid: None,
            status: AgentStatus::Running,
            exit_code: None,
            activity: "unknown".into(),
            activity_since: None,
            attached: 0,
            labels: Default::default(),
        }
    }

    #[test]
    fn names() {
        assert!(validate_name("research/claude-1").is_ok());
        assert!(validate_name("research/42").is_err());
        assert!(validate_name("Research").is_err());
        assert!(validate_name("a//b").is_err());
        assert_eq!(kind_of("/opt/bin/Claude"), "claude");
        assert_eq!(normalize_group("/proj/").unwrap().as_deref(), Some("proj"));
    }

    #[test]
    fn default_names_fill_gaps() {
        let taken = ["p/claude-1", "p/claude-3"];
        assert_eq!(default_name(Some("p"), "claude", |n| taken.contains(&n)), "p/claude-2");
        assert_eq!(default_name(None, "codex", |_| false), "codex-1");
    }

    #[test]
    fn resolution() {
        let all = [agent(1, "research/claude-1"), agent(2, "proj/claude-1"), agent(3, "proj/codex-1")];
        assert_eq!(resolve("2", all.iter()).unwrap(), vec![2]);
        assert_eq!(resolve("codex-1", all.iter()).unwrap(), vec![3]);
        assert_eq!(resolve("research/claude-1", all.iter()).unwrap(), vec![1]);
        assert_eq!(resolve("proj/**", all.iter()).unwrap(), vec![2, 3]);
        assert!(resolve("claude-1", all.iter()).is_err());
        assert!(resolve("9", all.iter()).is_err());
    }

    fn exited(id: u64, name: &str) -> AgentInfo {
        let mut a = agent(id, name);
        a.status = AgentStatus::Exited;
        a
    }

    #[test]
    fn a_recycled_name_resolves_to_the_live_agent() {
        let all = [exited(1, "codex-1"), agent(2, "codex-1")];
        assert_eq!(resolve("codex-1", all.iter()).unwrap(), vec![2]);
        // The exited one is still there under its ID, just not by name.
        assert_eq!(resolve("1", all.iter()).unwrap(), vec![1]);
    }

    #[test]
    fn two_exited_agents_with_the_same_recycled_name_need_an_id() {
        let all = [exited(1, "codex-1"), exited(2, "codex-1")];
        assert!(resolve("codex-1", all.iter()).is_err());
    }
}
