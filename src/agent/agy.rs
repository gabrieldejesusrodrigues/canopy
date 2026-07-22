use std::time::Instant;

use anyhow::{bail, Context};
use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time;

use crate::model::CliKind;

use super::{AgentCli, InvocationRequest, InvocationResult, Usage};

pub struct AgyCli;

#[async_trait]
impl AgentCli for AgyCli {
    fn kind(&self) -> CliKind {
        CliKind::Agy
    }

    async fn invoke(&self, req: &InvocationRequest) -> anyhow::Result<InvocationResult> {
        let start = Instant::now();

        // --print-timeout must exceed the harness timeout so agy doesn't kill itself first.
        // agy takes a Go duration string; add 60s of headroom.
        let agy_timeout_secs = req.timeout_secs + 60;
        let print_timeout = format!("{agy_timeout_secs}s");

        let mut cmd = Command::new("agy");
        cmd.arg("--print")
            .arg(&req.prompt)
            .arg("--model")
            .arg(&req.model)
            .arg("--dangerously-skip-permissions")
            .arg("--print-timeout")
            .arg(&print_timeout)
            .current_dir(&req.workdir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let child = cmd.spawn().context("failed to spawn agy")?;

        let result = time::timeout(
            std::time::Duration::from_secs(req.timeout_secs),
            child.wait_with_output(),
        )
        .await;

        let output = match result {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => bail!("agy process error: {e}"),
            Err(_) => bail!("agy timed out after {} seconds", req.timeout_secs),
        };

        let duration_ms = start.elapsed().as_millis();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let final_message = stdout.trim().to_string();

        let stderr_tail: String = {
            let s = String::from_utf8_lossy(&output.stderr);
            let chars: Vec<char> = s.chars().collect();
            let start_idx = chars.len().saturating_sub(2000);
            chars[start_idx..].iter().collect()
        };

        // Write transcript (plain text final message)
        {
            let mut f = tokio::fs::File::create(&req.transcript_path)
                .await
                .with_context(|| {
                    format!(
                        "could not create transcript at {}",
                        req.transcript_path.display()
                    )
                })?;
            f.write_all(final_message.as_bytes()).await?;
        }

        let exit_ok = output.status.success() && !final_message.is_empty();

        if !exit_ok {
            // Surface Error: lines from stderr per the contracts doc
            let error_lines: String = stderr_tail
                .lines()
                .filter(|l| l.starts_with("Error:"))
                .collect::<Vec<_>>()
                .join("; ");
            bail!(
                "agy failed (exit {}){}: {}",
                output.status,
                if error_lines.is_empty() {
                    String::new()
                } else {
                    format!("; {error_lines}")
                },
                if final_message.is_empty() {
                    "empty stdout"
                } else {
                    "non-zero exit"
                }
            );
        }

        Ok(InvocationResult {
            final_message,
            usage: Usage::default(), // agy reports no tokens
            exit_ok,
            duration_ms,
        })
    }
}
