//! Live process view.
//!
//! The dispatcher runs each agent as a child process (`opencode run`, `hermes
//! -z`, …), and those CLIs may spawn their own helpers/subagents. Reading the
//! process table is the cheapest way to show what is actually happening — far
//! more honest than an agent's self-reported status.

use serde::Serialize;
use std::collections::HashSet;
use std::process::Command;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    /// Seconds since the process started.
    pub secs: u64,
    pub cmd: String,
}

/// Parse the stable, machine-readable `ps -eo pid=,ppid=,etimes=,args=` format.
pub fn parse_ps(text: &str) -> Vec<Proc> {
    text.lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let ppid = it.next()?.parse().ok()?;
            let secs = it.next()?.parse().ok()?;
            let cmd = it.collect::<Vec<_>>().join(" ");
            if cmd.is_empty() {
                return None;
            }
            Some(Proc {
                pid,
                ppid,
                secs,
                cmd,
            })
        })
        .collect()
}

pub fn processes() -> Vec<Proc> {
    Command::new("ps")
        .args(["-eo", "pid=,ppid=,etimes=,args="])
        .output()
        .ok()
        .map(|o| parse_ps(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

/// Processes descended from `root` whose command line mentions `needle`, plus
/// their own descendants (subagents/helpers).
///
/// Restricting to the server's process tree matters: any unrelated process that
/// merely mentions a ticket ref on its command line (a shell, a log watcher)
/// must not show up as an agent.
pub fn matching_under(all: &[Proc], root: i32, needle: &str) -> Vec<Proc> {
    if needle.is_empty() {
        return Vec::new();
    }
    let descendants = descendants_of(all, root);
    let mut chosen: HashSet<i32> = all
        .iter()
        .filter(|p| descendants.contains(&p.pid) && p.cmd.contains(needle))
        .map(|p| p.pid)
        .collect();
    if chosen.is_empty() {
        return Vec::new();
    }
    loop {
        let before = chosen.len();
        for p in all {
            if chosen.contains(&p.ppid) {
                chosen.insert(p.pid);
            }
        }
        if chosen.len() == before {
            break;
        }
    }
    let mut out: Vec<Proc> = all
        .iter()
        .filter(|p| chosen.contains(&p.pid))
        .cloned()
        .collect();
    out.sort_by_key(|p| p.pid);
    out
}

/// Every pid below `root` (children, grandchildren, …), not including root.
pub fn descendants_of(all: &[Proc], root: i32) -> HashSet<i32> {
    let mut seen: HashSet<i32> = HashSet::new();
    seen.insert(root);
    loop {
        let before = seen.len();
        for p in all {
            if seen.contains(&p.ppid) && !seen.contains(&p.pid) {
                seen.insert(p.pid);
            }
        }
        if seen.len() == before {
            break;
        }
    }
    seen.remove(&root);
    seen
}

#[cfg(test)]
mod tests {
    use super::*;

    const PS: &str = "\
  100       1    1200 /usr/bin/bureau serve --bind 0.0.0.0:8787
  200     100     300 opencode run You own ticket BUR-9 — clouds
  201     200     300 /usr/bin/godot --headless --path .
  300       1     900 /usr/bin/htop
";

    #[test]
    fn parses_ps_columns() {
        let procs = parse_ps(PS);
        assert_eq!(procs.len(), 4);
        assert_eq!(procs[1].pid, 200);
        assert_eq!(procs[1].ppid, 100);
        assert_eq!(procs[1].secs, 300);
        assert!(procs[1].cmd.starts_with("opencode run"));
    }

    #[test]
    fn matching_under_includes_descendants_only() {
        let procs = parse_ps(PS);
        let pids: Vec<i32> = matching_under(&procs, 100, "BUR-9")
            .iter()
            .map(|p| p.pid)
            .collect();
        assert_eq!(pids, vec![200, 201]); // agent + its godot child, not htop
    }

    #[test]
    fn strangers_mentioning_the_ref_are_ignored() {
        let text = format!("{PS}  400       1     10 bash watcher BUR-9\n");
        let pids: Vec<i32> = matching_under(&parse_ps(&text), 100, "BUR-9")
            .iter()
            .map(|p| p.pid)
            .collect();
        assert_eq!(pids, vec![200, 201], "pid 400 is not under bureau");
    }

    #[test]
    fn empty_needle_matches_nothing() {
        assert!(matching_under(&parse_ps(PS), 100, "").is_empty());
    }
}
