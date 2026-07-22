//! Mechanism 1+2: shared design docs, checked references, divergence
//! detection for the Reconciler. Docs live in `design/DD-*.md` on the run
//! branch; the harness holds the pen (planners only *declare* decisions),
//! which is what turns planner contention into visible data instead of racing
//! writes.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::model::DesignDecision;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocMeta {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub topics: Vec<String>,
    #[serde(default = "default_status")]
    pub status: String, // active | superseded
    #[serde(default)]
    pub author_node: String,
}

fn default_status() -> String {
    "active".into()
}

#[derive(Debug, Clone)]
pub struct DesignDoc {
    pub meta: DocMeta,
    pub body: String,
    pub path: PathBuf,
}

fn slug(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|p| !p.is_empty())
        .take(6)
        .collect::<Vec<_>>()
        .join("-")
}

pub fn design_dir(repo_worktree: &Path) -> PathBuf {
    repo_worktree.join("design")
}

pub fn load_all(repo_worktree: &Path) -> Result<Vec<DesignDoc>> {
    let dir = design_dir(repo_worktree);
    let mut docs = Vec::new();
    if !dir.exists() {
        return Ok(docs);
    }
    for entry in std::fs::read_dir(&dir)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let raw = std::fs::read_to_string(&path)?;
        if let Some((meta, body)) = split_frontmatter(&raw) {
            match serde_yaml::from_str::<DocMeta>(meta) {
                Ok(m) => docs.push(DesignDoc {
                    meta: m,
                    body: body.to_owned(),
                    path: path.clone(),
                }),
                Err(e) => tracing::warn!("skipping malformed design doc {}: {e}", path.display()),
            }
        }
    }
    docs.sort_by(|a, b| a.meta.id.cmp(&b.meta.id));
    Ok(docs)
}

fn split_frontmatter(raw: &str) -> Option<(&str, &str)> {
    let rest = raw.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    let body = rest[end + 4..].trim_start_matches('\n');
    Some((&rest[..end], body))
}

fn render(meta: &DocMeta, body: &str) -> String {
    format!(
        "---\n{}---\n\n{}\n",
        serde_yaml::to_string(meta).expect("meta serializes"),
        body.trim()
    )
}

/// Next free DD number given existing docs.
pub fn next_number(existing: &[DesignDoc]) -> u32 {
    existing
        .iter()
        .filter_map(|d| d.meta.id.strip_prefix("DD-").and_then(|n| n.parse::<u32>().ok()))
        .max()
        .map(|n| n + 1)
        .unwrap_or(1)
}

/// A planner decision that clashes with an existing active doc from another
/// author: same id, or overlapping topics. This is the Reconciler trigger.
pub fn find_conflict<'a>(
    decision: &DesignDecision,
    author_node: &str,
    existing: &'a [DesignDoc],
) -> Option<&'a DesignDoc> {
    existing.iter().find(|d| {
        d.meta.status == "active"
            && d.meta.author_node != author_node
            && (d.meta.id == decision.id
                || d.meta
                    .topics
                    .iter()
                    .any(|t| decision.topics.contains(t)))
    })
}

