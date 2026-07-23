//! Git plumbing via the `git` CLI: run branch, per-node worktrees, the
//! serialized merge queue, and repo scans. This module plays the role the
//! article's custom VCS plays at datacenter scale: the single point every
//! change passes through, where conflicts surface first.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::process::Command;

pub struct GitOps {
    /// Target repo root.
    repo: PathBuf,
    /// `.canopy` inside the target repo (gitignored).
    state: PathBuf,
}

#[derive(Debug)]
pub enum MergeOutcome {
    Merged { commit: String },
    Conflicted { details: String },
    NothingToMerge,
}

impl GitOps {
    pub fn new(repo: &Path) -> GitOps {
        GitOps {
            repo: repo.to_path_buf(),
            state: repo.join(".canopy"),
        }
    }

    async fn git_in(&self, cwd: &Path, args: &[&str]) -> Result<String> {
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .with_context(|| format!("spawning git {args:?}"))?;
        if !out.status.success() {
            anyhow::bail!(
                "git {:?} in {} failed: {}{}",
                args,
                cwd.display(),
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    async fn git(&self, args: &[&str]) -> Result<String> {
        self.git_in(&self.repo.clone(), args).await
    }

    /// Merge worktree: run-branch checkout where merges, harness commits and
    /// verify runs happen. Single writer (the scheduler), so no locking.
    pub fn merge_dir(&self) -> PathBuf {
        self.state.join("merge")
    }

    pub fn worktree_dir(&self, node_id: &str) -> PathBuf {
        self.state.join("worktrees").join(node_id)
    }

    fn node_branch(node_id: &str) -> String {
        format!("canopy/node-{node_id}")
    }

    /// Create the run branch (from current HEAD if new) and its merge worktree.
    /// An existing merge worktree may be left over from a previous run: clear
    /// any in-progress merge (crash recovery) and re-point it at this branch.
    pub async fn ensure_run_branch(&self, branch: &str) -> Result<()> {
        // Deleting .canopy/ is the documented reset; clear the stale
        // registrations that leaves behind.
        let _ = self.git(&["worktree", "prune"]).await;
        let exists = self
            .git(&["rev-parse", "--verify", "--quiet", branch])
            .await
            .is_ok();
        if !exists {
            self.git(&["branch", branch]).await?;
        }
        let md = self.merge_dir();
        if !md.exists() {
            std::fs::create_dir_all(md.parent().unwrap())?;
            self.git(&["worktree", "add", md.to_str().unwrap(), branch])
                .await?;
        } else {
            self.abort_merge().await?;
            let head = self
                .git_in(&md, &["rev-parse", "--abbrev-ref", "HEAD"])
                .await?;
            if head.trim() != branch {
                self.git_in(&md, &["checkout", branch]).await?;
            }
        }
        Ok(())
    }

    pub fn snapshot_dir(&self, tag: &str) -> PathBuf {
        self.state.join("snapshots").join(tag)
    }

    /// Read-only detached checkout for planner/reviewer/reconciler processes,
    /// so agent reads never race the merge lane's own checkout.
    pub async fn create_snapshot(&self, tag: &str, commitish: &str) -> Result<PathBuf> {
        let path = self.snapshot_dir(tag);
        if path.exists() {
            self.remove_snapshot(tag).await?;
        }
        std::fs::create_dir_all(path.parent().unwrap())?;
        self.git(&[
            "worktree",
            "add",
            "--detach",
            path.to_str().unwrap(),
            commitish,
        ])
        .await?;
        Ok(path)
    }

    pub async fn remove_snapshot(&self, tag: &str) -> Result<()> {
        let path = self.snapshot_dir(tag);
        if path.exists() {
            self.git(&["worktree", "remove", "--force", path.to_str().unwrap()])
                .await?;
        }
        Ok(())
    }

    /// Last N commit subjects on the run branch (Merger context: what the
    /// "other side" of the serialized queue has been landing).
    pub async fn recent_subjects(&self, n: usize) -> Result<String> {
        let n = n.to_string();
        self.git_in(&self.merge_dir(), &["log", "-n", &n, "--format=%s"])
            .await
    }

    /// Files touched by one merge commit (review lens context).
    pub async fn commit_files(&self, commit: &str) -> Result<Vec<String>> {
        let out = self
            .git_in(
                &self.merge_dir(),
                &[
                    "show",
                    "--format=",
                    "--name-only",
                    "-m",
                    "--first-parent",
                    commit,
                ],
            )
            .await?;
        Ok(out.lines().filter(|l| !l.is_empty()).map(str::to_owned).collect())
    }

    /// Fresh worktree for a node, branched off the current run branch tip.
    /// Existing worktree/branch (a retry) is discarded first: retries restart
    /// from the latest merged state, which is the merge queue's rebase.
    pub async fn create_worktree(&self, node_id: &str, run_branch: &str) -> Result<PathBuf> {
        let wt = self.worktree_dir(node_id);
        let branch = Self::node_branch(node_id);
        if wt.exists() {
            self.remove_worktree(node_id).await?;
        }
        // Delete a stale node branch from a previous attempt, if any.
        let _ = self.git(&["branch", "-D", &branch]).await;
        std::fs::create_dir_all(wt.parent().unwrap())?;
        self.git(&[
            "worktree",
            "add",
            "-b",
            &branch,
            wt.to_str().unwrap(),
            run_branch,
        ])
        .await?;
        Ok(wt)
    }

    pub async fn remove_worktree(&self, node_id: &str) -> Result<()> {
        let wt = self.worktree_dir(node_id);
        if wt.exists() {
            self.git(&["worktree", "remove", "--force", wt.to_str().unwrap()])
                .await?;
        }
        Ok(())
    }

    /// Commit anything the agent left uncommitted in its worktree (leaves are
    /// told to commit, but a forgotten `git add` must not lose work).
    pub async fn commit_all(&self, dir: &Path, message: &str) -> Result<()> {
        self.git_in(dir, &["add", "-A"]).await?;
        let staged = self.git_in(dir, &["diff", "--cached", "--quiet"]).await;
        if staged.is_err() {
            self.git_in(dir, &["commit", "-m", message]).await?;
        }
        Ok(())
    }

    pub async fn has_commits(&self, node_id: &str, run_branch: &str) -> Result<bool> {
        let branch = Self::node_branch(node_id);
        let out = self
            .git(&["rev-list", "--count", &format!("{run_branch}..{branch}")])
            .await?;
        Ok(out.trim().parse::<u64>().unwrap_or(0) > 0)
    }

    /// Files a node's branch touches relative to the run branch.
    pub async fn changed_files(&self, node_id: &str, run_branch: &str) -> Result<Vec<String>> {
        let branch = Self::node_branch(node_id);
        let out = self
            .git(&["diff", "--name-only", &format!("{run_branch}...{branch}")])
            .await?;
        Ok(out.lines().map(str::to_owned).collect())
    }

    /// Serialized merge of a node branch into the run branch, inside the
    /// merge worktree. On conflict the worktree is LEFT CONFLICTED so the
    /// neutral Merger agent can resolve in place.
    pub async fn try_merge(&self, node_id: &str, run_branch: &str) -> Result<MergeOutcome> {
        if !self.has_commits(node_id, run_branch).await? {
            return Ok(MergeOutcome::NothingToMerge);
        }
        let md = self.merge_dir();
        let branch = Self::node_branch(node_id);
        let msg = format!("canopy: merge node {node_id}");
        let res = self
            .git_in(&md, &["merge", "--no-ff", "-m", &msg, &branch])
            .await;
        match res {
            Ok(_) => {
                let sha = self.git_in(&md, &["rev-parse", "HEAD"]).await?;
                Ok(MergeOutcome::Merged {
                    commit: sha.trim().to_owned(),
                })
            }
            Err(_) => {
                let details = self
                    .git_in(&md, &["diff", "--diff-filter=U", "--name-only"])
                    .await
                    .unwrap_or_default();
                if details.trim().is_empty() {
                    // Not a content conflict — abort and surface the error.
                    let _ = self.git_in(&md, &["merge", "--abort"]).await;
                    anyhow::bail!("merge of {branch} failed for a non-conflict reason");
                }
                Ok(MergeOutcome::Conflicted { details })
            }
        }
    }

    /// Conflicted hunks of the in-progress merge (context for the Merger).
    pub async fn conflict_hunks(&self) -> Result<String> {
        let md = self.merge_dir();
        let files = self
            .git_in(&md, &["diff", "--diff-filter=U", "--name-only"])
            .await?;
        let mut out = String::new();
        for f in files.lines() {
            out.push_str(&format!("--- {f} ---\n"));
            let content = tokio::fs::read_to_string(md.join(f))
                .await
                .unwrap_or_default();
            // Only the conflicted regions plus a little air, not whole files.
            let mut keep = false;
            for (i, line) in content.lines().enumerate() {
                if line.starts_with("<<<<<<<") {
                    keep = true;
                    out.push_str(&format!("@ line {}\n", i + 1));
                }
                if keep {
                    out.push_str(line);
                    out.push('\n');
                }
                if line.starts_with(">>>>>>>") {
                    keep = false;
                }
            }
        }
        Ok(out)
    }

    /// After the Merger ran: stage its resolution (git add clears unmerged
    /// index entries) and verify nothing conflicted remains — neither unmerged
    /// paths nor staged leftover conflict markers.
    pub async fn merge_resolved(&self) -> Result<bool> {
        let md = self.merge_dir();
        self.git_in(&md, &["add", "-A"]).await?;
        let unmerged = self
            .git_in(&md, &["diff", "--diff-filter=U", "--name-only"])
            .await?;
        if !unmerged.trim().is_empty() {
            return Ok(false);
        }
        let markers = Command::new("git")
            .args(["grep", "--cached", "-l", "^<<<<<<< "])
            .current_dir(&md)
            .stdin(std::process::Stdio::null())
            .output()
            .await?;
        // git grep exits 1 with empty stdout when nothing matches.
        Ok(String::from_utf8_lossy(&markers.stdout).trim().is_empty())
    }

    async fn merge_head_exists(&self, md: &Path) -> bool {
        self.git_in(md, &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"])
            .await
            .is_ok()
    }

    /// Commit a Merger resolution if it staged but didn't commit.
    pub async fn finalize_merge(&self, node_id: &str) -> Result<String> {
        let md = self.merge_dir();
        if self.merge_head_exists(&md).await {
            self.git_in(&md, &["add", "-A"]).await?;
            self.git_in(
                &md,
                &[
                    "commit",
                    "-m",
                    &format!("canopy: merge node {node_id} (resolved)"),
                ],
            )
            .await?;
        }
        let sha = self.git_in(&md, &["rev-parse", "HEAD"]).await?;
        Ok(sha.trim().to_owned())
    }

    pub async fn abort_merge(&self) -> Result<()> {
        let md = self.merge_dir();
        if self.merge_head_exists(&md).await {
            self.git_in(&md, &["merge", "--abort"]).await?;
        } else {
            self.git_in(&md, &["reset", "--hard", "HEAD"]).await?;
            self.git_in(&md, &["clean", "-fd"]).await?;
        }
        Ok(())
    }

    /// Undo the last merge commit on the run branch (verify failure, budget
    /// bounce). Only valid immediately after a merge — the queue guarantees.
    pub async fn revert_merge(&self, merge_commit: &str) -> Result<()> {
        let md = self.merge_dir();
        let head = self.git_in(&md, &["rev-parse", "HEAD"]).await?;
        anyhow::ensure!(
            head.trim() == merge_commit,
            "revert_merge: HEAD moved since the merge (queue invariant broken)"
        );
        self.git_in(&md, &["reset", "--hard", "HEAD^"]).await?;
        Ok(())
    }

    /// Diff introduced by one merge commit (review lens context).
    pub async fn merge_diff(&self, merge_commit: &str) -> Result<String> {
        self.git_in(
            &self.merge_dir(),
            &[
                "show",
                "--format=",
                "--patch",
                "-m",
                "--first-parent",
                merge_commit,
            ],
        )
        .await
    }

    /// Harness-authored change on the run branch (design docs, scaffold).
    pub async fn harness_commit(&self, message: &str) -> Result<()> {
        self.commit_all(&self.merge_dir(), message).await
    }

    /// Tracked files on the run branch (merge worktree).
    pub async fn ls_files(&self) -> Result<Vec<String>> {
        let out = self.git_in(&self.merge_dir(), &["ls-files"]).await?;
        Ok(out.lines().map(str::to_owned).collect())
    }

    /// Recover the merge commit of a node after a restart (its sha lives only
    /// in memory otherwise). Searches the run branch log by message.
    pub async fn find_merge_commit(&self, node_id: &str) -> Option<String> {
        let msg = format!("canopy: merge node {node_id}");
        self.git_in(
            &self.merge_dir(),
            &["log", "--grep", &msg, "--format=%H", "-n", "1"],
        )
        .await
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
    }

    /// Tracked source files over the line threshold (anti-megafile scan).
    /// design/, fieldguide/ and dotfiles are exempt.
    pub async fn megafile_scan(&self, threshold: usize) -> Result<Vec<(String, usize)>> {
        let md = self.merge_dir();
        let files = self.git_in(&md, &["ls-files"]).await?;
        let mut hits = Vec::new();
        for f in files.lines() {
            if f.starts_with("design/") || f.starts_with("fieldguide/") || f.starts_with('.') {
                continue;
            }
            if let Ok(content) = tokio::fs::read_to_string(md.join(f)).await {
                let lines = content.lines().count();
                if lines > threshold {
                    hits.push((f.to_owned(), lines));
                }
            }
        }
        Ok(hits)
    }
}
