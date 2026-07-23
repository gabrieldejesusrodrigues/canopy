# Codex pair-review triage — 2026-07-22

30 findings from the Codex review, triaged with verification before implementation.
Status: ACCEPT (will fix), ACCEPT-DOC (docs were wrong, code is the intended design),
PARTIAL (fix a narrower version), REJECT (not a real bug — reason given).

## Scheduler

| # | Finding | Verdict | Fix |
|---|---|---|---|
| 1 | Failed prerequisite leaves dependents permanently Blocked → run never terminates | **ACCEPT (critical)** | On permanent failure, propagate: Blocked nodes depending on the failed node become Failed too (recursive), so cascade/replan sees them |
| 2 | Error after successful merge leaves unverified commit on run branch; retry sees "Empty" → Done unreviewed | **ACCEPT (critical)** | Wrap post-merge gates; on any error revert the merge commit before returning MergeReport::Error |
| 3 | Failed children poison replans; empty replan marks parent Done | **PARTIAL** | New terminal state `Superseded`: replan marks old Failed children superseded (settled, not failed). Empty replan ⇒ Done is *intended* (planner accepts subtree) — documented in planner contract |
| 4 | Crash during Merging strands the node | **ACCEPT** | Startup recovery: abort any in-progress merge, Merging → NeedsMerge |
| 5 | Executor-reported megafile flag gates the reporter's own merge | **ACCEPT** | BlockList entries carry `reporter`; gate exempts owner (decomposer) and reporter |
| 6 | Decomposer landing Empty (no commits) never lifts its block | **ACCEPT** | Lift blocks on Empty and on permanent failure of the owner (re-scan re-flags if still fat) |
| 7 | MergerOutput.resolved never parsed | **ACCEPT** | Parse it; explicit `resolved:false` counts as unresolved even if git looks clean. Git state remains the positive gate |
| 8 | Forward/out-of-range depends_on silently dropped → premature Ready | **ACCEPT** | Validate indices (must reference earlier siblings); invalid ⇒ structured-output failure path (nudge retry, then Failed) |
| 9 | Replan cap process-local, resets on resume | **REJECT (v1 limitation)** | Documented. Worst case: extra replans after a manual resume — bounded by the human doing the resuming |

## Races

| # | Finding | Verdict | Fix |
|---|---|---|---|
| 10 | Lease expiry requeues node while its job is alive; worktree yanked under live process | **ACCEPT (critical)** | In-memory `inflight` set: settle never expires and claim never re-claims nodes in flight in this process; config validation `lease_secs > 2×agent_timeout + 300` (nudge retry headroom) |
| 11 | Concurrent reviewer lenses share transcript/.last.txt paths; clobber executor transcript | **ACCEPT** | Role/lens-suffixed transcript paths for mechanism roles; tree jobs keep the plain path |
| 12 | Reviews read merge worktree at moving HEAD | **ACCEPT** | Reviewers run in a detached snapshot worktree pinned at the node's merge commit, removed after review |
| 13 | Planner/reviewer/reconciler run inside the merge checkout | **ACCEPT** | Planner/reconciler also get detached snapshot worktrees; only the Merger works in the merge checkout (it needs the conflicted index) |
| 14 | Two daemons could share one merge worktree | **ACCEPT (cheap)** | `.canopy/daemon.pid` lock: refuse to start if another live pid holds the run |
| 15 | Deferred apply order nondeterministic | **REJECT** | Order-dependence between conflicting decisions is exactly what the Reconciler mechanism resolves |

## Adapters

| # | Finding | Verdict | Fix |
|---|---|---|---|
| 16 | Empty/malformed codex JSONL treated as success | **ACCEPT** | Require a `turn.completed` event (or non-empty final message) for exit_ok |
| 17 | stub.rs UTF-8 boundary panic in error path | **ACCEPT** | Char-boundary-safe truncation |
| — | codex stale `-o` file on nudge retry (same attempt) | **ACCEPT** | Delete the last-message file before spawn |
| — | timeout kills only the direct CLI process, orphaning its children | **ACCEPT** | Spawn in own process group (setsid); kill the group on timeout |
| — | codex usage: output + reasoning may double-count | **ACCEPT (conservative)** | Record `output_tokens` only; reasoning tokens noted separately in transcript |

