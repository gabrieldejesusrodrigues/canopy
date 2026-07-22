//! End-to-end smoke test: a full swarm run over a temp repo with stub agents
//! (zero tokens spent). Exercises: planner decomposition, design docs,
//! parallel executor worktrees, the serialized merge queue, a real add/add
//! merge conflict resolved by the neutral Merger, review lenses producing a
//! fix node, and the completion cascade.

use std::path::Path;
use std::process::Command;

use crate::config::Config;
use crate::scheduler::Scheduler;

fn sh(dir: &Path, cmd: &str) {
    let out = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`{cmd}` failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn write(dir: &Path, name: &str, content: &str) {
    std::fs::write(dir.join(name), content).unwrap();
}

const DONE_JSON: &str =
    "```json\n{\"status\":\"done\",\"summary\":\"done\",\"flagged_files\":[],\"breaks\":[]}\n```";

#[tokio::test(flavor = "multi_thread")]
async fn swarm_e2e_with_stub_agents() {
    // -- target repo the swarm will work on --
    let target = tempfile::tempdir().unwrap();
    sh(target.path(), "git init -qb main");
    sh(
        target.path(),
        "git config user.name canopy-test && git config user.email t@t",
    );
    write(target.path(), "README.md", "demo\n");
    sh(target.path(), "git add -A && git commit -qm init");

    // -- stub agent responses --
    let stubs = tempfile::tempdir().unwrap();
    write(
        stubs.path(),
        "planner-root.md",
        r#"Decomposing.
```json
{"children":[
  {"title":"part A","kind":"execute","spec":"stub:exec-a Create alpha honoring DD-1.","depends_on":[]},
  {"title":"part B","kind":"execute","spec":"stub:exec-b Create beta honoring DD-1.","depends_on":[]}
],"design_decisions":[
  {"id":"DD-1","title":"Naming convention","topics":["naming"],"content":"All demo files use lowercase names."}
]}
```"#,
    );
    write(
        stubs.path(),
        "exec-a.md",
        &format!("!touch alpha.txt=alpha\n!touch conflict.txt=A version\nDid A.\n{DONE_JSON}"),
    );
    write(
        stubs.path(),
        "exec-b.md",
        &format!("!touch beta.txt=beta\n!touch conflict.txt=B version\nDid B.\n{DONE_JSON}"),
    );
    // The Merger sees the second node's spec inside CONFLICT (marker exec-a or
    // exec-b depending on merge order) — same neutral resolution either way.
    let merger = "!touch conflict.txt=resolved\nResolved impartially.\n```json\n{\"resolved\":true,\"summary\":\"kept a merged version\"}\n```";
    write(stubs.path(), "exec-a.merger.md", merger);
    write(stubs.path(), "exec-b.merger.md", merger);
    // Review lens: one high finding on part A forces a fix node; B is clean.
    write(
        stubs.path(),
        "exec-a.reviewer.md",
        "```json\n{\"findings\":[{\"severity\":\"high\",\"file\":\"alpha.txt\",\"description\":\"alpha.txt must end with a newline\"}]}\n```",
    );
    write(
        stubs.path(),
        "exec-b.reviewer.md",
        "```json\n{\"findings\":[]}\n```",
    );
    // Harness-generated nodes (the fix node) carry no stub marker → defaults.
    write(
        stubs.path(),
        "default.executor.md",
        &format!("!touch alpha.txt=alpha fixed\nFixed the finding.\n{DONE_JSON}"),
    );
    write(
        stubs.path(),
        "default.reviewer.md",
        "```json\n{\"findings\":[]}\n```",
    );
    std::env::set_var("CANOPY_STUB_DIR", stubs.path());

    // -- config --
    let cfg_dir = tempfile::tempdir().unwrap();
    let toml = format!(
        r#"
[run]
repo = "{repo}"
verify = "true"
tracker = "sqlite"

[budgets]
max_usd = 5.0
max_parallel = 2
max_parallel_planners = 1
max_attempts = 2
max_tree_depth = 3
lease_secs = 600
agent_timeout_secs = 60

[routing]
mode = "static"
planner = {{ cli = "stub", model = "stub" }}
executor = {{ cli = "stub", model = "stub" }}
merger = {{ cli = "stub", model = "stub" }}
reviewers = [ {{ cli = "stub", model = "stub", lens = "output" }} ]
"#,
        repo = target.path().display()
    );
    let cfg_path = cfg_dir.path().join("canopy.toml");
    std::fs::write(&cfg_path, toml).unwrap();
    let cfg = Config::load(&cfg_path).unwrap();
    let state_dir = cfg.state_dir();

    // -- run the swarm to completion --
    tokio::time::timeout(
        std::time::Duration::from_secs(120),
        Scheduler::start(cfg, Some("stub:planner-root Build the demo.".into()), None),
    )
    .await
    .expect("swarm did not finish in time")
    .expect("swarm run failed");

    // -- assert the run branch state (the merge worktree sits on it) --
    let merged = state_dir.join("merge");
    let read = |f: &str| std::fs::read_to_string(merged.join(f)).unwrap_or_default();
    assert_eq!(
        read("conflict.txt"),
        "resolved",
        "merger resolution must win"
    );
    assert_eq!(
        read("alpha.txt"),
        "alpha fixed",
        "fix node must have landed"
    );
    assert_eq!(read("beta.txt"), "beta");
    assert!(
        merged.join("fieldguide/index.md").exists(),
        "field guide scaffold"
    );
    let dd: Vec<_> = std::fs::read_dir(merged.join("design"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("DD-1"))
        .collect();
    assert!(!dd.is_empty(), "design decision DD-1 must be committed");

    // Everything on the run branch is committed (no dirty state left behind).
    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&merged)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&status.stdout).trim(), "");

    // -- ledger recorded the economics --
    let ledger = crate::ledger::Ledger::open(&state_dir.join("ledger.db")).unwrap();
    let run_id = std::fs::read_to_string(state_dir.join("last-run")).unwrap();
    let cost = ledger.total_cost(run_id.trim()).unwrap();
    assert!(cost > 0.0, "invocations must be recorded");
    let report = ledger.report(run_id.trim()).unwrap();
    assert!(
        report.contains("planner"),
        "report breaks down by role:\n{report}"
    );
    assert!(report.contains("executor"), "report:\n{report}");
    assert!(
        report.contains("merger"),
        "the merger ran and was recorded:\n{report}"
    );
    assert!(report.contains("reviewer"), "reviews ran:\n{report}");
}
