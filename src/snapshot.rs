//! Snapshots: what to bring back after a reboot.
//!
//! One JSON file per snapshot in `<state>/snapshots/<stamp>.json`, newest
//! last by name. Scrollback lives beside them in `<state>/scrollback/`, one
//! file per distinct content, so fifty snapshots of an idle shell cost one
//! copy of its history. A save that would repeat the newest snapshot writes
//! nothing.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::claude::{self, TailCache};
use crate::config::Config;
use crate::live::{Kind, State};
use crate::procs;
use crate::tmux::Tmux;
use crate::util;

pub const VERSION: u32 = 1;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    pub version: u32,
    pub host: String,
    pub taken_at: i64,
    pub sessions: Vec<SSession>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SSession {
    pub name: String,
    #[serde(default)]
    pub auto_named: bool,
    pub windows: Vec<SWindow>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SWindow {
    pub index: u32,
    pub name: String,
    #[serde(default)]
    pub auto_rename: bool,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub layout: String,
    pub panes: Vec<SPane>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SPane {
    pub index: u32,
    #[serde(default)]
    pub active: bool,
    pub cwd: String,
    /// claude | shell | program
    pub kind: String,
    /// The shell the pane ran, so restore starts the same one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    #[serde(default)]
    pub title: String,
    /// A program to start again (only ones `Config::rerunnable` allows).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude: Option<SClaude>,
    /// Scrollback file name in `<state>/scrollback/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scrollback: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SClaude {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Flags carried over from the original command line.
    #[serde(default)]
    pub flags: Vec<String>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
}

impl Snapshot {
    pub fn claude_count(&self) -> usize {
        self.panes().filter(|p| p.claude.is_some()).count()
    }

    pub fn panes(&self) -> impl Iterator<Item = &SPane> {
        self.sessions
            .iter()
            .flat_map(|s| s.windows.iter().flat_map(|w| w.panes.iter()))
    }

    fn same_as(&self, other: &Snapshot) -> bool {
        let mut a = self.clone();
        let mut b = other.clone();
        a.taken_at = 0;
        b.taken_at = 0;
        a == b
    }
}

impl SSession {
    pub fn claude_ids(&self) -> Vec<&str> {
        self.windows
            .iter()
            .flat_map(|w| w.panes.iter())
            .filter_map(|p| p.claude.as_ref().map(|c| c.session_id.as_str()))
            .collect()
    }

    /// One line describing the session, for lists.
    pub fn describe(&self) -> String {
        let p = self
            .windows
            .iter()
            .find(|w| w.active)
            .or(self.windows.first())
            .and_then(|w| {
                w.panes
                    .iter()
                    .find(|p| p.claude.is_some())
                    .or(w.panes.first())
            });
        match p {
            Some(p) if !p.title.is_empty() => p.title.clone(),
            Some(p) => p.kind.clone(),
            None => String::new(),
        }
    }
}

pub fn dir() -> PathBuf {
    util::state_dir().join("snapshots")
}

pub fn scrollback_dir() -> PathBuf {
    util::state_dir().join("scrollback")
}

/// Snapshot files, oldest first.
pub fn list() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir())
        .map(|d| {
            d.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

pub fn load(path: &Path) -> Result<Snapshot> {
    let s = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&s).with_context(|| format!("parsing {}", path.display()))
}

pub fn latest() -> Option<(PathBuf, Snapshot)> {
    for p in list().into_iter().rev() {
        if let Ok(s) = load(&p) {
            return Some((p, s));
        }
    }
    None
}

/// A snapshot by stamp (`20261110-174500`, or any unique prefix of one).
pub fn find(stamp: &str) -> Option<PathBuf> {
    let hits: Vec<PathBuf> = list()
        .into_iter()
        .filter(|p| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.starts_with(stamp))
                .unwrap_or(false)
        })
        .collect();
    if hits.len() == 1 {
        hits.into_iter().next()
    } else {
        None
    }
}

pub fn stamp_of(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string()
}

