//! canopy.toml — the single configuration surface.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::model::{AgentRef, Lens};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub run: RunConfig,
    #[serde(default)]
    pub linear: Option<LinearConfig>,
    #[serde(default)]
    pub budgets: Budgets,
    #[serde(default)]
    pub thresholds: Thresholds,
    pub routing: Routing,
    /// USD per 1M tokens, for CLIs that report tokens but not cost.
    /// Key = model name. `{ input = 0.25, output = 2.0 }`
    #[serde(default)]
    pub pricing: HashMap<String, Price>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunConfig {
    /// Target repository (the codebase the swarm works on).
    pub repo: PathBuf,
    /// Command run after every merge (build/tests). Ground truth.
    pub verify: String,
    /// "sqlite" (default) or "linear".
    #[serde(default = "default_tracker")]
    pub tracker: String,
}

fn default_tracker() -> String {
    "sqlite".into()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinearConfig {
    /// Team whose board hosts the swarm runs.
    pub team_id: String,
    /// API key env var name (the key itself never lives in config).
    #[serde(default = "default_linear_key_env")]
    pub api_key_env: String,
}

fn default_linear_key_env() -> String {
    "LINEAR_API_KEY".into()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Budgets {
    /// Hard cap; the run pauses when the ledger reaches it.
    pub max_usd: f64,
    /// Concurrent Execute leaves.
    pub max_parallel: usize,
    /// Concurrent Plan trunks.
    pub max_parallel_planners: usize,
    /// Attempts per node before Failed.
    pub max_attempts: u32,
    pub max_tree_depth: u32,
    /// Claim lease; expired claims return to Ready. Clamped at load time to
    /// at least 2×agent_timeout_secs + 300 (a claim can span two invocations:
    /// the run plus one JSON-nudge retry) so live jobs never lose their lease.
    pub lease_secs: i64,
    /// Kill an agent process after this long.
    pub agent_timeout_secs: u64,
    /// Cap agentic turns per invocation where the CLI supports it.
    pub max_turns: Option<u32>,
}

impl Default for Budgets {
    fn default() -> Self {
        Budgets {
            max_usd: 10.0,
            max_parallel: 4,
            max_parallel_planners: 2,
            max_attempts: 3,
            max_tree_depth: 4,
            lease_secs: 2400,
            agent_timeout_secs: 1800,
            max_turns: Some(50),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Thresholds {
    /// Anti-megafile: lines after which a file is blocked and decomposed.
    pub megafile_lines: usize,
    /// Field Guide index.md line budget (the article's only constraint).
    pub fieldguide_line_budget: usize,
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds {
            megafile_lines: 1000,
            fieldguide_line_budget: 200,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Routing {
    /// "static" or "planner-routed".
    #[serde(default = "default_mode")]
    pub mode: RoutingMode,
    pub planner: AgentRef,
    pub executor: AgentRef,
    /// Default to planner if absent: mechanisms lean smart.
    pub merger: Option<AgentRef>,
    /// Cheap first-attempt merger. Most conflicts left after disjoint
    /// ownership are mechanical; the article's merge agent is "impartial and
    /// efficient, similar to the way merge queues work" — queues are
    /// mechanical first. On failure (or a resolution that bounces on the
    /// post-merge gates) the main `merger` takes over.
    pub merger_triage: Option<AgentRef>,
    pub reconciler: Option<AgentRef>,
    /// Default to executor if absent: mechanical splitting is leaf work.
    pub decomposer: Option<AgentRef>,
    #[serde(default)]
    pub reviewers: Vec<ReviewerConfig>,
    /// planner-routed mode: models the planner may assign to children.
    #[serde(default)]
    pub allowlist: Vec<AllowlistEntry>,
}

fn default_mode() -> RoutingMode {
    RoutingMode::Static
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RoutingMode {
    Static,
    PlannerRouted,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewerConfig {
    pub cli: crate::model::CliKind,
    pub model: String,
    pub lens: Lens,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllowlistEntry {
    pub cli: crate::model::CliKind,
    pub model: String,
    /// Shown to the planner: what this model is good for.
    pub good_for: String,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Price {
    /// USD per 1M input tokens.
    pub input: f64,
    /// USD per 1M output tokens.
    pub output: f64,
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let mut cfg: Config = toml::from_str(&raw).context("parsing canopy.toml")?;
        // Resolve repo relative to the config file's directory.
        if cfg.run.repo.is_relative() {
            if let Some(dir) = path.parent() {
                cfg.run.repo = dir.join(&cfg.run.repo);
            }
        }
        cfg.run.repo = cfg
            .run
            .repo
            .canonicalize()
            .with_context(|| format!("target repo {}", cfg.run.repo.display()))?;
        if cfg.run.tracker == "linear" && cfg.linear.is_none() {
            anyhow::bail!("tracker = \"linear\" requires a [linear] section");
        }
        let min_lease = 2 * cfg.budgets.agent_timeout_secs as i64 + 300;
        if cfg.budgets.lease_secs < min_lease {
            tracing::warn!(
                configured = cfg.budgets.lease_secs,
                clamped = min_lease,
                "lease_secs must outlive a claim (2×agent_timeout + 300) — clamped"
            );
            cfg.budgets.lease_secs = min_lease;
        }
        Ok(cfg)
    }

    pub fn merger(&self) -> AgentRef {
        self.routing
            .merger
            .clone()
            .unwrap_or_else(|| self.routing.planner.clone())
    }

    pub fn reconciler(&self) -> AgentRef {
        self.routing
            .reconciler
            .clone()
            .unwrap_or_else(|| self.routing.planner.clone())
    }

    pub fn decomposer(&self) -> AgentRef {
        self.routing
            .decomposer
            .clone()
            .unwrap_or_else(|| self.routing.executor.clone())
    }

    /// State dir inside the target repo (gitignored).
    pub fn state_dir(&self) -> PathBuf {
        self.run.repo.join(".canopy")
    }
}
