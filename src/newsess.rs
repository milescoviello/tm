//! Starting new sessions: where (recent folders, ssh hosts), what (claude,
//! a shell, ssh, a past chat picked in mnemosyne), and under which name.

use anyhow::{anyhow, Result};
use std::collections::HashSet;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::{Command, Stdio};

use crate::live::State;
use crate::naming;
use crate::restore;
use crate::snapshot;
use crate::tmux::{exact_s, scrub_env, Tmux};
use crate::util;

pub fn next_number(state: &State) -> u32 {
    state
        .sessions
        .iter()
        .filter_map(|s| s.number())
        .max()
        .unwrap_or(0)
        + 1
}

/// Folders worth offering, most relevant first: where panes are now, then
/// where Claude was used lately, then anything in the newest snapshot.
pub fn recent_folders(state: &State, limit: usize) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut push = |p: String, out: &mut Vec<String>| {
        if !p.is_empty() && !seen.contains(&p) && Path::new(&p).is_dir() {
            seen.insert(p.clone());
            out.push(p);
        }
    };
    let mut by_activity: Vec<(i64, String)> = state
        .sessions
        .iter()
        .flat_map(|s| s.windows.iter().map(move |w| (w, s)))
        .flat_map(|(w, _)| w.panes.iter().map(move |p| (w.activity, p.cwd.clone())))
        .collect();
    by_activity.sort_by_key(|a| std::cmp::Reverse(a.0));
    for (_, p) in by_activity {
        push(p, &mut out);
    }
    for p in history_projects(400) {
        push(p, &mut out);
    }
    if let Some((_, snap)) = snapshot::latest() {
        for p in snap.panes() {
            push(p.cwd.clone(), &mut out);
        }
    }
    push(util::home().to_string_lossy().into_owned(), &mut out);
    out.truncate(limit);
    out
}

/// `project` fields from the end of Claude's prompt history, newest first.
fn history_projects(max: usize) -> Vec<String> {
    let path = util::claude_dir().join("history.jsonl");
    let Ok(mut f) = fs::File::open(&path) else {
        return vec![];
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let from = len.saturating_sub(1024 * 1024);
    if f.seek(SeekFrom::Start(from)).is_err() {
        return vec![];
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return vec![];
    }
    let text = String::from_utf8_lossy(&buf);
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for line in text.lines().rev() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(p) = v.get("project").and_then(|p| p.as_str()) {
            if seen.insert(p.to_string()) {
                out.push(p.to_string());
                if out.len() >= max {
                    break;
                }
            }
        }
    }
    out
}

/// Host aliases from ~/.ssh/config (no wildcards).
pub fn ssh_hosts() -> Vec<String> {
    let Ok(s) = fs::read_to_string(util::home().join(".ssh/config")) else {
        return vec![];
    };
    parse_ssh_hosts(&s)
}

pub fn parse_ssh_hosts(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in s.lines() {
        let t = line.trim();
        let mut words = t.split_whitespace();
        let Some(k) = words.next() else { continue };
        if !k.eq_ignore_ascii_case("host") {
            continue;
        }
        for h in words {
            if h.contains(['*', '?', '!']) || out.iter().any(|o| o == h) {
                continue;
            }
            out.push(h.to_string());
        }
    }
    out
}

#[derive(Clone, Debug, PartialEq)]
pub struct MnPick {
    pub cwd: String,
    pub session_id: String,
    pub model: String,
    pub perms: String,
    pub title: String,
}

/// mnemosyne prints its decision on stdout as TSV:
/// `kind  cwd  session-id  model  permission-mode  title  [name]`.
pub fn parse_mn_plan(out: &str) -> Option<MnPick> {
    out.lines()
        .rev()
        .map(|l| l.split('\t').collect::<Vec<_>>())
        .find(|f| f.len() >= 3 && matches!(f[0], "here" | "tmux" | "window" | "wintmux"))
        .map(|f| MnPick {
            cwd: f[1].to_string(),
            session_id: f[2].to_string(),
            model: f.get(3).unwrap_or(&"").to_string(),
            perms: f.get(4).unwrap_or(&"").to_string(),
            title: f.get(5).unwrap_or(&"").to_string(),
        })
}

/// The same mapping mn's own shell wrapper uses.
pub fn mn_perm_flags(mode: &str) -> Vec<String> {
    match mode {
        "bypassPermissions" | "" => vec!["--dangerously-skip-permissions".into()],
        "plan" | "acceptEdits" | "auto" | "manual" | "dontAsk" => {
            vec!["--permission-mode".into(), mode.into()]
        }
        _ => vec![],
    }
}

/// Run mnemosyne's picker on this terminal and return what was picked.
pub fn pick_with_mn() -> Result<Option<MnPick>> {
    let bin = util::which("mnemosyne").ok_or_else(|| anyhow!("mnemosyne (mn) is not installed"))?;
    let out = Command::new(bin)
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit())
        .stdout(Stdio::piped())
        .output()?;
    Ok(parse_mn_plan(&String::from_utf8_lossy(&out.stdout)))
}

/// Make a detached session and type `typed` into it. Returns its name.
pub fn create(
    tmux: &Tmux,
    name: &str,
    cwd: &str,
    typed: Option<Vec<String>>,
    auto: bool,
) -> Result<String> {
    let args = vec![
        "new-session".to_string(),
        "-d".into(),
        "-P".into(),
        "-F".into(),
        "#{pane_id}".into(),
        "-s".into(),
        name.into(),
        "-c".into(),
        cwd.into(),
    ];
    let pane = if tmux.server_running() {
        tmux.run(&args)?
    } else {
        let mut c = tmux.command();
        c.args(&args);
        scrub_env(&mut c);
        let out = c.output()?;
        if !out.status.success() {
            return Err(anyhow!(
                "tmux: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let pane = pane.trim().to_string();
    if auto {
        let _ = tmux.run(["set-option", "-t", &exact_s(name), "@tm-auto", name]);
    }
    if let Some(argv) = typed {
        let shell = util::login_shell();
        let fish = util::basename(&shell) == "fish";
        restore::send_line(tmux, &pane, &util::join_command(&argv, fish))?;
    }
    Ok(name.to_string())
}

/// `<number>-<slug>` for a new session.
pub fn new_name(state: &State, hint: &str) -> String {
    let n = next_number(state);
    let s = naming::slug(hint);
    if s.is_empty() {
        n.to_string()
    } else {
        format!("{n}-{s}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mn_plan_lines() {
        let out = "noise\nhere\t/w\tSID\tmodel-x\tplan\tA title\t\n";
        let p = parse_mn_plan(out).unwrap();
        assert_eq!(p.cwd, "/w");
        assert_eq!(p.session_id, "SID");
        assert_eq!(p.model, "model-x");
        assert_eq!(mn_perm_flags(&p.perms), vec!["--permission-mode", "plan"]);
        assert_eq!(parse_mn_plan("nothing picked\n"), None);
        assert_eq!(mn_perm_flags("default"), Vec::<String>::new());
        assert_eq!(mn_perm_flags(""), vec!["--dangerously-skip-permissions"]);
    }

    #[test]
    fn ssh_config_hosts() {
        let cfg = "Host a b\n  HostName 1.2.3.4\nHost *\n  User x\nhost c.example.net\nMatch all\n";
        assert_eq!(parse_ssh_hosts(cfg), vec!["a", "b", "c.example.net"]);
    }
}
