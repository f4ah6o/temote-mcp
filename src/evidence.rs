use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Serialize;
use uuid::Uuid;

pub(crate) const DEFAULT_READ_BYTES: usize = 16 * 1024;
pub(crate) const MAX_READ_BYTES: usize = 64 * 1024;
const MAX_RECORD_BYTES: usize = 2 * 1024 * 1024;
const MAX_RECORDS_PER_SESSION: usize = 128;
const MAX_RECORDS_TOTAL: usize = 512;
const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;
const RETENTION: Duration = Duration::from_secs(30 * 60);

#[derive(Clone, Debug, Serialize)]
pub(crate) struct EvidenceRef {
    pub(crate) evidence_id: String,
    pub(crate) bytes: usize,
    pub(crate) retention_seconds: u64,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct EvidenceChunk {
    pub(crate) evidence_id: String,
    pub(crate) offset_bytes: usize,
    pub(crate) returned_bytes: usize,
    pub(crate) total_bytes: usize,
    pub(crate) content: String,
    pub(crate) truncated: bool,
    pub(crate) next_offset_bytes: Option<usize>,
}

struct Record {
    session_id: String,
    scope: PathBuf,
    content: String,
    created_at: Instant,
}

#[derive(Default)]
struct State {
    records: HashMap<Uuid, Record>,
    total_bytes: usize,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::default()))
}

pub(crate) fn canonical_scope(path: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(path)
        .with_context(|| format!("cannot resolve evidence scope {}", path.display()))
}

pub(crate) fn store(
    session_id: &str,
    scope: &Path,
    content: String,
) -> Result<Option<EvidenceRef>> {
    if content.is_empty() {
        return Ok(None);
    }
    anyhow::ensure!(
        content.len() <= MAX_RECORD_BYTES,
        "evidence exceeds {MAX_RECORD_BYTES} bytes"
    );
    let scope = canonical_scope(scope)?;
    let mut state = state().lock().unwrap();
    prune(&mut state, Instant::now());

    while state.records.len() >= MAX_RECORDS_TOTAL
        || session_record_count(&state, session_id) >= MAX_RECORDS_PER_SESSION
        || state.total_bytes.saturating_add(content.len()) > MAX_TOTAL_BYTES
    {
        let Some(oldest) = state
            .records
            .iter()
            .min_by_key(|(_, record)| record.created_at)
            .map(|(id, _)| *id)
        else {
            break;
        };
        remove_record(&mut state, oldest);
    }

    anyhow::ensure!(
        state.total_bytes.saturating_add(content.len()) <= MAX_TOTAL_BYTES,
        "evidence store is at its byte limit"
    );
    let id = Uuid::new_v4();
    let bytes = content.len();
    state.records.insert(
        id,
        Record {
            session_id: session_id.to_owned(),
            scope,
            content,
            created_at: Instant::now(),
        },
    );
    state.total_bytes = state.total_bytes.saturating_add(bytes);
    Ok(Some(EvidenceRef {
        evidence_id: id.to_string(),
        bytes,
        retention_seconds: RETENTION.as_secs(),
    }))
}

pub(crate) fn read(
    session_id: &str,
    scope: &Path,
    evidence_id: Uuid,
    offset_bytes: usize,
    max_bytes: usize,
) -> Result<EvidenceChunk> {
    anyhow::ensure!(
        (1..=MAX_READ_BYTES).contains(&max_bytes),
        "evidence max_bytes must be 1..={MAX_READ_BYTES}"
    );
    let scope = canonical_scope(scope)?;
    let mut state = state().lock().unwrap();
    prune(&mut state, Instant::now());
    let record = state
        .records
        .get(&evidence_id)
        .context("evidence is missing or expired")?;
    anyhow::ensure!(
        record.session_id == session_id,
        "evidence ownership mismatch"
    );
    anyhow::ensure!(record.scope == scope, "evidence scope mismatch");
    anyhow::ensure!(
        offset_bytes <= record.content.len(),
        "evidence offset exceeds content length"
    );
    anyhow::ensure!(
        record.content.is_char_boundary(offset_bytes),
        "evidence offset is not a UTF-8 boundary"
    );

    let desired_end = offset_bytes
        .saturating_add(max_bytes)
        .min(record.content.len());
    let end = next_char_boundary(&record.content, desired_end);
    let content = record.content[offset_bytes..end].to_owned();
    let truncated = end < record.content.len();
    Ok(EvidenceChunk {
        evidence_id: evidence_id.to_string(),
        offset_bytes,
        returned_bytes: content.len(),
        total_bytes: record.content.len(),
        content,
        truncated,
        next_offset_bytes: truncated.then_some(end),
    })
}

