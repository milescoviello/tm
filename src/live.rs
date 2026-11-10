//! The live picture: every session, window and pane on the server, what runs
//! in each, and how much it costs. Built from one `list-panes -a` call, one
//! pass over /proc and Claude's sessions files.

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use crate::claude;
use crate::naming;
use crate::procs::{self, Table};
use crate::tmux::{Tmux, SEP};
use crate::util;

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Claude,
    Shell,
    Program,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct ClaudePane {
    pub pid: i32,
    pub session_id: Option<String>,
    /// From the sessions file; empty when there was none.
    pub status: String,
    pub argv: Vec<String>,
    pub name: String,
    /// Process start, unix seconds.
    pub started: i64,
}

#[derive(Clone, Debug)]
#[allow(dead_code)] // the whole record, though not every field is shown yet
pub struct Pane {
    pub id: String,
    pub index: u32,
    pub active: bool,
    pub pid: i32,
    pub cwd: String,
    pub cmd: String,
    pub title: String,
    pub width: u16,
    pub height: u16,
    pub dead: bool,
    /// The pane's own process, when it is a shell: its executable.
    pub shell: Option<String>,
    pub fg_pid: i32,
    pub fg_argv: Vec<String>,
    pub claude: Option<ClaudePane>,
    pub kind: Kind,
    pub label: String,
}

#[derive(Clone, Debug)]
pub struct Window {
    pub id: String,
    pub index: u32,
    pub name: String,
    pub active: bool,
    pub layout: String,
    pub activity: i64,
    pub auto_rename: bool,
    pub panes: Vec<Pane>,
}

#[derive(Clone, Debug)]
pub struct Session {
    pub id: String,
    pub name: String,
    pub created: i64,
    pub activity: i64,
    pub attached: u32,
    /// The `@tm-auto` option: the name tm last gave this session, or empty.
    pub auto: String,
    pub windows: Vec<Window>,
    pub ticks: u64,
    pub rss: u64,
    pub cpu: Option<f32>,
}

impl Window {
    pub fn active_pane(&self) -> Option<&Pane> {
        self.panes.iter().find(|p| p.active).or(self.panes.first())
    }
}

impl Session {
    pub fn number(&self) -> Option<u32> {
        naming::number(&self.name)
    }

    pub fn active_window(&self) -> Option<&Window> {
        self.windows
            .iter()
            .find(|w| w.active)
            .or(self.windows.first())
    }

    /// The pane that speaks for the session: the active one.
    pub fn primary(&self) -> Option<&Pane> {
        self.active_window().and_then(|w| w.active_pane())
    }

    pub fn panes(&self) -> impl Iterator<Item = &Pane> {
        self.windows.iter().flat_map(|w| w.panes.iter())
    }

    /// The Claude pane a session is "about": in the active window first.
    pub fn main_claude(&self) -> Option<&Pane> {
        let in_active = self
            .active_window()
            .and_then(|w| w.panes.iter().find(|p| p.kind == Kind::Claude));
        in_active.or_else(|| self.panes().find(|p| p.kind == Kind::Claude))
    }

    pub fn auto_named(&self) -> bool {
        !self.auto.is_empty() && self.auto == self.name
    }

    /// What the list shows in the name column: just the number for a name
    /// tm made up (the title says the rest), else the name.
    pub fn short_name(&self) -> String {
        match (self.auto_named(), self.number()) {
            (true, Some(n)) => n.to_string(),
            _ => self.name.clone(),
        }
    }
}

#[derive(Debug, Default)]
pub struct State {
    pub sessions: Vec<Session>,
    pub at: Option<Instant>,
    pub running: bool,
    pub error: Option<String>,
    pub live_claude: Vec<claude::Live>,
}

impl State {
    pub fn session(&self, name: &str) -> Option<&Session> {
        self.sessions.iter().find(|s| s.name == name)
    }

    pub fn claude_count(&self) -> usize {
        self.sessions
            .iter()
            .flat_map(|s| s.panes())
            .filter(|p| p.kind == Kind::Claude)
            .count()
    }

    pub fn busy_count(&self) -> usize {
        self.sessions
            .iter()
            .flat_map(|s| s.panes())
            .filter(|p| {
                p.claude
                    .as_ref()
                    .map(|c| c.status == "busy")
                    .unwrap_or(false)
            })
            .count()
    }

