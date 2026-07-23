# Operating a canopy daemon

## Watching a run

```bash
canopy status --watch      # live task tree, refreshes every 3s
canopy report              # tokens / cost / agent-minutes per role and model
```

The run's terminal summary prints wall-clock time; `agt min` in the report is
agent process time (they differ by queue serialization and scheduling).

## Crash recovery / supervision

All coordination state survives the process: the board (sqlite/Linear), the
megafile blocklist (`.canopy/blocklist.json`) and the swarm context
(`.canopy/swarm-state.json` — pending flags, declared breaks, contention
counters, replan budget). If the daemon dies, resume with:

```bash
canopy run --resume <run-id>     # run id is in .canopy/last-run
```

Leases expire on their own; an orphaned leaf is re-claimed and its worktree
rebuilt from the current run-branch tip. This was validated in anger (host
killed the daemon mid-leaf; the resumed run completed to root Done at the
cost of one re-run invocation).

For unattended runs, supervise with systemd so resume is automatic:

```ini
# ~/.config/systemd/user/canopy-run.service
[Unit]
Description=canopy swarm run

[Service]
WorkingDirectory=%h/myproject          # dir containing canopy.toml
ExecStart=/usr/bin/env bash -c 'canopy run --resume "$(cat target/.canopy/last-run)"'
Restart=on-failure
RestartSec=10
```

Start the run once by hand (`canopy run "<objective>"`), then
`systemctl --user start canopy-run` to keep it alive. `daemon.pid` prevents a
second writer: a live pid refuses to start, a stale one is reclaimed.

## Resetting a repo

`rm -rf <target>/.canopy` is the supported reset: board, ledger, blocklist,
swarm state, worktrees and merge checkout all live there. Stale git worktree
registrations left by the delete are pruned automatically on the next start.
Run branches (`canopy/run-*`, `canopy/node-*`) are ordinary branches — delete
them with `git branch -D` when you're done inspecting.

## Security note

Leaf agents run headless with write access to their worktree and no
interactive approval: `claude` runs with `--permission-mode bypassPermissions`
(no sandbox), `codex` under its `workspace-write` sandbox, `agy` with
`--dangerously-skip-permissions`. Treat objectives as trusted input and run
swarms on repos/machines where that is acceptable. The merge queue's verify
command executes repo code — same trust boundary as your CI.