pub(crate) fn remove_session(session_id: &str) {
    let mut state = state().lock().unwrap();
    let ids = state
        .records
        .iter()
        .filter_map(|(id, record)| (record.session_id == session_id).then_some(*id))
        .collect::<Vec<_>>();
    for id in ids {
        remove_record(&mut state, id);
    }
}

fn next_char_boundary(text: &str, mut end: usize) -> usize {
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    end
}

fn session_record_count(state: &State, session_id: &str) -> usize {
    state
        .records
        .values()
        .filter(|record| record.session_id == session_id)
        .count()
}

fn prune(state: &mut State, now: Instant) {
    let expired = state
        .records
        .iter()
        .filter_map(|(id, record)| {
            (now.saturating_duration_since(record.created_at) >= RETENTION).then_some(*id)
        })
        .collect::<Vec<_>>();
    for id in expired {
        remove_record(state, id);
    }
}

fn remove_record(state: &mut State, id: Uuid) {
    if let Some(record) = state.records.remove(&id) {
        state.total_bytes = state.total_bytes.saturating_sub(record.content.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_is_session_and_scope_bound_and_utf8_chunked() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let reference = store("owner", root.path(), "aβcdef".to_owned())
            .unwrap()
            .unwrap();
        let id = Uuid::parse_str(&reference.evidence_id).unwrap();

        assert!(read("other", root.path(), id, 0, 2).is_err());
        assert!(read("owner", other.path(), id, 0, 2).is_err());
        let first = read("owner", root.path(), id, 0, 2).unwrap();
        assert_eq!(first.content, "aβ");
        assert_eq!(first.returned_bytes, 3);
        assert_eq!(first.next_offset_bytes, Some(3));
        let second = read(
            "owner",
            root.path(),
            id,
            first.next_offset_bytes.unwrap(),
            3,
        )
        .unwrap();
        assert_eq!(second.content, "cde");
        assert!(read("owner", root.path(), id, 2, 2).is_err());
    }

    #[test]
    fn tiny_utf8_pages_always_advance_and_round_trip() {
        let root = tempfile::tempdir().unwrap();
        let text = "aé水🦀z";
        let reference = store("tiny-pages", root.path(), text.to_owned())
            .unwrap()
            .unwrap();
        let id = Uuid::parse_str(&reference.evidence_id).unwrap();

        let mut offset = 0;
        let mut pages = String::new();
        while offset < text.len() {
            let page = read("tiny-pages", root.path(), id, offset, 1).unwrap();
            assert_eq!(page.offset_bytes, offset);
            assert!(page.returned_bytes > 0);
            pages.push_str(&page.content);
            if page.truncated {
                let next = page
                    .next_offset_bytes
                    .expect("truncated page must return a continuation offset");
                assert!(next > offset);
                offset = next;
            } else {
                assert_eq!(offset + page.returned_bytes, text.len());
                assert_eq!(page.next_offset_bytes, None);
                offset = text.len();
            }
        }

        assert_eq!(pages, text);
        let eof = read("tiny-pages", root.path(), id, offset, 1).unwrap();
        assert!(eof.content.is_empty());
        assert!(!eof.truncated);
        assert_eq!(eof.next_offset_bytes, None);
    }

    #[test]
    fn evidence_session_cleanup_removes_owned_records_only() {
        let root = tempfile::tempdir().unwrap();
        let a = store("a", root.path(), "one".to_owned()).unwrap().unwrap();
        let b = store("b", root.path(), "two".to_owned()).unwrap().unwrap();
        remove_session("a");
        assert!(
            read(
                "a",
                root.path(),
                Uuid::parse_str(&a.evidence_id).unwrap(),
                0,
                10
            )
            .is_err()
        );
        assert!(
            read(
                "b",
                root.path(),
                Uuid::parse_str(&b.evidence_id).unwrap(),
                0,
                10
            )
            .is_ok()
        );
    }
}
