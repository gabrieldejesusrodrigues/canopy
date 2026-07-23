# canopy 🌳

**Agent swarm harness over `claude`, `codex` and `agy` (Antigravity) CLIs**, implementing the
tree/trunk/leaf architecture — and all six coordination mechanisms — from Cursor's
[Agent swarm model economics](https://cursor.com/blog/agent-swarm-model-economics),
rescaled from their datacenter (custom VCS, 1,000 commits/sec) to a single machine with git.

The economics thesis: *few moments in a large task truly demand frontier intelligence.*
Expensive models plan (**trunks**); cheap models execute explicit instructions (**leaves**).
In Cursor's experiments this delivered equivalent quality at ~8× lower cost — their
GPT-5.5-everywhere run cost $10,565 while Opus-planner + cheap-executor cost $1,339.

## How it works

```
                    board (Linear project ─ or ─ local SQLite)
                    the board IS the task tree and the queue
                                    │
                             canopy scheduler
                     (single daemon, single writer)
                    ┌───────────────┼────────────────┐
                 trunks           leaves          mechanisms
               planner agents   executor agents   merger / reconciler /
               (smart models,   (cheap models,    decomposer / reviewers
                never touch      one git worktree  (triggered by events)
                code)            each)
                                    │
                          serialized merge queue
                 (the article's "custom VCS" role: the single
                  point every change passes, conflicts surface first)
```

- **Planners never implement.** They emit structured decompositions (children + design
  decisions); the harness materializes them as issues on the board. Their context never
  fills with low-level detail.
- **Executors see only their spec** + the Field Guide + the design docs their spec cites.
  One isolated worktree each; the merge queue lands their branches one at a time.
- **Agents are one-shot stateless processes.** All state lives on the board — kill the
  daemon and `canopy run --resume <id>` continues where it stopped.

## The six article mechanisms (all enforced, not prompt-hoped)

| # | Article | canopy |
|---|---|---|
| 1 | Shared design docs, compile-checked refs | `design/DD-*.md`; `canopy-design: DD-n` comments checked on every merge |
| 2 | Reconciler for contradicting planners | topic-overlap detection → reconciler agent merges docs; broken refs become fix nodes |
| 3 | Neutral third-party merge agent | conflict → impartial Merger resolves in the merge worktree |
| 4 | Megafile flag → block → decompose | executor flags + post-merge line scan → merges touching the file bounce until a Decomposer splits it |
| 5 | Permitted breaks (anti-ossification) | `canopy-break: <reason>` + `breaks[]`; verify propagates; failures become fix nodes |
| 6 | Layered review lenses | transcript / output / codebase lenses on different CLIs+models; high findings spawn fix nodes |

Plus the **Field Guide** (stigmergy): `fieldguide/index.md` is injected into *every* agent
prompt; agents curate it; the only rule is a line budget — enforced at merge time.

Full mapping with quotes and triggers: [docs/MECHANISMS.md](docs/MECHANISMS.md).
Architecture: [docs/DESIGN.md](docs/DESIGN.md).
First real run (real CLIs, real tokens, article-shaped economics): [docs/REALRUN.md](docs/REALRUN.md).

## Install

```bash
cargo install --path .   # needs the git CLI + whichever agent CLIs you route to
```

Agent CLIs are invoked headless and hermetic (verified flags in
[docs/research/cli-contracts.md](docs/research/cli-contracts.md)):
`claude -p --output-format json`, `codex exec --json`, `agy --print`.

## Quickstart

```bash
cd ~/my-project-dir && canopy init /path/to/target-repo   # writes canopy.toml
# edit canopy.toml: verify command + routing
canopy run "Implement a CSV import pipeline with tests"
canopy status          # tree view of the run
canopy report          # article-style economics: tokens% vs cost% per role/model
```

Interrupt any time (ctrl-c); `canopy run --resume <run-id>` picks the run back up.
The swarm works on branch `canopy/run-<ts>` of the target repo — merging it to main
stays a human decision.

## Model routing

Any of the three CLIs can play any role — trunk/leaf is a role, not a vendor.

```toml
[routing]
mode = "static"                       # or "planner-routed"
planner  = { cli = "claude", model = "opus" }
executor = { cli = "codex",  model = "gpt-5.4-mini" }
reviewers = [
  { cli = "agy",    model = "Gemini 3.6 Flash (Low)", lens = "output" },
  { cli = "claude", model = "haiku",                  lens = "codebase" },
]
```

In `planner-routed` mode the planner itself assigns each child an agent from your
`[[routing.allowlist]]` (each entry carries a `good_for` hint), matching task difficulty
to model tier — the harness validates every assignment against the allowlist.

## Boards

- **SQLite** (default): `.canopy/canopy.db` in the target repo. Offline, zero setup.
- **Linear**: `tracker = "linear"` + `[linear] team_id = "..."` + `LINEAR_API_KEY`.
  Each run becomes a Linear project; the tree is sub-issues; agent activity lands as
  comments; humans can cancel/edit issues and the swarm respects it. Canopy-authoritative
  state travels in a metadata footer (the public Linear API has no CAS — the daemon is
  the single writer at claim granularity).

## Economics guardrails

`[budgets]`: `max_usd` (hard stop → run pauses, resumable), `max_parallel`,
`max_parallel_planners`, `max_attempts`, `max_tree_depth`, `agent_timeout_secs`,
`max_turns`. Every invocation is recorded (tokens, cost, duration, role, model) in a
local ledger; `canopy report` shows whether you're getting the article's shape —
**leaves own most tokens, trunks own most cost.**

## Development

```bash
cargo test    # includes an end-to-end swarm run with stub agents (zero tokens):
              # plan → parallel executors → real merge conflict → neutral merger →
              # review finding → fix node → completion cascade
```

## Status / honest limits

- v1, single machine, polling (no Linear webhooks), no nested-run distribution.
- `agy` reports no token usage — its rows show as unpriced in the report.
- The merge queue serializes landings by design; throughput is bounded by your
  `verify` command's speed, exactly like a human merge train.
