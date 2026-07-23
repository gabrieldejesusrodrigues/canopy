# Canopy — Design

Canopy is a Rust harness that runs an **agent swarm** over headless coding-agent CLIs
(`claude`, `codex`, `agy`), following the tree/trunk/leaf architecture described in
Cursor's [Agent swarm model economics](https://cursor.com/blog/agent-swarm-model-economics)
article — including all six coordination mechanisms it describes, adapted from their
custom-VCS scale to single-machine git scale.

The economics thesis being implemented: *few moments in a large task truly demand
frontier intelligence*. Expensive models plan (trunks); cheap models execute explicit
instructions (leaves). In the article this produced equivalent quality at ~8× lower cost.

## 1. Topology: board-as-queue with a central scheduler

The task tree lives on a **tracker board** (Linear or local SQLite, behind one trait).
The board *is* the tree and the queue:

- run = Linear project (or `runs` row in SQLite)
- tree node = issue; `parent_id` encodes the tree
- node lifecycle = issue workflow state

The harness is a single daemon (`canopy run`). A scheduler claims ready issues and
spawns **stateless, one-shot agent processes** with a role contract. Agents never spawn
other agents; **planners declare decomposition as structured output** and the harness
materializes it on the board. This gives:

- global budget/concurrency control in one place,
- crash recovery for free (all state on the board),
- the article's coordination mechanisms as *deterministic enforcement*, not prompt hopes.

## 2. Domain model

### Node kinds
- `Plan` (trunk): decomposes an objective into children. Never implements — its context
  never accumulates low-level detail (per the article).
- `Execute` (leaf): implements exactly one explicit work unit in an isolated git worktree.

### Roles (agent invocation types)
`Planner`, `Executor`, `Merger` (neutral conflict resolver), `Reconciler` (design doc
merger), `Decomposer` (megafile splitter), `Reviewer` (one per lens). The last four are
*mechanism roles*: the scheduler triggers them on events, they are not tree nodes on the
board — they appear as comments/labels on the issues they touch.

### Node states
```
Ready ──claim──▶ Running ──┬─(plan)────▶ Decomposed ──children all Done──▶ Done
                           ├─(execute)─▶ NeedsMerge ─▶ Merging ─▶ InReview ─▶ Done
                           ├───────────▶ Blocked  (dependency / flagged file / budget)
                           └───────────▶ Failed   (after max_attempts)
```
Claims are **atomic** (SQLite transaction; Linear state-compare on update) and carry a
lease (`claimed_at + lease_secs`); expired leases return to `Ready` — no lost or
double-claimed work after a crash.

