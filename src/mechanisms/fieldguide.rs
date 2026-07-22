//! Mechanism 5: the Field Guide (stigmergy). Agent-owned folder whose
//! index.md is injected into every prompt. The harness enforces exactly one
//! constraint, per the article: a line budget on index.md.

use std::path::Path;

use anyhow::Result;

const SEED: &str = "# Field Guide

Agent-owned notes injected into every agent at start. Curate ruthlessly:
anything here shortens (or wastes) every future agent's trajectory. Stay
within the line budget — remove something less valuable before adding.
";

/// Scaffold design/, fieldguide/ and .canopy/ ignores in the target repo.
/// Idempotent; called on the run branch merge worktree.
pub fn ensure_scaffold(repo_worktree: &Path) -> Result<bool> {
    let mut changed = false;
    let fg = repo_worktree.join("fieldguide");
    if !fg.join("index.md").exists() {
        std::fs::create_dir_all(&fg)?;
        std::fs::write(fg.join("index.md"), SEED)?;
        changed = true;
    }
    let design = repo_worktree.join("design");
    if !design.exists() {
        std::fs::create_dir_all(&design)?;
        changed = true;
    }
    // .canopy must never be committed.
    let gi = repo_worktree.join(".gitignore");
    let cur = std::fs::read_to_string(&gi).unwrap_or_default();
    if !cur.lines().any(|l| l.trim() == ".canopy/") {
        std::fs::write(&gi, format!("{}{}.canopy/\n", cur, if cur.is_empty() || cur.ends_with('\n') { "" } else { "\n" }))?;
        changed = true;
    }
    Ok(changed)
}

/// index.md content for prompt injection (empty string if absent).
pub fn index_content(repo_worktree: &Path) -> String {
    std::fs::read_to_string(repo_worktree.join("fieldguide/index.md")).unwrap_or_default()
}

/// Lines over budget, if any — the merge bounce condition.
pub fn over_budget(repo_worktree: &Path, budget: usize) -> Option<usize> {
    let lines = index_content(repo_worktree).lines().count();
    (lines > budget).then_some(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaffold_and_budget() {
        let dir = tempfile::tempdir().unwrap();
        assert!(ensure_scaffold(dir.path()).unwrap());
        assert!(!ensure_scaffold(dir.path()).unwrap()); // idempotent
        assert!(index_content(dir.path()).contains("Field Guide"));
        assert!(over_budget(dir.path(), 200).is_none());
        assert!(over_budget(dir.path(), 2).is_some());
        let gi = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(gi.contains(".canopy/"));
    }
}
