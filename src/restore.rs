//! Bring a snapshot back: sessions, windows, splits and layouts, each pane in
//! its folder with its old scrollback printed back, allowlisted programs
//! started again, and every Claude resumed by its exact session id.
//!
//! Programs and Claude are TYPED into the pane's shell (send-keys) rather
//! than run as the pane's command, the way a person would start them: the
//! login shell sets PATH first, and quitting the program leaves a shell in
//! the right folder instead of closing the pane.

use anyhow::{anyhow, Result};
use std::path::Path;
use std::process::Command;

use crate::claude;
use crate::config::Config;
use crate::live::{self, State};
use crate::snapshot::{self, SPane, SSession, Snapshot};
use crate::tmux::{exact, exact_s, layout_size, scrub_env, wait_prompt, Tmux, SEP};
use crate::util;

#[derive(Clone, Debug, Default)]
pub struct Options {
    pub dry_run: bool,
    /// Boot mode: do nothing at all if the server already has sessions.
    pub boot: bool,
    /// Only these sessions (by name or number).
    pub only: Vec<String>,
    /// Plan as if nothing were running (dry runs against a live server).
    pub ignore_running: bool,
}

#[derive(Debug, Default)]
pub struct Report {
    pub made: Vec<String>,
    pub skipped: Vec<(String, String)>,
    pub failed: Vec<(String, String)>,
}

struct LockGuard(std::path::PathBuf);
impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn wanted(ss: &SSession, only: &[String]) -> bool {
    if only.is_empty() {
        return true;
    }
    only.iter().any(|o| {
        ss.name == *o || crate::naming::number(&ss.name).map(|n| n.to_string()) == Some(o.clone())
    })
}

/// Why this session should not be made now, if it shouldn't.
pub fn skip_reason(ss: &SSession, state: &State) -> Option<String> {
    if state.session(&ss.name).is_some() {
        return Some("already running".into());
    }
    if let Some(n) = crate::naming::number(&ss.name) {
        if let Some(s) = state.sessions.iter().find(|s| s.number() == Some(n)) {
            return Some(format!("already running as {}", s.name));
        }
    }
    for id in ss.claude_ids() {
        if let Some((where_, pid)) = state.where_is(id) {
            let at = match where_ {
                Some(s) => format!("in session {s}"),
                None => format!("as pid {pid}"),
            };
            return Some(format!(
                "its Claude {} is already open {at}",
                &id[..id.len().min(8)]
            ));
        }
    }
    if ss.windows.iter().all(|w| w.panes.is_empty()) {
        return Some("no panes".into());
    }
    None
}

fn is_fish(shell: Option<&str>) -> bool {
    shell.map(|s| util::basename(s) == "fish").unwrap_or(false)
}

/// The pane's own process: its old shell, after printing its old scrollback.
fn pane_command(p: &SPane) -> Vec<String> {
    let shell = p
        .shell
        .clone()
        .filter(|s| Path::new(s).is_file())
        .or_else(|| {
            p.shell
                .as_deref()
                .and_then(|s| util::which(util::basename(s)))
                .map(|x| x.to_string_lossy().into_owned())
        })
        .unwrap_or_else(util::login_shell);
    let sb = p
        .scrollback
        .as_ref()
        .map(|n| snapshot::scrollback_dir().join(n))
        .filter(|f| f.is_file())
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();
    vec![
        "/bin/sh".into(),
        "-c".into(),
        "[ -n \"$1\" ] && cat -- \"$1\"; exec \"$0\" -l".into(),
        shell,
        sb,
    ]
}

/// What gets typed into the pane once its shell is up.
pub fn typed_command(p: &SPane) -> Option<Vec<String>> {
    if let Some(c) = &p.claude {
        return Some(claude::resume_command(
            &c.session_id,
            c.model.as_deref(),
            &c.flags,
        ));
    }
    p.program.clone()
}

fn pane_env(p: &SPane) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(c) = &p.claude {
        for (k, val) in &c.env {
            v.push("-e".into());
            v.push(format!("{k}={val}"));
        }
    }
    v
}

fn folder(p: &SPane) -> (String, bool) {
    if Path::new(&p.cwd).is_dir() {
        (p.cwd.clone(), true)
    } else {
        (util::home().to_string_lossy().into_owned(), false)
    }
}

