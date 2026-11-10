//! "Number + short title" session names: `9` becomes `9-api-rate-limiter`
//! once the Claude in it has a title. The number stays first, so `tm 9` and
//! tab completion keep working.
//!
//! Only two kinds of session are ever renamed: a bare number, and a name tm
//! gave earlier and nobody changed since (tm stores the name it gave in the
//! session option `@tm-auto`; a different current name means a person renamed
//! it, and from then on it is theirs).

use crate::live::{Session, State};
use crate::tmux::{exact, exact_s, Tmux};

pub fn number(name: &str) -> Option<u32> {
    let digits: String = name.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || digits.len() > 9 {
        return None;
    }
    let rest = &name[digits.len()..];
    if rest.is_empty() || rest.starts_with('-') {
        digits.parse().ok()
    } else {
        None
    }
}

const STOP: &[&str] = &[
    "a", "an", "the", "and", "or", "of", "for", "to", "in", "on", "with", "as", "is", "my", "at",
    "by", "from", "into", "via",
];

/// Up to three meaningful words, at most 24 characters, `[a-z0-9-]` only.
pub fn slug(title: &str) -> String {
    let words: Vec<String> = title
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_ascii_lowercase())
        .collect();
    let mut picked: Vec<String> = words
        .iter()
        .filter(|w| !STOP.contains(&w.as_str()))
        .cloned()
        .collect();
    if picked.is_empty() {
        picked = words;
    }
    let mut out = String::new();
    for w in picked.iter().take(3) {
        let len = if out.is_empty() {
            w.len()
        } else {
            out.len() + 1 + w.len()
        };
        if len > 24 && !out.is_empty() {
            break;
        }
        if !out.is_empty() {
            out.push('-');
        }
        out.push_str(w);
    }
    let cut: String = out.chars().take(24).collect();
    cut.trim_end_matches('-').to_string()
}

pub fn eligible(s: &Session) -> bool {
    let bare = !s.name.is_empty() && s.name.chars().all(|c| c.is_ascii_digit());
    (bare && s.auto.is_empty()) || s.auto_named()
}

/// The name this session should have, when that differs from its name now.
pub fn wanted(s: &Session) -> Option<String> {
    if !eligible(s) {
        return None;
    }
    let n = s.number()?;
    let title = &s.main_claude()?.label;
    if title.is_empty() || title == "claude" {
        return None;
    }
    let sl = slug(title);
    if sl.is_empty() {
        return None;
    }
    let want = format!("{n}-{sl}");
    (want != s.name).then_some(want)
}

/// Rename what should be renamed. Returns (old, new) pairs.
pub fn apply(tmux: &Tmux, state: &State) -> Vec<(String, String)> {
    let mut done = Vec::new();
    for s in &state.sessions {
        let Some(want) = wanted(s) else { continue };
        if state.sessions.iter().any(|o| o.name == want) {
            continue;
        }
        if tmux
            .run(["rename-session", "-t", &exact(&s.name), &want])
            .is_ok()
        {
            let _ = tmux.run(["set-option", "-t", &exact_s(&want), "@tm-auto", &want]);
            done.push((s.name.clone(), want));
        }
    }
    done
}

/// A person named it: stop managing the name.
pub fn release(tmux: &Tmux, session: &str) {
    let _ = tmux.run(["set-option", "-u", "-t", &exact_s(session), "@tm-auto"]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        assert_eq!(number("9"), Some(9));
        assert_eq!(number("13-parser-state"), Some(13));
        assert_eq!(number("9a"), None);
        assert_eq!(number("notes"), None);
        assert_eq!(number("mn-1234abcd"), None);
    }

    #[test]
    fn slugs() {
        assert_eq!(slug("API rate limiter"), "api-rate-limiter");
        assert_eq!(slug("Parser state machine TUI"), "parser-state-machine");
        assert_eq!(
            slug("Release notes and changelog plans"),
            "release-notes-changelog"
        );
        assert_eq!(
            slug("a-very-long-wordthatgoesonandon plus more"),
            "very-long"
        );
        assert_eq!(slug("the"), "the");
        assert_eq!(slug("✳ ✳"), "");
    }
}
