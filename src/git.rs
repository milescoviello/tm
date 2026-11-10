//! Current branch of a folder, read straight from `.git/HEAD` so a refresh
//! never spawns git once per pane.

use std::fs;
use std::path::{Path, PathBuf};

pub fn branch(cwd: &Path) -> Option<String> {
    let mut d = Some(cwd);
    while let Some(dir) = d {
        let g = dir.join(".git");
        if g.is_dir() {
            return head(&g);
        }
        if g.is_file() {
            // worktree or submodule: `gitdir: <path>`
            let s = fs::read_to_string(&g).ok()?;
            let p = s.trim().strip_prefix("gitdir:")?.trim();
            let gp = if Path::new(p).is_absolute() {
                PathBuf::from(p)
            } else {
                dir.join(p)
            };
            return head(&gp);
        }
        d = dir.parent();
    }
    None
}

fn head(gitdir: &Path) -> Option<String> {
    let s = fs::read_to_string(gitdir.join("HEAD")).ok()?;
    let s = s.trim();
    match s.strip_prefix("ref: refs/heads/") {
        Some(r) => Some(r.to_string()),
        None => Some(s.chars().take(7).collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_head_and_detached() {
        let root = std::env::temp_dir().join(format!("tm-git-{}", std::process::id()));
        let sub = root.join("a/b");
        fs::create_dir_all(&sub).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".git/HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        assert_eq!(branch(&sub).as_deref(), Some("feature/x"));
        fs::write(root.join(".git/HEAD"), "0123456789abcdef\n").unwrap();
        assert_eq!(branch(&sub).as_deref(), Some("0123456"));
        fs::remove_dir_all(&root).ok();
    }
}
