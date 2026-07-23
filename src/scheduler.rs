//! The event loop. Claims ready nodes off the board, spawns one-shot agent
//! processes, applies their structured output, and runs the coordination
//! mechanisms — merges serialized, coordination debt paid before new work.
//!
//! Concurrency model: agent processes run as detached tokio tasks (a
//! JoinSet); ALL state mutations (tracker, blocklist, ledger, tree cascade)
//! happen on this loop when a task joins. One writer, no locks. The merge
//! lane is a task too, but at most one is ever in flight — that lane IS the
//! article's "single point every change passes through".

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::task::JoinSet;

use crate::agent::{self, InvocationRequest, InvocationResult};
use crate::config::{Config, RoutingMode};
use crate::gitops::GitOps;
use crate::ledger::{self, Ledger};
use crate::mechanisms::{designdocs, fieldguide, megafile::BlockList};
use crate::mergelane::MergeReport;
use crate::model::*;
use crate::prompt;
use crate::tracker::Tracker;

const REPLAN_CAP: u32 = 2;
/// A file that keeps causing merge conflicts is the article's "site of
/// constant collisions" — decompose it regardless of line count.
const CONFLICT_DECOMPOSE_THRESHOLD: u32 = 2;

pub struct Scheduler {
    cfg: Arc<Config>,
    tracker: Box<dyn Tracker>,
    git: GitOps,
    ledger: Ledger,
    run: Run,
    blocklist: BlockList,
    jobs: JoinSet<JobOut>,
    /// In-flight bookkeeping (counts enforce the budgets).
    inflight_plan: usize,
    inflight_exec: usize,
    merge_inflight: bool,
    /// Node ids with a live tree job in THIS process. A lease can expire
    /// under a long nudge retry; this set stops the claim path from yanking
    /// the worktree out from under the still-running agent.
    inflight: HashSet<String>,
    /// node → pending review lenses + findings so far.
    reviews: HashMap<String, ReviewAgg>,
    /// node → context injected on retry (verify failures, bounces).
    retry_ctx: HashMap<String, String>,
    /// Executor-reported megafile flags, applied when the node LANDS — its
    /// own flag must not gate its own merge, and the decomposer must split
    /// the file with this node's changes already in.
    pending_flags: HashMap<String, Vec<String>>,
    /// Declared breaks (mechanism 5): verify failure lands + propagates as
    /// fix nodes instead of bouncing.
    declared_breaks: HashMap<String, Vec<BreakNote>>,
    /// Merge-conflict count per file: the merge queue is where every
    /// collision surfaces, so it doubles as the megafile mechanism's
    /// contention sensor.
    conflict_counts: HashMap<String, u32>,
    /// planner node → replans consumed.
    replans: HashMap<String, u32>,
    paused_for_budget: bool,
    /// Applications that write to the merge worktree (design docs), deferred
    /// while a merge job is in flight — the merge lane owns that checkout.
    pending: Vec<Deferred>,
}

enum Deferred {
    Plan(Node, PlannerOutput),
    Reconcile {
        author_node: String,
        agent_ref: AgentRef,
        res: Result<InvocationResult>,
        incumbent_id: String,
        incoming_id: String,
    },
}

struct ReviewAgg {
    pending: usize,
    findings: Vec<Finding>,
    /// Lenses that errored or returned unparseable output — never "clean".
    failed: usize,
}

enum JobOut {
    Tree {
        node: Node,
        agent_ref: AgentRef,
        prompt_used: String,
        res: Result<InvocationResult>,
        is_retry_nudge: bool,
    },
    Merge {
        node: Node,
        report: MergeReport,
        merger_runs: Vec<(AgentRef, InvocationResult)>,
    },
    Review {
        node: Node,
        lens: Lens,
        agent_ref: AgentRef,
        res: Result<InvocationResult>,
    },
    Reconcile {
        author_node: String,
        agent_ref: AgentRef,
        res: Result<InvocationResult>,
        incumbent_id: String,
        incoming_id: String,
    },
}


impl Scheduler {
    pub async fn start(
        cfg: Config,
        objective: Option<String>,
        resume: Option<String>,
    ) -> Result<()> {
        let cfg = Arc::new(cfg);
        // One daemon per repo: the merge worktree and blocklist assume a
        // single writer. Stale locks (dead pid) are reclaimed.
        std::fs::create_dir_all(cfg.state_dir())?;
        let lock_path = cfg.state_dir().join("daemon.pid");
        if let Ok(prev) = std::fs::read_to_string(&lock_path) {
            let prev = prev.trim().to_owned();
            if !prev.is_empty() && std::path::Path::new(&format!("/proc/{prev}")).exists() {
                anyhow::bail!(
                    "another canopy daemon (pid {prev}) already runs on this repo — \
                     refusing a second writer ({})",
                    lock_path.display()
                );
            }
        }
        std::fs::write(&lock_path, std::process::id().to_string())?;

        let tracker = crate::tracker::from_config(&cfg).await?;
        let git = GitOps::new(&cfg.run.repo);
        let ledger = Ledger::open(&cfg.state_dir().join("ledger.db"))?;

        let run = match (objective, resume) {
            (_, Some(id)) => tracker.load_run(&id).await?,
            (Some(obj), None) => {
                let branch = format!("canopy/run-{}", chrono::Utc::now().format("%Y%m%d-%H%M%S"));
                tracker.init_run(&obj, &branch).await?
            }
            (None, None) => anyhow::bail!("provide an objective or --resume <run-id>"),
        };

        git.ensure_run_branch(&run.branch).await?;
        if fieldguide::ensure_scaffold(&git.merge_dir())? {
            git.harness_commit("canopy: scaffold design/ and fieldguide/")
                .await?;
        }
        // Crash recovery: a merge in flight when the daemon died left its
        // node Merging (ensure_run_branch already cleared the worktree side).
        for n in tracker
            .nodes_in_state(&run.id, NodeState::Merging)
            .await?
        {
            tracker.set_state(&n.id, NodeState::NeedsMerge).await?;
            tracker
                .comment(&n.id, "recovered from interrupted merge — requeued")
                .await?;
        }
        let blocklist = BlockList::load(&cfg.state_dir())?;

        tracing::info!(run = run.id, branch = run.branch, "canopy run starting");
        println!(
            "run: {}\nbranch: {}\nboard: {}",
            run.id, run.branch, cfg.run.tracker
        );
        // Convenience handle for `canopy status` / `canopy report`.
        std::fs::write(cfg.state_dir().join("last-run"), &run.id).ok();

        let mut sched = Scheduler {
            cfg,
            tracker,
            git,
            ledger,
            run,
            blocklist,
            jobs: JoinSet::new(),
            inflight_plan: 0,
            inflight_exec: 0,
            merge_inflight: false,
            inflight: HashSet::new(),
            reviews: HashMap::new(),
            retry_ctx: HashMap::new(),
            pending_flags: HashMap::new(),
            declared_breaks: HashMap::new(),
            conflict_counts: HashMap::new(),
            replans: HashMap::new(),
            paused_for_budget: false,
            pending: Vec::new(),
        };
        sched.recover_in_review().await?;
        let result = sched.run_loop().await;
        let _ = std::fs::remove_file(&lock_path);
        result
    }

