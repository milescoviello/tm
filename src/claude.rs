//! What Claude Code is running where, and how to start it again.
//!
//! Claude Code (2.1.3xx) writes `~/.claude/sessions/<pid>.json` for each live
//! process. That file names the exact session id and, when the process runs
//! inside tmux, its pane (`"tmux": "13:@11.%11"`). It is the only exact link
//! from a running process to a transcript: the process keeps no transcript fd
//! open and carries no session id in its environment. A file can outlive its
//! process, so each one is checked against /proc: the pid must be alive AND
//! have the recorded start time, or a reused pid would claim a dead session.

use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::procs::Table;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Live {
    pub pid: i32,
    pub session_id: String,
    pub cwd: String,
    /// busy | idle | shell | waiting ... whatever Claude reports.
    pub status: String,
    /// tmux pane id (`%11`) when the process runs inside tmux.
    pub pane: Option<String>,
    pub name: String,
    pub kind: String,
}

fn str_of(v: &Value, k: &str) -> Option<String> {
    match v.get(k)? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Parse one sessions/<pid>.json. `start_of` returns the /proc start time of
/// a pid, or None when it is gone.
pub fn parse_live(text: &str, start_of: impl Fn(i32) -> Option<u64>) -> Option<Live> {
    let v: Value = serde_json::from_str(text).ok()?;
    let pid = v.get("pid")?.as_i64()? as i32;
    let session_id = str_of(&v, "sessionId")?;
    let started = start_of(pid)?;
    if let Some(ps) = str_of(&v, "procStart") {
        if ps.parse::<u64>().ok() != Some(started) {
            return None;
        }
    }
    let pane = str_of(&v, "tmux").and_then(|t| {
        let id = t.rsplit('.').next()?.to_string();
        id.starts_with('%').then_some(id)
    });
    Some(Live {
        pid,
        session_id,
        cwd: str_of(&v, "cwd").unwrap_or_default(),
        status: str_of(&v, "status").unwrap_or_default(),
        pane,
        name: str_of(&v, "name").unwrap_or_default(),
        kind: str_of(&v, "kind").unwrap_or_default(),
    })
}

/// Every live Claude process that wrote a sessions file.
pub fn load_live(claude_dir: &Path, procs: &Table) -> Vec<Live> {
    let mut out = Vec::new();
    let Ok(dir) = fs::read_dir(claude_dir.join("sessions")) else {
        return out;
    };
    for e in dir.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&p) else {
            continue;
        };
        if let Some(l) = parse_live(&text, |pid| procs.procs.get(&pid).map(|p| p.start)) {
            out.push(l);
        }
    }
    out
}

pub fn is_claude_argv(argv: &[String], comm: &str) -> bool {
    if comm == "claude" || comm == "claude.exe" {
        return true;
    }
    argv.first()
        .map(|a| {
            let b = crate::util::basename(a);
            b == "claude" || b == "claude.exe"
        })
        .unwrap_or(false)
}

/// The session id a command line names, any of the ways claude accepts one.
pub fn session_from_argv(argv: &[String]) -> Option<String> {
    let mut it = argv.iter().skip(1).peekable();
    while let Some(a) = it.next() {
        for flag in ["--resume", "--session-id"] {
            if let Some(v) = a.strip_prefix(&format!("{flag}=")) {
                return Some(v.to_string());
            }
        }
        if a == "--resume" || a == "-r" || a == "--session-id" {
            if let Some(v) = it.peek() {
                if !v.starts_with('-') {
                    return Some((*v).clone());
                }
            }
        }
    }
    None
}

const BOOL_FLAGS: &[&str] = &[
    "--dangerously-skip-permissions",
    "--allow-dangerously-skip-permissions",
    "--strict-mcp-config",
    "--verbose",
    "--chrome",
    "--no-chrome",
    "--ide",
];
const VALUE_FLAGS: &[&str] = &[
    "--model",
    "--permission-mode",
    "--agent",
    "--settings",
    "--fallback-model",
    "--mcp-config",
    "--setting-sources",
];

