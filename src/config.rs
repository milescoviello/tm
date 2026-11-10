//! `~/.config/tm/config.toml`. Every key is optional.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub autosave_minutes: u64,
    pub keep_snapshots: usize,
    pub scrollback_lines: u32,
    pub claude_args: Vec<String>,
    pub claude_stagger_secs: u64,
    pub auto_name: bool,
    pub rerun: Vec<String>,
    pub never_rerun: Vec<String>,
    pub preview: bool,
    pub mouse: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            autosave_minutes: 15,
            keep_snapshots: 50,
            scrollback_lines: 2000,
            claude_args: vec!["--dangerously-skip-permissions".into()],
            claude_stagger_secs: 2,
            auto_name: true,
            rerun: vec![],
            never_rerun: vec![],
            preview: true,
            mouse: true,
        }
    }
}

/// Programs restore may start again. Interactive viewers only: nothing here
/// changes anything when it starts. A pane running anything else comes back
/// as a shell in the same folder.
pub const RERUN: &[&str] = &[
    "htop",
    "btop",
    "top",
    "atop",
    "nvtop",
    "glances",
    "iotop",
    "bmon",
    "nload",
    "s-tui",
    "nvim",
    "vim",
    "vi",
    "nano",
    "micro",
    "hx",
    "less",
    "man",
    "ssh",
    "mosh",
    "tail",
    "journalctl",
    "lazygit",
    "lazydocker",
    "k9s",
    "ncdu",
    "ranger",
    "yazi",
    "mc",
    "cmus",
    "ncmpcpp",
    "weechat",
    "irssi",
    "tig",
    "mn",
    "mnemosyne",
    "theia",
    "th",
];

pub const TEMPLATE: &str = r#"# tm configuration. Every key is optional; these are the defaults.

# How often `tm autosave` (started from ~/.tmux.conf) takes a snapshot.
autosave_minutes = 15

# Snapshots kept in ~/.local/state/tm/snapshots. Oldest go first.
keep_snapshots = 50

# Lines of scrollback saved per non-Claude pane and printed back on restore.
scrollback_lines = 2000

# Flags for a NEW Claude started from tm's `n` menu. Restored sessions keep
# the flags they were started with.
claude_args = ["--dangerously-skip-permissions"]

# Seconds between Claude starts during a restore, so a dozen don't all load
# at once.
claude_stagger_secs = 2

# Rename numbered sessions to "<number>-<claude title>" (e.g. 9 -> 9-api-rate-limiter).
# Sessions you name yourself are never touched.
auto_name = true

# Extra programs restore may start again (the built-in list is viewers and
# editors: htop, btop, nvim, less, ssh, tail, journalctl, ...).
rerun = []

# Programs restore must never start again, even if built in.
never_rerun = []

# Start with the live preview and mouse on (p and M toggle them).
preview = true
mouse = true
"#;

pub fn load() -> Config {
    let p = crate::util::config_path();
    match std::fs::read_to_string(&p) {
        Ok(s) => match toml::from_str(&s) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("tm: ignoring {}: {e}", p.display());
                Config::default()
            }
        },
        Err(_) => Config::default(),
    }
}

impl Config {
    /// May a pane running `argv` be started again on restore?
    pub fn rerunnable(&self, argv: &[String]) -> bool {
        let Some(first) = argv.first() else {
            return false;
        };
        let name = crate::util::basename(first);
        if self.never_rerun.iter().any(|n| n == name) {
            return false;
        }
        let listed = RERUN.contains(&name) || self.rerun.iter().any(|n| n == name);
        if !listed {
            return false;
        }
        match name {
            // ssh/mosh only as an interactive login: `ssh host cmd` runs cmd.
            "ssh" | "mosh" => remote_positionals(argv) == 1,
            _ => true,
        }
    }
}

/// Count ssh's non-option words (host, then any remote command).
fn remote_positionals(argv: &[String]) -> usize {
    const WITH_VALUE: &str = "BbcDEeFIiJLlmOoPpQRSWw";
    let mut n = 0;
    let mut i = 1;
    while i < argv.len() {
        let a = &argv[i];
        if a == "--" {
            n += argv.len() - i - 1;
            break;
        }
        if let Some(opt) = a.strip_prefix('-') {
            if opt.len() == 1 && WITH_VALUE.contains(opt) {
                i += 1;
            }
            i += 1;
            continue;
        }
        n += 1;
        i += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn template_parses_to_defaults() {
        let c: Config = toml::from_str(TEMPLATE).unwrap();
        assert_eq!(c, Config::default());
    }

    #[test]
    fn rerun_rules() {
        let c = Config::default();
        assert!(c.rerunnable(&s(&["htop"])));
        assert!(c.rerunnable(&s(&["/usr/bin/nvim", "notes.md"])));
        assert!(c.rerunnable(&s(&["ssh", "-p", "22", "host"])));
        assert!(!c.rerunnable(&s(&["ssh", "host", "rm", "-rf", "x"])));
        assert!(!c.rerunnable(&s(&["make", "-j8"])));
        assert!(!c.rerunnable(&s(&["watch", "ls"])));
        let c = Config {
            never_rerun: vec!["htop".into()],
            rerun: vec!["make".into()],
            ..Config::default()
        };
        assert!(!c.rerunnable(&s(&["htop"])));
        assert!(c.rerunnable(&s(&["make"])));
    }
}
