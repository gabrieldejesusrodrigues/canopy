mod agent;
mod config;
#[cfg(test)]
mod e2e;
mod gitops;
mod ledger;
mod mechanisms;
mod model;
mod prompt;
mod scheduler;
mod tracker;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::model::NodeState;

#[derive(Parser)]
#[command(
    name = "canopy",
    about = "Agent swarm harness — trees/trunks/leaves over claude/codex/agy",
    version
)]
struct Cli {
    /// Path to canopy.toml
    #[arg(short, long, default_value = "canopy.toml", global = true)]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Write a starter canopy.toml next to the target repo
    Init {
        /// Target repository the swarm will work on
        repo: PathBuf,
    },
    /// Start (or resume) a swarm run
    Run {
        /// The objective for the root planner
        objective: Option<String>,
        /// Resume a paused/crashed run by id
        #[arg(long)]
        resume: Option<String>,
    },
    /// Print the task tree of a run
    Status {
        #[arg(long)]
        run: Option<String>,
    },
    /// Article-style economics report (tokens vs cost, by role and model)
    Report {
        #[arg(long)]
        run: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "canopy=info".into()),
        )
        .init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Init { repo } => init(repo),
        Cmd::Run { objective, resume } => {
            let cfg = config::Config::load(&cli.config)?;
            scheduler::Scheduler::start(cfg, objective, resume).await
        }
        Cmd::Status { run } => {
            let cfg = config::Config::load(&cli.config)?;
            status(&cfg, run).await
        }
        Cmd::Report { run } => {
            let cfg = config::Config::load(&cli.config)?;
            let run_id = resolve_run(&cfg, run)?;
            let ledger = ledger::Ledger::open(&cfg.state_dir().join("ledger.db"))?;
            println!("{}", ledger.report(&run_id)?);
            Ok(())
        }
    }
}

const CONFIG_TEMPLATE: &str = r#"[run]
repo = "%REPO%"
verify = "true"          # replace with your build/test command, e.g. "cargo test"
tracker = "sqlite"       # or "linear" (requires [linear] section + LINEAR_API_KEY)

# [linear]
# team_id = "<team uuid>"

[budgets]
max_usd = 10.0
max_parallel = 4
max_parallel_planners = 2
max_attempts = 3
max_tree_depth = 4
lease_secs = 2400
agent_timeout_secs = 1800
max_turns = 50

[thresholds]
megafile_lines = 1000
fieldguide_line_budget = 200

[routing]
mode = "static"          # or "planner-routed"
planner = { cli = "claude", model = "opus" }
executor = { cli = "codex", model = "gpt-5.4-mini" }
merger = { cli = "claude", model = "sonnet" }
# Cheap first attempt on conflicts; `merger` takes over when it fails or its
# resolution bounces on the post-merge gates.
# merger_triage = { cli = "claude", model = "haiku" }
reviewers = [
  { cli = "agy", model = "Gemini 3.6 Flash (Low)", lens = "output" },
  { cli = "claude", model = "haiku", lens = "codebase" },
]

# Used when mode = "planner-routed": the planner assigns each child an agent
# from this list, matching difficulty to tier.
[[routing.allowlist]]
cli = "codex"
model = "gpt-5.4-mini"
good_for = "well-specified implementation, tests, mechanical refactors"

[[routing.allowlist]]
cli = "claude"
model = "sonnet"
good_for = "subtle or cross-cutting implementation work"

# USD per 1M tokens for CLIs that report tokens but not cost (codex).
[pricing]
"gpt-5.4-mini" = { input = 0.25, output = 2.00 }
"#;

fn init(repo: PathBuf) -> Result<()> {
    let repo = repo.canonicalize().context("target repo must exist")?;
    let cfg_path = std::env::current_dir()?.join("canopy.toml");
    anyhow::ensure!(!cfg_path.exists(), "canopy.toml already exists here");
    std::fs::write(
        &cfg_path,
        CONFIG_TEMPLATE.replace("%REPO%", &repo.display().to_string()),
    )?;
    println!(
        "wrote {} targeting {}\nEdit `verify` and routing, then: canopy run \"<objective>\"",
        cfg_path.display(),
        repo.display()
    );
    Ok(())
}

fn resolve_run(cfg: &config::Config, run: Option<String>) -> Result<String> {
    if let Some(r) = run {
        return Ok(r);
    }
    let last = cfg.state_dir().join("last-run");
    std::fs::read_to_string(&last)
        .map(|s| s.trim().to_owned())
        .context("no --run given and no previous run found in this repo")
}

async fn status(cfg: &config::Config, run: Option<String>) -> Result<()> {
    let run_id = resolve_run(cfg, run)?;
    let tracker = tracker::from_config(cfg).await?;
    let run = tracker.load_run(&run_id).await?;
    println!(
        "run {} — branch {}\nobjective: {}\n",
        run.id, run.branch, run.objective
    );
    print_subtree(tracker.as_ref(), &run.root_node, 0).await?;
    Ok(())
}

async fn print_subtree(tracker: &dyn tracker::Tracker, node_id: &str, depth: usize) -> Result<()> {
    let node = tracker.node(node_id).await?;
    let marker = match node.state {
        NodeState::Done => "✓",
        NodeState::Failed => "✗",
        NodeState::Running | NodeState::Merging => "▶",
        NodeState::Blocked => "⏸",
        _ => "·",
    };
    println!(
        "{}{} [{}] {} ({})",
        "  ".repeat(depth),
        marker,
        node.state.as_str(),
        node.title,
        node.id
    );
    for child in tracker.children(node_id).await? {
        Box::pin(print_subtree(tracker, &child.id, depth + 1)).await?;
    }
    Ok(())
}
