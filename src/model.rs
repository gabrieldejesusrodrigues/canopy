//! Domain types shared by every module. This file pins the vocabulary of the
//! system; changes here ripple everywhere, so keep it small and explicit.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Tree node kinds, per the article: trunks plan, leaves execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Plan,
    Execute,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    /// Dependencies satisfied, waiting for a slot.
    Ready,
    /// Claimed by the scheduler; an agent process is (or was) attached.
    Running,
    /// Planner finished: children materialized on the board.
    Decomposed,
    /// Executor finished its worktree; waiting in the merge queue.
    NeedsMerge,
    /// Being merged (possibly by the neutral Merger agent).
    Merging,
    /// Merged; review lenses pending or in flight.
    InReview,
    /// Waiting on dependencies, a flagged megafile, or budget.
    Blocked,
    Done,
    Failed,
    /// Terminal-settled: replaced during a replan. Not a failure — it no
    /// longer counts against its parent, but it is never retried either.
    Superseded,
}

impl NodeState {
    pub fn as_str(&self) -> &'static str {
        match self {
            NodeState::Ready => "ready",
            NodeState::Running => "running",
            NodeState::Decomposed => "decomposed",
            NodeState::NeedsMerge => "needs_merge",
            NodeState::Merging => "merging",
            NodeState::InReview => "in_review",
            NodeState::Blocked => "blocked",
            NodeState::Done => "done",
            NodeState::Failed => "failed",
            NodeState::Superseded => "superseded",
        }
    }

    pub fn parse(s: &str) -> Option<NodeState> {
        Some(match s {
            "ready" => NodeState::Ready,
            "running" => NodeState::Running,
            "decomposed" => NodeState::Decomposed,
            "needs_merge" => NodeState::NeedsMerge,
            "merging" => NodeState::Merging,
            "in_review" => NodeState::InReview,
            "blocked" => NodeState::Blocked,
            "done" => NodeState::Done,
            "failed" => NodeState::Failed,
            "superseded" => NodeState::Superseded,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CliKind {
    Claude,
    Codex,
    Agy,
    /// Deterministic fake for tests; reads canned responses from a directory.
    Stub,
}

impl CliKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            CliKind::Claude => "claude",
            CliKind::Codex => "codex",
            CliKind::Agy => "agy",
            CliKind::Stub => "stub",
        }
    }
}

/// Which CLI + model executes an invocation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRef {
    pub cli: CliKind,
    pub model: String,
}

/// Invocation types. Planner/Executor are tree work; the rest are the
/// article's coordination mechanisms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Planner,
    Executor,
    Merger,
    Reconciler,
    Decomposer,
    Reviewer,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Planner => "planner",
            Role::Executor => "executor",
            Role::Merger => "merger",
            Role::Reconciler => "reconciler",
            Role::Decomposer => "decomposer",
            Role::Reviewer => "reviewer",
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        Some(match s {
            "planner" => Role::Planner,
            "executor" => Role::Executor,
            "merger" => Role::Merger,
            "reconciler" => Role::Reconciler,
            "decomposer" => Role::Decomposer,
            "reviewer" => Role::Reviewer,
            _ => return None,
        })
    }
}

/// Review lens: what context the reviewer sees (uncorrelated views sum).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lens {
    /// Node spec + full agent transcript + diff.
    Transcript,
    /// Node spec + diff only.
    Output,
    /// Repo state only, no history.
    Codebase,
}

impl Lens {
    pub fn as_str(&self) -> &'static str {
        match self {
            Lens::Transcript => "transcript",
            Lens::Output => "output",
            Lens::Codebase => "codebase",
        }
    }
}

/// One swarm execution over a target repo.
#[derive(Debug, Clone)]
pub struct Run {
    pub id: String,
    pub objective: String,
    /// Root Plan node id.
    pub root_node: String,
    /// Run branch in the target repo (e.g. `canopy/run-<id>`).
    pub branch: String,
    pub created_at: DateTime<Utc>,
}

