//! The event loop. Claims ready nodes off the board, spawns one-shot agent
//! processes, applies their structured output, and runs the coordination
//! mechanisms — merges serialized, coordination debt paid before new work.
//!
//! Concurrency model: agent processes run as detached tokio tasks (a
//! JoinSet); ALL state mutations (tracker, blocklist, ledger, tree cascade)
//! happen on this loop when a task joins. One writer, no locks. The merge
//! lane is a task too, but at most one is ever in flight — that lane IS the
//! article's "single point every change passes through".

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::task::JoinSet;

use crate::agent::{self, InvocationRequest, InvocationResult};
use crate::config::{Config, RoutingMode};
use crate::gitops::{GitOps, MergeOutcome};
use crate::ledger::{self, Ledger};
use crate::mechanisms::{designdocs, fieldguide, megafile::BlockList};
use crate::model::*;
use crate::prompt;
use crate::tracker::Tracker;

const MERGER_MAX_TRIES: u32 = 2;
const REPLAN_CAP: u32 = 2;

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
    /// node → pending review lenses + findings so far.
    reviews: HashMap<String, ReviewAgg>,
    /// node → context injected on retry (verify failures, bounces).
    retry_ctx: HashMap<String, String>,
    /// planner node → replans consumed.
    replans: HashMap<String, u32>,
    paused_for_budget: bool,
}

struct ReviewAgg {
    pending: usize,
    findings: Vec<Finding>,
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

enum MergeReport {
    /// Touches a blocked megafile → node waits for the decomposer.
    Gated { file: String },
    /// No commits — nothing to merge or review.
    Empty,
    /// Reverted with a reason (field guide budget, design refs, verify).
    Bounced { reason: String },
    /// Conflict the Merger couldn't fix → retry node on the new base.
    ConflictFailed,
    Landed {
        commit: String,
        megafiles: Vec<String>,
    },
    Error(String),
}

impl Scheduler {
    pub async fn start(
        cfg: Config,
        objective: Option<String>,
        resume: Option<String>,
    ) -> Result<()> {
        let cfg = Arc::new(cfg);
        let tracker = crate::tracker::from_config(&cfg).await?;
        let git = GitOps::new(&cfg.run.repo);
        let ledger = Ledger::open(&cfg.state_dir().join("ledger.db"))?;

        let run = match (objective, resume) {
            (_, Some(id)) => tracker.load_run(&id).await?,
            (Some(obj), None) => {
                let branch =
                    format!("canopy/run-{}", chrono::Utc::now().format("%Y%m%d-%H%M%S"));
                tracker.init_run(&obj, &branch).await?
            }
            (None, None) => anyhow::bail!("provide an objective or --resume <run-id>"),
        };

        git.ensure_run_branch(&run.branch).await?;
        if fieldguide::ensure_scaffold(&git.merge_dir())? {
            git.harness_commit("canopy: scaffold design/ and fieldguide/").await?;
        }
        let blocklist = BlockList::load(&cfg.state_dir())?;

        tracing::info!(run = run.id, branch = run.branch, "canopy run starting");
        println!("run: {}\nbranch: {}\nboard: {}", run.id, run.branch, cfg.run.tracker);
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
            reviews: HashMap::new(),
            retry_ctx: HashMap::new(),
            replans: HashMap::new(),
            paused_for_budget: false,
        };
        sched.recover_in_review().await?;
        sched.run_loop().await
    }