fn systemd_user() -> bool {
    Path::new(&format!("/run/user/{}/systemd/private", util::uid())).exists()
        && util::which("systemd-run").is_some()
}

struct Maker<'a> {
    tmux: &'a Tmux,
    server_up: bool,
    /// (pane id, typed line, label, is claude)
    queue: Vec<(String, String, String, bool)>,
}

impl Maker<'_> {
    /// Run a tmux command. The first one, when no server is running, starts
    /// the server: with a clean environment, and on systemd in a scope of
    /// its own so it outlives whatever unit started it.
    fn tmux(&mut self, args: Vec<String>) -> Result<String> {
        if self.server_up {
            return self.tmux.run(&args);
        }
        if systemd_user() {
            let mut c = Command::new("systemd-run");
            c.args(["--user", "--scope", "--quiet", "--collect", "--", "tmux"]);
            c.args(self.tmux.socket_args()).args(&args);
            scrub_env(&mut c);
            c.stdin(std::process::Stdio::null());
            if let Ok(out) = c.output() {
                if out.status.success() {
                    self.server_up = true;
                    return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
                }
            }
            // no user bus (or the scope failed): start it directly instead,
            // unless the server came up anyway
            if self.tmux.server_running() {
                self.server_up = true;
                return Err(anyhow!("tmux started but the first session failed"));
            }
        }
        let mut c = Command::new("tmux");
        c.args(self.tmux.socket_args()).args(&args);
        scrub_env(&mut c);
        c.stdin(std::process::Stdio::null());
        let out = c.output().map_err(|e| anyhow!("can't start tmux: {e}"))?;
        if !out.status.success() {
            return Err(anyhow!(
                "tmux: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        self.server_up = true;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn queue_typed(&mut self, pane_id: &str, p: &SPane) {
        if let Some(argv) = typed_command(p) {
            let line = util::join_command(&argv, is_fish(p.shell.as_deref()));
            self.queue.push((
                pane_id.to_string(),
                line,
                p.title.clone(),
                p.claude.is_some(),
            ));
        }
    }

    fn make(&mut self, ss: &SSession, log: &mut dyn FnMut(String)) -> Result<()> {
        let name = &ss.name;
        let mut windows: Vec<_> = ss.windows.iter().filter(|w| !w.panes.is_empty()).collect();
        windows.sort_by_key(|w| w.index);
        let mut first_window = true;
        let mut active_window = None;
        for w in windows {
            let mut panes: Vec<&SPane> = w.panes.iter().collect();
            panes.sort_by_key(|p| p.index);
            let p0 = panes[0];
            let (cwd0, ok0) = folder(p0);
            if !ok0 {
                log(format!(
                    "    folder gone, using ~: {}",
                    util::tilde(&p0.cwd)
                ));
            }
            let first_pane: String;
            if first_window {
                let (cols, rows) = layout_size(&w.layout).unwrap_or((200, 50));
                let mut a: Vec<String> = vec![
                    "new-session".into(),
                    "-d".into(),
                    "-P".into(),
                    "-F".into(),
                    format!("#{{window_index}}{SEP}#{{pane_id}}"),
                    "-s".into(),
                    name.clone(),
                    "-c".into(),
                    cwd0,
                    "-x".into(),
                    cols.to_string(),
                    "-y".into(),
                    rows.to_string(),
                ];
                if !w.auto_rename {
                    a.push("-n".into());
                    a.push(w.name.clone());
                }
                a.extend(pane_env(p0));
                a.push("--".into());
                a.extend(pane_command(p0));
                let out = self.tmux(a)?;
                let out = out.trim();
                let (widx, pid) = out
                    .split_once(SEP)
                    .ok_or_else(|| anyhow!("unexpected tmux output: {out}"))?;
                first_pane = pid.to_string();
                if widx != w.index.to_string() {
                    let _ = self.tmux.run([
                        "move-window",
                        "-s",
                        &format!("{}:{widx}", exact(name)),
                        "-t",
                        &format!("{}:{}", exact(name), w.index),
                    ]);
                }
                first_window = false;
            } else {
                let mut a: Vec<String> = vec![
                    "new-window".into(),
                    "-d".into(),
                    "-P".into(),
                    "-F".into(),
                    "#{pane_id}".into(),
                    "-t".into(),
                    format!("{}:{}", exact(name), w.index),
                    "-c".into(),
                    cwd0,
                ];
                if !w.auto_rename {
                    a.push("-n".into());
                    a.push(w.name.clone());
                }
                a.extend(pane_env(p0));
                a.push("--".into());
                a.extend(pane_command(p0));
                first_pane = self.tmux(a)?.trim().to_string();
            }
            let mut ids = vec![(p0, first_pane.clone())];
            for p in &panes[1..] {
                let (cwd, ok) = folder(p);
                if !ok {
                    log(format!("    folder gone, using ~: {}", util::tilde(&p.cwd)));
                }
                let mut a: Vec<String> = vec![
                    "split-window".into(),
                    "-d".into(),
                    "-P".into(),
                    "-F".into(),
                    "#{pane_id}".into(),
                    "-t".into(),
                    first_pane.clone(),
                    "-c".into(),
                    cwd,
                ];
                a.extend(pane_env(p));
                a.push("--".into());
                a.extend(pane_command(p));
                match self.tmux(a) {
                    Ok(id) => ids.push((p, id.trim().to_string())),
                    Err(e) => log(format!("    pane {} not made: {e}", p.index)),
                }
                // keep room for the next split; the saved layout fixes sizes
                let _ = self.tmux.run(["select-layout", "-t", &first_pane, "tiled"]);
            }
            if ids.len() > 1 && !w.layout.is_empty() {
                if let Err(e) = self
                    .tmux
                    .run(["select-layout", "-t", &first_pane, &w.layout])
                {
                    log(format!("    layout not restored: {e}"));
                }
            }
            for (p, id) in &ids {
                if p.active {
                    let _ = self.tmux.run(["select-pane", "-t", id]);
                }
                self.queue_typed(id, p);
            }
            if w.active {
                active_window = Some(w.index);
            }
        }
        if let Some(i) = active_window {
            let _ = self
                .tmux
                .run(["select-window", "-t", &format!("{}:{i}", exact(name))]);
        }
        if ss.auto_named {
            let _ = self
                .tmux
                .run(["set-option", "-t", &exact_s(name), "@tm-auto", name]);
        }
        Ok(())
    }
}

/// Type a line into a pane's shell, once the shell is ready for it.
pub fn send_line(tmux: &Tmux, pane: &str, line: &str) -> Result<()> {
    wait_prompt(tmux, pane, std::time::Duration::from_secs(5));
    tmux.run(["send-keys", "-t", pane, "-l", "--", line])?;
    tmux.run(["send-keys", "-t", pane, "Enter"])?;
    Ok(())
}

pub fn restore(
    tmux: &Tmux,
    snap: &Snapshot,
    cfg: &Config,
    opts: &Options,
    log: &mut dyn FnMut(String),
) -> Result<Report> {
    let mut report = Report::default();
    let state = live::collect(tmux, &util::claude_dir(), None);
    if opts.boot && state.running && !state.sessions.is_empty() {
        log(format!(
            "tmux already has {} session(s); boot restore does nothing",
            state.sessions.len()
        ));
        return Ok(report);
    }
    let _lock = if opts.dry_run {
        None
    } else {
        let p = util::state_dir().join("restore.lock");
        let _ = std::fs::create_dir_all(util::state_dir());
        std::fs::write(&p, std::process::id().to_string())?;
        Some(LockGuard(p))
    };

    let mut sessions: Vec<&SSession> = snap
        .sessions
        .iter()
        .filter(|s| wanted(s, &opts.only))
        .collect();
    sessions.sort_by(|a, b| {
        let na = crate::naming::number(&a.name);
        let nb = crate::naming::number(&b.name);
        (na.is_none(), na, &a.name).cmp(&(nb.is_none(), nb, &b.name))
    });

    let mut maker = Maker {
        tmux,
        server_up: state.running,
        queue: Vec::new(),
    };
    for ss in sessions {
        if !opts.ignore_running {
            if let Some(why) = skip_reason(ss, &state) {
                log(format!("  skip  {}  ({why})", ss.name));
                report.skipped.push((ss.name.clone(), why));
                continue;
            }
        }
        let wins = ss.windows.len();
        log(format!(
            "  make  {}  {}{}",
            ss.name,
            ss.describe(),
            if wins > 1 {
                format!("  ({wins} windows)")
            } else {
                String::new()
            }
        ));
        if opts.dry_run {
            for w in &ss.windows {
                for p in &w.panes {
                    let what = match typed_command(p) {
                        Some(argv) => util::join_command(&argv, is_fish(p.shell.as_deref())),
                        None => format!(
                            "{} (shell)",
                            util::basename(p.shell.as_deref().unwrap_or("sh"))
                        ),
                    };
                    let sb = if p.scrollback.is_some() {
                        "  +scrollback"
                    } else {
                        ""
                    };
                    log(format!(
                        "        {}.{}  {what}   in {}{sb}",
                        w.index,
                        p.index,
                        util::tilde(&p.cwd)
                    ));
                }
            }
            report.made.push(ss.name.clone());
            continue;
        }
        match maker.make(ss, log) {
            Ok(()) => report.made.push(ss.name.clone()),
            Err(e) => {
                log(format!("  FAILED {}: {e}", ss.name));
                report.failed.push((ss.name.clone(), e.to_string()));
            }
        }
    }

    // viewers first, all at once; then each Claude, a few seconds apart
    let mut queue = std::mem::take(&mut maker.queue);
    queue.sort_by_key(|q| q.3);
    let mut started = 0;
    for (pane, line, label, is_claude) in &queue {
        if *is_claude {
            if started > 0 && cfg.claude_stagger_secs > 0 {
                std::thread::sleep(std::time::Duration::from_secs(cfg.claude_stagger_secs));
            }
            started += 1;
        }
        let (what, name) = if *is_claude {
            ("resumed", label.as_str())
        } else {
            ("started", line.as_str())
        };
        match send_line(tmux, pane, line) {
            Ok(()) => log(format!("  {what}  {name}")),
            Err(e) => log(format!("  could not start {name}: {e}")),
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{SClaude, SWindow};

    fn pane(kind: &str) -> SPane {
        SPane {
            index: 1,
            cwd: "/".into(),
            kind: kind.into(),
            shell: Some("/bin/sh".into()),
            ..Default::default()
        }
    }

    #[test]
    fn typed_commands() {
        let mut p = pane("claude");
        p.claude = Some(SClaude {
            session_id: "abc".into(),
            model: Some("m".into()),
            flags: vec!["--dangerously-skip-permissions".into()],
            env: vec![],
        });
        assert_eq!(
            typed_command(&p).unwrap(),
            vec![
                "claude",
                "--resume",
                "abc",
                "--model",
                "m",
                "--dangerously-skip-permissions"
            ]
        );
        let mut p = pane("program");
        p.program = Some(vec!["htop".into()]);
        assert_eq!(typed_command(&p).unwrap(), vec!["htop"]);
        assert_eq!(typed_command(&pane("shell")), None);
    }

    #[test]
    fn pane_command_execs_old_shell() {
        let p = pane("shell");
        let c = pane_command(&p);
        assert_eq!(c[0], "/bin/sh");
        assert_eq!(c[3], "/bin/sh");
        assert_eq!(c[4], "");
    }

    #[test]
    fn skips_running_and_numbered_twins() {
        let ss = SSession {
            name: "9-old-title".into(),
            windows: vec![SWindow {
                panes: vec![pane("shell")],
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut state = State::default();
        assert_eq!(skip_reason(&ss, &state), None);
        state.sessions.push(crate::live::Session {
            id: "$1".into(),
            name: "9-new-title".into(),
            created: 0,
            activity: 0,
            attached: 0,
            auto: String::new(),
            windows: vec![],
            ticks: 0,
            rss: 0,
            cpu: None,
        });
        assert_eq!(
            skip_reason(&ss, &state).as_deref(),
            Some("already running as 9-new-title")
        );
    }

    #[test]
    fn only_filter() {
        let ss = SSession {
            name: "9-x".into(),
            ..Default::default()
        };
        assert!(wanted(&ss, &[]));
        assert!(wanted(&ss, &["9".into()]));
        assert!(wanted(&ss, &["9-x".into()]));
        assert!(!wanted(&ss, &["1".into()]));
    }
}
