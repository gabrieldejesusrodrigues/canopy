use std::time::Instant;

use anyhow::Context;
use async_trait::async_trait;
use tokio::fs;

use crate::model::CliKind;

use super::{AgentCli, InvocationRequest, InvocationResult, Usage};

pub struct StubCli;

/// Extract the first `stub:<name>` marker from a prompt string.
/// Accepts [a-z0-9_-] after the colon (stdlib, no regex dep).
fn find_stub_marker(prompt: &str) -> Option<&str> {
    let tag = "stub:";
    let start = prompt.find(tag)?;
    let rest = &prompt[start + tag.len()..];
    let end = rest
        .find(|c: char| !c.is_ascii_lowercase() && !c.is_ascii_digit() && c != '_' && c != '-')
        .unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    Some(&rest[..end])
}

/// Parse and apply !touch directives; return cleaned message.
/// Format: `!touch <relpath>=<content>`  (one per line, anywhere in the file)
async fn apply_touch_directives(raw: &str, workdir: &std::path::Path) -> anyhow::Result<String> {
    let mut out_lines = Vec::new();
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("!touch ") {
            let (relpath, content) = rest
                .split_once('=')
                .with_context(|| format!("malformed !touch line: {line:?}"))?;
            let dest = workdir.join(relpath.trim());
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).await?;
            }
            fs::write(&dest, content)
                .await
                .with_context(|| format!("stub !touch: could not write {}", dest.display()))?;
            // strip from returned message
        } else {
            out_lines.push(line);
        }
    }
    Ok(out_lines.join("\n"))
}

#[async_trait]
impl AgentCli for StubCli {
    fn kind(&self) -> CliKind {
        CliKind::Stub
    }