/// A node of the task tree as stored on the board.
#[derive(Debug, Clone)]
pub struct Node {
    /// Tracker-native id (uuid). Stable handle everywhere.
    pub id: String,
    pub run_id: String,
    pub parent_id: Option<String>,
    pub kind: NodeKind,
    pub state: NodeState,
    pub title: String,
    /// Explicit instructions for the agent (markdown).
    pub spec: String,
    /// Assigned/suggested agent; None = use routing default for the role.
    pub agent: Option<AgentRef>,
    /// Node ids that must be Done before this becomes Ready.
    pub depends_on: Vec<String>,
    /// Mechanism role override (e.g. Decomposer); None = derived from kind.
    pub role_hint: Option<Role>,
    pub depth: u32,
    pub attempt: u32,
    pub claimed_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

/// Insert payload for a new node.
#[derive(Debug, Clone)]
pub struct NewNode {
    pub run_id: String,
    pub parent_id: Option<String>,
    pub kind: NodeKind,
    pub title: String,
    pub spec: String,
    pub agent: Option<AgentRef>,
    pub depends_on: Vec<String>,
    /// Mechanism role override (e.g. Decomposer); None = derived from kind.
    pub role_hint: Option<Role>,
    pub depth: u32,
    /// If false, node starts Blocked (waiting on depends_on).
    pub ready: bool,
}

// ---------------------------------------------------------------------------
// Structured agent outputs (the trailing fenced JSON block of each contract)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct PlannerOutput {
    pub children: Vec<ChildSpec>,
    #[serde(default)]
    pub design_decisions: Vec<DesignDecision>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ChildSpec {
    pub title: String,
    pub kind: NodeKind,
    /// Explicit, self-contained instructions — the leaf sees nothing else.
    pub spec: String,
    /// Indices into the same `children` array (siblings that must finish first).
    #[serde(default)]
    pub depends_on: Vec<usize>,
    /// Files this child owns (creates/edits). The harness rejects
    /// decompositions where two children claim the same path — the article's
    /// "no two delegated subtrees decide the same question", applied to files.
    #[serde(default)]
    pub files: Vec<String>,
    /// Only honored in planner-routed mode, validated against the allowlist.
    #[serde(default)]
    pub agent: Option<AgentRef>,
}

/// A decision destined for `design/DD-*.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesignDecision {
    /// "DD-<n>" — planner proposes, harness renumbers on collision.
    pub id: String,
    pub title: String,
    /// Topic slugs used for divergence detection between planners.
    #[serde(default)]
    pub topics: Vec<String>,
    /// Markdown body.
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecStatus {
    Done,
    Blocked,
    NeedsSplit,
}

#[derive(Debug, Deserialize)]
pub struct ExecutorOutput {
    pub status: ExecStatus,
    pub summary: String,
    /// Files the executor judges bloated (anti-megafile mechanism).
    #[serde(default)]
    pub flagged_files: Vec<String>,
    /// Out-of-scope focused patches (anti-ossification mechanism).
    #[serde(default)]
    pub breaks: Vec<BreakNote>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BreakNote {
    pub file: String,
    pub reason: String,
}

#[derive(Debug, Deserialize)]
pub struct MergerOutput {
    pub resolved: bool,
    pub summary: String,
}

#[derive(Debug, Deserialize)]
pub struct ReconcilerOutput {
    /// Doc id that survives.
    pub surviving_id: String,
    /// Doc id marked superseded.
    pub superseded_id: String,
    pub title: String,
    #[serde(default)]
    pub topics: Vec<String>,
    /// Merged markdown body for the surviving doc.
    pub merged_content: String,
}

#[derive(Debug, Deserialize)]
pub struct ReviewOutput {
    #[serde(default)]
    pub findings: Vec<Finding>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    #[serde(default)]
    pub file: Option<String>,
    pub description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    High,
    Low,
}

// ---------------------------------------------------------------------------
// Ledger
// ---------------------------------------------------------------------------

/// One CLI invocation, as recorded for the economics report.
#[derive(Debug, Clone)]
pub struct InvocationRecord {
    pub node_id: String,
    pub role: Role,
    pub cli: CliKind,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    /// Reported by the CLI (claude) or priced from tokens via config.
    pub cost_usd: Option<f64>,
    pub duration_ms: u128,
    pub attempt: u32,
    pub exit_ok: bool,
}
