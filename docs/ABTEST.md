# A/B: swarm harness vs solo agent — 2026-07-23

Same feature, four arms. **A** arms build with canopy (plan → parallel leaves → serialized
merges → review lens); **B** arms are one `claude -p` call with the same objective text.

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

¹ excluding a host-side daemon kill mid-run (see Crash recovery below).

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
