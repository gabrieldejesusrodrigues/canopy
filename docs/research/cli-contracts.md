# CLI Headless Contracts (verified live 2026-07-22)

claude 2.1.212 · codex-cli 0.144.6 · agy (Antigravity CLI) 1.0.9

## 1. claude

Template:
```
claude -p "<prompt>" --output-format json --model <m> \
  --permission-mode bypassPermissions --setting-sources "" \
  [--max-turns N] [--max-budget-usd X] [--add-dir <dir>]
```
- cwd = subprocess cwd. `--setting-sources ""` gives a hermetic run (skips user hooks/plugins which otherwise add ~38k cache tokens per run on this machine).
- Output: single JSON object on stdout. Keys: `result` (final message), `is_error` (bool — USE THIS, `subtype` says "success" even on API errors), `num_turns`, `session_id`, `total_cost_usd`, `usage.{input_tokens,output_tokens,cache_creation_input_tokens,cache_read_input_tokens}`, `modelUsage` (keyed by resolved full model id, has `costUSD`), `terminal_reason`.
- Errors: exit code 1, JSON still on stdout (`is_error:true`, `api_error_status`, message in `result`). stderr empty. Exit 0/1 verified.
- Model aliases: `fable`, `opus`, `sonnet`, `haiku` (haiku = claude-haiku-4-5-20251001, cheapest), full `claude-*` ids pass through.
- `--max-turns` exists but hidden in --help. No wall-clock timeout flag — harness enforces.

## 2. codex

Template:
```
codex exec --json -m <model> -C <workdir> --skip-git-repo-check \
  -s workspace-write --ignore-user-config --ephemeral \
  [-c model_reasoning_effort="low"] -o <last-message-file> "<prompt>"
```
- **stdin MUST be null** (piped stdin gets appended to the prompt).
- `--ignore-user-config --ephemeral` = hermetic (this machine's ~/.codex/config.toml otherwise routes through a localhost proxy with xhigh effort). Auth still works.
- Output: JSONL events on stdout. `thread.started`, `turn.started`, `item.started/updated/completed`, `turn.completed`, `turn.failed`, `error`. Final message = LAST `item.completed` with `item.type=="agent_message"` (`item.text`) — or, most reliable, the `-o` file which contains exactly the final message.
- Usage in `turn.completed.usage`: `input_tokens`, `cached_input_tokens`, `output_tokens`, `reasoning_output_tokens`. **No cost field** — price via config table.
- Models (from live models_cache.json): gpt-5.6-sol, gpt-5.6-terra, gpt-5.6-luna, gpt-5.5, gpt-5.4, gpt-5.4-mini (cheapest). Effort via `-c model_reasoning_effort="low|medium|high|xhigh|max|ultra"`.
- Exit 0 ok; nonzero on failure. stderr = human logs.

## 3. agy

Template:
```
agy --print "<prompt>" --model "<Model Name (Effort)>" \
  --dangerously-skip-permissions [--print-timeout 30m] [--add-dir <dir>]
```
- cwd = subprocess cwd. Plain text ONLY: stdout = final message verbatim. No JSON, no usage, no cost. Exit 0 ok / 1 error (`Error:` on stderr, empty stdout).
- Models (exact strings, `agy models`): "Gemini 3.6 Flash (High|Medium|Low)", "Gemini 3.5 Flash (High|Medium|Low)", "Gemini 3.1 Pro (High|Low)", "Claude Sonnet 4.6 (Thinking)", "Claude Opus 4.6 (Thinking)", "GPT-OSS 120B (Medium)". Cheapest: "Gemini 3.6 Flash (Low)".
- Hidden `--effort low|medium|high` splits effort from model name. `--print-timeout` (Go duration, default 5m0s) — set it above harness timeout.
- System prompt: only via GEMINI.md/AGENTS.md in the workspace — canopy assembles the full prompt inline instead.

## Cross-CLI summary

| | claude | codex | agy |
|---|---|---|---|
| one-shot | `claude -p` | `codex exec` | `agy --print` |
| output | single JSON | JSONL | plain text |
| final msg | `.result` | `-o` file / last agent_message | whole stdout |
| tokens | usage + modelUsage | turn.completed.usage | none |
| cost USD | `total_cost_usd` | none (price table) | none (price table impossible — no tokens; record zeros) |
| auto-approve | `--permission-mode bypassPermissions` | `-s workspace-write` | `--dangerously-skip-permissions` |
| hermetic | `--setting-sources ""` | `--ignore-user-config --ephemeral` | (default) |
| timeout | harness | harness | `--print-timeout` + harness |
