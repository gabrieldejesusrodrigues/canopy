//! Prompt assembly: role contract ⊕ field guide ⊕ context sections.
//! No template engine — contracts are static text and reference the section
//! headers emitted here (## FIELD GUIDE, ## WORK UNIT, ...).

use crate::config::AllowlistEntry;
use crate::mechanisms::designdocs::DesignDoc;
use crate::model::Lens;

const PLANNER: &str = include_str!("../prompts/planner.md");
const EXECUTOR: &str = include_str!("../prompts/executor.md");
const MERGER: &str = include_str!("../prompts/merger.md");
const RECONCILER: &str = include_str!("../prompts/reconciler.md");
const DECOMPOSER: &str = include_str!("../prompts/decomposer.md");
const REVIEWER_TRANSCRIPT: &str = include_str!("../prompts/reviewer_transcript.md");
const REVIEWER_OUTPUT: &str = include_str!("../prompts/reviewer_output.md");
const REVIEWER_CODEBASE: &str = include_str!("../prompts/reviewer_codebase.md");

fn assemble(fieldguide: &str, contract: &str, sections: &[(&str, &str)]) -> String {
    let mut s = String::new();
    if !fieldguide.trim().is_empty() {
        s.push_str("## FIELD GUIDE\n\n");
        s.push_str(fieldguide.trim());
        s.push_str("\n\n");
    }
    s.push_str(contract.trim());
    s.push('\n');
    for (header, body) in sections {
        if body.trim().is_empty() {
            continue;
        }
        s.push_str(&format!("\n## {header}\n\n{}\n", body.trim()));
    }
    s
}

fn docs_section(docs: &[DesignDoc]) -> String {
    docs.iter()
        .filter(|d| d.meta.status == "active")
        .map(|d| {
            format!(
                "### {} — {} (topics: {})\n\n{}",
                d.meta.id,
                d.meta.title,
                d.meta.topics.join(", "),
                d.body.trim()
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn allowlist_section(allowlist: &[AllowlistEntry]) -> String {
    allowlist
        .iter()
        .map(|e| {
            format!(
                "- cli: {} model: {} — {}",
                serde_json::to_string(&e.cli).unwrap().trim_matches('"'),
                e.model,
                e.good_for
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn planner(
    fieldguide: &str,
    spec: &str,
    docs: &[DesignDoc],
    allowlist: Option<&[AllowlistEntry]>,
    replan_context: Option<&str>,
) -> String {
    assemble(
        fieldguide,
        PLANNER,
        &[
            ("WORK UNIT", spec),
            ("DESIGN DOCS", &docs_section(docs)),
            (
                "ALLOWLIST",
                &allowlist.map(allowlist_section).unwrap_or_default(),
            ),
            ("REPLAN", replan_context.unwrap_or_default()),
        ],
    )
}

pub fn executor(
    fieldguide: &str,
    spec: &str,
    docs: &[DesignDoc],
    retry_context: Option<&str>,
) -> String {
    assemble(
        fieldguide,
        EXECUTOR,
        &[
            ("WORK UNIT", spec),
            ("DESIGN DOCS", &docs_section(docs)),
            ("VERIFY FAILURE", retry_context.unwrap_or_default()),
        ],
    )
}

pub fn merger(fieldguide: &str, conflict: &str, docs: &[DesignDoc]) -> String {
    assemble(
        fieldguide,
        MERGER,
        &[("CONFLICT", conflict), ("DESIGN DOCS", &docs_section(docs))],
    )
}

pub fn reconciler(fieldguide: &str, conflict: &str) -> String {
    assemble(fieldguide, RECONCILER, &[("CONFLICT", conflict)])
}

pub fn decomposer(fieldguide: &str, spec: &str, docs: &[DesignDoc]) -> String {
    assemble(
        fieldguide,
        DECOMPOSER,
        &[("WORK UNIT", spec), ("DESIGN DOCS", &docs_section(docs))],
    )
}

pub fn reviewer(
    fieldguide: &str,
    lens: Lens,
    spec: &str,
    diff: &str,
    transcript: Option<&str>,
    touched_files: &[String],
) -> String {
    let contract = match lens {
        Lens::Transcript => REVIEWER_TRANSCRIPT,
        Lens::Output => REVIEWER_OUTPUT,
        Lens::Codebase => REVIEWER_CODEBASE,
    };
    let files = touched_files.join("\n");
    let sections: Vec<(&str, &str)> = match lens {
        // Codebase lens deliberately sees no diff/history — only the repo it
        // sits in (its worktree), what unit was requested, and which files
        // that unit touched (so it knows where to look).
        Lens::Codebase => vec![("WORK UNIT", spec), ("FILES", &files)],
        Lens::Output => vec![("WORK UNIT", spec), ("DIFF", diff)],
        Lens::Transcript => vec![
            ("WORK UNIT", spec),
            ("DIFF", diff),
            (
                "TRANSCRIPT",
                transcript.unwrap_or("(transcript unavailable)"),
            ),
        ],
    };
    assemble(fieldguide, contract, &sections)
}

/// Nudge appended on a structured-output parse failure (single retry).
pub fn json_retry_nudge(prompt: &str, error: &str) -> String {
    format!(
        "{prompt}\n\n## RETRY\n\nYour previous attempt did not end with a valid \
         fenced ```json block ({error}). Redo the task if needed and END your \
         final message with the required ```json block. No text after it.\n"
    )
}
