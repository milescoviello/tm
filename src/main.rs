//! tm: see what every tmux session is running, snapshot it, bring it back.

mod claude;
mod config;
mod git;
mod live;
mod naming;
mod newsess;
mod procs;
mod restore;
mod snapshot;
mod tmux;
mod ui;
mod util;

use anyhow::{anyhow, bail, Result};
use std::time::Duration;

use crate::live::{Kind, State};
use crate::snapshot::Saved;
use crate::tmux::{exact, Tmux};
use crate::util::{ago, fit, fit_left, pad, pad_left, tilde};

const HELP: &str = "\
tm - a tmux manager

usage:
  tm                     open the live view (also: prefix + t in tmux)
  tm <n|name|words>      switch to a session: by number (tm 9), name, or
                         any unique piece of its name or Claude title
  tm ls                  print every session, what runs in it, and its cost
  tm save [-q]           take a snapshot now (written only if something changed)
  tm restore [options]   bring back the newest snapshot
      -n, --dry-run        print what would happen, change nothing
      --snapshot STAMP     use an older snapshot (see tm snapshots)
      --only N,NAME,...    only these sessions
      --boot               do nothing if tmux already has sessions (for boot)
      --ignore-running     with --dry-run: plan as if nothing were running
  tm snapshots           list saved snapshots, newest first
  tm autosave            save every autosave_minutes until the server exits
                         (started by ~/.tmux.conf; one copy runs at a time)
  tm write-config        write a commented ~/.config/tm/config.toml

options:
  --socket NAME|PATH     talk to another tmux server (-L NAME / -S PATH);
                         also TM_TMUX_SOCKET

files:
  ~/.local/state/tm/snapshots/   one JSON file per snapshot, newest 50 kept
  ~/.local/state/tm/scrollback/  saved pane history, shared between snapshots
  ~/.config/tm/config.toml       settings (tm write-config)
";

fn main() {
    if let Err(e) = real_main() {
        eprintln!("tm: {e:#}");
        std::process::exit(1);
    }
}

fn take_value(args: &mut Vec<String>, names: &[&str]) -> Result<Option<String>> {
    for n in names {
        if let Some(i) = args.iter().position(|a| a == n) {
            if i + 1 >= args.len() {
                bail!("{n} needs a value");
            }
            let v = args.remove(i + 1);
            args.remove(i);
            return Ok(Some(v));
        }
        let pre = format!("{n}=");
        if let Some(i) = args.iter().position(|a| a.starts_with(&pre)) {
            let v = args.remove(i)[pre.len()..].to_string();
            return Ok(Some(v));
        }
    }
    Ok(None)
}

fn take_flag(args: &mut Vec<String>, names: &[&str]) -> bool {
    let before = args.len();
    args.retain(|a| !names.contains(&a.as_str()));
    args.len() != before
}