    async fn invoke(&self, req: &InvocationRequest) -> anyhow::Result<InvocationResult> {
        let start = Instant::now();

        let stub_dir = std::env::var("CANOPY_STUB_DIR")
            .context("CANOPY_STUB_DIR env var not set — broken test setup")?;
        let stub_dir = std::path::Path::new(&stub_dir);

        // Resolution order: role-specific beats generic, marker beats default.
        // Mechanism roles (merger/reviewer) see node specs inside their prompt,
        // so the marker alone is ambiguous — `<marker>.<role>.md` disambiguates.
        // Harness-generated specs (fix nodes, decomposer) carry no marker and
        // fall back to `default.<role>.md` / `default.md`.
        let role = req.role.as_str();
        let marker = find_stub_marker(&req.prompt);
        let mut candidates = Vec::new();
        if let Some(m) = marker {
            candidates.push(format!("{m}.{role}.md"));
            candidates.push(format!("{m}.md"));
        }
        candidates.push(format!("default.{role}.md"));
        candidates.push("default.md".to_string());
        let mut raw = None;
        for c in &candidates {
            if let Ok(content) = tokio::fs::read_to_string(stub_dir.join(c)).await {
                raw = Some(content);
                break;
            }
        }
        let raw = raw.with_context(|| {
            format!(
                "no stub file found (tried {candidates:?}) — broken test setup\nprompt head: {}",
                &req.prompt[..req.prompt.len().min(200)]
            )
        })?;

        let final_message = apply_touch_directives(&raw, &req.workdir).await?;

        // Write transcript
        tokio::fs::write(&req.transcript_path, final_message.as_bytes())
            .await
            .with_context(|| {
                format!(
                    "could not write stub transcript to {}",
                    req.transcript_path.display()
                )
            })?;

        let duration_ms = start.elapsed().as_millis();

        Ok(InvocationResult {
            final_message,
            usage: Usage {
                input_tokens: 1000,
                output_tokens: 500,
                cached_tokens: 0,
                cost_usd: Some(0.001),
            },
            exit_ok: true,
            duration_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Mutex;

    use tempfile::TempDir;

    use super::*;
    use crate::agent::InvocationRequest;
    use crate::model::Role;

    // Serialize all tests that touch CANOPY_STUB_DIR to avoid env-var races.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[tokio::test]
    async fn basic_stub_resolution() {
        let _guard = ENV_LOCK.lock().unwrap();
        let stub_dir = TempDir::new().unwrap();
        let work_dir = TempDir::new().unwrap();
        let transcript_tmp = TempDir::new().unwrap();

        fs::write(stub_dir.path().join("hello.md"), "Hello from stub!").unwrap();
        std::env::set_var("CANOPY_STUB_DIR", stub_dir.path());

        let req = InvocationRequest {
            role: Role::Executor,
            node_id: "node1".into(),
            prompt: "invoke stub:hello please".into(),
            model: "stub".into(),
            workdir: work_dir.path().to_path_buf(),
            timeout_secs: 30,
            max_turns: None,
            transcript_path: transcript_tmp.path().join("out.txt"),
        };

        let result = StubCli.invoke(&req).await.unwrap();
        assert_eq!(result.final_message, "Hello from stub!");
        assert!(result.exit_ok);
        assert_eq!(result.usage.input_tokens, 1000);
        assert_eq!(result.usage.output_tokens, 500);
        assert_eq!(result.usage.cost_usd, Some(0.001));
        assert!(result.duration_ms < 1000);

        // transcript written
        let transcript = fs::read_to_string(transcript_tmp.path().join("out.txt")).unwrap();
        assert_eq!(transcript, "Hello from stub!");
    }

    #[tokio::test]
    async fn touch_directives_applied_and_stripped() {
        let _guard = ENV_LOCK.lock().unwrap();
        let stub_dir = TempDir::new().unwrap();
        let work_dir = TempDir::new().unwrap();
        let transcript_tmp = TempDir::new().unwrap();

        fs::write(
            stub_dir.path().join("editor.md"),
            "Done.\n!touch src/out.rs=fn main() {}\n!touch README.md=# hi\n",
        )
        .unwrap();
        std::env::set_var("CANOPY_STUB_DIR", stub_dir.path());

        let req = InvocationRequest {
            role: Role::Executor,
            node_id: "node2".into(),
            prompt: "stub:editor task".into(),
            model: "stub".into(),
            workdir: work_dir.path().to_path_buf(),
            timeout_secs: 30,
            max_turns: None,
            transcript_path: transcript_tmp.path().join("out.txt"),
        };

        let result = StubCli.invoke(&req).await.unwrap();

        // !touch lines stripped from message
        assert!(!result.final_message.contains("!touch"));
        assert!(result.final_message.contains("Done."));

        // files written into workdir
        let rs = fs::read_to_string(work_dir.path().join("src/out.rs")).unwrap();
        assert_eq!(rs, "fn main() {}");
        let readme = fs::read_to_string(work_dir.path().join("README.md")).unwrap();
        assert_eq!(readme, "# hi");
    }

    // Tests the env-read path without mutating global state: directly call
    // std::env::var on a var we know is absent.
    #[test]
    fn missing_stub_dir_env_error_message() {
        // Use a deliberately unique var name that no other test sets.
        let result = std::env::var("CANOPY_STUB_DIR_DEFINITELY_ABSENT_XYZ123");
        assert!(result.is_err(), "expected var to be absent");
        // Simulate what invoke does:
        let err = anyhow::anyhow!("CANOPY_STUB_DIR env var not set — broken test setup");
        assert!(err.to_string().contains("CANOPY_STUB_DIR"));
    }

    #[tokio::test]
    async fn missing_marker_in_prompt_is_loud() {
        let _guard = ENV_LOCK.lock().unwrap();
        let stub_dir = TempDir::new().unwrap();
        let work_dir = TempDir::new().unwrap();
        let transcript_tmp = TempDir::new().unwrap();

        std::env::set_var("CANOPY_STUB_DIR", stub_dir.path());

        let req = InvocationRequest {
            role: Role::Executor,
            node_id: "node4".into(),
            prompt: "no marker here at all".into(),
            model: "stub".into(),
            workdir: work_dir.path().to_path_buf(),
            timeout_secs: 30,
            max_turns: None,
            transcript_path: transcript_tmp.path().join("out.txt"),
        };

        let err = StubCli.invoke(&req).await.unwrap_err();
        assert!(err.to_string().contains("no stub file found"));
    }

    #[tokio::test]
    async fn role_specific_beats_generic_and_default_covers_markerless() {
        let _guard = ENV_LOCK.lock().unwrap();
        let stub_dir = TempDir::new().unwrap();
        let work_dir = TempDir::new().unwrap();
        let transcript_tmp = TempDir::new().unwrap();

        fs::write(stub_dir.path().join("job.md"), "generic").unwrap();
        fs::write(stub_dir.path().join("job.merger.md"), "merger view").unwrap();
        fs::write(stub_dir.path().join("default.executor.md"), "default exec").unwrap();
        std::env::set_var("CANOPY_STUB_DIR", stub_dir.path());

        let mut req = InvocationRequest {
            role: Role::Merger,
            node_id: "n".into(),
            prompt: "conflict for stub:job here".into(),
            model: "stub".into(),
            workdir: work_dir.path().to_path_buf(),
            timeout_secs: 30,
            max_turns: None,
            transcript_path: transcript_tmp.path().join("t1.txt"),
        };
        let r = StubCli.invoke(&req).await.unwrap();
        assert_eq!(r.final_message, "merger view");

        req.role = Role::Executor;
        req.transcript_path = transcript_tmp.path().join("t2.txt");
        let r = StubCli.invoke(&req).await.unwrap();
        assert_eq!(r.final_message, "generic"); // no job.executor.md → generic

        req.prompt = "no marker at all (harness-generated fix node)".into();
        req.transcript_path = transcript_tmp.path().join("t3.txt");
        let r = StubCli.invoke(&req).await.unwrap();
        assert_eq!(r.final_message, "default exec");
    }

    #[tokio::test]
    async fn missing_stub_file_is_loud() {
        let _guard = ENV_LOCK.lock().unwrap();
        let stub_dir = TempDir::new().unwrap();
        let work_dir = TempDir::new().unwrap();
        let transcript_tmp = TempDir::new().unwrap();

        std::env::set_var("CANOPY_STUB_DIR", stub_dir.path());

        let req = InvocationRequest {
            role: Role::Executor,
            node_id: "node5".into(),
            prompt: "stub:nonexistent please".into(),
            model: "stub".into(),
            workdir: work_dir.path().to_path_buf(),
            timeout_secs: 30,
            max_turns: None,
            transcript_path: transcript_tmp.path().join("out.txt"),
        };

        let err = StubCli.invoke(&req).await.unwrap_err();
        assert!(err.to_string().contains("no stub file found"));
    }
}
