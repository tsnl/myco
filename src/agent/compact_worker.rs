//! The `/compact` worker lifecycle: run a hidden agent that summarises a
//! session, then hand back the successor it seeds.
//!
//! An agent run: it builds a model, drives one turn against the harness to get
//! the summary written, then calls the session-side document logic
//! ([`crate::session::compact_session`], [`crate::session::link_compact_pair`])
//! to seed and link the successor.

use std::path::Path;
use std::sync::Arc;

use crate::core::{CancelToken, uuid_simple_hex};
use crate::generative_model::{self, CatalogModel, Content, GenerativeModelConfig};
use crate::harness::Harness;
use crate::prompts;
use crate::session::{CompactOutcome, Session, SessionKind, compact_session, link_compact_pair};

use super::{Agent, AgentInteractionError, NullEventSink, TraceContext};

/// Read the summary this run wrote, where `before` is the file's content from
/// before the worker started. Unchanged content is an error.
///
/// `{id}.summary.md` survives an earlier compaction of the same predecessor, so
/// its presence alone is not proof this run wrote one: a worker that ends its
/// turn without calling `write_summary` would otherwise get the previous
/// compaction's text — describing a shorter conversation — folded into the
/// successor with no error anywhere. The stale file stays on disk, because the
/// successor built by that earlier compaction names it by path.
fn read_fresh_summary(path: &Path, before: Option<&str>) -> Result<String, String> {
    let summary = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            return Err(format!(
                "worker finished but summary missing at {}: {e}",
                path.display()
            ));
        }
    };
    if summary.trim().is_empty() {
        return Err(format!(
            "worker finished but summary file is empty ({})",
            path.display()
        ));
    }
    if before == Some(summary.as_str()) {
        return Err(format!(
            "worker finished without writing a new summary; {} still holds the previous \
             compaction's text (session unchanged)",
            path.display()
        ));
    }
    Ok(summary)
}

/// How [`run_compact_worker`] ended without producing a successor.
#[derive(Debug)]
pub enum CompactWorkerError {
    /// The worker turn was cancelled (Ctrl-C); the predecessor is unchanged.
    Cancelled,
    /// Any other failure, as a printable reason.
    Failed(String),
}

/// The whole compact worker lifecycle against a saved `predecessor`: create
/// the hidden worker session, run the worker agent (which writes the summary
/// via `session_history`), read the summary back, then build and link the
/// successor. Pure orchestration — the caller owns all UI (progress, error
/// display) and installing the successor into the live REPL.
pub async fn run_compact_worker(
    predecessor: &Session,
    catalog_model: &CatalogModel,
    harness: Arc<Harness>,
    max_memory_bytes: usize,
    cancel: CancelToken,
) -> Result<(Session, CompactOutcome), CompactWorkerError> {
    let worker_id = uuid::Uuid::new_v4();
    let worker_hex = uuid_simple_hex(worker_id);
    let mut worker_session = Session::new_hidden(
        catalog_model.spec.key.clone(),
        worker_hex.clone(),
        SessionKind::Compact,
        Some(predecessor.id.clone()),
    );
    worker_session.title = Some(format!(
        "compact {}",
        &predecessor.id[..8.min(predecessor.id.len())]
    ));
    if let Err(e) = worker_session.save() {
        eprintln!("warning: could not save compact worker session: {e}");
    }

    // What the summary file holds before the worker runs, so a worker that never
    // writes one cannot have stale text compacted in (see `read_fresh_summary`).
    let summary_path = predecessor.summary_path();
    let summary_before = std::fs::read_to_string(&summary_path).ok();

    let model = match generative_model::new(GenerativeModelConfig {
        model: catalog_model.spec.clone(),
        tools: harness.tool_specs(),
        system_prompt: [
            "You are a myco compaction worker. Follow the user instruction exactly. \
             Prefer session_history over bash for reading sessions."
                .to_string(),
            prompts::agent_prompt_epilogue(max_memory_bytes),
            prompts::model_stamp(&catalog_model.spec.key),
        ]
        .join("\n\n"),
        backend_config: catalog_model.backend.clone(),
    }) {
        Ok(m) => m,
        Err(e) => {
            return Err(CompactWorkerError::Failed(format!(
                "failed to create model: {e:?}"
            )));
        }
    };

    let sink = Arc::new(NullEventSink);
    let mut worker = Agent::with_context(
        model,
        harness.clone(),
        sink,
        TraceContext {
            agent_id: worker_id,
            depth: 1,
        },
    );
    worker.set_context_window_tokens(catalog_model.spec.context_window_tokens);

    let prompt = compact_subagent_prompt(&predecessor.id);
    let result = worker
        .interact(vec![Content::Text { text: prompt }], cancel)
        .await;

    worker_session.messages = worker.history().to_vec();
    worker_session.touch();
    if let Err(e) = worker_session.save() {
        eprintln!("warning: could not save compact worker session: {e}");
    }

    match result {
        Ok(_) => {}
        Err(AgentInteractionError::Cancelled) => return Err(CompactWorkerError::Cancelled),
        Err(e) => return Err(CompactWorkerError::Failed(format!("worker failed: {e}"))),
    }

    let summary = read_fresh_summary(&summary_path, summary_before.as_deref())
        .map_err(CompactWorkerError::Failed)?;

    let (successor, outcome) = compact_session(predecessor, &summary, &catalog_model.spec.key)
        .map_err(|e| CompactWorkerError::Failed(format!("failed to build successor: {e}")))?;

    let mut pred = predecessor.clone();
    link_compact_pair(&mut pred, &successor)
        .map_err(|e| CompactWorkerError::Failed(format!("failed to link sessions: {e}")))?;

    Ok((successor, outcome))
}

