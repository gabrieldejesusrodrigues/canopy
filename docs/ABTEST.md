# A/B: swarm harness vs solo agent — 2026-07-23

Same feature, six arms across three rounds. **A** arms build with canopy (plan → parallel
leaves → serialized merges → review lens); **B** arms are one `claude -p` call with the
same objective text. Round 3 ran AFTER the round-1-driven fixes landed (enforced disjoint
`files[]` ownership, merger triage ladder, contention sensor, `agt min` metric).

**Feature ("explog")**: expense tracker in stdlib Python — `errors.py` (ValidationError /
StorageError), `storage.py` (atomic JSON writes, StorageError on corrupt file), `core.py`
(add/list/summary, strict validation), `cli.py` (argparse subcommands, exit 2 on bad
input + stderr message), unittest suites, discoverable via `python3 -m unittest discover`.

| arm | setup | cost | wall time | tests | battery |
|---|---|---|---|---|---|
| A1 | canopy: sonnet planner + codex gpt-5.4-mini leaves + agy review | **$1.85** | ~25 min | 20 pass | ✅ all |
| B1 | sonnet solo | **$0.44** | 1.5 min | 35 pass | ✅ all |
| A2 | canopy: opus 4.8 planner + sonnet leaves + agy review | **$1.34** | ~20 min¹ | 21 pass | ✅ all |
| B2 | opus 4.8 solo | **$0.43** | 1.4 min | 32 pass | ✅ all |
| A3² | canopy, same as A2 + all round-1 fixes + haiku merger triage | **$1.66** | ~11 min | **37 pass** | ✅ all |
| B3² | opus 4.8 solo | **$0.48** | 1.6 min | 29 pass | ✅ all |
| A4³ | canopy (2 lenses), 15-file objective | **$3.29** | ~12 min | **83 pass** | ✅ all |
| B4³ | opus 4.8 solo, same objective | **$0.80** | 2.8 min | 66 pass | ✅ all |

¹ excluding a host-side daemon kill mid-run (see Crash recovery below).
² round 3, post-fixes harness (commits bd9cd30/0026f68/9a9413a).
³ round 4, width probe ("taskflow", 8 modules + suites — see its section below).

**Battery** (identical, scripted): valid adds; bad date / negative amount / empty category
→ exit 2 + stderr; summary sorted desc with 2 decimals; `--month` filters; corrupt JSON →
exit 2; full unittest suite. **Every arm passed everything.** Structure was also
equivalent (four modules, shared exception classes, atomic temp-file+rename writes).

## Quality verdict

Functional quality is a tie — at this scale a single frontier agent one-shots the spec.
The differences are in the *artifacts*:

- **Solo arms wrote more tests** (35/32 vs 20/21) and were 13× faster wall-clock. One
  context = free global consistency; nothing to coordinate.
