//! The turn log: each finished turn's prompt and final reply, as the agent's
//! hooks reported them. The manager appends to it; `argus inspect --last`
//! reads it straight from disk, so it outlives the agent (until `rm`).

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use argus_proto::paths;
use serde::{Deserialize, Serialize};

/// Once the log grows past this, it is rewritten with only its newest turns:
/// at most `KEEP`, and at most `MAX_LOG / 2` bytes of them, so a rewrite always
/// leaves room for many appends before the next.
const MAX_LOG: u64 = 2 * 1024 * 1024;
const KEEP: usize = 50;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    /// The agent's `turns` count this turn brought it to.
    pub turn: u64,
    /// `done`, `error` or `interrupted`.
    pub ended: String,
    /// When it ended, in Unix seconds.
    pub at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<String>,
}

pub fn append(id: u64, turn: &Turn) -> io::Result<()> {
    append_to(&paths::turn_log(&paths::agent_dir(id)), turn)
}

fn append_to(path: &Path, turn: &Turn) -> io::Result<()> {
    let mut line = serde_json::to_string(turn)?;
    line.push('\n');
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(line.as_bytes())?;
    if file.metadata()?.len() > MAX_LOG {
        compact(path)?;
    }
    Ok(())
}

fn compact(path: &Path) -> io::Result<()> {
    let text = fs::read_to_string(path)?;
    let mut budget = (MAX_LOG / 2) as usize;
    let mut kept: Vec<&str> = text
        .lines()
        .rev()
        .filter(|l| serde_json::from_str::<Turn>(l).is_ok())
        .take(KEEP)
        .take_while(|l| {
            let fits = l.len() < budget;
            budget = budget.saturating_sub(l.len() + 1);
            fits
        })
        .collect();
    kept.reverse();
    let tmp = path.with_extension("jsonl.tmp");
    fs::write(&tmp, kept.iter().map(|l| format!("{l}\n")).collect::<String>())?;
    fs::rename(tmp, path)
}

/// The last `count` turns, oldest first. None recorded is an empty list.
pub fn read(id: u64, count: usize) -> io::Result<Vec<Turn>> {
    read_from(&paths::turn_log(&paths::agent_dir(id)), count)
}

fn read_from(path: &Path, count: usize) -> io::Result<Vec<Turn>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    // A line cut short by a crash mid-write is skipped, not fatal.
    let turns: Vec<Turn> = text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    Ok(turns[turns.len().saturating_sub(count)..].to_vec())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use argus_proto::text::MAX_TEXT;

    use super::*;

    fn turn(n: u64, reply: usize) -> Turn {
        Turn { turn: n, ended: "done".into(), at: n, prompt: Some(format!("p{n}")), reply: Some("r".repeat(reply)) }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("argus-turns-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir.join("turns.jsonl")
    }

    #[test]
    fn keeps_the_last_turns() {
        let path = scratch("last");
        assert_eq!(read_from(&path, 5).unwrap(), vec![], "no log yet");
        for n in 1..=3 {
            append_to(&path, &turn(n, 10)).unwrap();
        }
        let last: Vec<u64> = read_from(&path, 2).unwrap().iter().map(|t| t.turn).collect();
        assert_eq!(last, [2, 3]);

        // Small turns: compaction keeps the last KEEP.
        for n in 4..=200 {
            append_to(&path, &turn(n, 20_000)).unwrap();
        }
        let all = read_from(&path, usize::MAX).unwrap();
        assert!(all.len() <= KEEP + MAX_LOG as usize / 20_000, "{}", all.len());
        assert_eq!(all.last().unwrap().turn, 200);

        // A torn last line does not hide the others.
        fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"{\"turn\":6").unwrap();
        assert_eq!(read_from(&path, 1).unwrap()[0].turn, 200);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn compaction_frees_room_even_for_the_largest_turns() {
        let path = scratch("large");
        let big = |n| Turn { prompt: Some("p".repeat(MAX_TEXT)), ..turn(n, MAX_TEXT) };
        let mut rewrites = 0;
        let mut size = 0;
        for n in 1..=100 {
            append_to(&path, &big(n)).unwrap();
            let now = fs::metadata(&path).unwrap().len();
            if now < size {
                rewrites += 1;
                assert!(now <= MAX_LOG / 2, "compacted to {now}");
            }
            size = now;
        }
        // Each rewrite frees about MAX_LOG / 2, room for several turns.
        assert!(rewrites <= 100 / 7, "{rewrites} rewrites");
        assert_eq!(read_from(&path, 1).unwrap()[0].turn, 100);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
