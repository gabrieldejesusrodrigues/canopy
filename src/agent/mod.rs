//! Headless CLI adapters. One trait; claude/codex/agy/stub behind it.
//! Agents are one-shot, stateless processes: prompt in, transcript +
//! final message + usage out.

pub mod agy;
pub mod claude;
pub mod codex;
pub mod stub;

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use crate::model::{AgentRef, CliKind, Role};

#[derive(Debug, Clone)]
pub struct InvocationRequest {
    pub role: Role,
    pub node_id: String,
    /// Full assembled prompt (role contract ⊕ field guide ⊕ spec ⊕ design docs).
    pub prompt: String,
    pub model: String,
    /// Working directory the agent may write to (its worktree).
    pub workdir: PathBuf,
    pub timeout_secs: u64,
    pub max_turns: Option<u32>,
    /// Where the adapter should persist the raw transcript (JSONL/text).
    pub transcript_path: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    /// Only some CLIs report money directly (claude does).
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct InvocationResult {
    /// The agent's final message (we parse the trailing JSON block from it).
    pub final_message: String,
    pub usage: Usage,
    pub exit_ok: bool,
    pub duration_ms: u128,
}

#[async_trait]
pub trait AgentCli: Send + Sync {
    fn kind(&self) -> CliKind;
    async fn invoke(&self, req: &InvocationRequest) -> Result<InvocationResult>;
}

/// Resolve an AgentRef to its adapter.
pub fn for_ref(agent: &AgentRef) -> Box<dyn AgentCli> {
    match agent.cli {
        CliKind::Claude => Box::new(claude::ClaudeCli),
        CliKind::Codex => Box::new(codex::CodexCli),
        CliKind::Agy => Box::new(agy::AgyCli),
        CliKind::Stub => Box::new(stub::StubCli),
    }
}

/// Extract the trailing fenced ```json block from an agent's final message.
/// Contracts require it; parse failures trigger one retry with a nudge.
///
/// After the last ```json opener, try every subsequent closing fence in order
/// and return the first slice that parses as JSON — otherwise valid JSON whose
/// string content embeds ``` (e.g. a planner child spec with a code fence)
/// would be truncated at the wrong fence. Falls back to the first fence.
pub fn trailing_json(message: &str) -> Option<&str> {
    let open = message.rfind("```json")?;
    let after = &message[open + 7..];

    let mut first_fence: Option<&str> = None;
    let mut search_from = 0;
    while let Some(rel) = after[search_from..].find("```") {
        let close = search_from + rel;
        let slice = after[..close].trim();
        if first_fence.is_none() {
            first_fence = Some(slice);
        }
        if serde_json::from_str::<serde_json::Value>(slice).is_ok() {
            return Some(slice);
        }
        search_from = close + 3;
    }

    first_fence
}

#[cfg(unix)]
pub(crate) mod proc {
    use tokio::process::Command;

    /// Put the child in its own process group so a timeout can kill the whole
    /// tree (claude/codex/agy spawn children), not just the direct process.
    /// pgid == child pid because of process_group(0).
    pub fn own_group(cmd: &mut Command) {
        cmd.process_group(0);
    }

    /// Kill the process group by pid. Shells out to `kill` to avoid a libc dep.
    pub async fn kill_group(pid: u32) {
        let _ = Command::new("kill")
            .args(["-9", &format!("-{pid}")])
            .status()
            .await;
    }
}

#[cfg(not(unix))]
pub(crate) mod proc {
    use tokio::process::Command;
    pub fn own_group(_cmd: &mut Command) {}
    pub async fn kill_group(_pid: u32) {}
}

#[cfg(test)]
mod tests {
    use super::trailing_json;

    #[test]
    fn extracts_last_json_block() {
        let msg = "prose\n```json\n{\"a\":1}\n```\nmore\n```json\n{\"b\":2}\n```\n";
        assert_eq!(trailing_json(msg), Some("{\"b\":2}"));
        assert_eq!(trailing_json("no block"), None);
    }

    #[test]
    fn json_string_value_may_contain_triple_backticks() {
        // The JSON string embeds a ``` fence; the naive first-fence cut would
        // truncate at it and fail to parse. We must find the real closing fence.
        let msg = "```json\n{\"spec\":\"run ```rust\\nfn main(){}\\n``` now\"}\n```\n";
        let got = trailing_json(msg).unwrap();
        assert!(serde_json::from_str::<serde_json::Value>(got).is_ok());
        assert!(got.contains("```rust"));
    }

    #[test]
    fn falls_back_to_first_fence_when_nothing_parses() {
        let msg = "```json\nnot valid json ``` still not\n```\n";
        assert_eq!(trailing_json(msg), Some("not valid json"));
    }
}