- **Harness arms left a process trail the solo arms can't**: 3 design docs (`DD-1
  shared-exception-classes`, `DD-2 expense-record-shape`, `DD-3 cli-output-formats`)
  that the leaves' summaries actually cite; per-leaf granular commits
  (`canopy: add storage.py with atomic JSON load/save`); a reviewed merge per unit; a
  board a human can watch and cancel. In A2 the review lens came back clean — in A1 it
  caught real scope drift (see below) and auto-created the fix nodes that repaired it.

## Cost verdict

Solo won both rounds: 4.2× (round 1) and 3.1× (round 2). **This inverts the article's
economics, and the inversion is the finding**: coordination cost (planner + merger +
review) is ~fixed per decomposition, while its payoff scales with tree width and task
length. A 4-6 leaf toy task sits below the crossover. The article's ~8× savings needs
tasks too large for one context — wide trees, long horizons — plus a cheap-leaf tier.
Even here, the trunk/leaf shape held: A2 leaves owned 82.7% of dollars at sonnet prices
while the opus trunk cost 17.3% for one call; putting opus everywhere in A2's shape
would roughly triple its executor bill.

## What round 1 taught (fixes shipped in bd9cd30)

A1's merger burned **$0.90 = 48.9% of the run** resolving two conflicts. Root cause:
impl leaves also wrote test files a sibling owned — the review lens flagged exactly this
("undeclared scope expansion") and the fix nodes + merges cost another cycle. One drift,
paid for three times (conflict, review, fix). Shipped:

1. **Planner contract**: every file belongs to exactly one child; specs name the files
   each child owns; readers of sibling artifacts declare `depends_on`.
2. **Executor contract**: create only the files the spec names — no drive-by tests.
3. **`canopy report`**: new `agt min` column (agent wall-time per role/model).

Round 2 (planned by opus with proper `depends_on` sequencing, before the contract fix
even landed): **zero conflicts, zero merger spend, clean review**. The failure mode and
its fix are both visible in the numbers.

## Round 3 — the fixes, measured

Round 3 reran the round-2 matchup on the post-fix harness (enforced `files[]`
ownership, rerere + haiku triage ladder, contention sensor, `agt min` column).

- **The ownership contract changed the planner's decomposition shape.** Every leaf spec
  now reads "Files you own: storage.py, test_storage.py. Do NOT edit …" — and the
  planner paired each module WITH its tests under one owner instead of round 2's
  separate test node. Disjoint by construction: **zero conflicts for the second
  consecutive round**, the triage tier configured but never needed. The best merger
  spend is none.
- **First round where the harness arm out-tested the solo arm: 37 vs 29.** Pairing each
  module with its own tests in one leaf produced the highest test count of all six arms
  (A1 20 → A2 21 → A3 37). The contract written to prevent conflicts also bought
  per-module test depth.
- **The richer contract costs planner tokens**: the opus planning call went from $0.23
  (A2) to $0.54 (A3) — it now emits ownership lists and design docs with more care.
  Worth it: that $0.31 bought the conflict-free shape and the test-depth win.
- **`agt min` makes the latency overhead visible**: A3 spent 4.7 agent-minutes
  (2.5 exec + 1.9 plan + 0.3 review) vs B3's 1.6 — plus queue serialization on top
  (~11 min wall). The cost verdict is unchanged: solo remains ~3.5× cheaper and ~7×
  faster at single-feature scale; the harness arms keep the artifacts (3 design docs
  cited by leaves, per-leaf commits, reviewed merges) and now beat solo on test depth.

## Round 4 — width probe ("taskflow", 15 files)

A 2× wider objective (8 interdependent modules + per-module unittest suites, precise CLI
contract) to probe the crossover. A4: opus planner + sonnet leaves (max_parallel 3),
haiku triage, TWO lenses (agy output + haiku codebase). B4: opus solo.

| arm | cost | wall | tests | battery (23 checks) |
|---|---|---|---|---|
| A4 | **$3.29** | ~12 min | **83 pass** | ✅ identical, all pass |
| B4 | **$0.80** | 2.8 min | 66 pass | ✅ identical, all pass |

Run shape (A4): 1 opus plan → 5 sonnet leaves (3 in parallel, module-pairs + own tests,
explicit disjoint ownership) → 5 serialized merges, **zero conflicts, zero retries,
zero nudges** → 10 lens invocations → 3 low findings → root Done. 4 design docs, cited
from leaf commits.

### Analysis: cost

- **The crossover is about context saturation, not width.** Solo scaled sublinearly
  ($0.48 → $0.80 for ~2× the modules) because one context amortizes all reading; the
  harness scaled linearly with leaves ($1.91 executors). At 15 files solo is still
  comfortable — the gap (4.1×) didn't close. Next probe needs ~50+ files or long-horizon
  iterative work where a single context degrades. Cost structure confirms the article's
  scaling shape though: planner O(1) (22.8%), leaves O(width) (58%).
- **Found: the codebase lens is O(width × repo) — a quadratic tax.** The haiku codebase
  lens cost $0.63 (19% of the run, 48.5% of billed tokens): each landing re-read the
  repo (one call: 807k cached-read + 19k output tokens, $0.27) and the repo grows with
  every landing. *Fix applied*: the lens contract now scopes reading to `## FILES` +
  direct imports, and reviewer invocations are capped at 20 turns. Projected: roughly
  halves lens cost; re-measure next round.
- The new `cached` report column made this visible: executors 1.95M cached reads,
  reviewers 1.59M — the `tokens` column alone hid 97% of the real context volume.

### Analysis: quality

- **Harness out-tested solo again: 83 vs 66** (second consecutive round since the
  ownership contract). Same functional score on the 23-check battery — both arms
  implemented the CLI contract exactly (report lines, exit codes, filter composition,
  archived-project guard, atomic writes).
- **The agy output lens has produced zero findings across all rounds (~17 reviewed
  landings)** while the haiku codebase lens caught 3 real (low) gaps in the same run.
  Its output is well-formed (valid JSON, empty findings) — it works, it just doesn't
  see much at Flash-Low tier with only spec+diff. Recommendation: make the second lens
  a transcript lens on a cheap-but-stronger model, or accept it as a cheap tiebreaker.
  Uncorrelated lenses only sum if each actually finds things.
- The 3 low findings were all "missing canopy-design refs" — the lens catching what the
  merge gate can't (the gate checks refs that exist; the lens notices refs that are
  missing). Leaves cite DDs in commit messages but forget source comments; contract
  emphasis may help, low stakes.

### Analysis: bugs found

- **None in the harness this round**: no retries, no nudge respawns, no lease expiries,
  no conflicts; all terminal states correct; provenance clean. The round-1 bug class
  (contract-driven false "blocked") did not reappear.
- Two operational papercuts, both fixed in this pass: the release binary used for the
  run predated the `cached`/wall-time additions (report re-run with the new binary),
  and reviewer turn budgets inherited the executor's 50 (now capped at 20).

## Crash recovery, validated in anger

The A2 daemon was killed by the host mid-run (leaf claimed, work half-done). Twenty-five
minutes later: `canopy run --resume 484b2782` → lease expiry re-queued the orphaned
leaf, the three landed nodes stayed done, the run completed to root Done. Total extra
cost: one re-run leaf invocation. "All state on the board" held.

## Known metric caveat

Cross-CLI token counts are not comparable: claude reports non-cached input only
(A2 "17k tokens" ≈ 550k with cache reads), codex reports the full window. Compare
dollars and `agt min`, not raw token counts, across vendors.

## When to use which

- **Solo agent**: single well-specified feature that fits one context. Cheaper, faster,
  more tests per dollar.
- **canopy**: work too big for one context, needs parallel throughput, needs the audit
  trail (design docs, reviewed merges, per-leaf commits), or runs long enough that
  crash-resume and human cancellation matter. Route trunks smart, leaves cheap — the
  economics improve with exactly the scale that makes solo infeasible.
