//! Talking to tmux. Everything goes through the tmux binary, so tm works with
//! whatever server version is running and never links against it.

use anyhow::{anyhow, Result};
use std::ffi::OsStr;
use std::process::{Command, Stdio};

/// Field separator for `-F` formats. A pane title can hold almost anything,
/// a unit separator realistically never.
pub const SEP: char = '\u{1f}';

#[derive(Clone, Debug, PartialEq)]
pub enum Socket {
    Default,
    /// `-L name`
    Name(String),
    /// `-S path`
    Path(String),
}

#[derive(Clone, Debug)]
pub struct Tmux {
    pub socket: Socket,
}

/// Variables a tmux server must not inherit from whoever starts it. A server
/// started from inside Claude Code would otherwise pass `CLAUDECODE`,
/// `CLAUDE_PID`, `CLAUDE_EFFORT`, its messaging token and the rest to every
/// pane it ever makes. `CLAUDE_CONFIG_DIR` is the user's own and stays.
pub fn scrub_env(c: &mut Command) {
    for (k, _) in std::env::vars_os() {
        let Some(k) = k.to_str() else { continue };
        if scrubbed(k) {
            c.env_remove(k);
        }
    }
}

pub fn scrubbed(k: &str) -> bool {
    (k.starts_with("CLAUDE") && k != "CLAUDE_CONFIG_DIR") || k == "TMUX" || k == "TMUX_PANE"
}

/// Wait (at most `max`) until a pane's shell has printed its prompt: the
/// cursor leaves column 0. Typing before that echoes the line twice, once by
/// the terminal and once by the shell.
pub fn wait_prompt(tmux: &Tmux, pane: &str, max: std::time::Duration) {
    let until = std::time::Instant::now() + max;
    while std::time::Instant::now() < until {
        if let Ok(s) = tmux.run(["display-message", "-p", "-t", pane, "#{cursor_x}"]) {
            if s.trim().parse::<u32>().unwrap_or(0) > 0 {
                return;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

impl Tmux {
    /// `spec` (from `--socket`) or `TM_TMUX_SOCKET`: a path means `-S`, a
    /// bare word means `-L`.
    pub fn new(spec: Option<&str>) -> Tmux {
        let spec = spec
            .map(str::to_string)
            .or_else(|| std::env::var("TM_TMUX_SOCKET").ok())
            .filter(|s| !s.is_empty());
        let socket = match spec {
            None => Socket::Default,
            Some(s) if s.contains('/') => Socket::Path(s),
            Some(s) => Socket::Name(s),
        };
        Tmux { socket }
    }

    pub fn socket_args(&self) -> Vec<String> {
        match &self.socket {
            Socket::Default => vec![],
            Socket::Name(n) => vec!["-L".into(), n.clone()],
            Socket::Path(p) => vec!["-S".into(), p.clone()],
        }
    }

    pub fn command(&self) -> Command {
        let mut c = Command::new("tmux");
        c.args(self.socket_args());
        c.stdin(Stdio::null());
        c
    }

    pub fn run<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let b = self.run_bytes(args)?;
        Ok(String::from_utf8_lossy(&b).into_owned())
    }

    pub fn run_bytes<I, S>(&self, args: I) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let out = self
            .command()
            .args(args)
            .output()
            .map_err(|e| anyhow!("can't run tmux: {e}"))?;
        if out.status.success() {
            Ok(out.stdout)
        } else {
            Err(anyhow!(
                "tmux: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    pub fn quiet<I, S>(&self, args: I) -> bool
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.command()
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    pub fn server_running(&self) -> bool {
        self.quiet(["list-sessions"])
    }

    /// The visible screen of a pane, with colours. `history` adds that many
    /// lines of scrollback above it and joins wrapped lines.
    pub fn capture(&self, pane: &str, history: Option<u32>) -> Result<Vec<u8>> {
        let mut a: Vec<String> = vec!["capture-pane".into(), "-p".into(), "-e".into()];
        if let Some(n) = history {
            a.push("-J".into());
            a.push("-S".into());
            a.push(format!("-{n}"));
        }
        a.push("-t".into());
        a.push(pane.into());
        self.run_bytes(a)
    }

    pub fn server_pid(&self) -> Option<i32> {
        self.run(["display-message", "-p", "#{pid}"])
            .ok()
            .and_then(|s| s.trim().parse().ok())
    }

    /// The session the client running us is looking at, if any.
    pub fn current_session(&self) -> Option<String> {
        std::env::var_os("TMUX")?;
        let s = self
            .run(["display-message", "-p", "#{client_session}"])
            .ok()?;
        let s = s.trim().to_string();
        (!s.is_empty()).then_some(s)
    }
}

/// `=name` targets a session by exact name; without it tmux also accepts
/// prefixes and patterns, and `1` would match `10-...`.
pub fn exact(name: &str) -> String {
    format!("={name}")
}

/// The same, for commands whose -t is a PANE target (set-option,
/// show-options, display-message): those need the trailing colon, or tmux
/// looks for a session literally called "=name".
pub fn exact_s(name: &str) -> String {
    format!("={name}:")
}

/// Width and height from a `window_layout` string (`c3a1,127x33,0,0,...`).
pub fn layout_size(layout: &str) -> Option<(u16, u16)> {
    let dims = layout.split(',').nth(1)?;
    let (w, h) = dims.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(layout_size("c3a1,127x33,0,0,11"), Some((127, 33)));
        assert_eq!(
            layout_size("bb62,159x48,0,0{79x48,0,0,1,79x48,80,0,2}"),
            Some((159, 48))
        );
        assert_eq!(layout_size("junk"), None);
    }

    #[test]
    fn scrub_list() {
        assert!(scrubbed("CLAUDECODE"));
        assert!(scrubbed("CLAUDE_PID"));
        assert!(scrubbed("CLAUDE_CODE_MESSAGING_TOKEN"));
        assert!(scrubbed("TMUX"));
        assert!(!scrubbed("CLAUDE_CONFIG_DIR"));
        assert!(!scrubbed("PATH"));
    }

    #[test]
    fn socket_spec() {
        assert_eq!(Tmux::new(Some("t")).socket, Socket::Name("t".into()));
        assert_eq!(
            Tmux::new(Some("/tmp/s")).socket,
            Socket::Path("/tmp/s".into())
        );
    }
}
