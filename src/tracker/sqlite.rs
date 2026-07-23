use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use rusqlite::{params, Connection};

use crate::model::{AgentRef, NewNode, Node, NodeKind, NodeState, Role, Run};
use crate::tracker::Tracker;

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS runs (
    id         TEXT PRIMARY KEY NOT NULL,
    objective  TEXT NOT NULL,
    root_node  TEXT NOT NULL,
    branch     TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS nodes (
    id              TEXT PRIMARY KEY NOT NULL,
    run_id          TEXT NOT NULL,
    parent_id       TEXT,
    kind            TEXT NOT NULL,
    state           TEXT NOT NULL,
    title           TEXT NOT NULL,
    spec            TEXT NOT NULL,
    agent_json      TEXT,
    depends_on_json TEXT NOT NULL DEFAULT '[]',
    depth           INTEGER NOT NULL DEFAULT 0,
    attempt         INTEGER NOT NULL DEFAULT 0,
    claimed_at      TEXT,
    updated_at      TEXT NOT NULL,
    role_hint       TEXT
);

CREATE TABLE IF NOT EXISTS comments (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    node_id    TEXT NOT NULL,
    body       TEXT NOT NULL,
    created_at TEXT NOT NULL
);
"#;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn kind_to_str(k: NodeKind) -> &'static str {
    match k {
        NodeKind::Plan => "plan",
        NodeKind::Execute => "execute",
    }
}

fn str_to_kind(s: &str) -> Result<NodeKind> {
    match s {
        "plan" => Ok(NodeKind::Plan),
        "execute" => Ok(NodeKind::Execute),
        other => anyhow::bail!("unknown node kind '{other}'"),
    }
}

fn row_to_node(row: &rusqlite::Row<'_>) -> rusqlite::Result<Node> {
    let kind_str: String = row.get(3)?;
    let state_str: String = row.get(4)?;
    let agent_json: Option<String> = row.get(7)?;
    let depends_on_json: String = row.get(8)?;
    let claimed_at_str: Option<String> = row.get(11)?;
    let updated_at_str: String = row.get(12)?;
    let role_hint_str: Option<String> = row.get(13)?;

    let kind = str_to_kind(&kind_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                e.to_string(),
            )),
        )
    })?;
    let state = NodeState::parse(&state_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown state '{state_str}'"),
            )),
        )
    })?;

    let agent: Option<AgentRef> = agent_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                7,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    e.to_string(),
                )),
            )
        })?;

    let depends_on: Vec<String> = serde_json::from_str(&depends_on_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            8,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                e.to_string(),
            )),
        )
    })?;

    let claimed_at = claimed_at_str
        .as_deref()
        .map(|s| chrono::DateTime::parse_from_rfc3339(s).map(|dt| dt.with_timezone(&Utc)))
        .transpose()
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                11,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    e.to_string(),
                )),
            )
        })?;

    let updated_at = chrono::DateTime::parse_from_rfc3339(&updated_at_str)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                12,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    e.to_string(),
                )),
            )
        })?;

    let role_hint = role_hint_str
        .as_deref()
        .map(|s| {
            Role::parse(s).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    13,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("unknown role '{s}'"),
                    )),
                )
            })
        })
        .transpose()?;

    Ok(Node {
        id: row.get(0)?,
        run_id: row.get(1)?,
        parent_id: row.get(2)?,
        kind,
        state,
        title: row.get(5)?,
        spec: row.get(6)?,
        agent,
        depends_on,
        role_hint,
        depth: row.get(9)?,
        attempt: row.get(10)?,
        claimed_at,
        updated_at,
    })
}

// ---------------------------------------------------------------------------
// SqliteTracker
// ---------------------------------------------------------------------------

pub struct SqliteTracker {
    conn: Mutex<Connection>,
}

impl SqliteTracker {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating parent dirs for {}", path.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening sqlite db at {}", path.display()))?;
        conn.execute_batch(SCHEMA).context("initialising schema")?;
        // Best-effort migration for pre-existing dev DBs created before role_hint.
        // Errors (e.g. "duplicate column") are expected once the column exists.
        let _ = conn.execute_batch("ALTER TABLE nodes ADD COLUMN role_hint TEXT");
        // WAL mode improves concurrent reads.
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .context("setting pragma")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
}