## Linear tracker

| # | Finding | Verdict | Fix |
|---|---|---|---|
| 18 | try_claim returns Node with parent_id None (issueUpdate response lacks parent) | **ACCEPT (critical)** | Build the returned Node from the pre-read node + new state, not from the update response |
| 19 | try_claim is TOCTOU, no CAS | **ACCEPT-DOC** | Public Linear API has no CAS (verified in research). Single-daemon lock (#14) + single-writer design is the mitigation; documented in DESIGN/README |
| 20 | Metadata read-modify-write can clobber concurrent human edits | **ACCEPT-DOC** | Same class as 19; settle re-reads board state each tick — human edits between read and write of one mutation are a documented edge |
| — | init_run byte-slices the objective for the project name (UTF-8 panic) | **ACCEPT** | Char-safe truncation |

## Mechanism fidelity

| # | Finding | Verdict | Fix |
|---|---|---|---|
| 21 | Design-ref gate scans only touched files | **ACCEPT-DOC** | Deliberate: docs never disappear, so violations in untouched files can only come from supersede events — and apply_reconcile already creates fix nodes for exactly those. MECHANISMS.md now says this precisely |
| 22 | Reconciler failure leaves both docs active; subtrees not paused | **PARTIAL** | Failure ⇒ incumbent wins: incoming doc marked superseded + comment. "Pause subtrees" replaced in docs with the real mechanism (merge-time ref gate bounces stale citations) |
| 23 | Reconciler lacks both planners' specs | **ACCEPT** | Fetch both author nodes' specs into the CONFLICT context |
| 24 | Duplicate active files for one design id | **ACCEPT** | write_decision reuses the existing `DD-n-*.md` path for a known id; planner-loop snapshot updated per decision |
| 25 | Merger lacks the other side's context | **PARTIAL** | Conflict context gains the run branch's recent merge subjects (git log -5). The "other side" of a serialized queue is the accumulated branch, not one node |
| 26 | Breaks: verify failure doesn't become fix nodes | **ACCEPT (core fidelity)** | If the node declared breaks and verify fails: land the merge anyway and create fix nodes carrying the verify tail + break reasons (the article's "compiler propagates the change"). No declared breaks ⇒ bounce as today |
| 27 | Failed review lens treated as clean | **ACCEPT** | Lens errors/parse failures are counted and commented; only successful lenses contribute findings; all-lenses-failed is flagged loudly on the node |
| 28 | Decomposer role/prompt unreachable | **ACCEPT** | `role_hint` on nodes (sqlite column + linear metadata); decomposer nodes get Role::Decomposer + decomposer.md |
| 29 | Codebase lens gets no touched-file list | **ACCEPT** | `## FILES` section listing the node's touched files, referenced by the contract |
| 30 | Megafile-gated node busy-loops NeedsMerge | **ACCEPT** | pump_merges consults the in-memory blocklist and skips gated candidates without state churn (picks the next candidate instead) |

Also accepted from the same pass: budget cap now gates the merge lane and review
spawning too (previously only tree claims), and severity-low review findings are
recorded as comments (unchanged) — noted here for completeness.

## Second pass: ultracode review workflow (7 finder lenses + adversarial verify)

10 confirmed findings, overlapping codex #1/#3/#4/#5/#6/#8/#10/#12/#30 and the budget
gap above, plus one new: **cascade was event-driven only** — a crash between a child's
terminal write and the parent update (or a human settling issues on the board) left the
parent Decomposed forever on resume. Fixed with a board-state cascade sweep every tick.
Two finder claims that lost their verifiers to session limits were verified by hand and
confirmed: **ensure_run_branch reused a stale merge worktree still checked out on a
previous run's branch** (new runs would merge onto the old branch), and
**trailing_json truncated valid JSON containing ``` inside string values** (any planner
child spec embedding a code fence). Both fixed.

**Status: the entire batch above is implemented.** 38/38 tests pass (including the
stub-agent e2e), clippy clean of new warnings.
