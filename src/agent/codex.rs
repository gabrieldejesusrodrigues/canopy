use std::time::Instant;

use anyhow::{bail, Context};
use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time;

use crate::model::CliKind;

use super::{AgentCli, InvocationRequest, InvocationResult, Usage};

pub struct CodexCli;

// ── JSONL event shapes ────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct Event {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    usage: Option<TurnUsage>,
    #[serde(default)]
    item: Option<Item>,
}

#[derive(Deserialize)]
struct TurnUsage {
    input_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    reasoning_output_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct Item {
    #[serde(rename = "type")]
    kind: Option<String>,
    text: Option<String>,
}

pub struct ParsedEvents {
    pub final_message: Option<String>,
    pub usage: Usage,
    pub exit_ok: bool,
}

/// Parse a JSONL transcript from codex. Separated for unit-testability.
pub fn parse_events(jsonl: &str) -> ParsedEvents {
    let mut usage = Usage::default();
    let mut last_agent_message: Option<String> = None;
    let mut had_failure = false;

    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<Event>(line) else {
            continue;
        };
        match ev.kind.as_str() {
            "turn.completed" => {
                if let Some(u) = ev.usage {
                    usage.input_tokens = u.input_tokens.unwrap_or(0);
                    usage.cached_tokens = u.cached_input_tokens.unwrap_or(0);
                    usage.output_tokens =
                        u.output_tokens.unwrap_or(0) + u.reasoning_output_tokens.unwrap_or(0);
                }
            }
            "turn.failed" | "error" => {
                had_failure = true;
            }
            "item.completed" => {
                if let Some(item) = ev.item {
                    if item.kind.as_deref() == Some("agent_message") {
                        if let Some(text) = item.text {
                            last_agent_message = Some(text);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    ParsedEvents {
        final_message: last_agent_message,
        usage,
        exit_ok: !had_failure,
    }
}

#[async_trait]
impl AgentCli for CodexCli {
    fn kind(&self) -> CliKind {
        CliKind::Codex
    }

    async fn invoke(&self, req: &InvocationRequest) -> anyhow::Result<InvocationResult> {
        let start = Instant::now();

        // last_msg_file = transcript_path with extension replaced by "last.txt"
        let last_msg_file = req.transcript_path.with_extension("last.txt");

        let mut cmd = Command::new("codex");
        cmd.arg("exec")
            .arg("--json")
            .arg("-m")
            .arg(&req.model)
            .arg("-C")
            .arg(&req.workdir)
            .arg("--skip-git-repo-check")
            .arg("-s")
            .arg("workspace-write")
            .arg("--ignore-user-config")
            .arg("--ephemeral")
            .arg("-o")
            .arg(&last_msg_file)
            .arg(&req.prompt) // FINAL positional arg — must be last
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let child = cmd.spawn().context("failed to spawn codex")?;

        let result = time::timeout(
            std::time::Duration::from_secs(req.timeout_secs),
            child.wait_with_output(),
        )
        .await;

        let output = match result {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => bail!("codex process error: {e}"),
            Err(_) => bail!("codex timed out after {} seconds", req.timeout_secs),
        };

        let duration_ms = start.elapsed().as_millis();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr_tail: String = {
            let s = String::from_utf8_lossy(&output.stderr);
            let chars: Vec<char> = s.chars().collect();
            let start_idx = chars.len().saturating_sub(2000);
            chars[start_idx..].iter().collect()
        };

        // Write JSONL transcript
        {
            let mut f = tokio::fs::File::create(&req.transcript_path)
                .await
                .with_context(|| {
                    format!(
                        "could not create transcript at {}",
                        req.transcript_path.display()
                    )
                })?;
            f.write_all(output.stdout.as_ref()).await?;
        }

        let ParsedEvents {
            final_message: events_msg,
            usage,
            exit_ok: events_ok,
        } = parse_events(stdout.trim());

        // Prefer -o file; fall back to last item.completed agent_message
        let final_message = match tokio::fs::read_to_string(&last_msg_file).await {
            Ok(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => events_msg.unwrap_or_default(),
        };

        let exit_ok = output.status.success() && events_ok;

        if !exit_ok && final_message.is_empty() {
            bail!(
                "codex failed (exit {}); stderr tail: {}",
                output.status,
                stderr_tail
            );
        }

        Ok(InvocationResult {
            final_message,
            usage,
            exit_ok,
            duration_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::parse_events;

    /// Canned JSONL matching the event sequence from docs/research/cli-contracts.md
    const SAMPLE_JSONL: &str = r#"{"type":"thread.started","thread_id":"t1"}
{"type":"turn.started","turn_id":"turn1"}
{"type":"item.started","item":{"type":"agent_message","id":"msg1"}}
{"type":"item.updated","item":{"type":"agent_message","id":"msg1","text":"Hello"}}
{"type":"item.completed","item":{"type":"agent_message","id":"msg1","text":"Final answer here."}}
{"type":"turn.completed","usage":{"input_tokens":1200,"cached_input_tokens":300,"output_tokens":450,"reasoning_output_tokens":50}}
"#;

    const FAILED_JSONL: &str = r#"{"type":"turn.started","turn_id":"turn1"}
{"type":"turn.failed","error":"model overload"}
"#;

    const MULTI_MSG_JSONL: &str = r#"{"type":"item.completed","item":{"type":"agent_message","text":"first"}}
{"type":"item.completed","item":{"type":"agent_message","text":"last"}}
{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":20,"reasoning_output_tokens":0}}
"#;

    #[test]
    fn parse_success() {
        let parsed = parse_events(SAMPLE_JSONL);
        assert_eq!(parsed.final_message.as_deref(), Some("Final answer here."));
        assert!(parsed.exit_ok);
        assert_eq!(parsed.usage.input_tokens, 1200);
        assert_eq!(parsed.usage.cached_tokens, 300);
        // output = 450 + 50 reasoning
        assert_eq!(parsed.usage.output_tokens, 500);
        assert!(parsed.usage.cost_usd.is_none());
    }

    #[test]
    fn parse_failure_event() {
        let parsed = parse_events(FAILED_JSONL);
        assert!(!parsed.exit_ok);
    }

    #[test]
    fn parse_last_agent_message_wins() {
        let parsed = parse_events(MULTI_MSG_JSONL);
        assert_eq!(parsed.final_message.as_deref(), Some("last"));
    }

    #[test]
    fn parse_empty_jsonl() {
        let parsed = parse_events("");
        assert!(parsed.final_message.is_none());
        assert!(parsed.exit_ok); // no failure event = ok
        assert_eq!(parsed.usage.input_tokens, 0);
    }
}