    async fn run_loop(&mut self) -> Result<()> {
        loop {
            self.settle().await?;
            self.sweep_cascades().await?;
            self.pump_deferred().await?;
            self.pump_merges().await?;
            self.pump_reviews().await?;
            self.claim_and_spawn().await?;

            // Terminal check: root Done/Failed and nothing in flight.
            let root = self.tracker.node(&self.run.root_node.clone()).await?;
            if self.jobs.is_empty() && matches!(root.state, NodeState::Done | NodeState::Failed) {
                let report = self.ledger.report(&self.run.id)?;
                println!("{report}");
                println!(
                    "run {} finished: root {}. Branch `{}` is ready to inspect/PR.",
                    self.run.id,
                    root.state.as_str(),
                    self.run.branch
                );
                return Ok(());
            }
            if self.paused_for_budget && self.jobs.is_empty() {
                println!(
                    "budget cap ${} reached — run paused. Resume with more budget: canopy run --resume {}",
                    self.cfg.budgets.max_usd, self.run.id
                );
                return Ok(());
            }

            // Wait for the next job or poll the board again.
            let poll = self.poll_interval();
            tokio::select! {
                joined = self.jobs.join_next(), if !self.jobs.is_empty() => {
                    if let Some(out) = joined {
                        let out = out.context("agent task panicked")?;
                        self.apply(out).await?;
                        // Drain any other already-finished jobs.
                        while let Some(more) = self.jobs.try_join_next() {
                            let more = more.context("agent task panicked")?;
                            self.apply(more).await?;
                        }
                    }
                }
                _ = tokio::time::sleep(poll) => {}
            }
        }
    }

    fn poll_interval(&self) -> std::time::Duration {
        // Linear polling is rate-limited; sqlite is free.
        match self.cfg.run.tracker.as_str() {
            "linear" => std::time::Duration::from_secs(15),
            _ => std::time::Duration::from_secs(3),
        }
    }

    /// Leases, dependency unblocking, budget gate.
    async fn settle(&mut self) -> Result<()> {
        self.tracker
            .expire_leases(&self.run.id, self.cfg.budgets.lease_secs)
            .await?;
        self.tracker.unblock_satisfied(&self.run.id).await?;
        let spent = self.ledger.total_cost(&self.run.id)?;
        if spent >= self.cfg.budgets.max_usd && !self.paused_for_budget {
            tracing::warn!(spent, cap = self.cfg.budgets.max_usd, "budget cap hit");
            self.paused_for_budget = true;
        }
        Ok(())
    }