/// Write a decision to design/. Returns the path. `id` must already be final.
pub fn write_decision(
    repo_worktree: &Path,
    decision: &DesignDecision,
    author_node: &str,
) -> Result<PathBuf> {
    let dir = design_dir(repo_worktree);
    std::fs::create_dir_all(&dir)?;
    let meta = DocMeta {
        id: decision.id.clone(),
        title: decision.title.clone(),
        topics: decision.topics.clone(),
        status: "active".into(),
        author_node: author_node.to_owned(),
    };
    let path = dir.join(format!("{}-{}.md", decision.id, slug(&decision.title)));
    std::fs::write(&path, render(&meta, &decision.content))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

pub fn mark_superseded(doc: &DesignDoc) -> Result<()> {
    let mut meta = doc.meta.clone();
    meta.status = "superseded".into();
    std::fs::write(&doc.path, render(&meta, &doc.body))?;
    Ok(())
}

/// A `canopy-design: DD-n` reference found in source.
#[derive(Debug, Clone)]
pub struct DesignRef {
    pub file: String,
    pub doc_id: String,
}

/// Scan files for design references. `files` are worktree-relative paths.
pub fn scan_refs(repo_worktree: &Path, files: &[String]) -> Vec<DesignRef> {
    let mut refs = Vec::new();
    for f in files {
        let Ok(content) = std::fs::read_to_string(repo_worktree.join(f)) else {
            continue; // deleted or binary
        };
        for cap in find_all_refs(&content) {
            refs.push(DesignRef {
                file: f.clone(),
                doc_id: cap,
            });
        }
    }
    refs
}

fn find_all_refs(content: &str) -> Vec<String> {
    // Grep for "canopy-design:" then take the DD-<digits> token after it.
    let mut out = Vec::new();
    for line in content.lines() {
        if let Some(idx) = line.find("canopy-design:") {
            let rest = &line[idx + "canopy-design:".len()..];
            let tok: String = rest
                .trim_start()
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                .collect();
            if tok.starts_with("DD-") {
                out.push(tok);
            }
        }
    }
    out
}

/// The "compile-checked reference": refs must target an existing, active doc.
pub fn check_refs(refs: &[DesignRef], docs: &[DesignDoc]) -> Vec<String> {
    refs.iter()
        .filter_map(|r| {
            match docs.iter().find(|d| d.meta.id == r.doc_id) {
                None => Some(format!(
                    "{}: references {} which does not exist",
                    r.file, r.doc_id
                )),
                Some(d) if d.meta.status != "active" => Some(format!(
                    "{}: references {} which is superseded",
                    r.file, r.doc_id
                )),
                Some(_) => None,
            }
        })
        .collect()
}

/// All files on the run branch referencing a given doc (Reconciler fallout:
/// these become fix nodes).
pub fn files_referencing(repo_worktree: &Path, all_files: &[String], doc_id: &str) -> Vec<String> {
    scan_refs(repo_worktree, all_files)
        .into_iter()
        .filter(|r| r.doc_id == doc_id)
        .map(|r| r.file)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_roundtrip_and_conflict_detection() {
        let dir = tempfile::tempdir().unwrap();
        let d1 = DesignDecision {
            id: "DD-1".into(),
            title: "Error handling".into(),
            topics: vec!["errors".into()],
            content: "Use anyhow everywhere.".into(),
        };
        write_decision(dir.path(), &d1, "node-a").unwrap();
        let docs = load_all(dir.path()).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].meta.id, "DD-1");
        assert_eq!(next_number(&docs), 2);

        // Same topic, different author → conflict.
        let d2 = DesignDecision {
            id: "DD-9".into(),
            title: "Errors again".into(),
            topics: vec!["errors".into()],
            content: "Use thiserror.".into(),
        };
        assert!(find_conflict(&d2, "node-b", &docs).is_some());
        assert!(find_conflict(&d2, "node-a", &docs).is_none());

        // Superseding removes authority.
        mark_superseded(&docs[0]).unwrap();
        let docs = load_all(dir.path()).unwrap();
        assert_eq!(docs[0].meta.status, "superseded");
        assert!(find_conflict(&d2, "node-b", &docs).is_none());
    }

    #[test]
    fn ref_scan_and_check() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.rs"),
            "// canopy-design: DD-1\nfn x() {}\n// canopy-design: DD-7\n",
        )
        .unwrap();
        let d1 = DesignDecision {
            id: "DD-1".into(),
            title: "T".into(),
            topics: vec![],
            content: "c".into(),
        };
        write_decision(dir.path(), &d1, "n").unwrap();
        let docs = load_all(dir.path()).unwrap();
        let refs = scan_refs(dir.path(), &["a.rs".into()]);
        assert_eq!(refs.len(), 2);
        let violations = check_refs(&refs, &docs);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("DD-7"));
    }
}
