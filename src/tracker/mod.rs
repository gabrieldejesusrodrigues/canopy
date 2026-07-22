//! The board. Linear or local SQLite behind one trait — the board IS the
//! task tree and the queue; all run state lives here (crash recovery).

pub mod linear;
pub mod sqlite;

use anyhow::Result;
use async_trait::async_trait;

use crate::model::{NewNode, Node, NodeState, Run};

#[async_trait]
pub trait Tracker: Send + Sync {
    /// Create a run and its root Plan node. Returns the run with root_node set.
    async fn init_run(&self, objective: &str, branch: &str) -> Result<Run>;

    async fn load_run(&self, run_id: &str) -> Result<Run>;

    async fn create_node(&self, new: NewNode) -> Result<Node>;

    async fn node(&self, id: &str) -> Result<Node>;

    async fn children(&self, parent_id: &str) -> Result<Vec<Node>>;

    async fn nodes_in_state(&self, run_id: &str, state: NodeState) -> Result<Vec<Node>>;

    /// Atomic claim: Ready -> Running with a lease. Returns None if the node
    /// was no longer Ready (someone else moved it, or a human intervened).
    async fn try_claim(&self, id: &str) -> Result<Option<Node>>;

    /// Compare-and-set state transition. Returns false if `from` didn't match.
    async fn transition(&self, id: &str, from: NodeState, to: NodeState) -> Result<bool>;

    /// Unconditional state set (harness-internal moves).
    async fn set_state(&self, id: &str, to: NodeState) -> Result<()>;

    async fn bump_attempt(&self, id: &str) -> Result<u32>;

    async fn update_spec(&self, id: &str, spec: &str) -> Result<()>;

    async fn set_agent(&self, id: &str, agent: Option<&crate::model::AgentRef>) -> Result<()>;

    /// Progress note visible to humans (Linear comment / sqlite log row).
    async fn comment(&self, id: &str, body: &str) -> Result<()>;

    /// Return Running nodes whose lease expired to Ready. Returns count.
    async fn expire_leases(&self, run_id: &str, lease_secs: i64) -> Result<u32>;

    /// Blocked nodes whose dependencies are all Done -> Ready. Returns count.
    async fn unblock_satisfied(&self, run_id: &str) -> Result<u32>;
}

/// Build the configured tracker.
pub async fn from_config(cfg: &crate::config::Config) -> Result<Box<dyn Tracker>> {
    match cfg.run.tracker.as_str() {
        "sqlite" => Ok(Box::new(sqlite::SqliteTracker::open(
            &cfg.state_dir().join("canopy.db"),
        )?)),
        "linear" => {
            let lin = cfg.linear.as_ref().expect("validated in Config::load");
            let key = std::env::var(&lin.api_key_env).map_err(|_| {
                anyhow::anyhow!("Linear tracker: env var {} not set", lin.api_key_env)
            })?;
            Ok(Box::new(
                linear::LinearTracker::connect(key, lin.team_id.clone()).await?,
            ))
        }
        other => anyhow::bail!("unknown tracker '{other}' (expected sqlite|linear)"),
    }
}