    /// Flush deferred merge-worktree writers once the merge lane is idle.
    async fn pump_deferred(&mut self) -> Result<()> {
        if self.merge_inflight || self.pending.is_empty() {
            return Ok(());
        }
        for d in std::mem::take(&mut self.pending) {
            match d {
                Deferred::Plan(node, out) => self.apply_planner_output(node, out).await?,
                Deferred::Reconcile {
                    author_node,
                    agent_ref,
                    res,
                    incumbent_id,
                    incoming_id,
                } => {
                    self.apply_reconcile(author_node, agent_ref, res, incumbent_id, incoming_id)
                        .await?
                }
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Merge lane (mechanism 3 + post-merge gates 1, 4, 5, 7)
    // ------------------------------------------------------------------

    async fn pump_merges(&mut self) -> Result<()> {
        if self.merge_inflight || self.paused_for_budget {
            return Ok(());
        }
        let mut queue = self
            .tracker
            .nodes_in_state(&self.run.id, NodeState::NeedsMerge)
            .await?;
        queue.sort_by(|a, b| a.updated_at.cmp(&b.updated_at));
        for node in queue {
            // Candidates touching a blocked megafile wait in NeedsMerge with
            // no state churn; the next candidate gets the lane (mechanism 4).
            let files = self
                .git
                .changed_files(&node.id, &self.run.branch)
                .await
                .unwrap_or_default();
            if let Some(f) = self.blocklist.gate(&files, &node.id) {
                tracing::debug!(node = node.id, file = f, "merge candidate gated — skipped");
                continue;
            }
            if !self
                .tracker
                .transition(&node.id, NodeState::NeedsMerge, NodeState::Merging)
                .await?
            {
                continue;
            }
            self.merge_inflight = true;
            let cfg = self.cfg.clone();
            let run_branch = self.run.branch.clone();
            let breaks = self
                .declared_breaks
                .get(&node.id)
                .cloned()
                .unwrap_or_default();
            self.jobs.spawn(async move {
                let (report, merger_runs) =
                    crate::mergelane::run(cfg, node.clone(), run_branch, breaks).await;
                JobOut::Merge {
                    node,
                    report,
                    merger_runs,
                }
            });
            return Ok(());
        }
        Ok(())
    }

    async fn pump_reviews(&mut self) -> Result<()> {
        if self.paused_for_budget {
            return Ok(());
        }
        // Nodes sitting InReview with no aggregation entry (fresh merge or
        // post-crash recovery) get their lenses scheduled here.
        let nodes = self
            .tracker
            .nodes_in_state(&self.run.id, NodeState::InReview)
            .await?;
        for node in nodes {
            if self.reviews.contains_key(&node.id) {
                continue;
            }
            if self.cfg.routing.reviewers.is_empty() {
                self.finish_review(node, Vec::new()).await?;
                continue;
            }
            let Some(commit) = self.git.find_merge_commit(&node.id).await else {
                self.tracker
                    .comment(&node.id, "review skipped: merge commit not found")
                    .await?;
                self.finish_review(node, Vec::new()).await?;
                continue;
            };
            let diff = self.git.merge_diff(&commit).await.unwrap_or_default();
            let touched = self.git.commit_files(&commit).await.unwrap_or_default();
            let fg = fieldguide::index_content(&self.git.merge_dir());
            let transcript = std::fs::read_to_string(
                self.cfg
                    .state_dir()
                    .join("transcripts")
                    .join(format!("{}-{}.txt", node.id, node.attempt)),
            )
            .ok();
            // Reviewers read a snapshot pinned at the merge commit: the merge
            // lane keeps moving and must never move the tree under a lens.
            let snap = match self
                .git
                .create_snapshot(&format!("review-{}", node.id), &commit)
                .await
            {
                Ok(p) => p,
                Err(e) => {
                    self.tracker
                        .comment(&node.id, &format!("review skipped: snapshot failed: {e:#}"))
                        .await?;
                    self.finish_review(node, Vec::new()).await?;
                    continue;
                }
            };
            self.reviews.insert(
                node.id.clone(),
                ReviewAgg {
                    pending: self.cfg.routing.reviewers.len(),
                    findings: Vec::new(),
                    failed: 0,
                },
            );
            for rc in &self.cfg.routing.reviewers {
                let agent_ref = AgentRef {
                    cli: rc.cli,
                    model: rc.model.clone(),
                };
                let p = prompt::reviewer(
                    &fg,
                    rc.lens,
                    &node.spec,
                    &diff,
                    transcript.as_deref(),
                    &touched,
                );
                let mut req =
                    self.request(&node, Role::Reviewer, &agent_ref, p, snap.clone());
                // Lens-suffixed transcripts: concurrent lenses must not share
                // a file (and must never clobber the executor's transcript).
                req.transcript_path = self.cfg.state_dir().join("transcripts").join(format!(
                    "{}-{}-review-{}.txt",
                    node.id,
                    node.attempt,
                    rc.lens.as_str()
                ));
                let node_c = node.clone();
                let lens = rc.lens;
                let ar = agent_ref.clone();
                self.jobs.spawn(async move {
                    let res = agent::for_ref(&ar).invoke(&req).await;
                    JobOut::Review {
                        node: node_c,
                        lens,
                        agent_ref: ar,
                        res,
                    }
                });
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Claiming tree work
    // ------------------------------------------------------------------

    async fn claim_and_spawn(&mut self) -> Result<()> {
        if self.paused_for_budget {
            return Ok(());
        }
        let mut ready = self
            .tracker
            .nodes_in_state(&self.run.id, NodeState::Ready)
            .await?;
        // Deepest first: finish subtrees before opening new ones.
        ready.sort_by(|a, b| b.depth.cmp(&a.depth));
        for node in ready {
            if self.inflight.contains(&node.id) {
                // A live job (e.g. a nudge retry) outlived its lease: never
                // re-claim under it — its worktree is in use.
                continue;
            }
            let cap_ok = match node.kind {
                NodeKind::Plan => self.inflight_plan < self.cfg.budgets.max_parallel_planners,
                NodeKind::Execute => self.inflight_exec < self.cfg.budgets.max_parallel,
            };
            if !cap_ok {
                continue;
            }
            let Some(node) = self.tracker.try_claim(&node.id).await? else {
                continue;
            };
            self.spawn_tree_job(node, false).await?;
        }
        Ok(())
    }

    async fn spawn_tree_job(&mut self, node: Node, is_retry_nudge: bool) -> Result<()> {
        let agent_ref = self.agent_for(&node);
        let fg = fieldguide::index_content(&self.git.merge_dir());
        let all_docs = designdocs::load_all(&self.git.merge_dir())?;
        let retry = self.retry_ctx.remove(&node.id);

        let (prompt_used, workdir) = match node.kind {
            NodeKind::Plan => {
                let allow = (self.cfg.routing.mode == RoutingMode::PlannerRouted)
                    .then_some(self.cfg.routing.allowlist.as_slice());
                let p = prompt::planner(&fg, &node.spec, &all_docs, allow, retry.as_deref());
                // Planners read a snapshot of the run branch pinned at spawn
                // time — never the merge lane's own (moving) checkout.
                let wt = self
                    .git
                    .create_snapshot(&format!("plan-{}", node.id), &self.run.branch)
                    .await?;
                (p, wt)
            }
            NodeKind::Execute => {
                let docs: Vec<_> = all_docs
                    .iter()
                    .filter(|d| node.spec.contains(&d.meta.id))
                    .cloned()
                    .collect();
                let p = if node.role_hint == Some(Role::Decomposer) {
                    prompt::decomposer(&fg, &node.spec, &docs)
                } else {
                    prompt::executor(&fg, &node.spec, &docs, retry.as_deref())
                };
                let wt = self.git.create_worktree(&node.id, &self.run.branch).await?;
                (p, wt)
            }
        };
        match node.kind {
            NodeKind::Plan => self.inflight_plan += 1,
            NodeKind::Execute => self.inflight_exec += 1,
        }
        self.inflight.insert(node.id.clone());
        let role = node_role(&node);
        let req = self.request(&node, role, &agent_ref, prompt_used.clone(), workdir);
        let ar = agent_ref.clone();
        let node_c = node.clone();
        self.jobs.spawn(async move {
            let res = agent::for_ref(&ar).invoke(&req).await;
            JobOut::Tree {
                node: node_c,
                agent_ref: ar,
                prompt_used,
                res,
                is_retry_nudge,
            }
        });
        self.tracker
            .comment(
                &node.id,
                &format!(
                    "claimed by {}:{} (attempt {})",
                    agent_ref.cli.as_str(),
                    agent_ref.model,
                    node.attempt + 1
                ),
            )
            .await?;
        Ok(())
    }

    fn agent_for(&self, node: &Node) -> AgentRef {
        // planner-routed assignments were validated at creation time.
        if let Some(a) = &node.agent {
            return a.clone();
        }
        match node.kind {
            NodeKind::Plan => self.cfg.routing.planner.clone(),
            NodeKind::Execute => self.cfg.routing.executor.clone(),
        }
    }

    fn request(
        &self,
        node: &Node,
        role: Role,
        agent_ref: &AgentRef,
        prompt: String,
        workdir: std::path::PathBuf,
    ) -> InvocationRequest {
        let _ = agent_ref;
        let tdir = self.cfg.state_dir().join("transcripts");
        let _ = std::fs::create_dir_all(&tdir);
        InvocationRequest {
            role,
            node_id: node.id.clone(),
            prompt,
            model: agent_ref.model.clone(),
            workdir,
            timeout_secs: self.cfg.budgets.agent_timeout_secs,
            max_turns: self.cfg.budgets.max_turns,
            transcript_path: tdir.join(format!("{}-{}.txt", node.id, node.attempt)),
        }
    }

    // ------------------------------------------------------------------
    // Applying job results (single writer)
    // ------------------------------------------------------------------

    async fn apply(&mut self, out: JobOut) -> Result<()> {
        match out {
            JobOut::Tree {
                node,
                agent_ref,
                prompt_used,
                res,
                is_retry_nudge,
            } => {
                match node.kind {
                    NodeKind::Plan => self.inflight_plan = self.inflight_plan.saturating_sub(1),
                    NodeKind::Execute => self.inflight_exec = self.inflight_exec.saturating_sub(1),
                }
                // A nudge respawn re-inserts; otherwise the node is free.
                self.inflight.remove(&node.id);
                self.apply_tree(node, agent_ref, prompt_used, res, is_retry_nudge)
                    .await
            }
            JobOut::Merge {
                node,
                report,
                merger_runs,
            } => {
                self.merge_inflight = false;
                for (ar, inv) in merger_runs {
                    self.record(&node.id, Role::Merger, &ar, &inv, node.attempt);
                }
                self.apply_merge(node, report).await
            }
            JobOut::Review {
                node,
                lens,
                agent_ref,
                res,
            } => self.apply_review(node, lens, agent_ref, res).await,
            JobOut::Reconcile {
                author_node,
                agent_ref,
                res,
                incumbent_id,
                incoming_id,
            } => {
                if self.merge_inflight {
                    self.pending.push(Deferred::Reconcile {
                        author_node,
                        agent_ref,
                        res,
                        incumbent_id,
                        incoming_id,
                    });
                    return Ok(());
                }
                self.apply_reconcile(author_node, agent_ref, res, incumbent_id, incoming_id)
                    .await
            }
        }
    }

    fn record(
        &self,
        node_id: &str,
        role: Role,
        agent_ref: &AgentRef,
        inv: &InvocationResult,
        attempt: u32,
    ) {
        let cost = ledger::price(&inv.usage, &agent_ref.model, &self.cfg.pricing);
        let rec = InvocationRecord {
            node_id: node_id.to_owned(),
            role,
            cli: agent_ref.cli,
            model: agent_ref.model.clone(),
            input_tokens: inv.usage.input_tokens,
            output_tokens: inv.usage.output_tokens,
            cached_tokens: inv.usage.cached_tokens,
            cost_usd: cost,
            duration_ms: inv.duration_ms,
            attempt,
            exit_ok: inv.exit_ok,
        };
        if let Err(e) = self.ledger.record(&self.run.id, &rec) {
            tracing::error!("ledger record failed: {e}");
        }
    }

    async fn apply_tree(
        &mut self,
        node: Node,
        agent_ref: AgentRef,
        prompt_used: String,
        res: Result<InvocationResult>,
        was_nudged: bool,
    ) -> Result<()> {
        let inv = match res {
            Ok(inv) => {
                self.record(&node.id, node_role(&node), &agent_ref, &inv, node.attempt);
                if !inv.exit_ok {
                    let tail: String = inv
                        .final_message
                        .chars()
                        .rev()
                        .take(800)
                        .collect::<String>()
                        .chars()
                        .rev()
                        .collect();
                    return self
                        .fail_attempt(node, &format!("agent error: {tail}"))
                        .await;
                }
                inv
            }
            Err(e) => {
                return self
                    .fail_attempt(node, &format!("invocation failed: {e:#}"))
                    .await;
            }
        };

        let json = agent::trailing_json(&inv.final_message);
        match node.kind {
            NodeKind::Plan => {
                let parsed = json
                    .and_then(|j| serde_json::from_str::<PlannerOutput>(j).ok())
                    // depends_on must reference earlier siblings only; a
                    // forward/out-of-range index silently dropped would run a
                    // child before its declared prerequisite.
                    .filter(|out| {
                        out.children
                            .iter()
                            .enumerate()
                            .all(|(i, c)| c.depends_on.iter().all(|ix| *ix < i))
                    })
                    // File ownership must be disjoint (article: "no two
                    // delegated subtrees decide the same question").
                    .filter(|out| match ownership_overlap(&out.children) {
                        Some(overlap) => {
                            tracing::warn!(node = node.id, overlap, "decomposition rejected");
                            false
                        }
                        None => true,
                    });
                match parsed {
                    Some(out) if self.merge_inflight => {
                        self.git
                            .remove_snapshot(&format!("plan-{}", node.id))
                            .await
                            .ok();
                        // Design-doc writes must wait for the merge lane.
                        self.pending.push(Deferred::Plan(node, out));
                        Ok(())
                    }
                    Some(out) => {
                        self.git
                            .remove_snapshot(&format!("plan-{}", node.id))
                            .await
                            .ok();
                        self.apply_planner_output(node, out).await
                    }
                    None if !was_nudged => {
                        let nudged = prompt::json_retry_nudge(
                            &prompt_used,
                            "missing, invalid, depends_on referencing a non-earlier sibling, \
                             or two children owning the same file",
                        );
                        self.respawn_with_prompt(node, agent_ref, nudged).await
                    }
                    None => {
                        self.fail_attempt(node, "structured output unparseable twice")
                            .await
                    }
                }
            }
            NodeKind::Execute => {
                let parsed = json.and_then(|j| serde_json::from_str::<ExecutorOutput>(j).ok());
                match parsed {
                    Some(out) => self.apply_executor_output(node, out).await,
                    None if !was_nudged => {
                        let nudged = prompt::json_retry_nudge(&prompt_used, "missing or invalid");
                        self.respawn_with_prompt(node, agent_ref, nudged).await
                    }
                    None => {
                        self.fail_attempt(node, "structured output unparseable twice")
                            .await
                    }
                }
            }
        }
    }

    async fn respawn_with_prompt(
        &mut self,
        node: Node,
        agent_ref: AgentRef,
        prompt_used: String,
    ) -> Result<()> {
        // Same claim, same worktree — just re-ask with the nudge.
        match node.kind {
            NodeKind::Plan => self.inflight_plan += 1,
            NodeKind::Execute => self.inflight_exec += 1,
        }
        self.inflight.insert(node.id.clone());
        let workdir = match node.kind {
            NodeKind::Plan => self.git.snapshot_dir(&format!("plan-{}", node.id)),
            NodeKind::Execute => self.git.worktree_dir(&node.id),
        };
        let role = node_role(&node);
        let req = self.request(&node, role, &agent_ref, prompt_used.clone(), workdir);
        let node_c = node.clone();
        self.jobs.spawn(async move {
            let res = agent::for_ref(&agent_ref).invoke(&req).await;
            JobOut::Tree {
                node: node_c,
                agent_ref,
                prompt_used,
                res,
                is_retry_nudge: true,
            }
        });
        Ok(())
    }

    async fn fail_attempt(&mut self, node: Node, reason: &str) -> Result<()> {
        if node.kind == NodeKind::Plan {
            self.git
                .remove_snapshot(&format!("plan-{}", node.id))
                .await
                .ok();
        }
        let attempt = self.tracker.bump_attempt(&node.id).await?;
        self.tracker.comment(&node.id, reason).await?;
        if attempt < self.cfg.budgets.max_attempts {
            self.retry_ctx
                .insert(node.id.clone(), reason.chars().take(2000).collect());
            self.tracker.set_state(&node.id, NodeState::Ready).await?;
        } else {
            self.pending_flags.remove(&node.id);
            self.declared_breaks.remove(&node.id);
            // A dead decomposer must not leave its file blocked forever;
            // the post-merge scan re-flags it if it is still fat.
            let lifted = self.blocklist.lift_for_node(&node.id)?;
            if !lifted.is_empty() {
                self.tracker
                    .comment(
                        &node.id,
                        &format!("owner failed — megafile blocks lifted: {}", lifted.join(", ")),
                    )
                    .await?;
            }
            self.tracker.set_state(&node.id, NodeState::Failed).await?;
            self.cascade(&node).await?;
        }
        Ok(())
    }

    async fn apply_planner_output(&mut self, node: Node, out: PlannerOutput) -> Result<()> {
        // Design decisions first: divergence detection is mechanism 2.
        let mut wrote_docs = false;
        for mut dd in out.design_decisions {
            // Reload every iteration: an earlier decision from this same
            // output must be visible to the next one's conflict detection.
            let existing = designdocs::load_all(&self.git.merge_dir())?;
            let next = designdocs::next_number(&existing);
            if let Some(conflict) = designdocs::find_conflict(&dd, &node.id, &existing) {
                // Write the incoming doc under a fresh id, then reconcile.
                dd.id = format!("DD-{next}");
                designdocs::write_decision(&self.git.merge_dir(), &dd, &node.id)?;
                wrote_docs = true;
                self.spawn_reconciler(&node.id, conflict.meta.id.clone(), dd.clone())
                    .await?;
                continue;
            }
            // Renumber on plain id collisions (two planners both said DD-3).
            if existing.iter().any(|d| d.meta.id == dd.id) || !dd.id.starts_with("DD-") {
                dd.id = format!("DD-{next}");
            }
            designdocs::write_decision(&self.git.merge_dir(), &dd, &node.id)?;
            wrote_docs = true;
        }
        if wrote_docs {
            self.git
                .harness_commit(&format!("canopy: design decisions from node {}", node.id))
                .await?;
        }

        // Materialize children on the board.
        let deep = node.depth + 1 >= self.cfg.budgets.max_tree_depth;
        let mut created: Vec<String> = Vec::new();
        let mut summary = String::new();
        for (i, child) in out.children.iter().enumerate() {
            let kind = if deep { NodeKind::Execute } else { child.kind };
            let agent = child
                .agent
                .as_ref()
                .and_then(|a| self.validate_allowlisted(a));
            let depends_on: Vec<String> = child
                .depends_on
                .iter()
                .filter_map(|ix| created.get(*ix).cloned())
                .collect();
            let ready = depends_on.is_empty();
            let n = self
                .tracker
                .create_node(NewNode {
                    run_id: self.run.id.clone(),
                    parent_id: Some(node.id.clone()),
                    kind,
                    title: child.title.clone(),
                    spec: child.spec.clone(),
                    agent,
                    depends_on,
                    role_hint: None,
                    depth: node.depth + 1,
                    ready,
                })
                .await?;
            summary.push_str(&format!("{}. [{}] {}\n", i + 1, kind_str(kind), n.title));
            created.push(n.id);
        }
        self.tracker
            .comment(
                &node.id,
                &format!("decomposed into {} children:\n{summary}", created.len()),
            )
            .await?;
        if created.is_empty() {
            // Planner judged there is nothing to do.
            self.tracker.set_state(&node.id, NodeState::Done).await?;
            self.cascade(&node).await?;
        } else {
            self.tracker
                .set_state(&node.id, NodeState::Decomposed)
                .await?;
        }
        Ok(())
    }

    fn validate_allowlisted(&self, a: &AgentRef) -> Option<AgentRef> {
        if self.cfg.routing.mode != RoutingMode::PlannerRouted {
            return None;
        }
        self.cfg
            .routing
            .allowlist
            .iter()
            .find(|e| e.cli == a.cli && e.model == a.model)
            .map(|_| a.clone())
    }

    async fn apply_executor_output(&mut self, node: Node, out: ExecutorOutput) -> Result<()> {
        // Safety net for forgotten commits; also captures fieldguide edits.
        let wt = self.git.worktree_dir(&node.id);
        self.git
            .commit_all(&wt, &format!("canopy: node {} output", node.id))
            .await?;
        self.tracker
            .comment(&node.id, &format!("executor: {}", out.summary))
            .await?;
        for b in &out.breaks {
            self.tracker
                .comment(
                    &node.id,
                    &format!("declared break in {}: {}", b.file, b.reason),
                )
                .await?;
        }
        if !out.flagged_files.is_empty() {
            self.tracker
                .comment(
                    &node.id,
                    &format!("flagged megafiles: {}", out.flagged_files.join(", ")),
                )
                .await?;
        }
        match out.status {
            ExecStatus::Done => {
                // Stash for merge time: a node's own flag must not gate its
                // own merge, and the decomposer must split the file with this
                // node's changes already landed. Breaks let a verify failure
                // land + propagate (mechanism 5) instead of bouncing.
                if !out.flagged_files.is_empty() {
                    self.pending_flags.insert(node.id.clone(), out.flagged_files);
                }
                if !out.breaks.is_empty() {
                    self.declared_breaks.insert(node.id.clone(), out.breaks);
                }
                self.tracker
                    .set_state(&node.id, NodeState::NeedsMerge)
                    .await?;
            }
            ExecStatus::Blocked => {
                self.tracker.set_state(&node.id, NodeState::Failed).await?;
                self.tracker
                    .comment(&node.id, "executor reported blocked — parent will replan")
                    .await?;
                self.cascade(&node).await?;
            }
            ExecStatus::NeedsSplit => {
                self.tracker.set_state(&node.id, NodeState::Failed).await?;
                self.tracker
                    .create_node(NewNode {
                        run_id: self.run.id.clone(),
                        parent_id: node.parent_id.clone(),
                        kind: NodeKind::Plan,
                        title: format!("split: {}", node.title),
                        spec: node.spec.clone(),
                        agent: None,
                        depends_on: vec![],
                        role_hint: None,
                        depth: node.depth,
                        ready: true,
                    })
                    .await?;
                self.cascade(&node).await?;
            }
        }
        Ok(())
    }

    /// Conflict-frequency sensor: every collision surfaces at the merge
    /// queue, so repeat offenders are, by observation, the article's "site of
    /// constant collisions" — decompose them without waiting for the line
    /// threshold. Counts are process-local (a resume restarts them; the line
    /// scan remains the durable trigger).
    async fn record_conflicts(&mut self, files: &[String]) -> Result<()> {
        for f in files {
            let n = self.conflict_counts.entry(f.clone()).or_insert(0);
            *n += 1;
            if *n >= CONFLICT_DECOMPOSE_THRESHOLD {
                self.conflict_counts.remove(f);
                tracing::info!(file = f, "repeated merge conflicts — flagging for decomposition");
                self.flag_and_decompose(f).await?;
            }
        }
        Ok(())
    }

    async fn flag_and_decompose(&mut self, file: &str) -> Result<()> {
        if !self.blocklist.flag(file)? {
            return Ok(());
        }
        let n = self
            .tracker
            .create_node(NewNode {
                run_id: self.run.id.clone(),
                parent_id: Some(self.run.root_node.clone()),
                kind: NodeKind::Execute,
                title: format!("decompose megafile {file}"),
                spec: format!(
                    "The file `{file}` exceeded the size threshold and is \
                     blocking merges. Split it into cohesive modules preserving public \
                     behavior. Mechanical refactor only."
                ),
                agent: Some(self.cfg.decomposer()),
                depends_on: vec![],
                role_hint: Some(Role::Decomposer),
                depth: 1,
                ready: true,
            })
            .await?;
        self.blocklist.assign(file, &n.id)?;
        self.tracker
            .comment(&n.id, &format!("megafile block active on {file}"))
            .await?;
        Ok(())
    }

    async fn spawn_reconciler(
        &mut self,
        author_node: &str,
        incumbent_id: String,
        incoming: DesignDecision,
    ) -> Result<()> {
        let docs = designdocs::load_all(&self.git.merge_dir())?;
        let all_files = self.git.ls_files().await?;
        let refs_a =
            designdocs::files_referencing(&self.git.merge_dir(), &all_files, &incumbent_id);
        let incumbent = docs.iter().find(|d| d.meta.id == incumbent_id);
        // Both planners' specs: the reconciler must see each side's intent,
        // not just the two doc texts.
        let incumbent_author = incumbent
            .map(|d| d.meta.author_node.clone())
            .unwrap_or_default();
        let spec_a = match self.tracker.node(&incumbent_author).await {
            Ok(n) => n.spec,
            Err(_) => String::new(),
        };
        let spec_b = match self.tracker.node(author_node).await {
            Ok(n) => n.spec,
            Err(_) => String::new(),
        };
        let conflict = format!(
            "Two planners made contradictory decisions.\n\n### Doc A (incumbent, {} code references)\nid: {}\n{}\n\n### Doc B (incoming, 0 code references yet)\nid: {}\ntitle: {}\ntopics: {}\n\n{}\n\n### Planner A's work unit (authored doc A)\n{}\n\n### Planner B's work unit (authored doc B)\n{}",
            refs_a.len(),
            incumbent_id,
            incumbent.map(|d| d.body.as_str()).unwrap_or(""),
            incoming.id,
            incoming.title,
            incoming.topics.join(", "),
            incoming.content,
            spec_a,
            spec_b,
        );
        let fg = fieldguide::index_content(&self.git.merge_dir());
        let p = prompt::reconciler(&fg, &conflict);
        let agent_ref = self.cfg.reconciler();
        let fake_node = Node {
            id: format!("reconcile-{}", incoming.id),
            run_id: self.run.id.clone(),
            parent_id: None,
            kind: NodeKind::Plan,
            state: NodeState::Running,
            title: "reconcile".into(),
            spec: String::new(),
            agent: None,
            depends_on: vec![],
            role_hint: None,
            depth: 0,
            attempt: 1,
            claimed_at: None,
            updated_at: chrono::Utc::now(),
        };
        // Reconcilers only read; give them a pinned snapshot, not the merge
        // lane's checkout (the doc write happens harness-side on apply).
        let workdir = self
            .git
            .create_snapshot(&format!("reconcile-{}", incoming.id), &self.run.branch)
            .await?;
        let req = self.request(&fake_node, Role::Reconciler, &agent_ref, p, workdir);
        let author = author_node.to_owned();
        let inc_id = incoming.id.clone();
        self.jobs.spawn(async move {
            let res = agent::for_ref(&agent_ref).invoke(&req).await;
            JobOut::Reconcile {
                author_node: author,
                agent_ref,
                res,
                incumbent_id,
                incoming_id: inc_id,
            }
        });
        Ok(())
    }

    async fn apply_reconcile(
        &mut self,
        author_node: String,
        agent_ref: AgentRef,
        res: Result<InvocationResult>,
        incumbent_id: String,
        incoming_id: String,
    ) -> Result<()> {
        self.git
            .remove_snapshot(&format!("reconcile-{incoming_id}"))
            .await
            .ok();
        let inv = match res {
            Ok(inv) => {
                self.record(&author_node, Role::Reconciler, &agent_ref, &inv, 1);
                inv
            }
            Err(e) => {
                tracing::error!("reconciler failed: {e:#} — incumbent {incumbent_id} stands");
                return self.reconciler_default(&incumbent_id, &incoming_id).await;
            }
        };
        let parsed = agent::trailing_json(&inv.final_message)
            .and_then(|j| serde_json::from_str::<ReconcilerOutput>(j).ok());
        let Some(out) = parsed else {
            tracing::error!(
                "reconciler output unparseable — incumbent {incumbent_id} stands"
            );
            return self.reconciler_default(&incumbent_id, &incoming_id).await;
        };
        let docs = designdocs::load_all(&self.git.merge_dir())?;
        let survivor_id = if out.surviving_id == incoming_id {
            incoming_id.clone()
        } else {
            incumbent_id.clone()
        };
        let superseded_id = if survivor_id == incoming_id {
            incumbent_id.clone()
        } else {
            incoming_id.clone()
        };
        designdocs::write_decision(
            &self.git.merge_dir(),
            &DesignDecision {
                id: survivor_id.clone(),
                title: out.title,
                topics: out.topics,
                content: out.merged_content,
            },
            "reconciler",
        )?;
        if let Some(sup) = docs.iter().find(|d| d.meta.id == superseded_id) {
            designdocs::mark_superseded(sup)?;
        }
        self.git
            .harness_commit(&format!(
                "canopy: reconcile {superseded_id} into {survivor_id}"
            ))
            .await?;
        // References propagate the resolution downstream: every file citing
        // the superseded doc becomes a fix node.
        let all_files = self.git.ls_files().await?;
        for f in designdocs::files_referencing(&self.git.merge_dir(), &all_files, &superseded_id) {
            self.tracker
                .create_node(NewNode {
                    run_id: self.run.id.clone(),
                    parent_id: Some(self.run.root_node.clone()),
                    kind: NodeKind::Execute,
                    title: format!("align {f} to {survivor_id}"),
                    spec: format!(
                        "`{f}` references design doc {superseded_id}, which was superseded by \
                         {survivor_id}. Update the code (and the canopy-design comment) to \
                         comply with {survivor_id}."
                    ),
                    agent: None,
                    depends_on: vec![],
                    role_hint: None,
                    depth: 1,
                    ready: true,
                })
                .await?;
        }
        Ok(())
    }

    /// Reconciler failed: the incumbent (already-referenced) doc wins and the
    /// incoming one is superseded — never leave two active contradicting docs.
    async fn reconciler_default(&mut self, incumbent_id: &str, incoming_id: &str) -> Result<()> {
        let docs = designdocs::load_all(&self.git.merge_dir())?;
        if let Some(doc) = docs.iter().find(|d| d.meta.id == incoming_id) {
            designdocs::mark_superseded(doc)?;
            self.git
                .harness_commit(&format!(
                    "canopy: reconciler failed — {incoming_id} superseded, {incumbent_id} stands"
                ))
                .await?;
        }
        Ok(())
    }

    async fn apply_merge(&mut self, node: Node, report: MergeReport) -> Result<()> {
        match report {
            MergeReport::Gated { file } => {
                self.tracker
                    .comment(
                        &node.id,
                        &format!("merge gated: {file} is a blocked megafile"),
                    )
                    .await?;
                // Back to the queue; the gate lifts when the decomposer lands.
                self.tracker
                    .set_state(&node.id, NodeState::NeedsMerge)
                    .await?;
            }
            MergeReport::Empty => {
                self.tracker
                    .comment(&node.id, "no commits produced — nothing to merge")
                    .await?;
                self.pending_flags.remove(&node.id);
                self.declared_breaks.remove(&node.id);
                // An empty decomposer landing lifts its block — the post-merge
                // scan re-flags the file if it is still fat.
                let lifted = self.blocklist.lift_for_node(&node.id)?;
                if !lifted.is_empty() {
                    self.tracker
                        .comment(
                            &node.id,
                            &format!("megafile blocks lifted: {}", lifted.join(", ")),
                        )
                        .await?;
                }
                self.tracker.set_state(&node.id, NodeState::Done).await?;
                self.git.remove_worktree(&node.id).await?;
                self.cascade(&node).await?;
            }
            MergeReport::Bounced { reason } => {
                self.git.remove_worktree(&node.id).await.ok();
                self.fail_attempt(node, &format!("merge bounced: {reason}"))
                    .await?;
            }
            MergeReport::ConflictFailed { files } => {
                self.git.remove_worktree(&node.id).await.ok();
                self.record_conflicts(&files).await?;
                self.fail_attempt(
                    node,
                    "merge conflict unresolved by merger — retrying on new base",
                )
                .await?;
            }
            MergeReport::Error(e) => {
                self.git.remove_worktree(&node.id).await.ok();
                self.fail_attempt(node, &format!("merge error: {e}"))
                    .await?;
            }
            MergeReport::Landed {
                commit,
                megafiles,
                verify_debt,
                conflicted,
                resolved_by,
            } => {
                self.tracker
                    .comment(&node.id, &format!("merged as {commit}"))
                    .await?;
                self.git.remove_worktree(&node.id).await.ok();
                if !conflicted.is_empty() {
                    self.tracker
                        .comment(
                            &node.id,
                            &format!(
                                "conflict on {} file(s) resolved by {}",
                                conflicted.len(),
                                resolved_by.as_deref().unwrap_or("merger")
                            ),
                        )
                        .await?;
                    self.record_conflicts(&conflicted).await?;
                }
                // Executor-reported flags apply now that the node has landed,
                // then the post-merge scan flags (mechanism 4).
                for f in self.pending_flags.remove(&node.id).unwrap_or_default() {
                    self.flag_and_decompose(&f).await?;
                }
                for f in megafiles {
                    self.flag_and_decompose(&f).await?;
                }
                // Decomposer landings lift their blocks.
                let lifted = self.blocklist.lift_for_node(&node.id)?;
                if !lifted.is_empty() {
                    self.tracker
                        .comment(
                            &node.id,
                            &format!("megafile blocks lifted: {}", lifted.join(", ")),
                        )
                        .await?;
                }
                // Mechanism 5: a declared break landed and verify now fails —
                // the failure becomes fix work instead of a bounce.
                if let Some(tail) = verify_debt {
                    let breaks = self.declared_breaks.remove(&node.id).unwrap_or_default();
                    let reasons = breaks
                        .iter()
                        .map(|b| format!("- {}: {}", b.file, b.reason))
                        .collect::<Vec<_>>()
                        .join("\n");
                    self.tracker
                        .comment(
                            &node.id,
                            "verify failed after declared breaks — landed; propagating as a fix node",
                        )
                        .await?;
                    self.tracker
                        .create_node(NewNode {
                            run_id: self.run.id.clone(),
                            parent_id: node.parent_id.clone(),
                            kind: NodeKind::Execute,
                            title: format!("propagate break: {}", node.title),
                            spec: format!(
                                "The merged work unit \"{}\" made deliberate out-of-scope \
                                 changes (canopy-break) and verify now fails.\n\nDeclared \
                                 breaks:\n{reasons}\n\nVerify output tail:\n{tail}\n\nFix \
                                 everything the break broke so verify passes.",
                                node.title
                            ),
                            agent: None,
                            depends_on: vec![],
                            role_hint: None,
                            depth: node.depth,
                            ready: true,
                        })
                        .await?;
                } else {
                    self.declared_breaks.remove(&node.id);
                }
                self.tracker
                    .set_state(&node.id, NodeState::InReview)
                    .await?;
            }
        }
        Ok(())
    }

    async fn apply_review(
        &mut self,
        node: Node,
        lens: Lens,
        agent_ref: AgentRef,
        res: Result<InvocationResult>,
    ) -> Result<()> {
        let outcome = match res {
            Ok(inv) => {
                self.record(&node.id, Role::Reviewer, &agent_ref, &inv, node.attempt);
                match agent::trailing_json(&inv.final_message)
                    .and_then(|j| serde_json::from_str::<ReviewOutput>(j).ok())
                {
                    Some(r) => Ok(r.findings),
                    None => Err("output unparseable".to_owned()),
                }
            }
            Err(e) => Err(format!("{e:#}")),
        };
        // A failed lens is counted, never treated as a clean pass.
        let (findings, failed) = match outcome {
            Ok(f) => (f, false),
            Err(why) => {
                self.tracker
                    .comment(
                        &node.id,
                        &format!("review lens {} failed ({why}) — not counted", lens.as_str()),
                    )
                    .await?;
                (Vec::new(), true)
            }
        };
        let Some(agg) = self.reviews.get_mut(&node.id) else {
            return Ok(());
        };
        agg.findings.extend(findings);
        if failed {
            agg.failed += 1;
        }
        agg.pending -= 1;
        if agg.pending == 0 {
            let agg = self.reviews.remove(&node.id).unwrap();
            if !self.cfg.routing.reviewers.is_empty()
                && agg.failed == self.cfg.routing.reviewers.len()
            {
                self.tracker
                    .comment(
                        &node.id,
                        "WARNING: ALL review lenses failed — this merge landed effectively unreviewed",
                    )
                    .await?;
            }
            self.finish_review(node, agg.findings).await?;
        }
        Ok(())
    }

    async fn finish_review(&mut self, node: Node, findings: Vec<Finding>) -> Result<()> {
        self.git
            .remove_snapshot(&format!("review-{}", node.id))
            .await
            .ok();
        let highs: Vec<_> = findings
            .iter()
            .filter(|f| f.severity == Severity::High)
            .collect();
        for f in &findings {
            self.tracker
                .comment(
                    &node.id,
                    &format!(
                        "review [{}] {}: {}",
                        match f.severity {
                            Severity::High => "high",
                            Severity::Low => "low",
                        },
                        f.file.as_deref().unwrap_or("-"),
                        f.description
                    ),
                )
                .await?;
        }
        if !highs.is_empty() {
            // Fix node under the same parent: the subtree can't complete
            // until the debt is paid — that's how "high blocks Done" lands.
            let spec = highs
                .iter()
                .map(|f| format!("- {}: {}", f.file.as_deref().unwrap_or("-"), f.description))
                .collect::<Vec<_>>()
                .join("\n");
            self.tracker
                .create_node(NewNode {
                    run_id: self.run.id.clone(),
                    parent_id: node.parent_id.clone(),
                    kind: NodeKind::Execute,
                    title: format!("fix review findings: {}", node.title),
                    spec: format!(
                        "Review of the merged work unit \"{}\" found blocking issues:\n{spec}\n\nFix them.",
                        node.title
                    ),
                    agent: None,
                    depends_on: vec![],
                    role_hint: None,
                    depth: node.depth,
                    ready: true,
                })
                .await?;
        }
        self.tracker.set_state(&node.id, NodeState::Done).await?;
        self.cascade(&node).await?;
        Ok(())
    }

    /// Completion cascade + replanning (trunks own their subtrees).
    async fn cascade(&mut self, node: &Node) -> Result<()> {
        let Some(parent_id) = node.parent_id.clone() else {
            return Ok(()); // root: run_loop notices terminal state
        };
        self.settle_parent(parent_id).await
    }

    /// If every child of `parent_id` is settled, decide the parent's fate:
    /// replan (failures, cap left), Failed, or Done — then recurse upward.
    async fn settle_parent(&mut self, parent_id: String) -> Result<()> {
        let children = self.tracker.children(&parent_id).await?;
        let all_settled = children.iter().all(|c| {
            matches!(
                c.state,
                NodeState::Done | NodeState::Failed | NodeState::Superseded
            )
        });
        if children.is_empty() || !all_settled {
            return Ok(());
        }
        let parent = self.tracker.node(&parent_id).await?;
        if parent.state != NodeState::Decomposed {
            return Ok(());
        }
        let any_failed = children.iter().any(|c| c.state == NodeState::Failed);
        let used = *self.replans.get(&parent_id).unwrap_or(&0);
        if any_failed && used < REPLAN_CAP {
            self.replans.insert(parent_id.clone(), used + 1);
            let summary = children
                .iter()
                .map(|c| {
                    format!(
                        "- [{}] {} — {}",
                        c.state.as_str(),
                        c.title,
                        c.spec.lines().next().unwrap_or("")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            self.retry_ctx.insert(
                parent_id.clone(),
                format!(
                    "Some children failed. Replan ONLY the failed/missing work — completed \
                     children must not be redone.\n\nChildren outcomes:\n{summary}"
                ),
            );
            // Failed children are settled history now — supersede them so
            // the replacement children alone decide the next cascade.
            for c in children.iter().filter(|c| c.state == NodeState::Failed) {
                self.tracker
                    .set_state(&c.id, NodeState::Superseded)
                    .await?;
                self.tracker.comment(&c.id, "superseded by replan").await?;
            }
            self.tracker
                .comment(&parent_id, "children settled with failures — replanning")
                .await?;
            // Re-run the planner on this node (fresh claim path).
            self.tracker.set_state(&parent_id, NodeState::Ready).await?;
            return Ok(());
        }
        let new_state = if any_failed {
            NodeState::Failed
        } else {
            NodeState::Done
        };
        self.tracker.set_state(&parent_id, new_state).await?;
        self.tracker
            .comment(
                &parent_id,
                &format!("all children settled — subtree {}", new_state.as_str()),
            )
            .await?;
        // Recurse upward.
        let parent = self.tracker.node(&parent_id).await?;
        Box::pin(self.cascade(&parent)).await
    }

    /// Board-state cascade sweep. The event cascade misses transitions this
    /// process didn't write (a crash between a child's terminal write and the
    /// parent update, a human settling issues on the board, dependents failed
    /// by the tracker's unblock pass) — re-derive parent completion from
    /// board state every tick.
    async fn sweep_cascades(&mut self) -> Result<()> {
        let parents = self
            .tracker
            .nodes_in_state(&self.run.id, NodeState::Decomposed)
            .await?;
        for p in parents {
            Box::pin(self.settle_parent(p.id)).await?;
        }
        Ok(())
    }

    /// Post-crash: InReview nodes lost their in-memory aggregation; they get
    /// re-reviewed from scratch by pump_reviews (idempotent — reviews are
    /// read-only and re-running lenses is cheap by design).
    async fn recover_in_review(&mut self) -> Result<()> {
        Ok(())
    }
}

fn node_role(node: &Node) -> Role {
    node.role_hint.unwrap_or(match node.kind {
        NodeKind::Plan => Role::Planner,
        NodeKind::Execute => Role::Executor,
    })
}

fn kind_str(k: NodeKind) -> &'static str {
    match k {
        NodeKind::Plan => "plan",
        NodeKind::Execute => "execute",
    }
}

/// First file claimed by two different children, if any. Empty `files`
/// lists are allowed (the contract asks for them; older planners may omit).
fn ownership_overlap(children: &[ChildSpec]) -> Option<String> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    for (i, c) in children.iter().enumerate() {
        for f in &c.files {
            let key = f.trim().trim_start_matches("./").to_owned();
            if key.is_empty() {
                continue;
            }
            match seen.insert(key.clone(), i) {
                Some(j) if j != i => {
                    return Some(format!("children {j} and {i} both own `{key}`"));
                }
                _ => {}
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn child(files: &[&str]) -> ChildSpec {
        ChildSpec {
            title: "t".into(),
            kind: NodeKind::Execute,
            spec: "s".into(),
            depends_on: vec![],
            files: files.iter().map(|s| s.to_string()).collect(),
            agent: None,
        }
    }

    #[test]
    fn ownership_overlap_detection() {
        assert!(ownership_overlap(&[child(&["a.py"]), child(&["b.py"])]).is_none());
        assert!(ownership_overlap(&[child(&["a.py"]), child(&["./a.py"])]).is_some());
        // duplicates within ONE child are that child's own business
        assert!(ownership_overlap(&[child(&["a.py", "a.py"])]).is_none());
        assert!(ownership_overlap(&[child(&[]), child(&[])]).is_none());
    }
}

