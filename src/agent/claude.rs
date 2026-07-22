use std::time::Instant;

use anyhow::{bail, Context};
use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::process::Child;
use tokio::process::Command;
use tokio::time;

use crate::model::CliKind;

use super::{AgentCli, InvocationRequest, InvocationResult, Usage};

pub struct ClaudeCli;

// Shape of the single JSON object claude writes to stdout.
#[derive(Deserialize)]
struct ClaudeOutput {
    result: Option<String>,
    is_error: Option<bool>,
    total_cost_usd: Option<f64>,
    usage: Option<ClaudeUsage>,
}

#[derive(Deserialize)]
struct ClaudeUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
}

/// Parse the single-JSON-object stdout claude emits.
/// Separated for unit-testability.
pub fn parse_output(stdout: &str) -> anyhow::Result<(String, Usage, bool)> {
    let parsed: ClaudeOutput =
        serde_json::from_str(stdout).context("claude stdout is not valid JSON")?;

    let is_error = parsed.is_error.unwrap_or(false);
    let final_message = parsed.result.unwrap_or_default();

    let usage = if let Some(u) = parsed.usage {
        let cached =
            u.cache_creation_input_tokens.unwrap_or(0) + u.cache_read_input_tokens.unwrap_or(0);
        Usage {
            input_tokens: u.input_tokens.unwrap_or(0),
            output_tokens: u.output_tokens.unwrap_or(0),
            cached_tokens: cached,
            cost_usd: parsed.total_cost_usd,
        }
    } else {
        Usage {
            cost_usd: parsed.total_cost_usd,
            ..Default::default()
        }
    };

    // exit_ok = NOT is_error (the caller will AND with process exit status)
    Ok((final_message, usage, !is_error))
}

async fn kill_child(mut child: Child) {
    let _ = child.start_kill();
    let _ = child.wait().await;
}

#[async_trait]
impl AgentCli for ClaudeCli {
    fn kind(&self) -> CliKind {
        CliKind::Claude
    }

    async fn invoke(&self, req: &InvocationRequest) -> anyhow::Result<InvocationResult> {
        let start = Instant::now();

        let mut cmd = Command::new("claude");
        cmd.arg("-p")
            .arg(&req.prompt)
            .arg("--output-format")
            .arg("json")
            .arg("--model")
            .arg(&req.model)
            .arg("--permission-mode")
            .arg("bypassPermissions")
            .arg("--setting-sources")
            .arg("");

        if let Some(turns) = req.max_turns {
            cmd.arg("--max-turns").arg(turns.to_string());
        }

        cmd.current_dir(&req.workdir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let child = cmd.spawn().context("failed to spawn claude")?;

        let result = time::timeout(
            std::time::Duration::from_secs(req.timeout_secs),
            child.wait_with_output(),
        )
        .await;

        let output = match result {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => bail!("claude process error: {e}"),
            Err(_) => {
                // Timeout — child is dropped (kill_on_drop), but be explicit
                bail!("claude timed out after {} seconds", req.timeout_secs);
            }
        };

        let duration_ms = start.elapsed().as_millis();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr_tail: String = {
            let s = String::from_utf8_lossy(&output.stderr);
            let chars: Vec<char> = s.chars().collect();
            let start_idx = chars.len().saturating_sub(2000);
            chars[start_idx..].iter().collect()
        };

        // Always write transcript even if parse fails
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

        if stdout.trim().is_empty() {
            bail!(
                "claude produced no stdout (exit {}); stderr tail: {}",
                output.status,
                stderr_tail
            );
        }

        let (final_message, usage, json_ok) =
            parse_output(stdout.trim()).with_context(|| format!("stderr tail: {stderr_tail}"))?;

        // exit_ok = process exited 0 AND is_error == false
        let exit_ok = output.status.success() && json_ok;

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
    use super::parse_output;

    const SUCCESS_JSON: &str = r#"{
        "result": "Here is the plan.",
        "is_error": false,
        "total_cost_usd": 0.0042,
        "usage": {
            "input_tokens": 800,
            "output_tokens": 200,
            "cache_creation_input_tokens": 100,
            "cache_read_input_tokens": 50
        },
        "num_turns": 3,
        "session_id": "sess-abc"
    }"#;

    const ERROR_JSON: &str = r#"{
        "result": "API error: rate limit exceeded",
        "is_error": true,
        "total_cost_usd": 0.0,
        "usage": {
            "input_tokens": 100,
            "output_tokens": 0,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0
        },
        "api_error_status": 429
    }"#;

    #[test]
    fn parse_success() {
        let (msg, usage, ok) = parse_output(SUCCESS_JSON).unwrap();
        assert_eq!(msg, "Here is the plan.");
        assert!(ok);
        assert_eq!(usage.input_tokens, 800);
        assert_eq!(usage.output_tokens, 200);
        assert_eq!(usage.cached_tokens, 150); // 100 + 50
        assert!((usage.cost_usd.unwrap() - 0.0042).abs() < 1e-9);
    }

    #[test]
    fn parse_api_error_surfaces_message() {
        let (msg, _usage, ok) = parse_output(ERROR_JSON).unwrap();
        // is_error:true → exit_ok false, but we still get the message
        assert!(!ok);
        assert!(msg.contains("rate limit"));
    }

    #[test]
    fn parse_invalid_json_errors() {
        assert!(parse_output("not json").is_err());
    }
}
