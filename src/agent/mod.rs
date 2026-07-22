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
pub fn trailing_json(message: &str) -> Option<&str> {
    let open = message.rfind("```json")?;
    let after = &message[open + 7..];
    let close = after.find("```")?;
    Some(after[..close].trim())
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
}