/// Prompt for a compact subagent.
pub fn compact_subagent_prompt(predecessor_id: &str) -> String {
    format!(
        r#"You are a compaction worker. Explore session `{predecessor_id}` with the `session_history` tool (stats, search, range, expand). Do NOT use bash to read session JSON.

Write a concise markdown summary via `session_history` action `write_summary` for that same session_id. The summary MUST use these headings:

# Goal / active task
# Decisions
# Key paths
# Todos / open work
# Constraints
# Recent outcome

Rules:
- Prefer absolute paths, hosts, branch names, PR links, and concrete decisions.
- Drop raw tool stdout and exploratory dead-ends unless they constrain next steps.
- Keep the whole summary under ~1500 tokens.
- After write_summary succeeds, reply with only: SUMMARY_OK path=<path from tool>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A summary file left behind by an earlier compaction must not be mistaken
    /// for this run's output: reusing it would compact a summary describing a
    /// different conversation, with no error anywhere.
    #[test]
    fn a_stale_summary_is_rejected_instead_of_reused() {
        let tmp = crate::test_support::temp_dir("compact-stale");
        let dir = tmp.path();
        let path = dir.join("s.summary.md");
        let previous = "# Summary of an earlier compaction\n";
        std::fs::write(&path, previous).unwrap();

        let err = read_fresh_summary(&path, Some(previous)).expect_err("stale must be rejected");
        assert!(err.contains("without writing a new summary"), "{err}");
        assert!(err.contains("session unchanged"), "{err}");

        // The old summary is left on disk — a successor from the earlier
        // compaction still references it.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), previous);

        // A worker that actually rewrote it is accepted.
        std::fs::write(&path, "# Fresh summary\n").unwrap();
        assert_eq!(
            read_fresh_summary(&path, Some(previous)).unwrap(),
            "# Fresh summary\n"
        );
    }

    #[test]
    fn missing_or_empty_summary_fails_loudly() {
        let tmp = crate::test_support::temp_dir("compact-missing");
        let dir = tmp.path();
        let path = dir.join("s.summary.md");

        let err = read_fresh_summary(&path, None).expect_err("missing must fail");
        assert!(err.contains("summary missing"), "{err}");

        std::fs::write(&path, "   \n\t\n").unwrap();
        let err = read_fresh_summary(&path, None).expect_err("empty must fail");
        assert!(err.contains("summary file is empty"), "{err}");

        // First-ever compaction: nothing there before, so any content is fresh.
        std::fs::write(&path, "# First\n").unwrap();
        assert_eq!(read_fresh_summary(&path, None).unwrap(), "# First\n");
    }
}
