//! The serialized merge lane — the article's "custom VCS" role on one
//! machine: the single point every change passes through, where conflicts
//! surface first. One job in flight at a time (the scheduler enforces it);
//! everything here is worktree-side and owns the merge checkout while it runs.
//!
//! Conflicts climb a resolution ladder, cheapest tier first, because the
//! article's merge agent is "impartial and efficient, similar to the way
//! merge queues work in engineering teams" — and engineering merge queues
//! are mechanical first, smart last:
//!
//!   rerere (free) → triage merger (cheap, optional) → merger (smart, final)
//!
//! Whichever tier resolves is a neutral third party; leaves never resolve
//! their own conflicts ("worker agents … either overwrite the other change
//! or abandon their own").

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Result;

use crate::agent::{self, InvocationRequest, InvocationResult};
use crate::config::Config;
use crate::gitops::{GitOps, MergeOutcome};
use crate::mechanisms::{designdocs, fieldguide, megafile::BlockList};
use crate::model::{AgentRef, BreakNote, MergerOutput, Node, Role};

/// What the lane tells the scheduler about one node's merge attempt.
pub enum MergeReport {
    /// Touches a blocked megafile → node waits for the decomposer.
    Gated {
        file: String,
    },
    /// No commits — nothing to merge or review.
    Empty,
    /// Reverted with a reason (field guide budget, design refs, verify).
    Bounced {
        reason: String,
    },
    /// Conflict no ladder tier could fix → retry node on the new base.
    ConflictFailed {
        files: Vec<String>,
    },
    Landed {
        commit: String,
        megafiles: Vec<String>,
        /// Verify failed but the node declared breaks: land anyway, the
        /// failure tail becomes fix work (mechanism 5's "compiler").
        verify_debt: Option<String>,
        /// Files that needed conflict resolution to land (conflict-frequency
        /// sensor input) and which ladder tier resolved them.
        conflicted: Vec<String>,
        resolved_by: Option<String>,
    },
    Error(String),
}

/// A merge that produced a commit, plus how its conflicts (if any) were paid.
struct Resolution {
    commit: String,
    conflicted: Vec<String>,
    resolved_by: Option<String>,
}

enum Landing {
    Empty,
    /// Conflict no ladder tier could resolve (caller aborts the merge).
    Failed(Vec<String>),
    Resolved(Resolution),
}

/// Entry point: run one node's merge as a detached task.
pub async fn run(
    cfg: Arc<Config>,
    node: Node,
    run_branch: String,
    breaks: Vec<BreakNote>,
) -> (MergeReport, Vec<(AgentRef, InvocationResult)>) {
    let git = GitOps::new(&cfg.run.repo);
    let mut merger_runs = Vec::new();
    let report = job(&cfg, &git, &node, &run_branch, &breaks, &mut merger_runs)
        .await
        .unwrap_or_else(|e| MergeReport::Error(format!("{e:#}")));
    (report, merger_runs)
}

async fn job(
    cfg: &Config,
    git: &GitOps,
    node: &Node,
    run_branch: &str,
    breaks: &[BreakNote],
    merger_runs: &mut Vec<(AgentRef, InvocationResult)>,
) -> Result<MergeReport> {
    let files = git.changed_files(&node.id, run_branch).await?;
    // Megafile gate (blocklist re-read for freshness).
    let bl = BlockList::load(&cfg.state_dir())?;
    if let Some(f) = bl.gate(&files, &node.id) {
        return Ok(MergeReport::Gated { file: f.to_owned() });
    }

    // Without a configured triage tier the smart merger simply gets both
    // attempts, as before.
    let ladder: Vec<AgentRef> = match &cfg.routing.merger_triage {
        Some(t) => vec![t.clone(), cfg.merger()],
        None => vec![cfg.merger(), cfg.merger()],
    };

    let res = match land(cfg, git, node, run_branch, &ladder, merger_runs).await? {
        Landing::Empty => return Ok(MergeReport::Empty),
        Landing::Failed(files) => {
            git.abort_merge().await?;
            return Ok(MergeReport::ConflictFailed { files });
        }
        Landing::Resolved(res) => res,
    };
    let gates = gates_or_revert(cfg, git, &res.commit, &files, breaks).await?;

    // Escalation: a triage-tier resolution that bounced on the gates gets ONE
    // redo by the top-tier merger before the node pays with a re-execution.
    // Forget the recorded resolution first or rerere would replay the bad one.
    if !(triage_resolved(cfg, &res) && matches!(gates, MergeReport::Bounced { .. })) {
        return Ok(stamp(gates, res));
    }
    git.rerere_forget(&res.conflicted).await;
    match land(cfg, git, node, run_branch, &[cfg.merger()], merger_runs).await? {
        Landing::Empty => Ok(MergeReport::Empty),
        Landing::Failed(files) => {
            git.abort_merge().await?;
            Ok(MergeReport::ConflictFailed { files })
        }
        Landing::Resolved(res) => {
            let gates = gates_or_revert(cfg, git, &res.commit, &files, breaks).await?;
            Ok(stamp(gates, res))
        }
    }
}

