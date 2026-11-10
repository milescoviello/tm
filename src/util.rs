//! Small helpers shared by everything else: where files live, time and size
//! formatting, and quoting words for the shell a pane runs.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Snapshots, scrollback and logs. `TM_STATE_DIR` wins so tests never touch
/// the real history.
pub fn state_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("TM_STATE_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    if let Some(d) = std::env::var_os("XDG_STATE_HOME").filter(|d| !d.is_empty()) {
        return PathBuf::from(d).join("tm");
    }
    home().join(".local/state/tm")
}

pub fn config_path() -> PathBuf {
    if let Some(p) = std::env::var_os("TM_CONFIG").filter(|d| !d.is_empty()) {
        return PathBuf::from(p);
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"));
    base.join("tm/config.toml")
}

/// Where Claude Code keeps `sessions/` and `projects/`.
pub fn claude_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("TM_CLAUDE_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    if let Some(d) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    home().join(".claude")
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// "12s", "5m", "3h", "4d": the width of a column never changes much.
pub fn ago(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86_400)
    }
}

pub fn bytes(b: u64) -> String {
    const K: f64 = 1024.0;
    let f = b as f64;
    if f < K * K {
        format!("{}K", (f / K).round() as u64)
    } else if f < K * K * K {
        format!("{}M", (f / K / K).round() as u64)
    } else {
        format!("{:.1}G", f / K / K / K)
    }
}

/// `/home/me/x` -> `~/x`.
pub fn tilde(p: &str) -> String {
    let h = home();
    let h = h.to_string_lossy();
    if h == "/" {
        return p.to_string();
    }
    if p == h {
        return "~".to_string();
    }
    match p.strip_prefix(&format!("{h}/")) {
        Some(rest) => format!("~/{rest}"),
        None => p.to_string(),
    }
}

/// `~/x` -> `/home/me/x`, for typed paths.
pub fn expand(p: &str) -> PathBuf {
    if p == "~" {
        return home();
    }
    if let Some(rest) = p.strip_prefix("~/") {
        return home().join(rest);
    }
    PathBuf::from(p)
}

pub fn basename(p: &str) -> &str {
    Path::new(p)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(p)
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LocalTime {
    pub year: i32,
    pub mon: u32,
    pub day: u32,
    pub hour: u32,
    pub min: u32,
    pub sec: u32,
    pub wday: u32,
}

pub fn local(ts: i64) -> LocalTime {
    let t: libc::time_t = ts as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&t, &mut tm).is_null() };
    if !ok {
        return LocalTime::default();
    }
    LocalTime {
        year: tm.tm_year + 1900,
        mon: (tm.tm_mon + 1) as u32,
        day: tm.tm_mday as u32,
        hour: tm.tm_hour as u32,
        min: tm.tm_min as u32,
        sec: tm.tm_sec as u32,
        wday: tm.tm_wday as u32,
    }
}

/// File-name stamp, sorts by time: `20261110-174500`.
pub fn stamp(ts: i64) -> String {
    let t = local(ts);
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        t.year, t.mon, t.day, t.hour, t.min, t.sec
    )
}

/// For people: `Tue 11-10 17:45`.
pub fn pretty_time(ts: i64) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let t = local(ts);
    format!(
        "{} {:02}-{:02} {:02}:{:02}",
        DAYS[(t.wday % 7) as usize],
        t.mon,
        t.day,
        t.hour,
        t.min
    )
}

/// Quote one word so the pane's shell reads it back unchanged. Plain words
/// stay bare so the typed command looks like something a person would type.
pub fn quote(word: &str, fish: bool) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./:=@%+,".contains(c));
    if plain {
        return word.to_string();
    }
    if fish {
        format!("'{}'", word.replace('\\', "\\\\").replace('\'', "\\'"))
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

pub fn join_command(argv: &[String], fish: bool) -> String {
    argv.iter()
        .map(|w| quote(w, fish))
        .collect::<Vec<_>>()
        .join(" ")
}

/// FNV-1a, 64 bit. Only used to name scrollback files by their content, so
/// collisions would cost one wrong scrollback, never data.
pub fn fnv64(data: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Truncate to a display width, adding an ellipsis when something was cut.
pub fn fit(s: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    if width == 0 {
        return String::new();
    }
    let total: usize = s.chars().map(|c| c.width().unwrap_or(0)).sum();
    if total <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw + 1 > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

/// Like `fit` but keeps the end, for paths: `…/deep/folder`.
pub fn fit_left(s: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let total: usize = s.chars().map(|c| c.width().unwrap_or(0)).sum();
    if total <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut rev = Vec::new();
    let mut w = 0;
    for c in s.chars().rev() {
        let cw = c.width().unwrap_or(0);
        if w + cw + 1 > width {
            break;
        }
        rev.push(c);
        w += cw;
    }
    let mut out = String::from("…");
    out.extend(rev.into_iter().rev());
    out
}

pub fn width(s: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(s)
}

pub fn pad(s: &str, w: usize) -> String {
    let have = width(s);
    if have >= w {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(w - have))
    }
}

pub fn pad_left(s: &str, w: usize) -> String {
    let have = width(s);
    if have >= w {
        s.to_string()
    } else {
        format!("{}{s}", " ".repeat(w - have))
    }
}

pub fn pid_alive(pid: i32) -> bool {
    pid > 0 && Path::new(&format!("/proc/{pid}")).exists()
}

/// The user's login shell from the password database, for panes whose own
/// shell was not recorded. `$SHELL` is not set under a boot-time service.
pub fn login_shell() -> String {
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if !pw.is_null() && !(*pw).pw_shell.is_null() {
            if let Ok(s) = std::ffi::CStr::from_ptr((*pw).pw_shell).to_str() {
                if !s.is_empty() {
                    return s.to_string();
                }
            }
        }
    }
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
}

pub fn uid() -> u32 {
    unsafe { libc::getuid() }
}

/// An exclusive `flock` on `path`, held until the File is dropped. With
/// `wait = false`, None means someone else holds it.
pub fn lock(path: &Path, wait: bool) -> Option<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .ok()?;
    let mut op = libc::LOCK_EX;
    if !wait {
        op |= libc::LOCK_NB;
    }
    let r = unsafe { libc::flock(f.as_raw_fd(), op) };
    (r == 0).then_some(f)
}

/// Find a program on PATH.
pub fn which(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let p = PathBuf::from(name);
        return p.is_file().then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_round_trips_for_both_shells() {
        assert_eq!(quote("--model", false), "--model");
        assert_eq!(quote("a b", false), "'a b'");
        assert_eq!(quote("it's", false), "'it'\\''s'");
        assert_eq!(quote("it's", true), "'it\\'s'");
        assert_eq!(quote("", false), "''");
        assert_eq!(quote("~/x", false), "'~/x'");
    }

    #[test]
    fn fitting() {
        assert_eq!(fit("abcdef", 4), "abc…");
        assert_eq!(fit("abc", 4), "abc");
        assert_eq!(fit_left("/a/b/c/d", 5), "…/c/d");
    }

    #[test]
    fn ages() {
        assert_eq!(ago(5), "5s");
        assert_eq!(ago(300), "5m");
        assert_eq!(ago(7200), "2h");
        assert_eq!(ago(200_000), "2d");
    }
}
