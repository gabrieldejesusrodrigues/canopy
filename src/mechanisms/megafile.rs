//! Mechanism 4: anti-megafile. Flagged files block every merge that touches
//! them until a Decomposer splits them. Persisted in `.canopy/blocklist.json`
//! so a restart keeps the blocks.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    /// file → decomposer node id handling it ("" until created).
    blocked: BTreeMap<String, String>,
}

pub struct BlockList {
    path: PathBuf,
    state: State,
}

impl BlockList {
    pub fn load(state_dir: &Path) -> Result<BlockList> {
        let path = state_dir.join("blocklist.json");
        let state = match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw)?,
            Err(_) => State::default(),
        };
        Ok(BlockList { path, state })
    }

    fn save(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&self.path, serde_json::to_string_pretty(&self.state)?)?;
        Ok(())
    }

    /// Flag a file. Returns true if newly flagged (caller creates the
    /// Decomposer node and then records it via `assign`).
    pub fn flag(&mut self, file: &str) -> Result<bool> {
        if self.state.blocked.contains_key(file) {
            return Ok(false);
        }
        self.state.blocked.insert(file.to_owned(), String::new());
        self.save()?;
        Ok(true)
    }

    pub fn assign(&mut self, file: &str, decomposer_node: &str) -> Result<()> {
        if let Some(v) = self.state.blocked.get_mut(file) {
            *v = decomposer_node.to_owned();
        }
        self.save()
    }

    /// The decomposer for `node_id` finished: lift its blocks.
    pub fn lift_for_node(&mut self, node_id: &str) -> Result<Vec<String>> {
        let lifted: Vec<String> = self
            .state
            .blocked
            .iter()
            .filter(|(_, v)| v.as_str() == node_id)
            .map(|(k, _)| k.clone())
            .collect();
        for f in &lifted {
            self.state.blocked.remove(f);
        }
        if !lifted.is_empty() {
            self.save()?;
        }
        Ok(lifted)
    }

    /// First blocked file among `files`, if any — the merge gate.
    /// The decomposer itself (node id matches) is exempt: it must touch the
    /// file to split it.
    pub fn gate<'a>(&self, files: &'a [String], node_id: &str) -> Option<&'a str> {
        files.iter().find_map(|f| {
            match self.state.blocked.get(f) {
                Some(owner) if owner != node_id => Some(f.as_str()),
                _ => None,
            }
        })
    }

    pub fn is_empty(&self) -> bool {
        self.state.blocked.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_gate_lift() {
        let dir = tempfile::tempdir().unwrap();
        let mut bl = BlockList::load(dir.path()).unwrap();
        assert!(bl.flag("src/huge.rs").unwrap());
        assert!(!bl.flag("src/huge.rs").unwrap()); // already flagged
        bl.assign("src/huge.rs", "node-d").unwrap();

        let touching = vec!["src/huge.rs".to_string()];
        assert_eq!(bl.gate(&touching, "node-x"), Some("src/huge.rs"));
        assert_eq!(bl.gate(&touching, "node-d"), None); // decomposer exempt

        // Persistence across reload.
        let mut bl2 = BlockList::load(dir.path()).unwrap();
        assert_eq!(bl2.gate(&touching, "node-x"), Some("src/huge.rs"));

        let lifted = bl2.lift_for_node("node-d").unwrap();
        assert_eq!(lifted, vec!["src/huge.rs".to_string()]);
        assert!(bl2.gate(&touching, "node-x").is_none());
        assert!(bl2.is_empty());
    }
}