/// Lines with trailing blanks removed; None when nothing is left.
fn trim_capture(raw: &[u8]) -> Option<Vec<u8>> {
    let text = String::from_utf8_lossy(raw);
    let mut lines: Vec<&str> = text.split('\n').collect();
    while let Some(l) = lines.last() {
        if strip_ansi(l).trim().is_empty() {
            lines.pop();
        } else {
            break;
        }
    }
    if lines.is_empty() {
        return None;
    }
    let mut out = lines.join("\n");
    out.push('\n');
    Some(out.into_bytes())
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut esc = false;
    for c in s.chars() {
        if esc {
            if c.is_ascii_alphabetic() {
                esc = false;
            }
            continue;
        }
        if c == '\x1b' {
            esc = true;
            continue;
        }
        out.push(c);
    }
    out
}

/// Build a snapshot of the live state. Writes scrollback files as it goes
/// (content-addressed, so a file that exists is already right).
pub fn build(tmux: &Tmux, state: &State, cfg: &Config, cache: &mut TailCache) -> Snapshot {
    let cdir = util::claude_dir();
    let sb_dir = scrollback_dir();
    let mut snap = Snapshot {
        version: VERSION,
        host: util::hostname(),
        taken_at: util::now(),
        sessions: vec![],
    };
    for s in &state.sessions {
        let mut ss = SSession {
            name: s.name.clone(),
            auto_named: s.auto_named(),
            windows: vec![],
        };
        for w in &s.windows {
            let mut sw = SWindow {
                index: w.index,
                name: w.name.clone(),
                auto_rename: w.auto_rename,
                active: w.active,
                layout: w.layout.clone(),
                panes: vec![],
            };
            for p in &w.panes {
                let mut sp = SPane {
                    index: p.index,
                    active: p.active,
                    cwd: p.cwd.clone(),
                    kind: match p.kind {
                        Kind::Claude => "claude",
                        Kind::Shell => "shell",
                        Kind::Program => "program",
                    }
                    .into(),
                    shell: p.shell.clone(),
                    title: p.label.clone(),
                    ..Default::default()
                };
                if let Some(c) = &p.claude {
                    let id = c.session_id.clone().or_else(|| {
                        claude::match_by_title(
                            &cdir,
                            &p.cwd,
                            &p.label,
                            c.started.saturating_sub(60),
                        )
                    });
                    if let Some(id) = id {
                        let tail = claude::find_transcript(&cdir, &id, Some(&p.cwd))
                            .map(|t| cache.get(&t))
                            .unwrap_or_default();
                        if let Some(t) = &tail.title {
                            if p.label == "claude" || p.label.is_empty() {
                                sp.title = t.clone();
                            }
                        }
                        sp.claude = Some(SClaude {
                            session_id: id,
                            model: tail.model,
                            flags: claude::carried_flags(&c.argv),
                            env: procs::environ(c.pid, claude::keep_env),
                        });
                    } else {
                        // a claude we can't identify comes back as a shell
                        sp.kind = "shell".into();
                    }
                } else {
                    if p.kind == Kind::Program && cfg.rerunnable(&p.fg_argv) {
                        sp.program = Some(p.fg_argv.clone());
                    }
                    if cfg.scrollback_lines > 0 {
                        if let Ok(raw) = tmux.capture(&p.id, Some(cfg.scrollback_lines)) {
                            if let Some(text) = trim_capture(&raw) {
                                let name = format!("{}.txt", util::fnv64(&text));
                                let path = sb_dir.join(&name);
                                if !path.exists() {
                                    let _ = fs::create_dir_all(&sb_dir);
                                    if write_private(&path, &text).is_ok() {
                                        sp.scrollback = Some(name);
                                    }
                                } else {
                                    sp.scrollback = Some(name);
                                }
                            }
                        }
                    }
                }
                sw.panes.push(sp);
            }
            ss.windows.push(sw);
        }
        snap.sessions.push(ss);
    }
    snap
}

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
        f.write_all(data)?;
        f.sync_all().ok();
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

