//! Owner-only debug surface for the observation journal.
//!
//! `temote-mcp observation ...` reads local journal state for the operator.
//! These commands never run inside the MCP boundary: the remote surface sees
//! only the bounded `context_resolve` / `context_status` projections.

use anyhow::Result;
use serde_json::json;
use uuid::Uuid;

use super::{
    JournalStatus, ListFilter, ObservationKind, ObservationStore, listing_view, session_status,
};

/// Owner-only observation commands.
#[derive(Clone, Debug)]
pub(crate) enum ObservationCommand {
    /// List journal records for one session, bounded metadata by default.
    List {
        session_id: String,
        kind: Option<ObservationKind>,
        task_id: Option<String>,
        after_revision: Option<u64>,
        limit: usize,
        include_content: bool,
    },
    /// Fetch one journal record in full.
    Get {
        session_id: String,
        observation_id: Uuid,
    },
    /// Journal counters and degradation flags for one session.
    Status { session_id: String },
}

/// JSON-lines output keeps listings stream-friendly and never inlines
/// content unless `--include-content` was passed explicitly.
pub(crate) fn run_observation_command(command: ObservationCommand) -> Result<()> {
    match command {
        ObservationCommand::List {
            session_id,
            kind,
            task_id,
            after_revision,
            limit,
            include_content,
        } => {
            let store = ObservationStore::default_store()?;
            let (records, corrupt) = store.list(
                &session_id,
                &ListFilter {
                    kind,
                    task_id,
                    after_revision,
                    limit: Some(limit),
                },
            )?;
            for record in &records {
                println!(
                    "{}",
                    serde_json::to_string(&listing_view(record, include_content))?
                );
            }
            if corrupt > 0 {
                eprintln!("{corrupt} journal line(s) could not be parsed");
            }
            Ok(())
        }
        ObservationCommand::Get {
            session_id,
            observation_id,
        } => {
            let store = ObservationStore::default_store()?;
            let record = store
                .get(&session_id, observation_id)?
                .ok_or_else(|| anyhow::anyhow!("observation {observation_id} was not found"))?;
            println!("{}", serde_json::to_string_pretty(&record)?);
            Ok(())
        }
        ObservationCommand::Status { session_id } => {
            let status: JournalStatus = session_status(&session_id)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "session_id": session_id,
                    "journal": status,
                    "memory": {
                        "worker": "not_implemented",
                        "stale": true,
                    },
                }))?
            );
            Ok(())
        }
    }
}
