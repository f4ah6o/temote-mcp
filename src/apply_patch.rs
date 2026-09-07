use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{approvals, config, friction, sandbox};

const MAX_PATCH_BYTES: usize = 1024 * 1024;
const MAX_PATCH_FILES: usize = 128;
const MAX_PATCH_PATH_BYTES: usize = 4096;
const MAX_PATCH_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ApplyPatchRequest {
    pub session_id: String,
    pub patch: String,
}

#[derive(Clone, Debug)]
enum ParsedOperation {
    Add {
        path: String,
        content: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        hunks: Vec<Hunk>,
    },
    Delete {
        path: String,
    },
}

#[derive(Clone, Debug)]
struct Hunk {
    lines: Vec<HunkLine>,
    end_of_file: bool,
}

#[derive(Clone, Debug)]
enum HunkLine {
    Context(String),
    Remove(String),
    Add(String),
}

#[derive(Clone, Debug)]
enum PreparedOperation {
    Add {
        path: PathBuf,
        content: String,
    },
    Update {
        path: PathBuf,
        content: String,
    },
    Move {
        source: PathBuf,
        destination: PathBuf,
        content: Option<String>,
    },
    Delete {
        path: PathBuf,
    },
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct CommitRecord {
    pub action: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct ApplyPatchOutcome {
    pub status: String,
    pub committed: Vec<CommitRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub(crate) fn parse_request(value: &Value) -> Result<ApplyPatchRequest> {
    let request: ApplyPatchRequest =
        serde_json::from_value(value.clone()).context("invalid apply_patch arguments")?;
    config::validate_session_id(&request.session_id)?;
    anyhow::ensure!(
        !request.patch.is_empty() && request.patch.len() <= MAX_PATCH_BYTES,
        "patch must contain 1..={MAX_PATCH_BYTES} UTF-8 bytes"
    );
    Ok(request)
}

pub(crate) async fn apply(
    session: &config::Session,
    request: ApplyPatchRequest,
) -> Result<ApplyPatchOutcome> {
    anyhow::ensure!(request.session_id == session.id, "session ID mismatch");
    let parsed = parse_patch(&request.patch)?;
    let prepared = preflight(session, &parsed)?;
    let detail = approval_detail(&prepared);
    let approved = if session.yolo {
        true
    } else {
        approvals::request(&session.id, "apply_patch", detail, session.cwd.clone()).await?
    };
    let outcome = apply_prepared_after_approval(session, prepared, approved).await?;
    if outcome.status == "partial_failure" {
        friction::record_observed(
            session,
            friction::FrictionKind::AmbiguousMutationDetected,
            Some("filesystem"),
            Some("apply_patch"),
            friction::EventOutcome::Ambiguous,
            None,
            None,
        );
    }
    approvals::activity(
        &session.id,
        format!("Applied patch: {}", outcome.status),
        Some(format!("committed operations: {}", outcome.committed.len())),
    )
    .await;
    Ok(outcome)
}

fn approval_detail(operations: &[PreparedOperation]) -> String {
    let mut adds = 0usize;
    let mut updates = 0usize;
    let mut moves = 0usize;
    let mut deletes = 0usize;
    for operation in operations {
        match operation {
            PreparedOperation::Add { .. } => adds += 1,
            PreparedOperation::Update { .. } => updates += 1,
            PreparedOperation::Move { .. } => moves += 1,
            PreparedOperation::Delete { .. } => deletes += 1,
        }
    }
    format!(
        "files: {}; add: {adds}; update: {updates}; move: {moves}; delete: {deletes}",
        operations.len()
    )
}

async fn apply_prepared_after_approval(
    session: &config::Session,
    operations: Vec<PreparedOperation>,
    approved: bool,
) -> Result<ApplyPatchOutcome> {
    if !approved {
        anyhow::bail!("user denied apply_patch")
    }

    let mut committed = Vec::new();
    for operation in operations {
        if let Err(error) = apply_one(session, &operation, &mut committed).await {
            return Ok(ApplyPatchOutcome {
                status: "partial_failure".to_owned(),
                committed,
                error: Some(error.to_string()),
            });
        }
    }
    Ok(ApplyPatchOutcome {
        status: "committed".to_owned(),
        committed,
        error: None,
    })
}

async fn apply_one(
    session: &config::Session,
    operation: &PreparedOperation,
    committed: &mut Vec<CommitRecord>,
) -> Result<()> {
    match operation {
        PreparedOperation::Add { path, content } => {
            write_content(session, path, content).await?;
            committed.push(commit_record("add", path, None));
        }
        PreparedOperation::Update { path, content } => {
            write_content(session, path, content).await?;
            committed.push(commit_record("update", path, None));
        }
        PreparedOperation::Move {
            source,
            destination,
            content,
        } => {
            if let Some(content) = content {
                write_content(session, source, content).await?;
                committed.push(commit_record("update", source, None));
            }
            move_path(session, source, destination).await?;
            committed.push(commit_record("move", source, Some(destination)));
        }
        PreparedOperation::Delete { path } => {
            remove_path(session, path).await?;
            committed.push(commit_record("delete", path, None));
        }
    }
    Ok(())
}

fn commit_record(action: &str, path: &Path, destination: Option<&PathBuf>) -> CommitRecord {
    CommitRecord {
        action: action.to_owned(),
        path: path.display().to_string(),
        destination: destination.map(|path| path.display().to_string()),
    }
}

async fn write_content(session: &config::Session, path: &Path, content: &str) -> Result<()> {
    let parent = path.parent().context("patch target has no parent")?;
    if session.yolo {
        tokio::fs::write(path, content)
            .await
            .with_context(|| format!("failed to write {}", path.display()))?;
        return Ok(());
    }
    let command = vec!["tee".to_owned(), path.display().to_string()];
    let output = sandbox::run(
        &command,
        parent,
        &[parent.to_path_buf()],
        Some(content.as_bytes()),
    )
    .await?;
    anyhow::ensure!(
        output.status == 0,
        "sandboxed patch write failed for {}: {}",
        path.display(),
        output.stderr.trim()
    );
    Ok(())
}

async fn move_path(session: &config::Session, source: &Path, destination: &Path) -> Result<()> {
    if session.yolo {
        tokio::fs::rename(source, destination)
            .await
            .with_context(|| {
                format!(
                    "failed to move {} to {}",
                    source.display(),
                    destination.display()
                )
            })?;
        return Ok(());
    }
    let command = vec![
        "mv".to_owned(),
        "--".to_owned(),
        source.display().to_string(),
        destination.display().to_string(),
    ];
    let mut roots = vec![
        source
            .parent()
            .context("patch source has no parent")?
            .to_path_buf(),
        destination
            .parent()
            .context("patch destination has no parent")?
            .to_path_buf(),
    ];
    roots.sort();
    roots.dedup();
    let output = sandbox::run(&command, &session.cwd, &roots, None).await?;
    anyhow::ensure!(
        output.status == 0,
        "sandboxed patch move failed for {}: {}",
        source.display(),
        output.stderr.trim()
    );
    Ok(())
}

async fn remove_path(session: &config::Session, path: &Path) -> Result<()> {
    let parent = path.parent().context("patch target has no parent")?;
    if session.yolo {
        tokio::fs::remove_file(path)
            .await
            .with_context(|| format!("failed to delete {}", path.display()))?;
        return Ok(());
    }
    let command = vec!["rm".to_owned(), "--".to_owned(), path.display().to_string()];
    let output = sandbox::run(&command, parent, &[parent.to_path_buf()], None).await?;
    anyhow::ensure!(
        output.status == 0,
        "sandboxed patch delete failed for {}: {}",
        path.display(),
        output.stderr.trim()
    );
    Ok(())
}

fn parse_patch(patch: &str) -> Result<Vec<ParsedOperation>> {
    let normalized = patch.replace("\r\n", "\n");
    let lines = normalized.split('\n').collect::<Vec<_>>();
    anyhow::ensure!(
        lines.first() == Some(&"*** Begin Patch"),
        "patch must start with *** Begin Patch"
    );

    let mut operations = Vec::new();
    let mut index = 1usize;
    while index < lines.len() {
        let line = lines[index];
        if line == "*** End Patch" {
            anyhow::ensure!(
                lines[index + 1..].iter().all(|line| line.is_empty()),
                "unexpected content after *** End Patch"
            );
            anyhow::ensure!(
                !operations.is_empty(),
                "patch must contain at least one file operation"
            );
            anyhow::ensure!(
                operations.len() <= MAX_PATCH_FILES,
                "patch contains too many file operations"
            );
            return Ok(operations);
        }

        if let Some(path) = line.strip_prefix("*** Add File: ") {
            validate_patch_path_text(path)?;
            index += 1;
            let mut content = Vec::new();
            while index < lines.len() && !lines[index].starts_with("*** ") {
                let line = lines[index];
                anyhow::ensure!(line.starts_with('+'), "add-file lines must start with '+'");
                content.push(&line[1..]);
                index += 1;
            }
            let mut content = content.join("\n");
            if !content.is_empty() {
                content.push('\n');
            }
            anyhow::ensure!(
                content.len() <= MAX_PATCH_OUTPUT_BYTES,
                "patched file exceeds size limit"
            );
            operations.push(ParsedOperation::Add {
                path: path.to_owned(),
                content,
            });
            continue;
        }

        if let Some(path) = line.strip_prefix("*** Delete File: ") {
            validate_patch_path_text(path)?;
            operations.push(ParsedOperation::Delete {
                path: path.to_owned(),
            });
            index += 1;
            continue;
        }

        if let Some(path) = line.strip_prefix("*** Update File: ") {
            validate_patch_path_text(path)?;
            index += 1;
            let mut move_to = None;
            if index < lines.len()
                && let Some(destination) = lines[index].strip_prefix("*** Move to: ")
            {
                validate_patch_path_text(destination)?;
                move_to = Some(destination.to_owned());
                index += 1;
            }
            let mut hunks = Vec::new();
            while index < lines.len() && !lines[index].starts_with("*** ") {
                let header = lines[index];
                anyhow::ensure!(
                    header.starts_with("@@"),
                    "update sections require @@ hunk headers"
                );
                index += 1;
                let mut hunk_lines = Vec::new();
                let mut end_of_file = false;
                while index < lines.len()
                    && !lines[index].starts_with("@@")
                    && !lines[index].starts_with("*** ")
                {
                    let line = lines[index];
                    let (prefix, rest) = line.split_at(
                        line.char_indices()
                            .nth(1)
                            .map_or(line.len(), |(offset, _)| offset),
                    );
                    match prefix {
                        " " => hunk_lines.push(HunkLine::Context(rest.to_owned())),
                        "-" => hunk_lines.push(HunkLine::Remove(rest.to_owned())),
                        "+" => hunk_lines.push(HunkLine::Add(rest.to_owned())),
                        _ => anyhow::bail!("hunk lines must start with space, '-' or '+'"),
                    }
                    index += 1;
                }
                if index < lines.len() && lines[index] == "*** End of File" {
                    end_of_file = true;
                    index += 1;
                }
                anyhow::ensure!(!hunk_lines.is_empty(), "update hunk must not be empty");
                hunks.push(Hunk {
                    lines: hunk_lines,
                    end_of_file,
                });
            }
            anyhow::ensure!(
                !hunks.is_empty() || move_to.is_some(),
                "update requires at least one hunk or a move destination"
            );
            operations.push(ParsedOperation::Update {
                path: path.to_owned(),
                move_to,
                hunks,
            });
            continue;
        }

        anyhow::bail!("unsupported patch construct: {line}");
    }
    anyhow::bail!("patch is missing *** End Patch")
}

fn validate_patch_path_text(path: &str) -> Result<()> {
    anyhow::ensure!(!path.is_empty(), "patch path must not be empty");
    anyhow::ensure!(path.len() <= MAX_PATCH_PATH_BYTES, "patch path is too long");
    let path = Path::new(path);
    anyhow::ensure!(
        !path.is_absolute(),
        "apply_patch paths must be relative to the session cwd"
    );
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            _ => anyhow::bail!("apply_patch paths may not contain '.', '..', roots, or prefixes"),
        }
    }
    Ok(())
}

fn preflight(
    session: &config::Session,
    operations: &[ParsedOperation],
) -> Result<Vec<PreparedOperation>> {
    anyhow::ensure!(
        operations.len() <= MAX_PATCH_FILES,
        "patch contains too many file operations"
    );
    let mut touched = HashSet::new();
    let mut prepared = Vec::with_capacity(operations.len());

    for operation in operations {
        match operation {
            ParsedOperation::Add { path, content } => {
                let path = resolve_new_path(session, path)?;
                reserve_path(&mut touched, &path)?;
                anyhow::ensure!(
                    std::fs::symlink_metadata(&path).is_err(),
                    "add target already exists: {}",
                    path.display()
                );
                prepared.push(PreparedOperation::Add {
                    path,
                    content: content.clone(),
                });
            }
            ParsedOperation::Delete { path } => {
                let path = resolve_existing_regular(session, path)?;
                reserve_path(&mut touched, &path)?;
                prepared.push(PreparedOperation::Delete { path });
            }
            ParsedOperation::Update {
                path,
                move_to,
                hunks,
            } => {
                let source = resolve_existing_regular(session, path)?;
                reserve_path(&mut touched, &source)?;
                let original = read_bounded_utf8(&source)?;
                let updated = apply_hunks(&original, hunks)?;
                anyhow::ensure!(
                    updated.len() <= MAX_PATCH_OUTPUT_BYTES,
                    "patched file exceeds size limit"
                );
                if let Some(destination) = move_to {
                    let destination = resolve_new_path(session, destination)?;
                    reserve_path(&mut touched, &destination)?;
                    anyhow::ensure!(
                        std::fs::symlink_metadata(&destination).is_err(),
                        "move destination already exists: {}",
                        destination.display()
                    );
                    prepared.push(PreparedOperation::Move {
                        source,
                        destination,
                        content: (updated != original).then_some(updated),
                    });
                } else {
                    prepared.push(PreparedOperation::Update {
                        path: source,
                        content: updated,
                    });
                }
            }
        }
    }
    Ok(prepared)
}

fn reserve_path(touched: &mut HashSet<PathBuf>, path: &Path) -> Result<()> {
    anyhow::ensure!(
        touched.insert(path.to_path_buf()),
        "patch touches the same source or destination more than once: {}",
        path.display()
    );
    Ok(())
}

fn resolve_new_path(session: &config::Session, path: &str) -> Result<PathBuf> {
    validate_patch_path_text(path)?;
    let resolved = config::resolve_write_path(session, Path::new(path))?;
    ensure_patch_scope(session, &resolved)?;
    Ok(resolved)
}

fn resolve_existing_regular(session: &config::Session, path: &str) -> Result<PathBuf> {
    validate_patch_path_text(path)?;
    let resolved = config::resolve_existing_path(session, Path::new(path))?;
    ensure_patch_scope(session, &resolved)?;
    let metadata = std::fs::metadata(&resolved)
        .with_context(|| format!("cannot inspect patch source {}", resolved.display()))?;
    anyhow::ensure!(
        metadata.is_file(),
        "patch source is not a regular file: {}",
        resolved.display()
    );
    Ok(resolved)
}

fn ensure_patch_scope(session: &config::Session, path: &Path) -> Result<()> {
    let roots = if session.permitted_directories.is_empty() {
        std::slice::from_ref(&session.cwd)
    } else {
        session.permitted_directories.as_slice()
    };
    anyhow::ensure!(
        roots
            .iter()
            .any(|root| path == root || path.starts_with(root)),
        "apply_patch path is outside the session roots: {}",
        path.display()
    );
    Ok(())
}

fn read_bounded_utf8(path: &Path) -> Result<String> {
    let metadata = std::fs::metadata(path)?;
    anyhow::ensure!(
        metadata.len() <= MAX_PATCH_OUTPUT_BYTES as u64,
        "patch source exceeds size limit"
    );
    let bytes = std::fs::read(path)?;
    anyhow::ensure!(
        bytes.len() <= MAX_PATCH_OUTPUT_BYTES,
        "patch source exceeds size limit"
    );
    String::from_utf8(bytes).context("patch source is not UTF-8")
}

fn apply_hunks(original: &str, hunks: &[Hunk]) -> Result<String> {
    if hunks.is_empty() {
        return Ok(original.to_owned());
    }
    let had_trailing_newline = original.ends_with('\n');
    let mut lines = original
        .strip_suffix('\n')
        .unwrap_or(original)
        .split('\n')
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if original.is_empty() {
        lines.clear();
    }
    let mut cursor = 0usize;
    for hunk in hunks {
        let old = hunk
            .lines
            .iter()
            .filter_map(|line| match line {
                HunkLine::Context(line) | HunkLine::Remove(line) => Some(line.clone()),
                HunkLine::Add(_) => None,
            })
            .collect::<Vec<_>>();
        let new = hunk
            .lines
            .iter()
            .filter_map(|line| match line {
                HunkLine::Context(line) | HunkLine::Add(line) => Some(line.clone()),
                HunkLine::Remove(_) => None,
            })
            .collect::<Vec<_>>();
        let start = find_subsequence(&lines, &old, cursor, hunk.end_of_file)
            .context("patch hunk did not match the expected preimage")?;
        let end = start + old.len();
        lines.splice(start..end, new.iter().cloned());
        cursor = start + new.len();
    }
    let mut output = lines.join("\n");
    if had_trailing_newline && !output.is_empty() {
        output.push('\n');
    }
    Ok(output)
}

fn find_subsequence(
    lines: &[String],
    needle: &[String],
    cursor: usize,
    end_of_file: bool,
) -> Option<usize> {
    if needle.is_empty() {
        return Some(if end_of_file {
            lines.len()
        } else {
            cursor.min(lines.len())
        });
    }
    if needle.len() > lines.len() {
        return None;
    }
    if end_of_file {
        let start = lines.len() - needle.len();
        return (start >= cursor && lines[start..] == *needle).then_some(start);
    }
    (cursor..=lines.len() - needle.len())
        .find(|start| lines[*start..*start + needle.len()] == *needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn session(root: &Path, yolo: bool) -> config::Session {
        let cwd = config::canonical_directory(root).unwrap();
        config::Session {
            id: "apply-patch-test".to_owned(),
            cwd: cwd.clone(),
            permitted_directories: vec![cwd],
            started_at: 1,
            process_id: 2,
            yolo,
        }
    }

    #[test]
    fn apply_patch_parses_add_update_move_delete_without_shell() {
        let parsed = parse_patch(
            "*** Begin Patch\n*** Add File: a.txt\n+hello\n*** Update File: b.txt\n*** Move to: c.txt\n@@\n-old\n+new\n*** Delete File: d.txt\n*** End Patch",
        )
        .unwrap();
        assert_eq!(parsed.len(), 3);
    }

    #[test]
    fn apply_patch_preflights_all_files_before_first_write() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("keep.txt"), "before\n").unwrap();
        let session = session(root.path(), true);
        let parsed = parse_patch(
            "*** Begin Patch\n*** Update File: keep.txt\n@@\n-before\n+after\n*** Update File: missing.txt\n@@\n-x\n+y\n*** End Patch",
        )
        .unwrap();
        assert!(preflight(&session, &parsed).is_err());
        assert_eq!(
            std::fs::read_to_string(root.path().join("keep.txt")).unwrap(),
            "before\n"
        );
    }

