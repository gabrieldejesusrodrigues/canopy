# Real harness run — 2026-07-23

First end-to-end swarm run with real CLIs and real tokens, against a scratch Python repo.
This exercised the full loop: plan → parallel leaves → serialized merges → review lens →
cascade → economics report.

## Setup

| | |
|---|---|
| target repo | scratch git repo (one `init` commit) |
| objective | `fizzbuzz.py` CLI (argv N, exit 2 on bad input) + `stats.py` (mean/median, ValueError on empty) + unittest suites |
| planner / merger | `claude:sonnet` |
| executors | `codex:gpt-5.4-mini` (the cheapest codex tier), `max_parallel = 2` |
| reviewer | `agy:Gemini 3.6 Flash (Low)`, `output` lens |
| tracker | sqlite · `max_usd` $5 · depth 2 · attempts 2 |
| verify | `python3 -m unittest discover -q` (rc 5 = no tests yet tolerated) |

## What happened

**Run 1 — root failed, and the failure was a real finding.** All six executor
invocations (three planner rounds) produced working, locally-tested code — and every one
reported `blocked`. The board trail made the diagnosis trivial: codex's `workspace-write`
sandbox mounts the repo's git metadata read-only, and a *worktree's* metadata lives
outside the workdir (`<repo>/.git/worktrees/<id>`), so `git commit` died with
`Read-only file system` — and the contract said done means *committed*. The swarm
mechanics were flawless the whole time: replan fired, old children became `Superseded`,
the cap stopped the third round, the root failed honestly. Two fixes:

1. `prompts/executor.md`: git failures are never blockers — the harness commits whatever
   the leaf leaves behind (`commit_all` safety net, which already existed).
2. codex adapter: grant the main `.git` dir as a sandbox writable root
   (`-c sandbox_workspace_write.writable_roots=[...]`, derived from the worktree's
   `.git` file; unknown keys degrade safely).

**Run 2 — startup abort.** The manual scenario reset (`rm -rf .canopy`) left stale git
worktree registrations. Hardening: `ensure_run_branch` now prunes first — deleting
`.canopy/` is the documented reset and must just work.

**Run 3 — root done.** Single planner pass, both leaves in parallel, two serialized
merges, review lens clean, cascade to root. The generated branch:

```
9921749 canopy: merge node 2536cc36…   (fizzbuzz)
106aa68 canopy: merge node f801013e…   (stats)
79b9e18 canopy: scaffold design/ and fieldguide/
```

Validation on the run branch: **13/13 unittest pass**, `python3 fizzbuzz.py 5` prints the
right sequence, `python3 fizzbuzz.py abc` → stderr error + exit 2. Spec met exactly.

## Economics (run 3, `canopy report`)

```
== Spend by role ==
group        calls    tokens    tok %   cost USD  cost %  unpriced
executor         2    743672    99.7%     0.2023   59.5%       -
planner          1      2359     0.3%     0.1379   40.5%       -
reviewer         2         0     0.0%     0.0000    0.0%       2

TOTAL: 746031 tokens, $0.3403 priced spend
```

The article's shape is visible even at toy scale: **leaves own 99.7% of tokens; the
trunk owns 0.3% of tokens but 40% of cost**. On a two-leaf task the trunk share is at
its worst — planner cost is ~fixed per decomposition while executor cost scales with
fan-out, so the gap the article reports (~8×) opens with tree width. `agy` reports no
usage (documented limit) — its two review calls show as unpriced.

## Notes

- Run 1's $1.21 was not wasted: it bought a real dual-bug find (sandbox × contract
  interaction) that no stub test could have caught.
- The config lease clamp (`lease_secs → 2×timeout+300`) fired on every run, as designed.
- No merge conflicts occurred (disjoint files); the Merger path is covered by the stub
  e2e, which forces a real conflict.