    /// Pane id where a given Claude session is running right now, or the
    /// pid when it runs outside this tmux server.
    pub fn where_is(&self, session_id: &str) -> Option<(Option<String>, i32)> {
        for s in &self.sessions {
            for p in s.panes() {
                if let Some(c) = &p.claude {
                    if c.session_id.as_deref() == Some(session_id) {
                        return Some((Some(s.name.clone()), c.pid));
                    }
                }
            }
        }
        self.live_claude
            .iter()
            .find(|l| l.session_id == session_id)
            .map(|l| (None, l.pid))
    }
}

const FIELDS: &[&str] = &[
    "#{session_id}",
    "#{session_name}",
    "#{session_created}",
    "#{session_activity}",
    "#{session_attached}",
    "#{@tm-auto}",
    "#{window_id}",
    "#{window_index}",
    "#{window_name}",
    "#{window_active}",
    "#{window_layout}",
    "#{window_activity}",
    "#{automatic-rename}",
    "#{pane_id}",
    "#{pane_index}",
    "#{pane_active}",
    "#{pane_pid}",
    "#{pane_current_path}",
    "#{pane_current_command}",
    "#{pane_width}",
    "#{pane_height}",
    "#{pane_dead}",
    // last: the one field that could hold anything
    "#{pane_title}",
];

fn boot_time() -> i64 {
    std::fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("btime "))
                .and_then(|v| v.trim().parse().ok())
        })
        .unwrap_or(0)
}

fn describe(argv: &[String], comm: &str) -> String {
    if argv.is_empty() {
        return comm.to_string();
    }
    let mut words = vec![util::basename(&argv[0]).to_string()];
    words.extend(argv.iter().skip(1).cloned());
    words.join(" ")
}

fn find_claude(procs: &Table, pane_pid: i32) -> Option<(i32, Vec<String>)> {
    for pid in procs.tree(pane_pid) {
        let comm = &procs.procs.get(&pid)?.comm;
        if !matches!(comm.as_str(), "claude" | "claude.exe" | "node" | "bun") {
            continue;
        }
        let argv = procs::cmdline(pid);
        if claude::is_claude_argv(&argv, comm) {
            return Some((pid, argv));
        }
    }
    None
}

