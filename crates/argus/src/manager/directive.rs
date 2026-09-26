//! Directives: what the manager asks an agent to do before its turn ends.
//!
//! Each check looks at one agent and returns at most one directive, so the
//! levels of one concern (e.g. labels unset vs. stale) collapse inside their
//! check. All directives for a turn go out together in one reason, since an
//! agent is held at most once per turn.

use argus_proto::msg::AgentInfo;

type Check = fn(&AgentInfo) -> Option<String>;

const CHECKS: &[Check] = &[labels];

/// The reason to hold the agent's turn open with, if any check has something
/// to ask.
pub fn at_stop(info: &AgentInfo) -> Option<String> {
    let directives: Vec<String> = CHECKS.iter().filter_map(|check| check(info)).collect();
    match directives.as_slice() {
        [] => None,
        [one] => Some(format!("argus: {one}")),
        many => {
            let items: Vec<String> = many.iter().enumerate().map(|(i, d)| format!("{}. {d}", i + 1)).collect();
            Some(format!("argus:\n{}", items.join("\n")))
        }
    }
}

/// Labels the user and other agents read the agent by; unset ones only.
fn labels(info: &AgentInfo) -> Option<String> {
    let missing: Vec<&str> = ["title", "recap"].into_iter().filter(|k| !info.labels.contains_key(*k)).collect();
    let (what, example) = match missing.as_slice() {
        [] => return None,
        [_, _] => (
            "your title and recap labels are",
            "argus label self title='<what this session is about>' recap='<where things stand>'",
        ),
        ["title"] => ("your title label is", "argus label self title='<what this session is about>'"),
        _ => ("your recap label is", "argus label self recap='<where things stand>'"),
    };
    Some(format!("{what} not set yet. Set them now with one command, then finish: {example}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(labels: &[(&str, &str)]) -> AgentInfo {
        let mut info: AgentInfo = serde_json::from_value(serde_json::json!({
            "id": 1, "name": "claude-1", "kind": "claude", "command": ["claude"], "cwd": "/",
            "created_at": 0, "holder_pid": 1, "status": "running", "activity": "working",
        }))
        .unwrap();
        info.labels = labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        info
    }

    #[test]
    fn labels_asked_for_only_when_unset() {
        let both = at_stop(&agent(&[])).unwrap();
        assert!(both.starts_with("argus: your title and recap labels are not set"), "{both}");
        let recap = at_stop(&agent(&[("title", "Fix auth")])).unwrap();
        assert!(recap.contains("recap label is") && !recap.contains("title='"), "{recap}");
        assert_eq!(at_stop(&agent(&[("title", "Fix auth"), ("recap", "Testing")])), None);
    }
}