/// The flags worth carrying into a resumed run. Everything else goes: an old
/// `--resume`/`--continue` would pick the wrong session, and a prompt given
/// on the command line would be sent a second time.
pub fn carried_flags(argv: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 1;
    while i < argv.len() {
        let a = &argv[i];
        if BOOL_FLAGS.contains(&a.as_str()) {
            if !out.contains(a) {
                out.push(a.clone());
            }
            i += 1;
            continue;
        }
        if let Some((k, v)) = a.split_once('=') {
            if VALUE_FLAGS.contains(&k) {
                out.push(k.to_string());
                out.push(v.to_string());
            }
            i += 1;
            continue;
        }
        if VALUE_FLAGS.contains(&a.as_str()) {
            if let Some(v) = argv.get(i + 1) {
                out.push(a.clone());
                out.push(v.clone());
                i += 2;
                continue;
            }
        }
        if a == "--add-dir" {
            // Variadic in claude. Only real directories count, so a prompt
            // after it is not mistaken for one.
            let mut j = i + 1;
            let mut dirs = Vec::new();
            while let Some(d) = argv.get(j) {
                if d.starts_with('-') || !Path::new(d).is_dir() {
                    break;
                }
                dirs.push(d.clone());
                j += 1;
            }
            if !dirs.is_empty() {
                out.push(a.clone());
                out.extend(dirs);
            }
            i = j;
            continue;
        }
        i += 1;
    }
    out
}

/// `claude --resume <id> [--model m] <flags>`: `model` (from the transcript,
/// so a mid-session `/model` switch survives) replaces any `--model` flag.
pub fn resume_command(session_id: &str, model: Option<&str>, flags: &[String]) -> Vec<String> {
    let mut out = vec![
        "claude".to_string(),
        "--resume".into(),
        session_id.to_string(),
    ];
    let mut rest = Vec::new();
    let mut i = 0;
    let mut had_model = None;
    while i < flags.len() {
        if flags[i] == "--model" {
            had_model = flags.get(i + 1).cloned();
            i += 2;
            continue;
        }
        rest.push(flags[i].clone());
        i += 1;
    }
    if let Some(m) = model.map(str::to_string).or(had_model) {
        out.push("--model".into());
        out.push(m);
    }
    out.extend(rest);
    out
}

/// Environment worth carrying: workspace ids some tools (wsx) need, and the
/// endpoint/model overrides that point a session at a local model. Never
/// tokens or keys.
pub fn keep_env(k: &str) -> bool {
    if k.ends_with("_TOKEN") || k.ends_with("_KEY") || k.contains("SECRET") {
        return false;
    }
    k.starts_with("WSX_")
        || k == "ANTHROPIC_BASE_URL"
        || k == "ANTHROPIC_MODEL"
        || k == "ANTHROPIC_SMALL_FAST_MODEL"
        || k.starts_with("ANTHROPIC_DEFAULT_")
}

/// Claude sets the terminal title to its session title behind a spinner or
/// status glyph. Strip the glyph; drop generic titles.
pub fn clean_title(t: &str) -> String {
    let t = t.trim_start_matches(|c: char| {
        c.is_whitespace() || ('\u{2800}'..='\u{28ff}').contains(&c) || "·✢✳✶✻✽*●◉○◌⏺".contains(c)
    });
    let t = t.trim();
    if t.eq_ignore_ascii_case("claude") || t.eq_ignore_ascii_case("claude code") {
        return String::new();
    }
    t.to_string()
}