#[async_trait]
impl Tracker for SqliteTracker {
    async fn init_run(&self, objective: &str, branch: &str) -> Result<Run> {
        let run_id = uuid::Uuid::new_v4().to_string();
        let node_id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();

        let conn = self.conn.lock().unwrap();
        conn.execute_batch("BEGIN")?;

        conn.execute(
            "INSERT INTO runs (id, objective, root_node, branch, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![run_id, objective, node_id, branch, now],
        ).context("inserting run")?;

        conn.execute(
            "INSERT INTO nodes (id, run_id, parent_id, kind, state, title, spec, agent_json, depends_on_json, depth, attempt, claimed_at, updated_at)
             VALUES (?1, ?2, NULL, 'plan', 'ready', 'Objective', ?3, NULL, '[]', 0, 0, NULL, ?4)",
            params![node_id, run_id, objective, now],
        ).context("inserting root node")?;

        conn.execute_batch("COMMIT")?;

        Ok(Run {
            id: run_id,
            objective: objective.to_string(),
            root_node: node_id,
            branch: branch.to_string(),
            created_at: Utc::now(),
        })
    }

    async fn load_run(&self, run_id: &str) -> Result<Run> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, objective, root_node, branch, created_at FROM runs WHERE id = ?1",
            params![run_id],
            |row| {
                let created_at_str: String = row.get(4)?;
                let created_at = chrono::DateTime::parse_from_rfc3339(&created_at_str)
                    .map(|dt| dt.with_timezone(&Utc))
                    .map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            4,
                            rusqlite::types::Type::Text,
                            Box::new(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                e.to_string(),
                            )),
                        )
                    })?;
                Ok(Run {
                    id: row.get(0)?,
                    objective: row.get(1)?,
                    root_node: row.get(2)?,
                    branch: row.get(3)?,
                    created_at,
                })
            },
        )
        .with_context(|| format!("loading run {run_id}"))
    }

    async fn create_node(&self, new: NewNode) -> Result<Node> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now();
        let now_s = now.to_rfc3339();
        let state = if new.ready {
            NodeState::Ready
        } else {
            NodeState::Blocked
        };
        let state_str = state.as_str();
        let kind_str = kind_to_str(new.kind);
        let agent_json = new
            .agent
            .as_ref()
            .map(|a| serde_json::to_string(a).unwrap());
        let depends_on_json = serde_json::to_string(&new.depends_on).unwrap();
        let role_hint_str = new.role_hint.map(|r| r.as_str());

        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO nodes (id, run_id, parent_id, kind, state, title, spec, agent_json, depends_on_json, depth, attempt, claimed_at, updated_at, role_hint)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 0, NULL, ?11, ?12)",
            params![
                id, new.run_id, new.parent_id, kind_str, state_str,
                new.title, new.spec, agent_json, depends_on_json,
                new.depth, now_s, role_hint_str,
            ],
        ).context("inserting node")?;

        Ok(Node {
            id,
            run_id: new.run_id,
            parent_id: new.parent_id,
            kind: new.kind,
            state,
            title: new.title,
            spec: new.spec,
            agent: new.agent,
            depends_on: new.depends_on,
            role_hint: new.role_hint,
            depth: new.depth,
            attempt: 0,
            claimed_at: None,
            updated_at: now,
        })
    }

    async fn node(&self, id: &str) -> Result<Node> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, run_id, parent_id, kind, state, title, spec, agent_json, depends_on_json, depth, attempt, claimed_at, updated_at, role_hint
             FROM nodes WHERE id = ?1",
            params![id],
            row_to_node,
        ).with_context(|| format!("loading node {id}"))
    }

    async fn children(&self, parent_id: &str) -> Result<Vec<Node>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, run_id, parent_id, kind, state, title, spec, agent_json, depends_on_json, depth, attempt, claimed_at, updated_at, role_hint
             FROM nodes WHERE parent_id = ?1",
        )?;
        let rows = stmt.query_map(params![parent_id], row_to_node)?;
        rows.map(|r| r.map_err(anyhow::Error::from)).collect()
    }

    async fn nodes_in_state(&self, run_id: &str, state: NodeState) -> Result<Vec<Node>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, run_id, parent_id, kind, state, title, spec, agent_json, depends_on_json, depth, attempt, claimed_at, updated_at, role_hint
             FROM nodes WHERE run_id = ?1 AND state = ?2",
        )?;
        let rows = stmt.query_map(params![run_id, state.as_str()], row_to_node)?;
        rows.map(|r| r.map_err(anyhow::Error::from)).collect()
    }

    async fn try_claim(&self, id: &str) -> Result<Option<Node>> {
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        // Atomic: only succeeds if state is currently 'ready'.
        let affected = conn.execute(
            "UPDATE nodes SET state = 'running', claimed_at = ?1, updated_at = ?1 WHERE id = ?2 AND state = 'ready'",
            params![now, id],
        )?;
        if affected == 0 {
            return Ok(None);
        }
        let node = conn.query_row(
            "SELECT id, run_id, parent_id, kind, state, title, spec, agent_json, depends_on_json, depth, attempt, claimed_at, updated_at, role_hint
             FROM nodes WHERE id = ?1",
            params![id],
            row_to_node,
        )?;
        Ok(Some(node))
    }

    async fn transition(&self, id: &str, from: NodeState, to: NodeState) -> Result<bool> {
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        let affected = conn.execute(
            "UPDATE nodes SET state = ?1, updated_at = ?2 WHERE id = ?3 AND state = ?4",
            params![to.as_str(), now, id, from.as_str()],
        )?;
        Ok(affected > 0)
    }

    async fn set_state(&self, id: &str, to: NodeState) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE nodes SET state = ?1, updated_at = ?2 WHERE id = ?3",
            params![to.as_str(), now, id],
        )?;
        Ok(())
    }

    async fn bump_attempt(&self, id: &str) -> Result<u32> {
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE nodes SET attempt = attempt + 1, updated_at = ?1 WHERE id = ?2",
            params![now, id],
        )?;
        let attempt: u32 = conn.query_row(
            "SELECT attempt FROM nodes WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )?;
        Ok(attempt)
    }

    async fn update_spec(&self, id: &str, spec: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE nodes SET spec = ?1, updated_at = ?2 WHERE id = ?3",
            params![spec, now, id],
        )?;
        Ok(())
    }

    async fn set_agent(&self, id: &str, agent: Option<&crate::model::AgentRef>) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let agent_json = agent.map(|a| serde_json::to_string(a).unwrap());
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE nodes SET agent_json = ?1, updated_at = ?2 WHERE id = ?3",
            params![agent_json, now, id],
        )?;
        Ok(())
    }

    async fn comment(&self, id: &str, body: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO comments (node_id, body, created_at) VALUES (?1, ?2, ?3)",
            params![id, body, now],
        )?;
        Ok(())
    }

    async fn expire_leases(&self, run_id: &str, lease_secs: i64) -> Result<u32> {
        let cutoff = Utc::now() - chrono::Duration::seconds(lease_secs);
        let cutoff_s = cutoff.to_rfc3339();
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        // Running nodes whose claimed_at is older than cutoff -> back to ready.
        let affected = conn.execute(
            "UPDATE nodes SET state = 'ready', claimed_at = NULL, updated_at = ?1
             WHERE run_id = ?2 AND state = 'running' AND claimed_at < ?3",
            params![now, run_id, cutoff_s],
        )?;
        Ok(affected as u32)
    }

    async fn unblock_satisfied(&self, run_id: &str) -> Result<u32> {
        let conn = self.conn.lock().unwrap();
        // Load all blocked nodes for this run.
        let blocked: Vec<(String, String)> = {
            let mut stmt = conn.prepare(
                "SELECT id, depends_on_json FROM nodes WHERE run_id = ?1 AND state = 'blocked'",
            )?;
            let rows = stmt
                .query_map(params![run_id], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };

        if blocked.is_empty() {
            return Ok(0);
        }

        let now = Utc::now().to_rfc3339();
        let mut unblocked = 0u32;

        for (node_id, deps_json) in blocked {
            let deps: Vec<String> = serde_json::from_str(&deps_json).unwrap_or_default();

            if deps.is_empty() {
                // No dependencies — just unblock it.
                conn.execute(
                    "UPDATE nodes SET state = 'ready', updated_at = ?1 WHERE id = ?2",
                    params![now, node_id],
                )?;
                unblocked += 1;
                continue;
            }

            // Resolve each dependency's current state.
            let dep_states: Vec<(String, String)> = deps
                .iter()
                .map(|dep_id| {
                    let state = conn
                        .query_row(
                            "SELECT state FROM nodes WHERE id = ?1",
                            params![dep_id],
                            |row| row.get::<_, String>(0),
                        )
                        .unwrap_or_default();
                    (dep_id.clone(), state)
                })
                .collect();

            // A dead prerequisite (failed/superseded) can never satisfy — the
            // dependent is settled Failed so the scheduler can cascade/replan.
            if let Some((dead_id, dead_state)) = dep_states
                .iter()
                .find(|(_, s)| s == "failed" || s == "superseded")
            {
                conn.execute(
                    "UPDATE nodes SET state = 'failed', updated_at = ?1 WHERE id = ?2",
                    params![now, node_id],
                )?;
                conn.execute(
                    "INSERT INTO comments (node_id, body, created_at) VALUES (?1, ?2, ?3)",
                    params![
                        node_id,
                        format!("dependency {dead_id} is {dead_state}; marking this node failed"),
                        now
                    ],
                )?;
                continue;
            }

            // Otherwise, unblock only once every dependency is done.
            let all_done = dep_states.iter().all(|(_, s)| s == "done");
            if all_done {
                conn.execute(
                    "UPDATE nodes SET state = 'ready', updated_at = ?1 WHERE id = ?2",
                    params![now, node_id],
                )?;
                unblocked += 1;
            }
        }

        Ok(unblocked)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NewNode, NodeKind, NodeState, Role};
    use tempfile::tempdir;

    fn make_tracker() -> (SqliteTracker, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let db = dir.path().join("test.db");
        let tracker = SqliteTracker::open(&db).unwrap();
        (tracker, dir)
    }

    #[tokio::test]
    async fn contract_full() {
        let (t, _dir) = make_tracker();

        // init_run creates run + root ready node
        let run = t.init_run("build a blog", "canopy/run-test").await.unwrap();
        assert!(!run.id.is_empty());
        assert!(!run.root_node.is_empty());

        let root = t.node(&run.root_node).await.unwrap();
        assert_eq!(root.state, NodeState::Ready);
        assert_eq!(root.depth, 0);
        assert_eq!(root.title, "Objective");
        assert_eq!(root.spec, "build a blog");

        // try_claim succeeds first time
        let claimed = t.try_claim(&root.id).await.unwrap();
        assert!(claimed.is_some());
        let claimed_node = claimed.unwrap();
        assert_eq!(claimed_node.state, NodeState::Running);
        assert!(claimed_node.claimed_at.is_some());

        // second try_claim on the same node returns None (already running)
        let second = t.try_claim(&root.id).await.unwrap();
        assert!(second.is_none());

        // transition CAS: wrong `from` returns false
        let ok = t
            .transition(&root.id, NodeState::Ready, NodeState::Done)
            .await
            .unwrap();
        assert!(!ok, "should fail: node is running, not ready");

        // correct CAS succeeds
        let ok = t
            .transition(&root.id, NodeState::Running, NodeState::Decomposed)
            .await
            .unwrap();
        assert!(ok);

        // create two children: child_b depends on child_a
        let child_a = t
            .create_node(NewNode {
                run_id: run.id.clone(),
                parent_id: Some(root.id.clone()),
                kind: NodeKind::Execute,
                title: "step A".into(),
                spec: "do A".into(),
                agent: None,
                depends_on: vec![],
                role_hint: None,
                depth: 1,
                ready: true,
            })
            .await
            .unwrap();
        assert_eq!(child_a.state, NodeState::Ready);

        let child_b = t
            .create_node(NewNode {
                run_id: run.id.clone(),
                parent_id: Some(root.id.clone()),
                kind: NodeKind::Execute,
                title: "step B".into(),
                spec: "do B".into(),
                agent: None,
                depends_on: vec![child_a.id.clone()],
                role_hint: None,
                depth: 1,
                ready: false,
            })
            .await
            .unwrap();
        assert_eq!(child_b.state, NodeState::Blocked);

        // unblock_satisfied: child_b's dep (A) is not done yet -> 0
        let n = t.unblock_satisfied(&run.id).await.unwrap();
        assert_eq!(n, 0);

        // mark A done
        t.set_state(&child_a.id, NodeState::Done).await.unwrap();

        // now unblock_satisfied should flip child_b to ready
        let n = t.unblock_satisfied(&run.id).await.unwrap();
        assert_eq!(n, 1);
        let b = t.node(&child_b.id).await.unwrap();
        assert_eq!(b.state, NodeState::Ready);

        // expire_leases: claim child_b, then expire with lease_secs=0
        t.try_claim(&child_b.id).await.unwrap();
        let b = t.node(&child_b.id).await.unwrap();
        assert_eq!(b.state, NodeState::Running);

        // lease_secs=0 means everything expires immediately
        let expired = t.expire_leases(&run.id, 0).await.unwrap();
        assert_eq!(expired, 1);
        let b = t.node(&child_b.id).await.unwrap();
        assert_eq!(b.state, NodeState::Ready);
        assert!(b.claimed_at.is_none());
    }

    #[tokio::test]
    async fn role_hint_roundtrips() {
        let (t, _dir) = make_tracker();
        let run = t.init_run("obj", "canopy/run-rh").await.unwrap();

        let node = t
            .create_node(NewNode {
                run_id: run.id.clone(),
                parent_id: Some(run.root_node.clone()),
                kind: NodeKind::Plan,
                title: "decomposer".into(),
                spec: "split".into(),
                agent: None,
                depends_on: vec![],
                role_hint: Some(Role::Decomposer),
                depth: 1,
                ready: true,
            })
            .await
            .unwrap();
        assert_eq!(node.role_hint, Some(Role::Decomposer));

        // Reload from the board to prove the column persisted.
        let reloaded = t.node(&node.id).await.unwrap();
        assert_eq!(reloaded.role_hint, Some(Role::Decomposer));
    }

    #[tokio::test]
    async fn unblock_satisfied_both_ways() {
        let (t, _dir) = make_tracker();
        let run = t.init_run("obj", "canopy/run-ub").await.unwrap();
        let root = t.node(&run.root_node).await.unwrap();

        let new_dep = |title: &str| NewNode {
            run_id: run.id.clone(),
            parent_id: Some(root.id.clone()),
            kind: NodeKind::Execute,
            title: title.into(),
            spec: "x".into(),
            agent: None,
            depends_on: vec![],
            role_hint: None,
            depth: 1,
            ready: true,
        };

        // Two independent dependencies, each with a dependent blocked on it.
        let dep_ok = t.create_node(new_dep("dep-ok")).await.unwrap();
        let dep_bad = t.create_node(new_dep("dep-bad")).await.unwrap();

        let blocked = |dep_id: String, title: &str| NewNode {
            run_id: run.id.clone(),
            parent_id: Some(root.id.clone()),
            kind: NodeKind::Execute,
            title: title.into(),
            spec: "x".into(),
            agent: None,
            depends_on: vec![dep_id],
            role_hint: None,
            depth: 1,
            ready: false,
        };
        let dependent_ok = t
            .create_node(blocked(dep_ok.id.clone(), "dependent-ok"))
            .await
            .unwrap();
        let dependent_bad = t
            .create_node(blocked(dep_bad.id.clone(), "dependent-bad"))
            .await
            .unwrap();
        assert_eq!(dependent_ok.state, NodeState::Blocked);
        assert_eq!(dependent_bad.state, NodeState::Blocked);

        // dep done -> dependent becomes ready; dep failed -> dependent becomes failed.
        t.set_state(&dep_ok.id, NodeState::Done).await.unwrap();
        t.set_state(&dep_bad.id, NodeState::Failed).await.unwrap();

        let moved = t.unblock_satisfied(&run.id).await.unwrap();
        assert_eq!(moved, 1, "only the done-dep dependent counts as unblocked");

        assert_eq!(
            t.node(&dependent_ok.id).await.unwrap().state,
            NodeState::Ready
        );
        assert_eq!(
            t.node(&dependent_bad.id).await.unwrap().state,
            NodeState::Failed
        );

        // A superseded dependency settles the dependent Failed too.
        let dep_sup = t.create_node(new_dep("dep-sup")).await.unwrap();
        let dependent_sup = t
            .create_node(blocked(dep_sup.id.clone(), "dependent-sup"))
            .await
            .unwrap();
        t.set_state(&dep_sup.id, NodeState::Superseded)
            .await
            .unwrap();
        t.unblock_satisfied(&run.id).await.unwrap();
        assert_eq!(
            t.node(&dependent_sup.id).await.unwrap().state,
            NodeState::Failed
        );
    }
}