    async fn run_loop(&mut self) -> Result<()> {
        loop {
            self.settle().await?;
            self.pump_merges().await?;
            self.pump_reviews().await?;
            self.claim_and_spawn().await?;

            // Terminal check: root Done/Failed and nothing in flight.
            let root = self.tracker.node(&self.run.root_node.clone()).await?;
            if self.jobs.is_empty()
                && matches!(root.state, NodeState::Done | NodeState::Failed)
            {
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

    // ------------------------------------------------------------------
    // Merge lane (mechanism 3 + post-merge gates 1, 4, 5, 7)
    // ------------------------------------------------------------------

    async fn pump_merges(&mut self) -> Result<()> {
        if self.merge_inflight {
            return Ok(());
        }
        let mut queue = self
            .tracker
            .nodes_in_state(&self.run.id, NodeState::NeedsMerge)
            .await?;
        queue.sort_by(|a, b| a.updated_at.cmp(&b.updated_at));
        let Some(node) = queue.into_iter().next() else {
            return Ok(());
        };
        if !self
            .tracker
            .transition(&node.id, NodeState::NeedsMerge, NodeState::Merging)
            .await?
        {
            return Ok(());
        }
        self.merge_inflight = true;
        let cfg = self.cfg.clone();
        let run_branch = self.run.branch.clone();
        self.jobs.spawn(async move {
            let (report, merger_runs) = merge_job(cfg, node.clone(), run_branch).await;
            JobOut::Merge {
                node,
                report,
                merger_runs,
            }
        });
        Ok(())
    }

    async fn pump_reviews(&mut self) -> Result<()> {
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
            let fg = fieldguide::index_content(&self.git.merge_dir());
            let transcript = std::fs::read_to_string(
                self.cfg
                    .state_dir()
                    .join("transcripts")
                    .join(format!("{}-{}.txt", node.id, node.attempt)),
            )
            .ok();
            self.reviews.insert(
                node.id.clone(),
                ReviewAgg {
                    pending: self.cfg.routing.reviewers.len(),
                    findings: Vec::new(),
                },
            );
            for rc in &self.cfg.routing.reviewers {
                let agent_ref = AgentRef {
                    cli: rc.cli,
                    model: rc.model.clone(),
                };
                let p = prompt::reviewer(&fg, rc.lens, &node.spec, &diff, transcript.as_deref());
                let req = self.request(&node, Role::Reviewer, &agent_ref, p, self.git.merge_dir());
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
                // Planners read the run branch state, never a private worktree.
                (p, self.git.merge_dir())
            }
            NodeKind::Execute => {
                let docs: Vec<_> = all_docs
                    .iter()
                    .filter(|d| node.spec.contains(&d.meta.id))
                    .cloned()
                    .collect();
                let p = prompt::executor(&fg, &node.spec, &docs, retry.as_deref());
                let wt = self.git.create_worktree(&node.id, &self.run.branch).await?;
                (p, wt)
            }
        };
        match node.kind {
            NodeKind::Plan => self.inflight_plan += 1,
            NodeKind::Execute => self.inflight_exec += 1,
        }
        let role = match node.kind {
            NodeKind::Plan => Role::Planner,
            NodeKind::Execute => Role::Executor,
        };
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
                &format!("claimed by {}:{} (attempt {})", agent_ref.cli.as_str(), agent_ref.model, node.attempt + 1),
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
                    let tail: String = inv.final_message.chars().rev().take(800).collect::<String>().chars().rev().collect();
                    return self.fail_attempt(node, &format!("agent error: {tail}")).await;
                }
                inv
            }
            Err(e) => {
                return self.fail_attempt(node, &format!("invocation failed: {e:#}")).await;
            }
        };

