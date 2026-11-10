//! One pass over /proc per refresh: who is whose child, which process owns a
//! terminal's foreground, and how much CPU and memory a tree uses.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Clone, Debug)]
#[allow(dead_code)] // the whole record, though not every field is shown yet
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    /// Foreground process group of the process's controlling terminal.
    pub tpgid: i32,
    pub comm: String,
    /// utime + stime, in clock ticks.
    pub ticks: u64,
    /// Start time after boot, in clock ticks. With the pid it names one
    /// process for good: pids get reused, (pid, start) does not.
    pub start: u64,
    pub rss: u64,
}

fn page_size() -> u64 {
    static P: OnceLock<u64> = OnceLock::new();
    *P.get_or_init(|| unsafe { libc::sysconf(libc::_SC_PAGESIZE) as u64 })
}

pub fn clk_tck() -> u64 {
    static T: OnceLock<u64> = OnceLock::new();
    *T.get_or_init(|| {
        let t = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if t > 0 {
            t as u64
        } else {
            100
        }
    })
}

/// Parse `/proc/<pid>/stat`. The command name sits in parentheses and may
/// itself contain spaces and parentheses, so fields are counted from the
/// LAST `)`.
pub fn parse_stat(pid: i32, s: &str) -> Option<Proc> {
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    let comm = s.get(open + 1..close)?.to_string();
    let rest: Vec<&str> = s.get(close + 1..)?.split_whitespace().collect();
    let f = |i: usize| rest.get(i).and_then(|v| v.parse::<i64>().ok());
    Some(Proc {
        pid,
        ppid: f(1)? as i32,
        tpgid: f(5)? as i32,
        comm,
        ticks: (f(11)? + f(12)?).max(0) as u64,
        start: f(19)?.max(0) as u64,
        rss: f(21)?.max(0) as u64 * page_size(),
    })
}

pub fn read_stat(pid: i32) -> Option<Proc> {
    let s = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat(pid, &s)
}

pub fn cmdline(pid: i32) -> Vec<String> {
    let raw = match fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    raw.split(|b| *b == 0)
        .filter(|w| !w.is_empty())
        .map(|w| String::from_utf8_lossy(w).into_owned())
        .collect()
}

/// Environment variables of a process whose names pass `keep`.
pub fn environ(pid: i32, keep: impl Fn(&str) -> bool) -> Vec<(String, String)> {
    let raw = match fs::read(format!("/proc/{pid}/environ")) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    raw.split(|b| *b == 0)
        .filter_map(|kv| {
            let kv = String::from_utf8_lossy(kv);
            let (k, v) = kv.split_once('=')?;
            keep(k).then(|| (k.to_string(), v.to_string()))
        })
        .collect()
}

pub fn exe(pid: i32) -> Option<PathBuf> {
    fs::read_link(format!("/proc/{pid}/exe")).ok()
}

#[derive(Default)]
pub struct Table {
    pub procs: HashMap<i32, Proc>,
    pub children: HashMap<i32, Vec<i32>>,
}

impl Table {
    pub fn scan() -> Table {
        let mut t = Table::default();
        let Ok(dir) = fs::read_dir("/proc") else {
            return t;
        };
        for e in dir.flatten() {
            let name = e.file_name();
            let Some(pid) = name.to_str().and_then(|s| s.parse::<i32>().ok()) else {
                continue;
            };
            if let Some(p) = read_stat(pid) {
                t.children.entry(p.ppid).or_default().push(pid);
                t.procs.insert(pid, p);
            }
        }
        t
    }

    /// `pid` and everything under it.
    pub fn tree(&self, pid: i32) -> Vec<i32> {
        let mut out = Vec::new();
        let mut stack = vec![pid];
        while let Some(p) = stack.pop() {
            if !self.procs.contains_key(&p) || out.contains(&p) {
                continue;
            }
            out.push(p);
            if let Some(kids) = self.children.get(&p) {
                stack.extend(kids.iter().copied());
            }
        }
        out
    }

    /// The process a person is looking at in this pane: the leader of the
    /// terminal's foreground process group, else the pane's own process.
    pub fn foreground(&self, pane_pid: i32) -> i32 {
        match self.procs.get(&pane_pid) {
            Some(p) if p.tpgid > 0 && self.procs.contains_key(&p.tpgid) => p.tpgid,
            _ => pane_pid,
        }
    }
}

pub const SHELLS: &[&str] = &[
    "bash", "zsh", "fish", "sh", "dash", "ksh", "mksh", "tcsh", "csh", "nu", "elvish", "xonsh",
    "ash",
];

pub fn is_shell(comm: &str) -> bool {
    let c = comm.trim_start_matches('-');
    SHELLS.contains(&c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_with_parens_in_name() {
        let s = "1234 (my (odd) prog) S 1 1234 1234 34817 1234 4194560 100 0 0 0 7 3 0 0 20 0 1 0 5555 1000 50 18446744073709551615";
        let p = parse_stat(1234, s).unwrap();
        assert_eq!(p.comm, "my (odd) prog");
        assert_eq!(p.ppid, 1);
        assert_eq!(p.tpgid, 1234);
        assert_eq!(p.ticks, 10);
        assert_eq!(p.start, 5555);
        assert_eq!(p.rss, 50 * page_size());
    }

    #[test]
    fn shells() {
        assert!(is_shell("-bash"));
        assert!(is_shell("fish"));
        assert!(!is_shell("claude"));
    }
}
