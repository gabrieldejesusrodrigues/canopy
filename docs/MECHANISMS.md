# Article mechanisms → Canopy implementation, 1:1

Source: [Agent swarm model economics](https://cursor.com/blog/agent-swarm-model-economics) (Cursor).
Each section quotes the problem the article reports, the solution it describes, and how
Canopy implements that solution deterministically (harness-enforced, not prompt-hoped).

---

## 1. Design divergence → shared design docs with checked references

**Article problem.** Multiple planners independently implement the same concept
differently across sections of the codebase.

**Article solution.** "Agents record decisions in shared design docs. Code that depends
on a decision carries a compile-checked reference back to its doc."

**Canopy.**
- `design/DD-<n>-<slug>.md` in the target repo, one decision per file, YAML frontmatter:
  `id`, `title`, `topics: [slugs]`, `status: active|superseded`, `author_node`.
- Planners emit `design_decisions` in structured output; the harness writes the files
  (planners never hold the pen directly — that is what makes conflicts *visible* as data
  instead of racing writes).
- Any code that depends on a decision carries a comment `canopy-design: DD-<n>` on the
  relevant item. Post-merge, the **design-ref check** greps all refs and fails the merge
  if a ref targets a missing or `superseded` doc. This is the article's "compile-checked
  reference" implemented language-agnostically (a grep gate on the only path into the
  run branch is equivalent in force to a compile error at our scale).

## 2. Planner contention → Reconciler

**Article problem.** Planners that don't know about each other contradict each other;
shared files ping-pong between conflicting intents.

**Article solution.** "When planners unknowingly contradict each other, a reconciler
merges the docs and the references propagate the resolution downstream."

**Canopy trigger.** On applying planner output, if a new decision's `topics` intersect
an existing *active* doc from a different `author_node` — or two decisions land on the
same `id` — the scheduler pauses child creation for both subtrees and spawns a
**Reconciler** (smart-model role). It receives both docs, both planners' specs, and
must output a single merged doc (one `id` survives; the other is marked `superseded`).

**Propagation downstream.** The design-ref check now fails for every file referencing
the superseded doc; each failing ref becomes an auto-created Execute node ("update to
DD-<survivor>"), which is exactly "the references propagate the resolution downstream".

## 3. Merge conflicts → neutral third-party Merger

**Article problem.** Leaves constantly collide on the same files and cannot absorb
enough context to resolve conflicts themselves.

**Article solution.** "A neutral third-party agent intervenes on merge conflicts and
resolves them on behalf of all parties. Its only goal is to be impartial and efficient."

**Canopy.** Merges into the run branch are strictly serialized (§4 of DESIGN.md). On
`git merge` conflict the harness spawns a **Merger** in the merge worktree with: the
conflicted hunks, both node specs, both summaries, and referenced design docs. Its
contract states impartiality explicitly and forbids expanding scope beyond resolving
the conflict. If the Merger fails twice, the younger branch's node is requeued with the
merged state as its new base (rebase-and-retry), which is what a human merge queue does.

## 4. Megafiles → flag, block, decompose

**Article problem.** Hot files grow without bound and become coordination bottlenecks.

**Article solution.** "Worker agents flag bloated files. Once flagged, we block new
commits and an outside agent decomposes the overgrown file into smaller modules."

**Canopy.** Two flag paths: executors return `flagged_files`, and the harness hard-scans
line counts post-merge (`megafile_lines` threshold, default 1000). A flagged file enters
the **block list**: any queued merge whose diff touches it is rejected and its node
parked `Blocked`. A **Decomposer** node is created for the file (cheap model, explicit
split instructions); when its merge lands, the block lifts and parked nodes requeue.

## 5. Ossification → permitted breaks

**Article problem.** Agents treat existing core code as untouchable, so architecture
fossilizes around early mistakes.

**Article solution.** "An agent that judges a core change worthwhile can make a focused
patch outside its scope and leave a comment explaining why." The compiler propagates the
change; dependents fail and get fixed.

**Canopy.** Executor contracts explicitly grant this right. Out-of-scope patches must
carry `canopy-break: <reason>` at the change site and be listed in the `breaks` output
field (a break without both is a review finding). The post-merge `verify` command plays
the compiler's role: anything the break broke fails verify, and each failure becomes a
fix node carrying the break's reason as context.

## 6. Error accumulation → layered review lenses

**Article problem.** In long-running swarms small errors compound; no single reviewer
catches everything.

**Article solution.** Reviewers with uncorrelated views — "full transcript, or only its
output, or nothing but the codebase", on "different models, with different training and
a different personality"; uncorrelated lenses sum. "Reviewing is far cheaper than the
work being audited."

**Canopy.** After a node's merge, each configured lens runs:

| lens | context given |
|---|---|
| `transcript` | node spec + full agent transcript + diff |
| `output` | node spec + diff only |
| `codebase` | repo tree + touched files at HEAD, no history |

Lenses are configured with *different CLIs/models on purpose* (decorrelation across
vendors, not just prompts). Findings come back as structured JSON with severity;
`high` blocks the node's `Done` transition and spawns a fix node, `low` lands as a
backlog node. Reviews use cheap models by default — the article's point is that many
cheap uncorrelated lenses beat one expensive one.

## 7. Field Guide → stigmergy

**Article problem.** Model weights are frozen; every agent rediscovers the same
environment quirks.

**Article solution.** "A folder owned entirely by the agents, whose index.md is
automatically injected into every agent at start. It is the agents' job to curate what
goes into the guide and their only constraint is a line budget."

**Canopy.** `fieldguide/` in the target repo; `fieldguide/index.md` is prepended to
**every** role prompt (all roles, including mechanism roles). Agents edit it as normal
files in their worktree. The single constraint is enforced exactly as stated: if a merge
leaves `index.md` over `fieldguide_line_budget` (default 200), the merge bounces with
"over line budget — curate before adding". Nothing else about the folder is policed.

---

## Explicitly rescaled (not skipped)

| Article | Why it exists there | Canopy equivalent |
|---|---|---|
| Custom VCS (1,000 commits/sec) | hundreds of concurrent agents; git's locks melt | serialized merge queue on git worktrees — same architectural role: the single point every change passes through, where conflicts surface first |
| Thousands of leaves | datacenter | `max_parallel` on one machine; the tree shape, roles and mechanisms are scale-free |