fn real_main() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let socket = take_value(&mut args, &["--socket"])?;
    let tmux = Tmux::new(socket.as_deref());
    let cfg = config::load();
    let first = args.first().cloned();
    let rest: Vec<String> = args.iter().skip(1).cloned().collect();
    match first.as_deref() {
        None => ui::run(tmux, cfg, ui::Opts { popup: false }),
        Some("--popup") => ui::run(tmux, cfg, ui::Opts { popup: true }),
        Some("-h" | "--help" | "help") => {
            print!("{HELP}");
            Ok(())
        }
        Some("-V" | "--version" | "version") => {
            println!("tm {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("ls" | "list") => cmd_ls(&tmux),
        Some("save") => cmd_save(&tmux, &cfg, rest),
        Some("restore") => cmd_restore(&tmux, &cfg, rest),
        Some("snapshots" | "snaps") => cmd_snapshots(),
        Some("autosave") => cmd_autosave(&tmux, rest),
        Some("write-config") => cmd_write_config(),
        Some("a" | "attach" | "switch") => {
            let t = rest
                .first()
                .ok_or_else(|| anyhow!("switch to which session?"))?;
            cmd_attach(&tmux, t)
        }
        Some(x) if x.starts_with('-') => bail!("unknown option {x} (see tm --help)"),
        Some(_) => cmd_attach(&tmux, &args.join(" ")),
    }
}

/// Which session does `q` mean? A number first, then an exact name, then a
/// unique prefix, then a unique piece of a name or Claude title.
fn resolve(state: &State, q: &str) -> Result<String> {
    let q = q.trim();
    if let Ok(n) = q.parse::<u32>() {
        if let Some(s) = state.sessions.iter().find(|s| s.number() == Some(n)) {
            return Ok(s.name.clone());
        }
    }
    if let Some(s) = state.session(q) {
        return Ok(s.name.clone());
    }
    let pre: Vec<&str> = state
        .sessions
        .iter()
        .filter(|s| s.name.starts_with(q))
        .map(|s| s.name.as_str())
        .collect();
    if pre.len() == 1 {
        return Ok(pre[0].to_string());
    }
    let ql = q.to_lowercase();
    let hits: Vec<&str> = state
        .sessions
        .iter()
        .filter(|s| {
            s.name.to_lowercase().contains(&ql)
                || s.panes().any(|p| p.label.to_lowercase().contains(&ql))
        })
        .map(|s| s.name.as_str())
        .collect();
    match hits.len() {
        1 => Ok(hits[0].to_string()),
        0 => bail!("no session matches '{q}' (tm ls lists them)"),
        _ => bail!("'{q}' matches {}: be more specific", hits.join(", ")),
    }
}

fn cmd_attach(tmux: &Tmux, q: &str) -> Result<()> {
    let state = live::collect(tmux, &util::claude_dir(), None);
    if !state.running {
        bail!("no tmux server is running (tm restore brings back the last snapshot)");
    }
    let name = resolve(&state, q)?;
    if std::env::var_os("TMUX").is_some() {
        tmux.run(["switch-client", "-t", &exact(&name)])?;
        Ok(())
    } else {
        ui::attach_outside(tmux, &exact(&name))
    }
}

fn cmd_ls(tmux: &Tmux) -> Result<()> {
    let cdir = util::claude_dir();
    let first = live::collect(tmux, &cdir, None);
    if !first.running {
        println!("no tmux server is running");
        if let Some((p, s)) = snapshot::latest() {
            println!(
                "newest snapshot: {} ({} sessions, {} claude). tm restore brings it back.",
                snapshot::stamp_of(&p),
                s.sessions.len(),
                s.claude_count()
            );
        }
        return Ok(());
    }
    std::thread::sleep(Duration::from_millis(300));
    let state = live::collect(tmux, &cdir, Some(&first));
    let now = util::now();
    let nw = state
        .sessions
        .iter()
        .map(|s| util::width(&s.name))
        .max()
        .unwrap_or(4)
        .clamp(4, 28);
    println!(
        "{}  {}  {}  {}  {}  {}  {}",
        pad("SESSION", nw),
        pad("STATE", 6),
        pad("WHAT", 40),
        pad("FOLDER", 26),
        pad_left("ACTIVE", 6),
        pad_left("CPU", 4),
        pad_left("RAM", 6)
    );
    for s in &state.sessions {
        let many = s.windows.len() > 1;
        for (wi, w) in s.windows.iter().enumerate() {
            for p in &w.panes {
                let first_line =
                    wi == 0 && p.index == w.panes.first().map(|x| x.index).unwrap_or(0);
                let name = if first_line {
                    format!("{}{}", s.name, if s.attached > 0 { "*" } else { "" })
                } else {
                    String::new()
                };
                let state_word = match (&p.kind, &p.claude) {
                    (Kind::Claude, Some(c)) if !c.status.is_empty() => c.status.clone(),
                    (Kind::Claude, _) => "claude".into(),
                    (Kind::Program, _) => "prog".into(),
                    (Kind::Shell, _) => "shell".into(),
                };
                let what = if many || w.panes.len() > 1 {
                    format!("{}.{} {}", w.index, p.index, p.label)
                } else {
                    p.label.clone()
                };
                let mut folder = tilde(&p.cwd);
                if let Some(b) = git::branch(std::path::Path::new(&p.cwd)) {
                    folder = format!("{folder} ({b})");
                }
                let (cpu, ram) = if first_line {
                    (
                        s.cpu.map(|c| format!("{c:.0}%")).unwrap_or_default(),
                        util::bytes(s.rss),
                    )
                } else {
                    (String::new(), String::new())
                };
                println!(
                    "{}  {}  {}  {}  {}  {}  {}",
                    pad(&fit(&name, nw), nw),
                    pad(&state_word, 6),
                    pad(&fit(&what, 40), 40),
                    pad(&fit_left(&folder, 26), 26),
                    pad_left(&ago(now - w.activity), 6),
                    pad_left(&cpu, 4),
                    pad_left(&ram, 6)
                );
            }
        }
    }
    Ok(())
}

fn cmd_save(tmux: &Tmux, cfg: &config::Config, mut rest: Vec<String>) -> Result<()> {
    let quiet = take_flag(&mut rest, &["-q", "--quiet"]);
    let cdir = util::claude_dir();
    let mut state = live::collect(tmux, &cdir, None);
    if cfg.auto_name && state.running && !naming::apply(tmux, &state).is_empty() {
        state = live::collect(tmux, &cdir, None);
    }
    let mut cache = claude::TailCache::default();
    match snapshot::save(tmux, &state, cfg, &mut cache)? {
        Saved::Written(p) => {
            if !quiet {
                println!(
                    "saved {} ({} sessions, {} claude)",
                    p.display(),
                    state.sessions.len(),
                    state.claude_count()
                );
            }
        }
        Saved::Unchanged(p) => {
            if !quiet {
                println!("nothing changed since {}", snapshot::stamp_of(&p));
            }
        }
        Saved::Empty => {
            if !quiet {
                println!("no sessions: nothing saved (the last snapshot is kept)");
            }
        }
        Saved::Skipped(why) => {
            if !quiet {
                println!("not saved: {why}");
            }
        }
    }
    Ok(())
}

fn cmd_restore(tmux: &Tmux, cfg: &config::Config, mut rest: Vec<String>) -> Result<()> {
    let dry_run = take_flag(&mut rest, &["-n", "--dry-run"]);
    let boot = take_flag(&mut rest, &["--boot"]);
    let ignore_running = take_flag(&mut rest, &["--ignore-running"]);
    let stamp = take_value(&mut rest, &["--snapshot", "-s"])?;
    let mut only: Vec<String> = take_value(&mut rest, &["--only"])?
        .map(|v| {
            v.split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect()
        })
        .unwrap_or_default();
    for r in rest {
        if r.starts_with('-') {
            bail!("unknown restore option {r}");
        }
        only.push(r);
    }
    if ignore_running && !dry_run {
        bail!("--ignore-running only makes sense with --dry-run");
    }
    let (path, snap) = match &stamp {
        Some(st) => {
            let p = snapshot::find(st)
                .ok_or_else(|| anyhow!("no single snapshot matches '{st}' (tm snapshots)"))?;
            let s = snapshot::load(&p)?;
            (p, s)
        }
        None => match snapshot::latest() {
            Some(x) => x,
            None => {
                println!("no snapshots yet: nothing to restore");
                return Ok(());
            }
        },
    };
    let log_path = util::state_dir().join("restore.log");
    let mut log_file = if boot && !dry_run {
        let _ = std::fs::create_dir_all(util::state_dir());
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .ok()
    } else {
        None
    };
    let mut log = |line: String| {
        println!("{line}");
        if let Some(f) = log_file.as_mut() {
            use std::io::Write;
            let _ = writeln!(f, "{line}");
        }
    };
    log(format!(
        "{} {} from {} ({} sessions, {} claude){}",
        if dry_run {
            "would restore"
        } else {
            "restoring"
        },
        snapshot::stamp_of(&path),
        if snap.host.is_empty() {
            "?"
        } else {
            &snap.host
        },
        snap.sessions.len(),
        snap.claude_count(),
        if boot {
            format!("  [boot {}]", util::pretty_time(util::now()))
        } else {
            String::new()
        }
    ));
    if !snap.host.is_empty() && snap.host != util::hostname() {
        log(format!(
            "  note: taken on {}, this is {}",
            snap.host,
            util::hostname()
        ));
    }
    let opts = restore::Options {
        dry_run,
        boot,
        only,
        ignore_running,
    };
    let r = restore::restore(tmux, &snap, cfg, &opts, &mut log)?;
    log(format!(
        "{} {} made, {} skipped, {} failed",
        if dry_run { "would be:" } else { "done:" },
        r.made.len(),
        r.skipped.len(),
        r.failed.len()
    ));
    if !r.failed.is_empty() {
        bail!("{} session(s) failed", r.failed.len());
    }
    Ok(())
}

fn cmd_snapshots() -> Result<()> {
    let files = snapshot::list();
    if files.is_empty() {
        println!("no snapshots yet (tm save takes one)");
        return Ok(());
    }
    for p in files.iter().rev() {
        let st = snapshot::stamp_of(p);
        match snapshot::load(p) {
            Ok(s) => println!(
                "{}  {}  {:>2} sessions  {:>2} claude  {}",
                st,
                util::pretty_time(s.taken_at),
                s.sessions.len(),
                s.claude_count(),
                s.host
            ),
            Err(e) => println!("{st}  unreadable: {e}"),
        }
    }
    println!("{} kept in {}", files.len(), snapshot::dir().display());
    Ok(())
}

/// Saves every `autosave_minutes` for as long as the tmux server lives.
/// Each save runs as a fresh process, so installing a new tm binary takes
/// effect at the next save without restarting anything.
fn cmd_autosave(tmux: &Tmux, mut rest: Vec<String>) -> Result<()> {
    let pid_arg = take_value(&mut rest, &["--server-pid"])?;
    let lock = util::lock(&util::state_dir().join("autosave.lock"), false);
    if lock.is_none() {
        return Ok(()); // another copy is already saving
    }
    let server = pid_arg
        .and_then(|p| p.parse::<i32>().ok())
        .or_else(|| tmux.server_pid())
        .ok_or_else(|| anyhow!("no tmux server to watch"))?;
    let exe = std::env::current_exe()?;
    let exe = std::path::PathBuf::from(
        exe.to_string_lossy()
            .trim_end_matches(" (deleted)")
            .to_string(),
    );
    loop {
        let minutes = config::load().autosave_minutes.max(1);
        let until = std::time::Instant::now() + Duration::from_secs(minutes * 60);
        while std::time::Instant::now() < until {
            if !util::pid_alive(server) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_secs(5));
        }
        let mut c = std::process::Command::new(&exe);
        if !tmux.socket_args().is_empty() {
            c.arg("--socket").arg(&tmux.socket_args()[1]);
        }
        let _ = c
            .args(["save", "-q"])
            .stdin(std::process::Stdio::null())
            .status();
    }
}