/// Claude's own folder naming for projects/: every non-alphanumeric -> '-'.
pub fn project_dir_name(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

pub fn find_transcript(claude_dir: &Path, session_id: &str, cwd: Option<&str>) -> Option<PathBuf> {
    let projects = claude_dir.join("projects");
    let file = format!("{session_id}.jsonl");
    if let Some(cwd) = cwd {
        let p = projects.join(project_dir_name(cwd)).join(&file);
        if p.is_file() {
            return Some(p);
        }
    }
    for e in fs::read_dir(&projects).ok()?.flatten() {
        let p = e.path().join(&file);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TailInfo {
    pub title: Option<String>,
    pub model: Option<String>,
}

fn real_model(m: &str) -> bool {
    !m.is_empty() && !m.starts_with('<') && m != "inherit" && m != "sniff"
}

/// Title and latest model from the end of a transcript. The title line can
/// sit far from the end, so when the tail has none the whole file is
/// scanned, looking only at lines that begin with the title record.
pub fn tail_info(path: &Path) -> TailInfo {
    const TAIL: u64 = 2 * 1024 * 1024;
    let mut info = TailInfo::default();
    let Ok(mut f) = fs::File::open(path) else {
        return info;
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let from = len.saturating_sub(TAIL);
    if f.seek(SeekFrom::Start(from)).is_err() {
        return info;
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return info;
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<&str> = text.split('\n').collect();
    if from > 0 && !lines.is_empty() {
        lines.remove(0); // cut mid-line
    }
    for line in lines.iter().rev() {
        if info.title.is_none() && line.starts_with("{\"type\":\"ai-title\"") {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                info.title = v
                    .get("aiTitle")
                    .and_then(|t| t.as_str())
                    .map(str::to_string);
            }
        }
        if info.model.is_none() && line.contains("\"role\":\"assistant\"") {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                if let Some(m) = v.pointer("/message/model").and_then(|m| m.as_str()) {
                    if real_model(m) {
                        info.model = Some(m.to_string());
                    }
                }
            }
        }
        if info.title.is_some() && info.model.is_some() {
            return info;
        }
    }
    if info.title.is_none() && from > 0 {
        info.title = scan_title(path);
    }
    info
}

fn scan_title(path: &Path) -> Option<String> {
    let f = fs::File::open(path).ok()?;
    let mut r = BufReader::new(f);
    let mut line = Vec::new();
    let mut found = None;
    loop {
        line.clear();
        match r.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if line.starts_with(b"{\"type\":\"ai-title\"") {
            if let Ok(v) = serde_json::from_slice::<Value>(&line) {
                if let Some(t) = v.get("aiTitle").and_then(|t| t.as_str()) {
                    found = Some(t.to_string());
                }
            }
        }
    }
    found
}

/// Cache of `tail_info` keyed by file length: transcripts only grow, so an
/// unchanged length means an unchanged answer.
#[derive(Default)]
pub struct TailCache {
    map: HashMap<PathBuf, (u64, TailInfo)>,
}

impl TailCache {
    pub fn get(&mut self, path: &Path) -> TailInfo {
        let len = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if let Some((l, info)) = self.map.get(path) {
            if *l == len {
                return info.clone();
            }
        }
        let info = tail_info(path);
        self.map.insert(path.to_path_buf(), (len, info.clone()));
        info
    }
}

/// A transcript's own title, cached across calls (only re-read when the file
/// grows). Used when a pane's terminal title doesn't name the session.
pub fn transcript_title(claude_dir: &Path, session_id: &str, cwd: &str) -> Option<String> {
    use std::sync::{Mutex, OnceLock};
    type Cache = (TailCache, HashMap<String, Option<PathBuf>>);
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    let m = CACHE.get_or_init(|| Mutex::new((TailCache::default(), HashMap::new())));
    let mut g = m.lock().ok()?;
    let (cache, paths) = &mut *g;
    let path = paths
        .entry(session_id.to_string())
        .or_insert_with(|| find_transcript(claude_dir, session_id, Some(cwd)))
        .clone()?;
    cache.get(&path).title
}

/// Last resort for a claude with no sessions file and no id on its command
/// line: the transcript in its folder whose title matches the pane title,
/// among those written since the process started.
pub fn match_by_title(
    claude_dir: &Path,
    cwd: &str,
    title: &str,
    since_unix: i64,
) -> Option<String> {
    if title.is_empty() {
        return None;
    }
    let dir = claude_dir.join("projects").join(project_dir_name(cwd));
    let mut best: Option<(i64, String)> = None;
    for e in fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("jsonl") {
            continue;
        }
        let mtime = e
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        if mtime < since_unix {
            continue;
        }
        if tail_info(&p).title.as_deref() == Some(title) {
            let id = p.file_stem()?.to_str()?.to_string();
            if best.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
                best = Some((mtime, id));
            }
        }
    }
    best.map(|(_, id)| id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn live_file_checks_pid_start() {
        let j = r#"{"pid":42,"sessionId":"abc","cwd":"/w","procStart":"777","status":"busy","tmux":"13:@11.%11","kind":"interactive"}"#;
        let l = parse_live(j, |p| (p == 42).then_some(777)).unwrap();
        assert_eq!(l.session_id, "abc");
        assert_eq!(l.pane.as_deref(), Some("%11"));
        assert_eq!(l.status, "busy");
        // reused pid: same number, different start time
        assert!(parse_live(j, |_| Some(778)).is_none());
        // dead pid
        assert!(parse_live(j, |_| None).is_none());
        // background job, no tmux
        let j = r#"{"pid":42,"sessionId":"abc","procStart":777,"kind":"bg"}"#;
        let l = parse_live(j, |_| Some(777)).unwrap();
        assert_eq!(l.pane, None);
    }

    #[test]
    fn session_id_any_spelling() {
        assert_eq!(
            session_from_argv(&s(&["claude", "--resume", "X"])).as_deref(),
            Some("X")
        );
        assert_eq!(
            session_from_argv(&s(&["claude", "-r", "X"])).as_deref(),
            Some("X")
        );
        assert_eq!(
            session_from_argv(&s(&["claude", "--session-id=X"])).as_deref(),
            Some("X")
        );
        assert_eq!(session_from_argv(&s(&["claude", "--resume"])), None);
        assert_eq!(session_from_argv(&s(&["claude", "-p", "hi"])), None);
    }

    #[test]
    fn carries_only_safe_flags() {
        let argv = s(&[
            "claude",
            "--resume",
            "OLD",
            "--model",
            "m1",
            "--dangerously-skip-permissions",
            "--permission-mode=plan",
            "-c",
            "do the thing",
        ]);
        let f = carried_flags(&argv);
        assert_eq!(
            f,
            s(&[
                "--model",
                "m1",
                "--dangerously-skip-permissions",
                "--permission-mode",
                "plan"
            ])
        );
        let cmd = resume_command("NEW", Some("m2"), &f);
        assert_eq!(
            cmd,
            s(&[
                "claude",
                "--resume",
                "NEW",
                "--model",
                "m2",
                "--dangerously-skip-permissions",
                "--permission-mode",
                "plan"
            ])
        );
        let cmd = resume_command("NEW", None, &f);
        assert_eq!(&cmd[3..5], &s(&["--model", "m1"])[..]);
    }

    #[test]
    fn add_dir_takes_only_real_dirs() {
        let argv = s(&["claude", "--add-dir", "/", "/definitely/not/here", "hello"]);
        assert_eq!(carried_flags(&argv), s(&["--add-dir", "/"]));
    }

    #[test]
    fn titles() {
        assert_eq!(
            clean_title("✳ Parser state machine TUI"),
            "Parser state machine TUI"
        );
        assert_eq!(clean_title("⠂ Busy one"), "Busy one");
        assert_eq!(clean_title("plain-name"), "plain-name");
        assert_eq!(clean_title("✳ Claude Code"), "");
    }

    #[test]
    fn env_filter() {
        assert!(keep_env("WSX_WORKSPACE_ID"));
        assert!(keep_env("ANTHROPIC_BASE_URL"));
        assert!(!keep_env("ANTHROPIC_AUTH_TOKEN"));
        assert!(!keep_env("ANTHROPIC_API_KEY"));
        assert!(!keep_env("PATH"));
    }

    #[test]
    fn project_names() {
        assert_eq!(project_dir_name("/home/u/my.proj"), "-home-u-my-proj");
    }

    #[test]
    fn tail_reads_title_and_model() {
        let dir = std::env::temp_dir().join(format!("tm-tail-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.jsonl");
        let body = [
            r#"{"type":"ai-title","aiTitle":"First"}"#,
            r#"{"parentUuid":null,"message":{"role":"assistant","model":"model-a","content":[{"type":"text","text":"x"}]}}"#,
            r#"{"type":"ai-title","aiTitle":"Second"}"#,
            r#"{"message":{"role":"assistant","model":"<synthetic>"}}"#,
        ]
        .join("\n");
        fs::write(&p, body).unwrap();
        let i = tail_info(&p);
        assert_eq!(i.title.as_deref(), Some("Second"));
        assert_eq!(i.model.as_deref(), Some("model-a"));
        fs::remove_dir_all(&dir).ok();
    }
}