/// Did the cheap triage tier produce this resolution? (Only then is a smart
/// redo worth anything — a smart-tier resolution that bounced would just be
/// redone identically.)
fn triage_resolved(cfg: &Config, res: &Resolution) -> bool {
    let Some(t) = &cfg.routing.merger_triage else {
        return false;
    };
    !res.conflicted.is_empty()
        && res.resolved_by.as_deref() == Some(&format!("{}:{}", t.cli.as_str(), t.model))
}

/// Write the resolution provenance into a Landed report.
fn stamp(mut gates: MergeReport, res: Resolution) -> MergeReport {
    if let MergeReport::Landed {
        conflicted,
        resolved_by,
        ..
    } = &mut gates
    {
        *conflicted = res.conflicted;
        *resolved_by = res.resolved_by;
    }
    gates
}

/// Post-merge gates with the lane's revert invariant: any ERROR after the
/// commit exists must revert it — otherwise the retry sees "nothing to
/// merge" and the work lands unverified and unreviewed.
async fn gates_or_revert(
    cfg: &Config,
    git: &GitOps,
    commit: &str,
    files: &[String],
    breaks: &[BreakNote],
) -> Result<MergeReport> {
    match post_merge_gates(cfg, git, commit, files, breaks).await {
        Ok(report) => Ok(report),
        Err(e) => {
            if let Err(rev) = git.revert_merge(commit).await {
                tracing::error!("revert after gate error failed: {rev:#}");
            }
            Err(e)
        }
    }
}

/// One pass through the merge + resolution ladder: mechanical (rerere) tier
/// first, then each agent tier in `ladder`.
async fn land(
    cfg: &Config,
    git: &GitOps,
    node: &Node,
    run_branch: &str,
    ladder: &[AgentRef],
    merger_runs: &mut Vec<(AgentRef, InvocationResult)>,
) -> Result<Landing> {
    let details = match git.try_merge(&node.id, run_branch).await? {
        MergeOutcome::NothingToMerge => return Ok(Landing::Empty),
        MergeOutcome::Merged { commit } => {
            return Ok(Landing::Resolved(Resolution {
                commit,
                conflicted: Vec::new(),
                resolved_by: None,
            }))
        }
        MergeOutcome::Conflicted { details } => details,
    };
    let conflicted: Vec<String> = details.lines().map(str::to_owned).collect();
    // Capture hunks before anything stages the index (staging empties the
    // unmerged set that hunk extraction reads from).
    let hunks0 = git.conflict_hunks().await.unwrap_or_else(|_| details.clone());

    // Tier 0, free: rerere already replayed a recorded resolution.
    if !git.worktree_has_markers().await? && git.merge_resolved().await? {
        let commit = git.finalize_merge(&node.id).await?;
        return Ok(Landing::Resolved(Resolution {
            commit,
            conflicted,
            resolved_by: Some("rerere".into()),
        }));
    }

    let recent = git.recent_subjects(5).await.unwrap_or_default();
    for agent_ref in ladder {
        let hunks = {
            let h = git.conflict_hunks().await.unwrap_or_default();
            if h.trim().is_empty() {
                hunks0.clone()
            } else {
                h
            }
        };
        let conflict = format!(
            "Node \"{}\" (spec below) conflicts with the current run branch.\n\n### Node spec\n{}\n\n### Recent landings on the run branch (the other side)\n{}\n\n### Conflicted hunks\n{}",
            node.title, node.spec, recent, hunks
        );
        let fg = fieldguide::index_content(&git.merge_dir());
        // Context diet: only design docs the conflicted files or the node
        // spec actually reference — not the whole design/ folder.
        let mut docs = designdocs::load_all(&git.merge_dir())?;
        let cited: HashSet<String> = designdocs::scan_refs(&git.merge_dir(), &conflicted)
            .into_iter()
            .map(|r| r.doc_id)
            .collect();
        docs.retain(|d| cited.contains(&d.meta.id) || node.spec.contains(&d.meta.id));
        let p = crate::prompt::merger(&fg, &conflict, &docs);
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
        match agent::for_ref(agent_ref).invoke(&req).await {
            Ok(inv) => {
                let ok = inv.exit_ok;
                let parsed = agent::trailing_json(&inv.final_message)
                    .and_then(|j| serde_json::from_str::<MergerOutput>(j).ok());
                merger_runs.push((agent_ref.clone(), inv));
                // The Merger's own verdict counts: an explicit resolved=false
                // is a failure even if git looks clean. Git state remains the
                // positive gate.
                let declined = parsed.map(|o| !o.resolved).unwrap_or(false);
                if ok && !declined && git.merge_resolved().await? {
                    let commit = git.finalize_merge(&node.id).await?;
                    return Ok(Landing::Resolved(Resolution {
                        commit,
                        conflicted,
                        resolved_by: Some(format!(
                            "{}:{}",
                            agent_ref.cli.as_str(),
                            agent_ref.model
                        )),
                    }));
                }
            }
            Err(e) => tracing::warn!("merger invocation failed: {e:#}"),
        }
    }
    Ok(Landing::Failed(conflicted))
}