fn cmd_write_config() -> Result<()> {
    let p = util::config_path();
    if p.exists() {
        println!("{} already exists; not touching it", p.display());
        return Ok(());
    }
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(&p, config::TEMPLATE)?;
    println!("wrote {}", p.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::{Pane, Session, Window};

    fn sess(name: &str, label: &str) -> Session {
        Session {
            id: format!("${name}"),
            name: name.into(),
            created: 0,
            activity: 0,
            attached: 0,
            auto: String::new(),
            windows: vec![Window {
                id: "@1".into(),
                index: 1,
                name: "w".into(),
                active: true,
                layout: String::new(),
                activity: 0,
                auto_rename: true,
                panes: vec![Pane {
                    id: "%1".into(),
                    index: 1,
                    active: true,
                    pid: 1,
                    cwd: "/".into(),
                    cmd: "x".into(),
                    title: String::new(),
                    width: 80,
                    height: 24,
                    dead: false,
                    shell: None,
                    fg_pid: 1,
                    fg_argv: vec![],
                    claude: None,
                    kind: Kind::Shell,
                    label: label.into(),
                }],
            }],
            ticks: 0,
            rss: 0,
            cpu: None,
        }
    }

    #[test]
    fn resolving_sessions() {
        let state = State {
            sessions: vec![
                sess("1-api-work", "API work"),
                sess("10-docs", "Docs cleanup"),
                sess("13-parser", "Parser rewrite"),
                sess("notes", "Scratch"),
            ],
            running: true,
            ..Default::default()
        };
        assert_eq!(resolve(&state, "1").unwrap(), "1-api-work");
        assert_eq!(resolve(&state, "13").unwrap(), "13-parser");
        assert_eq!(resolve(&state, "notes").unwrap(), "notes");
        assert_eq!(resolve(&state, "cleanup").unwrap(), "10-docs");
        assert_eq!(resolve(&state, "Parser").unwrap(), "13-parser");
        assert!(resolve(&state, "nothing").is_err());
        assert!(resolve(&state, "a").is_err()); // ambiguous
    }
}
