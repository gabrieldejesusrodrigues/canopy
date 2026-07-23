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
| A5⁴ | canopy swarm: opus trunk + sonnet leaves | **$3.68** | 13.7 min | 85 pass | ✅ all |
| B5⁴ | canopy swarm: opus everywhere | **$3.50** | 12.3 min | **95 pass** | ✅ all |

¹ excluding a host-side daemon kill mid-run (see Crash recovery below).
² round 3, post-fixes harness (commits bd9cd30/0026f68/9a9413a).
³ round 4, width probe ("taskflow", 8 modules + suites — see its section below).
⁴ round 5, swarm-vs-swarm routing comparison (the article's own experiment — see below).

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

## Round 5 — the article's own comparison: routed vs frontier-everywhere

Both arms are canopy swarms on the taskflow objective; the ONLY config delta is the
executor model. This is Cursor's $10,565-vs-$1,339 experiment at bonsai scale.

| arm | leaves | cost | wall | tests | battery |
|---|---|---|---|---|---|
| A5: opus trunk + **sonnet** leaves | 7 | **$3.68** | 13.7 min | 85 | ✅ 23/23 |
| B5: **opus everywhere** | 5 | **$3.50** | 12.3 min | 95 | ✅ 23/23 |

**The routed arm saved nothing — it cost 5% more.** Sonnet leaves ARE cheaper per leaf
($0.33 vs $0.51, −36%), but the planner's decomposition varied (7 leaves vs 5), and each
extra leaf buys a cold start, a merge, and two lens landings (14 vs 10 review calls,
$0.87 vs $0.56). At this scale **tree shape dominates leaf price**; the article's ~8×
needs leaf work volume that dwarfs coordination overhead. n=1 per arm — decomposition
nondeterminism could flip a 5% delta either way; the structural conclusion (shape >
rate at small width) is the robust part.

### Deep source review (not just tests)

Read every module of both arms side by side; differences verified with runtime probes:

| probe | A5 sonnet | B5 opus |
|---|---|---|
| `task list --due-before 2026-99-99` | exit 0, silently wrong | **exit 2** ✓ |
| `report --today garbage` | exit 0, silently wrong | **exit 2** ✓ |
| state file = valid JSON, not a dict | **StorageError, exit 2** ✓ | uncaught TypeError, exit 1 |
| `--status bogus` as list filter | silent empty | silent empty (both weak) |

**Opus leaves wrote visibly higher-craft code**: contract-grade docstrings on every
module and function; canonical date parsing (round-trip check) reused for *query inputs*
(hence the two probe wins); a shared `validate_status` helper where sonnet duplicated
the check inline; ids allocated only after validation (sonnet mutates `next_id` before
validating — in-memory-state-on-failure smell); defensive copies from `list_projects`;
exception chaining and `BaseException` temp-file cleanup; `--help` text on every
subcommand. Sonnet's arm is leaner (1,148 vs 1,418 LOC) and its `_run_mutating` helper
is DRYer than opus's repeated load/save — but four of its five review findings were the
kind of gaps opus didn't make (missing design refs, construct-then-mutate).

**And one robustness bug only the opus arm has**: `load_state` trusts any valid JSON —
a non-dict state file crashes with a raw traceback where sonnet's shape check returns a
clean StorageError. Craft and blind spots are orthogonal; neither lens caught it.

**Net**: quality edge to opus leaves (real but confined to edge cases and craft — every
user-facing behavior tied), cost edge to nobody. At this width, pick leaves for quality,
not price; the price argument only turns on when leaves outnumber coordination.

### Root cause: the gap was in the spec, not the leaf

The article's thesis is that *"once a frontier planner has collapsed the ambiguity into a
detailed, explicit instruction, less expensive models simply have to follow it."* So the
right question about the two probe losses above is not "is sonnet weaker?" but "did the
trunk actually collapse the ambiguity?" We pulled the two arms' `query.py` specs off their
boards to check. Both arms were planned by the **same opus trunk**; both foundations
exposed a shared date validator (A5's `validate_date`, B5's `parse_date`). The specs
diverged:

- **A5 (→ sonnet leaf)** on date handling: *"compare them as strings OR via
  `datetime.date.fromisoformat` … but if you parse, guard is unnecessary since stored
  dates are valid."* Two options, and an explicit licence to skip the guard. It never told
  the leaf to run the untrusted `--due-before` CLI value through `validate_date`. The
  sonnet leaf followed the spec faithfully — including its permission to not validate.
- **B5 (→ opus leaf)** on the same: *"Compare dates via `models.parse_date` (parse both
  sides to `datetime.date`)."* One canonical path, through the shared validator that
  raises on bad input. The opus leaf followed that too — hence exit 2.