#[derive(Debug, PartialEq)]
pub enum Saved {
    Written(PathBuf),
    Unchanged(PathBuf),
    Empty,
    Skipped(String),
}

/// Is a restore running right now? Saving mid-restore would record half the
/// sessions as the latest state.
pub fn restore_in_progress() -> bool {
    let p = util::state_dir().join("restore.lock");
    match fs::read_to_string(&p) {
        Ok(s) => s
            .trim()
            .parse::<i32>()
            .map(util::pid_alive)
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// During shutdown sessions disappear one by one; a save then would record
/// whatever was left as the newest state.
pub fn system_stopping() -> bool {
    if !Path::new("/run/systemd/system").exists() {
        return false;
    }
    std::process::Command::new("systemctl")
        .arg("is-system-running")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "stopping")
        .unwrap_or(false)
}

pub fn save(tmux: &Tmux, state: &State, cfg: &Config, cache: &mut TailCache) -> Result<Saved> {
    if restore_in_progress() {
        return Ok(Saved::Skipped("a restore is running".into()));
    }
    if system_stopping() {
        return Ok(Saved::Skipped("the system is shutting down".into()));
    }
    if state.sessions.is_empty() {
        return Ok(Saved::Empty);
    }
    // one save at a time: pruning must not collect scrollback that a
    // concurrent save just wrote and has not referenced yet
    let _guard = util::lock(&util::state_dir().join("save.lock"), true);
    let snap = build(tmux, state, cfg, cache);
    if let Some((path, last)) = latest() {
        if last.same_as(&snap) {
            return Ok(Saved::Unchanged(path));
        }
    }
    let d = dir();
    fs::create_dir_all(&d)?;
    let mut path = d.join(format!("{}.json", util::stamp(snap.taken_at)));
    let mut n = 1;
    while path.exists() {
        path = d.join(format!("{}-{n}.json", util::stamp(snap.taken_at)));
        n += 1;
    }
    let body = serde_json::to_vec_pretty(&snap)?;
    write_private(&path, &body)?;
    prune(cfg.keep_snapshots.max(1));
    Ok(Saved::Written(path))
}

/// Keep the newest `keep` snapshots and the scrollback they still use.
pub fn prune(keep: usize) {
    let all = list();
    if all.len() > keep {
        for p in &all[..all.len() - keep] {
            let _ = fs::remove_file(p);
        }
    }
    let mut used: HashSet<String> = HashSet::new();
    for p in list() {
        if let Ok(s) = load(&p) {
            for pane in s.panes() {
                if let Some(sb) = &pane.scrollback {
                    used.insert(sb.clone());
                }
            }
        }
    }
    if let Ok(d) = fs::read_dir(scrollback_dir()) {
        for e in d.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !used.contains(&name) {
                let _ = fs::remove_file(e.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_blank_tail_including_colour_only_lines() {
        let raw = b"$ ls\nfile\n\x1b[0m\n   \n\n";
        assert_eq!(trim_capture(raw).unwrap(), b"$ ls\nfile\n".to_vec());
        assert_eq!(trim_capture(b"\n\n  \n"), None);
    }

    #[test]
    fn equality_ignores_time() {
        let a = Snapshot {
            version: 1,
            taken_at: 1,
            sessions: vec![SSession {
                name: "1".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut b = a.clone();
        b.taken_at = 99;
        assert!(a.same_as(&b));
        b.sessions[0].name = "2".into();
        assert!(!a.same_as(&b));
    }

    #[test]
    fn old_files_still_parse() {
        // fields added later must default, so old snapshots stay usable
        let j = r#"{"version":1,"host":"h","taken_at":5,"sessions":[{"name":"1","windows":[{"index":1,"name":"w","panes":[{"index":1,"cwd":"/","kind":"shell"}]}]}]}"#;
        let s: Snapshot = serde_json::from_str(j).unwrap();
        assert_eq!(s.sessions[0].windows[0].panes[0].kind, "shell");
    }
}