pub fn collect(tmux: &Tmux, claude_dir: &Path, prev: Option<&State>) -> State {
    let procs = Table::scan();
    let live_claude = claude::load_live(claude_dir, &procs);
    let fmt = FIELDS.join(&SEP.to_string());
    let out = match tmux.run(["list-panes", "-a", "-F", &fmt]) {
        Ok(o) => o,
        Err(e) => {
            return State {
                sessions: vec![],
                at: Some(Instant::now()),
                running: false,
                error: Some(e.to_string()),
                live_claude,
            }
        }
    };
    // Matched by pid inside each pane's own process tree. The sessions
    // file's `tmux` pane id is NOT used for this: pane ids (%0, %1, ...) are
    // per server, and the files cover every Claude on the machine, so a pane
    // on a second server would claim a Claude running on the first.
    let by_pid: HashMap<i32, &claude::Live> = live_claude.iter().map(|l| (l.pid, l)).collect();
    let btime = boot_time();
    let host = util::hostname();
    let tck = procs::clk_tck() as i64;

    let mut sessions: Vec<Session> = Vec::new();
    for line in out.lines() {
        let f: Vec<&str> = line.splitn(FIELDS.len(), SEP).collect();
        if f.len() < FIELDS.len() {
            continue;
        }
        let num = |i: usize| f[i].parse::<i64>().unwrap_or(0);
        let si = match sessions.iter().position(|s| s.id == f[0]) {
            Some(i) => i,
            None => {
                sessions.push(Session {
                    id: f[0].to_string(),
                    name: f[1].to_string(),
                    created: num(2),
                    activity: num(3),
                    attached: num(4) as u32,
                    auto: f[5].to_string(),
                    windows: vec![],
                    ticks: 0,
                    rss: 0,
                    cpu: None,
                });
                sessions.len() - 1
            }
        };
        let s = &mut sessions[si];
        let wi = match s.windows.iter().position(|w| w.id == f[6]) {
            Some(i) => i,
            None => {
                s.windows.push(Window {
                    id: f[6].to_string(),
                    index: num(7) as u32,
                    name: f[8].to_string(),
                    active: f[9] == "1",
                    layout: f[10].to_string(),
                    activity: num(11),
                    auto_rename: f[12] == "1",
                    panes: vec![],
                });
                s.windows.len() - 1
            }
        };

        let pane_id = f[13].to_string();
        let pid = num(16) as i32;
        let title = f[22].to_string();
        let fg_pid = procs.foreground(pid);
        let fg_argv = procs::cmdline(fg_pid);
        let fg_comm = procs
            .procs
            .get(&fg_pid)
            .map(|p| p.comm.clone())
            .unwrap_or_default();
        let shell = procs
            .procs
            .get(&pid)
            .filter(|p| procs::is_shell(&p.comm))
            .map(|p| {
                procs::exe(pid)
                    .map(|e| e.to_string_lossy().into_owned())
                    .unwrap_or_else(|| p.comm.trim_start_matches('-').to_string())
            });

        let started_of = |p: i32| {
            procs
                .procs
                .get(&p)
                .map(|pr| btime + pr.start as i64 / tck)
                .unwrap_or(0)
        };
        let tree = procs.tree(pid);
        let depth = |mut p: i32| {
            let mut d = 0;
            while p != pid && d < 64 {
                p = procs.procs.get(&p).map(|x| x.ppid).unwrap_or(pid);
                d += 1;
            }
            d
        };
        let registered = tree
            .iter()
            .filter_map(|p| by_pid.get(p).copied())
            .min_by_key(|l| depth(l.pid));
        let claude_pane = if let Some(l) = registered {
            Some(ClaudePane {
                pid: l.pid,
                session_id: Some(l.session_id.clone()),
                status: l.status.clone(),
                argv: procs::cmdline(l.pid),
                name: l.name.clone(),
                started: started_of(l.pid),
            })
        } else {
            find_claude(&procs, pid).map(|(cpid, argv)| ClaudePane {
                pid: cpid,
                session_id: claude::session_from_argv(&argv),
                status: String::new(),
                argv,
                name: String::new(),
                started: started_of(cpid),
            })
        };

        let (kind, label) = if let Some(c) = &claude_pane {
            // tmux's default pane title is the host name: until Claude sets
            // its own, that says nothing about the session
            let mut t = claude::clean_title(&title);
            if t == host || t.split('.').next() == Some(host.as_str()) {
                t.clear();
            }
            let label = if !t.is_empty() {
                t
            } else if let Some(tt) = c
                .session_id
                .as_deref()
                .and_then(|sid| claude::transcript_title(claude_dir, sid, f[17]))
            {
                tt
            } else if !c.name.is_empty() {
                c.name.clone()
            } else {
                "claude".to_string()
            };
            (Kind::Claude, label)
        } else if fg_pid == pid && shell.is_some() {
            let name = util::basename(shell.as_deref().unwrap_or("sh")).to_string();
            (Kind::Shell, name)
        } else if procs::is_shell(&fg_comm) && fg_argv.len() <= 1 {
            (Kind::Shell, fg_comm.trim_start_matches('-').to_string())
        } else {
            (Kind::Program, describe(&fg_argv, &fg_comm))
        };

        // cost of everything under this pane
        for p in &tree {
            if let Some(pr) = procs.procs.get(p) {
                s.ticks += pr.ticks;
                s.rss += pr.rss;
            }
        }

        s.windows[wi].panes.push(Pane {
            id: pane_id,
            index: num(14) as u32,
            active: f[15] == "1",
            pid,
            cwd: f[17].to_string(),
            cmd: f[18].to_string(),
            title,
            width: num(19) as u16,
            height: num(20) as u16,
            dead: f[21] == "1",
            shell,
            fg_pid,
            fg_argv,
            claude: claude_pane,
            kind,
            label,
        });
    }

    let now = Instant::now();
    if let Some(prev) = prev {
        if let Some(at) = prev.at {
            let dt = now.duration_since(at).as_secs_f32();
            if dt > 0.2 {
                for s in sessions.iter_mut() {
                    if let Some(old) = prev.sessions.iter().find(|o| o.id == s.id) {
                        let d = s.ticks.saturating_sub(old.ticks) as f32;
                        s.cpu = Some(d / (dt * tck as f32) * 100.0);
                    }
                }
            }
        }
    }

    sessions.sort_by(|a, b| {
        (a.number().is_none(), a.number(), &a.name).cmp(&(
            b.number().is_none(),
            b.number(),
            &b.name,
        ))
    });

    State {
        sessions,
        at: Some(now),
        running: true,
        error: None,
        live_claude,
    }
}