    #[test]
    fn apply_patch_rejects_symlink_escape_and_move_destination_outside_root() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "secret\n").unwrap();
        symlink(outside.path(), root.path().join("outside")).unwrap();
        let session = session(root.path(), true);
        let escaped = parse_patch(
            "*** Begin Patch\n*** Update File: outside/secret.txt\n@@\n-secret\n+changed\n*** End Patch",
        )
        .unwrap();
        assert!(preflight(&session, &escaped).is_err());

        std::fs::write(root.path().join("source.txt"), "a\n").unwrap();
        let moved = parse_patch(
            "*** Begin Patch\n*** Update File: source.txt\n*** Move to: ../outside.txt\n*** End Patch",
        );
        assert!(moved.is_err());
    }

    #[tokio::test]
    async fn apply_patch_denied_approval_writes_nothing() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.txt"), "before\n").unwrap();
        let session = session(root.path(), true);
        let parsed = parse_patch(
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-before\n+after\n*** End Patch",
        )
        .unwrap();
        let prepared = preflight(&session, &parsed).unwrap();
        assert!(
            apply_prepared_after_approval(&session, prepared, false)
                .await
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
            "before\n"
        );
    }

    #[test]
    fn apply_patch_does_not_widen_session_permissions() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "secret\n").unwrap();
        symlink(outside.path(), root.path().join("outside")).unwrap();

        let session = session(root.path(), false);
        let parsed = parse_patch(
            "*** Begin Patch\n*** Update File: outside/secret.txt\n@@\n-secret\n+changed\n*** End Patch",
        )
        .unwrap();

        assert!(preflight(&session, &parsed).is_err());
        assert_eq!(
            std::fs::read_to_string(outside.path().join("secret.txt")).unwrap(),
            "secret\n"
        );
    }

    #[test]
    fn apply_patch_malformed_multi_file_patch_writes_nothing() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.txt"), "before\n").unwrap();
        let malformed = parse_patch(
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-before\n+after\n*** Add File: b.txt\nthis lacks plus\n*** End Patch",
        );
        assert!(malformed.is_err());
        assert_eq!(
            std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
            "before\n"
        );
        assert!(!root.path().join("b.txt").exists());
    }

    #[tokio::test]
    async fn apply_patch_partial_io_failure_reports_exact_commit_state() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first.txt");
        let blocked_parent = root.path().join("blocked");
        std::fs::write(&blocked_parent, "not a directory\n").unwrap();
        let second = blocked_parent.join("second.txt");
        let session = session(root.path(), true);
        let operations = vec![
            PreparedOperation::Add {
                path: first.clone(),
                content: "first\n".to_owned(),
            },
            PreparedOperation::Add {
                path: second.clone(),
                content: "second\n".to_owned(),
            },
        ];

        let outcome = apply_prepared_after_approval(&session, operations, true)
            .await
            .unwrap();

        assert_eq!(outcome.status, "partial_failure");
        assert_eq!(outcome.committed, vec![commit_record("add", &first, None)]);
        assert!(
            outcome
                .error
                .as_deref()
                .is_some_and(|error| error.contains("failed to write"))
        );
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "first\n");
        assert!(!second.exists());
    }

    #[tokio::test]
    async fn apply_patch_commits_yolo_patch() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.txt"), "before\n").unwrap();
        let session = session(root.path(), true);
        let request = ApplyPatchRequest {
            session_id: session.id.clone(),
            patch: "*** Begin Patch\n*** Update File: a.txt\n@@\n-before\n+after\n*** Add File: b.txt\n+new\n*** End Patch".to_owned(),
        };
        let outcome = apply(&session, request).await.unwrap();
        assert_eq!(outcome.status, "committed");
        assert_eq!(
            std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
            "after\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("b.txt")).unwrap(),
            "new\n"
        );
    }

    #[test]
    fn apply_patch_secret_content_is_not_copied_into_audit_metadata() {
        let root = tempfile::tempdir().unwrap();
        let session = session(root.path(), true);
        let marker = "secret-content-sentinel";
        let parsed = parse_patch(&format!(
            "*** Begin Patch\n*** Add File: a.txt\n+{marker}\n*** End Patch"
        ))
        .unwrap();
        let prepared = preflight(&session, &parsed).unwrap();
        let detail = approval_detail(&prepared);
        assert!(!detail.contains(marker));
    }
}