/// Everything between "the merge commit exists" and "it may stay": field
/// guide budget, design-ref check, verify (with mechanism-5 break landing),
/// megafile scan. Bounces revert the commit themselves; hard errors are the
/// caller's to revert (see `gates_or_revert`).
async fn post_merge_gates(
    cfg: &Config,
    git: &GitOps,
    commit: &str,
    files: &[String],
    breaks: &[BreakNote],
) -> Result<MergeReport> {
    // Mechanism 7: field guide line budget.
    if let Some(lines) =
        fieldguide::over_budget(&git.merge_dir(), cfg.thresholds.fieldguide_line_budget)
    {
        git.revert_merge(commit).await?;
        return Ok(MergeReport::Bounced {
            reason: format!(
                "fieldguide/index.md is {lines} lines (budget {}) — curate before adding",
                cfg.thresholds.fieldguide_line_budget
            ),
        });
    }
    // Mechanism 1: checked design references (files this node touched; the
    // supersede path is covered by the reconciler's fix nodes).
    let docs = designdocs::load_all(&git.merge_dir())?;
    let refs = designdocs::scan_refs(&git.merge_dir(), files);
    let violations = designdocs::check_refs(&refs, &docs);
    if !violations.is_empty() {
        git.revert_merge(commit).await?;
        return Ok(MergeReport::Bounced {
            reason: format!("design reference check failed:\n{}", violations.join("\n")),
        });
    }
    // Ground truth: the verify command.
    let verify = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(&cfg.run.verify)
        .current_dir(git.merge_dir())
        .output()
        .await?;
    let megafiles = || async {
        Ok::<Vec<String>, anyhow::Error>(
            git.megafile_scan(cfg.thresholds.megafile_lines)
                .await?
                .into_iter()
                .map(|(f, _)| f)
                .collect(),
        )
    };
    if !verify.status.success() {
        let tail = |b: &[u8]| -> String {
            let s = String::from_utf8_lossy(b);
            s.chars()
                .skip(s.chars().count().saturating_sub(1500))
                .collect()
        };
        let out = format!("{}\n{}", tail(&verify.stdout), tail(&verify.stderr));
        if !breaks.is_empty() {
            // Mechanism 5: a declared break lands anyway; the verify failure
            // propagates as fix work (the article's compiler role).
            return Ok(MergeReport::Landed {
                commit: commit.to_owned(),
                megafiles: megafiles().await?,
                verify_debt: Some(out),
                conflicted: Vec::new(),
                resolved_by: None,
            });
        }
        git.revert_merge(commit).await?;
        return Ok(MergeReport::Bounced {
            reason: format!("verify failed:\n{out}"),
        });
    }
    // Mechanism 4: megafile scan.
    Ok(MergeReport::Landed {
        commit: commit.to_owned(),
        megafiles: megafiles().await?,
        verify_debt: None,
        conflicted: Vec::new(),
        resolved_by: None,
    })
}