So the two probe wins were **not** the leaf model being smarter; they were the spec being
more explicit for that arm. The same planner produced a laxer spec for the routed run, and
the leaf inherited the laxity. Compounding it: A5 split tests into two **test-only leaves**
(7 leaves total), so the implementer of `query.py` never wrote the tests that would have
forced it to confront the `--due-before` edge case; B5's leaves each owned their own tests
(5 leaves), so B5's query author met the edge case while writing `test_query.py`. The
test-only split cost the extra landings *and* removed the pressure that produces the craft.

**The quality lever and the cost lever are the same lever: trunk decomposition.** Fewer,
fuller, test-owning leaves = fewer cold starts + merges + review landings (cost is
O(width)) *and* implementers who meet their own edge cases (quality).

### Fixes shipped (this session)

Prompt and harness changes targeting exactly the above:

1. **`planner.md` — "Collapse the ambiguity".** A spec has uncollapsed ambiguity when it
   offers a choice, calls a check optional/unnecessary, names a behavior without its
   boundary conditions, or relies on a shared helper without naming it. Untrusted input
   must be validated at the boundary through a *named* shared validator, recorded as a
   design decision. This directly forbids the exact A5 spec language ("guard is
   unnecessary", "strings OR fromisoformat").
2. **`planner.md` — "Decomposition economics".** Prefer fewer, fuller children; **each
   child owns its module AND that module's tests**; **no test-only children**; shared
   utilities live in one foundation child imported by name. Kills the 7-vs-5 width
   regression at its source.
3. **`executor.md` — craft bar.** Call the shared helpers your spec names (never
   reimplement/bypass); validate untrusted input before mutating state; cover the edge
   cases your assigned tests list.
4. **Push-context codebase lens (cost).** The round-4 prompt-only "scope your reading"
   nudge measurably failed ($0.124/call, unchanged). Replaced with a mechanism: the
   harness now injects the touched files' full bodies into the codebase-lens prompt
   (bounded 48 KB) and caps that lens at 8 turns, so it judges from context instead of
   re-reading the repo every turn (that re-acquisition was its dominant cost). Effect to
   be measured in a round 6.

These are unmeasured until a round 6 A/B repeats the taskflow objective; the diagnosis
(same planner, laxer spec → laxer leaf) is the evidence-backed part.

## Round 6 — the fixes, measured

Same taskflow objective, fresh repos, the round-5 configs verbatim (only the executor
model differs between arms), run on the harness *after* the collapse-ambiguity +
decomposition-economics prompt changes and the push-context codebase lens.

| arm | leaves | cost | wall | tests | battery |
|---|---|---|---|---|---|
| A6: opus trunk + **sonnet** leaves | 4 | **$2.50** | 9.1 min | 70 | ✅ 23/23 |
| B6: **opus everywhere** | 6 | **$3.85** | 9.4 min | 117 | ✅ 23/23 |

**The cost verdict flipped.** In round 5 the routed arm cost 5% *more* than
opus-everywhere ($3.68 vs $3.50); now it costs **35% less** ($2.50 vs $3.85). Two drivers,
both the shipped fixes:

- **Decomposition economics.** The A6 planner emitted 4 fuller leaves, each owning its
  own module *and* tests — no test-only children — versus A5's 7 (2 of them test-only).
  Cost is O(width): fewer leaves = fewer cold starts, merges, and review landings. (B6's
  planner happened to pick 6; decomposition is still nondeterministic, but the prompt now
  pushes toward "fewer, fuller.")
- **Push-context codebase lens.** Per-call cost of the haiku codebase lens fell from
  **$0.124** (round 5) to **$0.062–0.067**, and its cached repo re-reads fell from ~365 K
  per call to ~33 K — an ~11× drop. The round-4 prompt-only "scope your reading" nudge
  hadn't moved this number at all; pushing the touched-file bodies into the prompt and
  capping the lens at 8 turns did.

### Quality: the round-5 gaps closed

The two runtime probes that separated the arms in round 5 were input-validation on
untrusted CLI values. This round both arms handle them:

| probe | A5 sonnet (r5) | A6 sonnet (r6) | B6 opus (r6) |
|---|---|---|---|
| `task list --due-before 2026-99-99` | exit 0, silent | **exit 2** ✓ | exit 2 ✓ |
| `report --today <garbage>` | exit 0, silent | **exit 2** ✓ | exit 2 ✓ |
| state file = valid JSON, non-dict | exit 2 ✓ | exit 2 ✓ | **exit 2 ✓** (r5 opus crashed) |
| `--status bogus` (list filter) | silent empty | silent empty | argparse rejects |

A6's `filter_tasks` now opens with `models.parse_date(due_before)` — the leaf validated the
untrusted input because the spec named the shared validator and forbade a private date
parser, where A5's spec had offered *"strings OR fromisoformat… guard is unnecessary."*
**Same leaf model as round 5; the only thing that changed was the trunk's spec.** That is
the article's thesis reproduced on demand.

Both arms pass the full 23-check battery and their whole unittest suite (A6 70 tests /
1,018 LOC; B6 117 / 1,549). Review-finding severity actually favored the sonnet arm this
round: A6 drew only *low* findings (missing `canopy-design:` ref comments), while B6's opus
leaves drew two *high* findings (test files importing `errors` directly, against the spec's
import rule) that the review loop then fixed. Zero merge conflicts, zero retries, zero
failed nodes in both arms.

### The lever

Round 5 said "at small width, tree shape dominates leaf price." Round 6 shows the
corollary: **improve the trunk and both levers move at once.** The planner change made the
sonnet leaf validate (quality) *and* cut the leaf count (cost); the lens change cut review
cost without blinding it. Routed-swarm now beats opus-everywhere on cost at this scale and
ties it on user-facing quality — the shape the article promised, at bonsai scale. n=1 per
arm, so decomposition nondeterminism could shift the exact percentages on a repeat; the
probe-level quality closure and the per-call lens cost drop are mechanism-level, not luck.

## Round 7 — 3×3, and what variance actually is

Round 6 was n=1 per arm. To characterize the *distribution* (not one sample) we ran the
same two configs **three times each** — A = opus trunk + sonnet leaves, B = opus
everywhere — on identical fresh repos. Both arms share the same opus planner, so the six
runs are really one decomposition distribution sampled six times.

| run | leaves | cost | wall | tests | battery | high findings |
|---|---|---|---|---|---|---|
| a1 sonnet | 9 | $4.66 | 11.4 | 61 | ✅ | 4 |
| a2 sonnet | 6 | $3.57 | 12.4 | 89 | ✅ | 1 |
| a3 sonnet | 5 | $2.43 | 9.3 | 85 | ✅ | 0 |
| b1 opus | 5 | $2.83 | 9.4 | 103 | ✅ | 0 |
| b2 opus | 5 | $2.85 | 9.2 | 59 | ✅ | 0 |
| b3 opus | 7 | $3.91 | 11.2 | 121 | ✅ | 2 |

**The tree shape is the trunk's decision, and it is the dominant cost lever — bigger than
the executor model.** The opus planner decomposed this one objective into `{5,5,5,6,7,9}`
leaves (median 5.5, range 5–9), and cost tracks leaf count almost linearly. Controlling for
shape — the three 5-leaf runs — the routed arm is cheaper: **a3 $2.43 vs b1/b2 $2.83/$2.85,
~15% less**, the clean apples-to-apples result. But across the full sample the shape draw
dominates: the sonnet arm happened to draw high (9, 6) and ended with a *higher* mean
($3.55) than the opus arm ($3.20). Round 6's "routed is 35% cheaper" was a lucky low draw
(4 leaves); the honest statement is **routed is ~15% cheaper at equal shape, and shape
variance (±2 leaves) swings a single run's cost more than the model choice does.**

### Quality: read the code, not the test count

All six pass the 23-check battery and their whole unit suite. Test *counts* are noise and
actively misleading: opus b2 wrote **59** tests, fewer than sonnet a2/a3 (89/85); opus b3
wrote 121. What matters is *what* they tested.

- **Robustness (state file = valid JSON, non-dict):** all 3 sonnet runs return a clean
  StorageError (exit 2); **2 of 3 opus runs (b2, b3) crash with a raw traceback (exit 1).**
  The cause is visible in the source: a3's `load_state` guards
  `isinstance(state, dict) and required keys`, and its `test_storage.py` has
  `test_json_list_raises_storage_error` (writes `[1,2,3]`, asserts StorageError). b2's
  `load_state` is `return json.load(f)` with no shape check, and its tests never feed it a
  non-dict. **The missing test is the crash.** Test count inverted the truth — b2 (fewer
  tests) shipped the bug the more-tested-looking arm avoided.
- **Validation (bad `--due-before` / `--today`):** all six exit 2. Both arms' `models`
  reject bool-as-int (`isinstance(x, bool)` guard) and impossible calendar dates (strptime
  rejects `2026-02-30`). The round-5 gap is closed everywhere.
- **Craft edge to opus, locally:** b2's `models.py` carries module/function docstrings,
  `canopy-design:` refs, and validates `id`/`project_id` types that a3 skips. So a3 won
  storage robustness; b2 won models craft. Neither arm dominates — the differences track
  run-to-run variance more than the model tier.
- **A recurring, arm-independent gap:** `save_state` catches only `OSError`, so a
  `json.dumps` failure (a non-serializable state) leaks the temp file instead of raising
  `StorageError`. It appears in sonnet (a1, flagged) and opus (b3, flagged; a3, present but
  unflagged) alike — an *uncollapsed spec ambiguity* (the objective never says what happens
  when serialization itself fails), the same class of defect as the round-5 date bug, one
  level up. The fix is planner-side: specify it.
- **Review lens is not blinded by push-context:** it caught fd-leaks on Windows, temp-file
  cleanup on non-`OSError`, and whitespace-normalized duplicate-name bypasses — sophisticated
  findings, at ~$0.06/call.

### The lever, restated

The trunk owns the tree shape; the shape is the biggest single knob on cost *and* the thing
that varies most run-to-run. Making leaves cheaper (routing) buys ~15% at fixed shape;
making the trunk decide shape *well and consistently* is worth more. Decomposition still
ranges 5–9 on an 8-module objective — the next lever is a more prescriptive decomposition
contract ("one leaf per cohesive module named in the objective, tests included") to pull
that range in, plus collapsing the remaining edge-case ambiguities (serialization-failure
policy, non-dict state) in the spec so neither model has to guess.

## Round 8 — swarm vs solo (canopy Opus+Sonnet vs solo Opus)

The earlier rounds compared two *swarms*. This one asks the sharper question: is the swarm
worth it at all versus a single Opus agent with no harness? A = the three canopy runs from
round 7 (opus trunk + sonnet leaves, a1/a2/a3). B = **solo Opus**: one `claude -p --model
opus` agent, same objective, same fresh repo, told to write everything and make the tests
pass — no planner, no leaves, no reviewers, no merge queue.

| arm | run | cost | wall | LOC | tests | battery | invalid-date input (P1/P2) |
|---|---|---|---|---|---|---|---|
| A canopy O+S | a1 | $4.66 | 11.4 | 994 | 61 | ✅ | exit 2 ✓ |
| A canopy O+S | a2 | $3.57 | 12.4 | 1140 | 89 | ✅ | exit 2 ✓ |
| A canopy O+S | a3 | $2.43 | 9.3 | 1201 | 85 | ✅ | exit 2 ✓ |
| B solo Opus | s1 | $1.03 | 2.7 | 986 | 58 | ✅ | exit 2 ✓ |
| B solo Opus | s2 | $0.70 | 2.3 | 931 | 65 | ✅ | **exit 1 (crash)** |
| B solo Opus | s3 | $0.74 | 2.5 | 962 | 55 | ✅ | **exit 0 (silent)** |

**Cost and speed: solo wins decisively.** Solo Opus averaged **$0.82** and **2.5 min**;
canopy Opus+Sonnet averaged **$3.55** and **~11 min** — the swarm is ~4.3× more expensive
and ~4.4× slower for one bounded feature that fits a single context. This is the article's
own boundary: routing/swarming pays off at scale (work too big for one context, parallel
throughput), not on a single feature where coordination overhead — planner + reviewers +
merges + per-leaf context re-acquisition — dominates.

**Consistency of edge-case correctness: canopy wins.** All six pass the 23-check battery
(functional tie). But on the untrusted-input edge (`--due-before 2026-99-99`, `--today
garbage`), the three canopy runs were **3/3 correct** (clean exit 2), while the three solo
Opus runs were **1 correct, 1 crash, 1 silent-wrong**. Reading the code shows three
*different* architectures the solo agent invented for the same ambiguity:

- **s1**: shared `models.parse_date`, called at the boundary, `ValidationError` mapped to
  exit 2. Correct.
- **s2**: no shared validator; a private `_as_date` using `date.fromisoformat`, which
  raises `ValueError` — but `cli.py` catches only `(ValidationError, NotFoundError,
  StorageError)`, so it escapes uncaught → traceback, exit 1.
- **s3**: no validation at all — raw string comparison `task["due"] < due_before`. Invalid
  input silently accepted, exit 0.

That is the article's thesis in its purest form: resolving the ambiguity is a judgment
call, and **even Opus resolves it inconsistently across runs when working solo from the raw
objective.** canopy's trunk collapses that decision once ("validate at the boundary through
the shared `parse_date`") and every leaf — even a cheaper Sonnet one — follows it, so the
swarm's *cheaper* arm was *more reliable* on this edge than solo *Opus*. And test counts
(55–65) again failed to predict it: the solo failures trace to a test gap — s2/s3 never
exercised `task list --due-before <invalid>` end to end.

**When to use which.** Solo Opus for a single well-scoped feature that fits one context —
far cheaper and faster, but you are rolling dice on whatever the objective left unspecified.
canopy when the work is too big for one context (parallel throughput, resumability) or when
you need *consistent, auditable* handling of the decisions a solo agent would otherwise make
ad hoc — the trunk pins them down once so every leaf obeys.

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