        let json = agent::trailing_json(&inv.final_message);
        match node.kind {
            NodeKind::Plan => {
                let parsed = json.and_then(|j| serde_json::from_str::<PlannerOutput>(j).ok());
                match parsed {
                    Some(out) => self.apply_planner_output(node, out).await,
                    None if !was_nudged => {
                        let nudged = prompt::json_retry_nudge(&prompt_used, "missing or invalid");
                        self.respawn_with_prompt(node, agent_ref, nudged).await
                    }
                    None => self.fail_attempt(node, "structured output unparseable twice").await,
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
                    None => self.fail_attempt(node, "structured output unparseable twice").await,
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
        let workdir = match node.kind {
            NodeKind::Plan => self.git.merge_dir(),
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
        let attempt = self.tracker.bump_attempt(&node.id).await?;
        self.tracker.comment(&node.id, reason).await?;
        if attempt < self.cfg.budgets.max_attempts {
            self.retry_ctx
                .insert(node.id.clone(), reason.chars().take(2000).collect());
            self.tracker.set_state(&node.id, NodeState::Ready).await?;
        } else {
            self.tracker.set_state(&node.id, NodeState::Failed).await?;
            self.cascade(&node).await?;
        }
        Ok(())
    }

    async fn apply_planner_output(&mut self, node: Node, out: PlannerOutput) -> Result<()> {
        // Design decisions first: divergence detection is mechanism 2.
        let existing = designdocs::load_all(&self.git.merge_dir())?;
        let mut next = designdocs::next_number(&existing);
        let mut wrote_docs = false;
        for mut dd in out.design_decisions {
            if let Some(conflict) = designdocs::find_conflict(&dd, &node.id, &existing) {
                // Write the incoming doc under a fresh id, then reconcile.
                dd.id = format!("DD-{next}");
                next += 1;
                designdocs::write_decision(&self.git.merge_dir(), &dd, &node.id)?;
                wrote_docs = true;
                self.spawn_reconciler(&node.id, conflict.meta.id.clone(), dd.clone())
                    .await?;
                continue;
            }
            // Renumber on plain id collisions (two planners both said DD-3).
            if existing.iter().any(|d| d.meta.id == dd.id)
                || !dd.id.starts_with("DD-")
            {
                dd.id = format!("DD-{next}");
                next += 1;
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
            let agent = child.agent.as_ref().and_then(|a| self.validate_allowlisted(a));
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
                    depth: node.depth + 1,
                    ready,
                })
                .await?;
            summary.push_str(&format!("{}. [{}] {}\n", i + 1, kind_str(kind), n.title));
            created.push(n.id);
        }
        self.tracker
            .comment(&node.id, &format!("decomposed into {} children:\n{summary}", created.len()))
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
                .comment(&node.id, &format!("declared break in {}: {}", b.file, b.reason))
                .await?;
        }
        // Flags are applied post-merge (Landed) so a node's own flag can't
        // gate its own merge; stash them in retry_ctx-adjacent storage.
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
                self.tracker
                    .set_state(&node.id, NodeState::NeedsMerge)
                    .await?;
                // Executor-reported flags join the scan at merge time via the
                // comment trail; hard flags come from the post-merge scan.
                for f in out.flagged_files {
                    self.flag_and_decompose(&f).await?;
                }
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
                        depth: node.depth,
                        ready: true,
                    })
                    .await?;
                self.cascade(&node).await?;
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
        let conflict = format!(
            "Two planners made contradictory decisions.\n\n### Doc A (incumbent, {} code references)\nid: {}\n{}\n\n### Doc B (incoming, 0 code references yet)\nid: {}\ntitle: {}\ntopics: {}\n\n{}",
            refs_a.len(),
            incumbent_id,
            incumbent.map(|d| d.body.as_str()).unwrap_or(""),
            incoming.id,
            incoming.title,
            incoming.topics.join(", "),
            incoming.content,
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
            depth: 0,
            attempt: 1,
            claimed_at: None,
            updated_at: chrono::Utc::now(),
        };
        let req = self.request(&fake_node, Role::Reconciler, &agent_ref, p, self.git.merge_dir());
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
        let inv = match res {
            Ok(inv) => {
                self.record(&author_node, Role::Reconciler, &agent_ref, &inv, 1);
                inv
            }
            Err(e) => {
                tracing::error!("reconciler failed: {e:#} — keeping incumbent {incumbent_id}");
                return Ok(());
            }
        };
        let parsed = agent::trailing_json(&inv.final_message)
            .and_then(|j| serde_json::from_str::<ReconcilerOutput>(j).ok());
        let Some(out) = parsed else {
            tracing::error!("reconciler output unparseable — keeping incumbent {incumbent_id}");
            return Ok(());
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
                    depth: 1,
                    ready: true,
                })
                .await?;
        }
        Ok(())
    }

    async fn apply_merge(&mut self, node: Node, report: MergeReport) -> Result<()> {
        match report {
            MergeReport::Gated { file } => {
                self.tracker
                    .comment(&node.id, &format!("merge gated: {file} is a blocked megafile"))
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
                self.tracker.set_state(&node.id, NodeState::Done).await?;
                self.git.remove_worktree(&node.id).await?;
                self.cascade(&node).await?;
            }
            MergeReport::Bounced { reason } => {
                self.git.remove_worktree(&node.id).await.ok();
                self.fail_attempt(node, &format!("merge bounced: {reason}")).await?;
            }
            MergeReport::ConflictFailed => {
                self.git.remove_worktree(&node.id).await.ok();
                self.fail_attempt(node, "merge conflict unresolved by merger — retrying on new base")
                    .await?;
            }
            MergeReport::Error(e) => {
                self.git.remove_worktree(&node.id).await.ok();
                self.fail_attempt(node, &format!("merge error: {e}")).await?;
            }
            MergeReport::Landed { commit, megafiles } => {
                self.tracker
                    .comment(&node.id, &format!("merged as {commit}"))
                    .await?;
                self.git.remove_worktree(&node.id).await.ok();
                // Post-merge megafile scan flags (mechanism 4).
                for f in megafiles {
                    self.flag_and_decompose(&f).await?;
                }
                // Decomposer landings lift their blocks.
                let lifted = self.blocklist.lift_for_node(&node.id)?;
                if !lifted.is_empty() {
                    self.tracker
                        .comment(&node.id, &format!("megafile blocks lifted: {}", lifted.join(", ")))
                        .await?;
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
        let findings = match res {
            Ok(inv) => {
                self.record(&node.id, Role::Reviewer, &agent_ref, &inv, node.attempt);
                agent::trailing_json(&inv.final_message)
                    .and_then(|j| serde_json::from_str::<ReviewOutput>(j).ok())
                    .map(|r| r.findings)
                    .unwrap_or_else(|| {
                        tracing::warn!("reviewer ({}) output unparseable — treated as clean", lens.as_str());
                        Vec::new()
                    })
            }
            Err(e) => {
                tracing::warn!("reviewer ({}) failed: {e:#} — lens skipped", lens.as_str());
                Vec::new()
            }
        };
        let Some(agg) = self.reviews.get_mut(&node.id) else {
            return Ok(());
        };
        agg.findings.extend(findings);
        agg.pending -= 1;
        if agg.pending == 0 {
            let agg = self.reviews.remove(&node.id).unwrap();
            self.finish_review(node, agg.findings).await?;
        }
        Ok(())
    }

    async fn finish_review(&mut self, node: Node, findings: Vec<Finding>) -> Result<()> {
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
        let children = self.tracker.children(&parent_id).await?;
        let all_settled = children
            .iter()
            .all(|c| matches!(c.state, NodeState::Done | NodeState::Failed));
        if !all_settled {
            return Ok(());
        }
        let parent = self.tracker.node(&parent_id).await?;
        if parent.state != NodeState::Decomposed {
            return Ok(());
        }
        let any_failed = children.iter().any(|c| c.state == NodeState::Failed);
        let replans = self.replans.entry(parent_id.clone()).or_insert(0);
        if any_failed && *replans < REPLAN_CAP {
            *replans += 1;
            let summary = children
                .iter()
                .map(|c| format!("- [{}] {} — {}", c.state.as_str(), c.title, c.spec.lines().next().unwrap_or("")))
                .collect::<Vec<_>>()
                .join("\n");
            self.retry_ctx.insert(
                parent_id.clone(),
                format!(
                    "Some children failed. Replan ONLY the failed/missing work — completed \
                     children must not be redone.\n\nChildren outcomes:\n{summary}"
                ),
            );
            self.tracker
                .comment(&parent_id, "children settled with failures — replanning")
                .await?;
            // Re-run the planner on this node (fresh claim path).
            self.tracker.set_state(&parent_id, NodeState::Ready).await?;
            return Ok(());
        }
        let new_state = if any_failed { NodeState::Failed } else { NodeState::Done };
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

    /// Post-crash: InReview nodes lost their in-memory aggregation; they get
    /// re-reviewed from scratch by pump_reviews (idempotent — reviews are
    /// read-only and re-running lenses is cheap by design).
    async fn recover_in_review(&mut self) -> Result<()> {
        Ok(())
    }
}

fn node_role(node: &Node) -> Role {
    match node.kind {
        NodeKind::Plan => Role::Planner,
        NodeKind::Execute => Role::Executor,
    }
}

fn kind_str(k: NodeKind) -> &'static str {
    match k {
        NodeKind::Plan => "plan",
        NodeKind::Execute => "execute",
    }
}

// ---------------------------------------------------------------------------
// The merge job: everything worktree-side for one node's merge, run as a
// detached task. At most one in flight — the serialized queue.
// ---------------------------------------------------------------------------

async fn merge_job(
    cfg: Arc<Config>,
    node: Node,
    run_branch: String,
) -> (MergeReport, Vec<(AgentRef, InvocationResult)>) {
    let git = GitOps::new(&cfg.run.repo);
    let mut merger_runs = Vec::new();
    let report = async {
        let files = git.changed_files(&node.id, &run_branch).await?;
        // Megafile gate (blocklist re-read for freshness).
        let bl = BlockList::load(&cfg.state_dir())?;
        if let Some(f) = bl.gate(&files, &node.id) {
            return Ok(MergeReport::Gated { file: f.to_owned() });
        }
        let outcome = git.try_merge(&node.id, &run_branch).await?;
        let commit = match outcome {
            MergeOutcome::NothingToMerge => return Ok(MergeReport::Empty),
            MergeOutcome::Merged { commit } => commit,
            MergeOutcome::Conflicted { details } => {
                // Mechanism 3: the neutral Merger.
                let mut resolved = false;
                for _try in 0..MERGER_MAX_TRIES {
                    let hunks = git.conflict_hunks().await.unwrap_or(details.clone());
                    let conflict = format!(
                        "Node \"{}\" (spec below) conflicts with the current run branch.\n\n### Node spec\n{}\n\n### Conflicted hunks\n{}",
                        node.title, node.spec, hunks
                    );
                    let fg = fieldguide::index_content(&git.merge_dir());
                    let docs = designdocs::load_all(&git.merge_dir())?;
                    let p = prompt::merger(&fg, &conflict, &docs);
                    let agent_ref = cfg.merger();
                    let tdir = cfg.state_dir().join("transcripts");
                    let _ = std::fs::create_dir_all(&tdir);
                    let req = InvocationRequest {
                        role: Role::Merger,
                        node_id: node.id.clone(),
                        prompt: p,
                        model: agent_ref.model.clone(),
                        workdir: git.merge_dir(),
                        timeout_secs: cfg.budgets.agent_timeout_secs,
                        max_turns: cfg.budgets.max_turns,
                        transcript_path: tdir.join(format!("{}-merge.txt", node.id)),
                    };
                    match agent::for_ref(&agent_ref).invoke(&req).await {
                        Ok(inv) => {
                            let ok = inv.exit_ok;
                            merger_runs.push((agent_ref, inv));
                            if ok && git.merge_resolved().await? {
                                resolved = true;
                                break;
                            }
                        }
                        Err(e) => tracing::warn!("merger invocation failed: {e:#}"),
                    }
                }
                if !resolved {
                    git.abort_merge().await?;
                    return Ok(MergeReport::ConflictFailed);
                }
                git.finalize_merge(&node.id).await?
            }
        };

        // --- Post-merge gates, in bounce-cheapest order ---
        // Mechanism 7: field guide line budget.
        if let Some(lines) = fieldguide::over_budget(&git.merge_dir(), cfg.thresholds.fieldguide_line_budget)
        {
            git.revert_merge(&commit).await?;
            return Ok(MergeReport::Bounced {
                reason: format!(
                    "fieldguide/index.md is {lines} lines (budget {}) — curate before adding",
                    cfg.thresholds.fieldguide_line_budget
                ),
            });
        }
        // Mechanism 1: checked design references (only files this node touched).
        let docs = designdocs::load_all(&git.merge_dir())?;
        let refs = designdocs::scan_refs(&git.merge_dir(), &files);
        let violations = designdocs::check_refs(&refs, &docs);
        if !violations.is_empty() {
            git.revert_merge(&commit).await?;
            return Ok(MergeReport::Bounced {
                reason: format!("design reference check failed:\n{}", violations.join("\n")),
            });
        }
        // Ground truth: the verify command (also propagates declared breaks).
        let verify = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&cfg.run.verify)
            .current_dir(git.merge_dir())
            .output()
            .await?;
        if !verify.status.success() {
            let tail = |b: &[u8]| -> String {
                let s = String::from_utf8_lossy(b);
                s.chars().skip(s.chars().count().saturating_sub(1500)).collect()
            };
            git.revert_merge(&commit).await?;
            return Ok(MergeReport::Bounced {
                reason: format!(
                    "verify failed:\n{}\n{}",
                    tail(&verify.stdout),
                    tail(&verify.stderr)
                ),
            });
        }
        // Mechanism 4: megafile scan.
        let megafiles = git
            .megafile_scan(cfg.thresholds.megafile_lines)
            .await?
            .into_iter()
            .map(|(f, _)| f)
            .collect();
        Ok::<MergeReport, anyhow::Error>(MergeReport::Landed { commit, megafiles })
    }
    .await
    .unwrap_or_else(|e: anyhow::Error| MergeReport::Error(format!("{e:#}")));
    (report, merger_runs)
}