A `Decomposed` planner node is **revisited** (re-invoked with its children's outcomes)
when a child fails permanently or a reviewer escalates — the trunk owns its subtree.

## 3. Scheduler loop

```
tick:
  1. settle: expire leases, unblock nodes whose deps completed, check budget
  2. mechanisms first: pending merges → merge queue; conflicts → Merger;
     design-doc conflicts → Reconciler; flagged megafiles → Decomposer;
     merged-but-unreviewed → Reviewers
  3. claim ready nodes up to max_parallel (leaves) / max_parallel_planners
  4. spawn agent per node: worktree + prompt(role contract ⊕ fieldguide/index.md ⊕
     node spec ⊕ referenced design docs) → parse trailing JSON block → apply
```

Mechanism work is scheduled *before* new tree work: coordination debt is paid first.

### Structured output contract
Every role prompt ends with a required JSON schema. The agent's final message must end
with a fenced ```json block; the harness parses it (retrying the invocation once with a
"your output did not parse" nudge). Planner output:

```json
{ "children": [ { "title": "...", "kind": "plan|execute", "spec": "explicit instructions",
    "depends_on": [0], "agent": {"cli": "codex", "model": "..."}? } ],
  "design_decisions": [ { "id": "DD-004", "title": "...", "content": "markdown" } ] }
```

Executor output: `{ "status": "done|blocked|needs_split", "summary", "flagged_files": [],
"breaks": [ { "file", "reason" } ] }` — code/fieldguide/design edits happen as real file
edits + commits in the worktree, not in JSON.

## 4. Git strategy (replacing the custom VCS)

The article's custom VCS exists for 1,000 commits/second across hundreds of agents. At
single-machine scale, its *architectural role* — one point where every change passes and
conflicts surface first — is played by a **serialized merge queue**:

- each Execute node gets branch `canopy/<node-id>` in its own worktree under
  `.canopy/worktrees/`,
- merges into the run branch (`canopy/run-<id>`) happen strictly one at a time,
- post-merge pipeline (§5) runs before the next merge is admitted,
- the run branch is merged/PR'd to the user's main branch only at the end, by the human.

## 5. The six article mechanisms, enforced

Full mapping with triggers/thresholds in [MECHANISMS.md](MECHANISMS.md). Summary:

1. **Shared design docs + compile-checked refs + Reconciler.** Decisions live in
   `design/DD-*.md`. Code depending on a decision carries a `canopy-design: DD-xxx`
   comment; the post-merge check fails if a ref points to a missing/superseded doc
   (language-agnostic implementation of the article's "compile-checked reference").
   Two planners writing conflicting docs on one topic → Reconciler merges them; nodes
   referencing the losing decision get fix-issues created from the broken refs.
2. **Neutral merge agent.** Merge conflict → `Merger` role with both diffs, both node
   specs, and the design docs; goal stated in its contract: *impartial and efficient*,
   no loyalty to either side.
3. **Anti-megafile.** Executors flag bloated files (`flagged_files`) and the harness
   also hard-scans line counts post-merge. Flagged file → merges touching it are
   rejected (commit block) → `Decomposer` node splits it → block lifts.
4. **Anti-ossification.** Executors may patch outside their scope with a
   `canopy-break: <reason>` comment (`breaks` in output). The verify command propagates
   the breakage; failing dependents become new Execute nodes. Core code stays evolvable.
5. **Field Guide (stigmergy).** `fieldguide/` is agent-owned; `fieldguide/index.md` is
   injected into **every** agent prompt. Only constraint, per the article: a line
   budget — a merge that leaves `index.md` over budget is bounced back for curation.
6. **Layered review lenses.** Post-merge, N uncorrelated reviewers: *transcript* lens
   (full executor transcript), *output* lens (diff only), *codebase* lens (repo only) —
   ideally on different CLIs/models (config). High-severity findings block `Done` and
   spawn fix nodes; low-severity become backlog nodes. Reviewing is cheap relative to
   the work audited, so lenses default on.

## 6. Model routing

```toml
[routing]
mode = "static"            # or "planner-routed"

[routing.static]
planner    = { cli = "claude", model = "opus" }
executor   = { cli = "codex",  model = "gpt-5.1-codex-mini" }
merger     = { cli = "claude", model = "sonnet" }
reconciler = { cli = "claude", model = "opus" }
decomposer = { cli = "codex",  model = "gpt-5.1-codex-mini" }
reviewers  = [ { cli = "agy", model = "gemini-3-flash", lens = "output" },
               { cli = "claude", model = "haiku", lens = "codebase" } ]

[[routing.allowlist]]       # used when mode = "planner-routed"
cli = "codex"; model = "gpt-5.1-codex-mini"; tier = "cheap"
good_for = "well-specified implementation, tests, mechanical refactors"
```

In `planner-routed` mode the planner contract includes the allowlist with `good_for`
guidance and it assigns `agent` per child; the harness validates against the allowlist
and falls back to the static default on violation. Any of the three CLIs can serve any
role — trunk/leaf is a *role*, not a vendor.

## 7. Tracker abstraction

```rust
trait Tracker {
    fn create_run(...); fn create_node(...); fn claim(node, lease) -> bool;
    fn transition(node, from, to) -> bool;   // compare-and-set
    fn comment(node, body); fn ready_nodes(run) -> Vec<Node>; ...
}
```
- **SQLite** (`.canopy/canopy.db`, rusqlite): default, offline, no rate limits;
  transactions make claims trivially atomic.
- **Linear** (GraphQL, `LINEAR_API_KEY`): run = project, node = issue (sub-issues =
  tree), states mapped to a team's workflow, roles/lenses as labels, agent summaries as
  comments. Claim atomicity is emulated with compare-and-set on state + a local claim
  registry (the daemon is the only writer at claim granularity, so lost-update risk is
  confined to human edits, which the settle step re-reads).

The board is *operable*: moving an issue to a canceled state in Linear cancels the node;
editing its description before `Ready` edits the spec.

## 8. Economics: ledger, budgets, report

Every invocation records: node, role, cli, model, input/output/cached tokens, cost
(claude reports `total_cost_usd`; codex/agy report tokens → priced via a config price
table), duration, attempt. Ledger is always local SQLite (even with Linear tracker).

Hard caps: `max_usd` (run pauses at cap), `max_parallel`, `max_attempts`,
`max_tree_depth`, per-role `max_turns`. `canopy report` prints the article-style
analysis: share of tokens vs share of cost per role and per model.

## 9. Verification

- Post-merge: user-configured `verify` command (build/tests) + design-ref check +
  megafile scan. Verify failure → merge reverted, node requeued with the failure log.
- Review lenses (§5.6) on top — uncorrelated errors sum, per the article.
- Harness's own test anchor: an end-to-end smoke test with a deterministic **stub
  AgentCli** and the SQLite tracker exercises plan → execute → merge-conflict → Merger →
  review → done without spending tokens.

## 10. Crate layout

Single binary crate (`canopy`):

```
src/main.rs        clap: run|status|pause|resume|report|init
src/config.rs      canopy.toml (routing, budgets, verify, thresholds, tracker)
src/model.rs       NodeKind, NodeState, Role, Node, Run
src/tracker/       mod.rs (trait) + sqlite.rs + linear.rs
src/agent/         mod.rs (AgentCli trait) + claude.rs + codex.rs + agy.rs + stub.rs
src/gitops.rs      worktrees, branches, merge plumbing, rerere, line-count scan
src/scheduler.rs   the loop (§3): claiming, applying, reviews, cascade/replan
src/mergelane.rs   the serialized merge job: resolution ladder (rerere →
                   triage → merger), post-merge gates, escalation
src/mechanisms/    designdocs.rs, megafile.rs, fieldguide.rs
src/ledger.rs      costs + report
src/prompt.rs      contract assembly (role md ⊕ field guide ⊕ spec ⊕ design docs)
prompts/*.md       role contracts (embedded via include_str!)
src/e2e.rs         the stub-agent smoke test (§9)
```

First run against a target repo scaffolds `design/`, `fieldguide/index.md` and
`.canopy/` (gitignored: worktrees, db, logs).

## Non-goals (v1)

- Custom VCS, 1,000 commits/sec — out of scale, mechanism preserved by the merge queue.
- Multi-machine distribution; webhooks (we poll).
- Automatic merge of the run branch into the user's main — a human does that.
