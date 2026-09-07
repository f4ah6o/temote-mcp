use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::config;

const DEFAULT_KNOWLEDGE_ROOT: &str = "learnings";
const MAX_LEARNING_FILES: usize = 256;
const MAX_DIRECTORY_ENTRIES: usize = 1024;
const MAX_LEARNING_BYTES: usize = 64 * 1024;
const MAX_QUERY_BYTES: usize = 2048;
const MAX_QUERY_TERMS: usize = 64;
const MAX_RECALL_LIMIT: usize = 20;
const MAX_TITLE_BYTES: usize = 256;
const MAX_TAGS: usize = 16;
const MAX_TAG_BYTES: usize = 64;

#[derive(Clone, Debug)]
struct LearningDocument {
    learning_id: String,
    title: String,
    tags: Vec<String>,
    verification: Option<String>,
    title_terms: HashSet<String>,
    tag_terms: HashSet<String>,
    body_terms: HashSet<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub(crate) struct RecallHit {
    pub learning_id: String,
    pub title: String,
    pub score: f64,
    pub matched_terms: Vec<String>,
    pub missing_terms: Vec<String>,
    pub tags: Vec<String>,
    pub verification: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub(crate) struct RecallResponse {
    pub source: String,
    pub knowledge_root: PathBuf,
    pub knowledge_root_present: bool,
    pub query_terms: Vec<String>,
    pub hits: Vec<RecallHit>,
    pub index: String,
    pub network_required: bool,
}

pub(crate) fn search(
    session: &config::Session,
    query: &str,
    knowledge_root: Option<&str>,
    limit: usize,
) -> Result<RecallResponse> {
    anyhow::ensure!(
        !query.trim().is_empty() && query.len() <= MAX_QUERY_BYTES,
        "recall query must contain 1..={MAX_QUERY_BYTES} UTF-8 bytes"
    );
    anyhow::ensure!(
        (1..=MAX_RECALL_LIMIT).contains(&limit),
        "recall limit must be 1..={MAX_RECALL_LIMIT}"
    );
    let query_terms = tokenize_query(query)?;
    let root_text = knowledge_root.unwrap_or(DEFAULT_KNOWLEDGE_ROOT);
    validate_relative_root(root_text)?;
    let candidate = session.cwd.join(root_text);
    let root = match std::fs::canonicalize(&candidate) {
        Ok(root) => {
            ensure_recall_scope(session, &root)?;
            anyhow::ensure!(
                root.is_dir(),
                "knowledge root is not a directory: {}",
                root.display()
            );
            root
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = candidate.parent().context("knowledge root has no parent")?;
            let canonical_parent = std::fs::canonicalize(parent)?;
            ensure_recall_scope(session, &canonical_parent)?;
            return Ok(RecallResponse {
                source: "repo_managed_markdown".to_owned(),
                knowledge_root: candidate,
                knowledge_root_present: false,
                query_terms,
                hits: Vec::new(),
                index: "rebuilt_per_request".to_owned(),
                network_required: false,
            });
        }
        Err(error) => return Err(error).context("cannot resolve knowledge root"),
    };

    let documents = load_documents(&root)?;
    let hits = rank_documents(&documents, &query_terms, limit);
    Ok(RecallResponse {
        source: "repo_managed_markdown".to_owned(),
        knowledge_root: root,
        knowledge_root_present: true,
        query_terms,
        hits,
        index: "rebuilt_per_request".to_owned(),
        network_required: false,
    })
}

fn load_documents(root: &Path) -> Result<Vec<LearningDocument>> {
    let metadata = std::fs::symlink_metadata(root)?;
    anyhow::ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "knowledge root must be a real directory"
    );
    let mut paths = Vec::new();
    collect_markdown(root, root, 0, &mut paths)?;
    anyhow::ensure!(
        paths.len() <= MAX_LEARNING_FILES,
        "knowledge root contains too many Markdown files"
    );
    paths.sort();
    paths
        .into_iter()
        .map(|path| load_document(root, &path))
        .collect()
}

fn collect_markdown(
    root: &Path,
    directory: &Path,
    depth: usize,
    paths: &mut Vec<PathBuf>,
) -> Result<()> {
    anyhow::ensure!(depth <= 4, "knowledge root nesting exceeds 4 directories");
    let mut entries = 0usize;
    for entry in std::fs::read_dir(directory)? {
        entries += 1;
        anyhow::ensure!(
            entries <= MAX_DIRECTORY_ENTRIES,
            "knowledge directory contains too many entries"
        );
        let entry = entry?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "recall does not follow symlinks: {}",
            path.display()
        );
        if metadata.is_dir() {
            collect_markdown(root, &path, depth + 1, paths)?;
        } else if metadata.is_file() && path.extension().is_some_and(|extension| extension == "md")
        {
            anyhow::ensure!(
                path.starts_with(root),
                "learning path escaped knowledge root"
            );
            paths.push(path);
            anyhow::ensure!(
                paths.len() <= MAX_LEARNING_FILES,
                "knowledge root contains too many Markdown files"
            );
        }
    }
    Ok(())
}

fn load_document(root: &Path, path: &Path) -> Result<LearningDocument> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    anyhow::ensure!(metadata.is_file(), "learning is not a regular file");
    anyhow::ensure!(
        metadata.len() <= MAX_LEARNING_BYTES as u64,
        "learning exceeds size limit"
    );
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_LEARNING_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= MAX_LEARNING_BYTES,
        "learning exceeds size limit"
    );
    let text = String::from_utf8(bytes).context("learning Markdown must be UTF-8")?;
    let parsed = parse_learning(&text)?;
    let relative = path
        .strip_prefix(root)
        .context("learning path escaped root")?;
    let learning_id = relative.to_string_lossy().replace('\\', "/");
    Ok(LearningDocument {
        learning_id,
        title_terms: tokenize_document(&parsed.title).into_iter().collect(),
        tag_terms: parsed
            .tags
            .iter()
            .flat_map(|tag| tokenize_document(tag))
            .collect(),
        body_terms: tokenize_document(&parsed.body).into_iter().collect(),
        title: parsed.title,
        tags: parsed.tags,
        verification: parsed.verification,
    })
}

struct ParsedLearning {
    title: String,
    tags: Vec<String>,
    verification: Option<String>,
    body: String,
}

fn parse_learning(text: &str) -> Result<ParsedLearning> {
    anyhow::ensure!(
        text.starts_with("---\n"),
        "learning Markdown requires YAML-like front matter"
    );
    let rest = &text[4..];
    let end = rest
        .find("\n---\n")
        .context("learning front matter is not terminated")?;
    let front = &rest[..end];
    let body = rest[end + 5..].to_owned();
    let mut title = None;
    let mut date = None;
    let mut tags = None;
    let mut domain = None;
    let mut verification = None;
    for line in front.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "title" => title = Some(unquote(value).to_owned()),
            "date" => date = Some(unquote(value).to_owned()),
            "tags" => tags = Some(parse_tags(value)?),
            "domain" => domain = Some(unquote(value).to_owned()),
            "verification" => verification = Some(unquote(value).to_owned()),
            _ => {}
        }
    }
    let title = title.context("learning front matter requires title")?;
    let date = date.context("learning front matter requires date")?;
    let tags = tags.context("learning front matter requires tags")?;
    let domain = domain.context("learning front matter requires domain")?;
    let verification = verification.context("learning front matter requires verification")?;
    anyhow::ensure!(
        !title.is_empty() && title.len() <= MAX_TITLE_BYTES,
        "learning title is invalid"
    );
    anyhow::ensure!(
        date.len() == 10
            && date.as_bytes()[4] == b'-'
            && date.as_bytes()[7] == b'-'
            && date
                .bytes()
                .enumerate()
                .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit()),
        "learning date must use YYYY-MM-DD"
    );
    anyhow::ensure!(
        !tags.is_empty() && tags.len() <= MAX_TAGS,
        "learning requires 1..={MAX_TAGS} tags"
    );
    anyhow::ensure!(
        !domain.is_empty() && domain.len() <= MAX_TAG_BYTES,
        "learning domain is invalid"
    );
    for tag in &tags {
        anyhow::ensure!(
            !tag.is_empty() && tag.len() <= MAX_TAG_BYTES,
            "learning tag is invalid"
        );
    }
    anyhow::ensure!(
        !verification.is_empty() && verification.len() <= MAX_TAG_BYTES,
        "learning verification is invalid"
    );
    for heading in ["## Problem", "## Resolution", "## Reusable lesson"] {
        anyhow::ensure!(body.contains(heading), "learning body requires {heading}");
    }
    Ok(ParsedLearning {
        title,
        tags,
        verification: Some(verification),
        body,
    })
}

fn parse_tags(value: &str) -> Result<Vec<String>> {
    let value = value.trim();
    anyhow::ensure!(
        value.starts_with('[') && value.ends_with(']'),
        "learning tags must use [a, b] syntax"
    );
    let inner = &value[1..value.len() - 1];
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    Ok(inner
        .split(',')
        .map(|tag| unquote(tag.trim()).to_owned())
        .collect())
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(value)
}

fn rank_documents(
    documents: &[LearningDocument],
    query_terms: &[String],
    limit: usize,
) -> Vec<RecallHit> {
    if query_terms.is_empty() || documents.is_empty() {
        return Vec::new();
    }
    let mut document_frequency = HashMap::<&str, usize>::new();
    for term in query_terms {
        let count = documents
            .iter()
            .filter(|document| {
                document.title_terms.contains(term)
                    || document.tag_terms.contains(term)
                    || document.body_terms.contains(term)
            })
            .count();
        document_frequency.insert(term, count);
    }

    let mut hits = Vec::new();
    for document in documents {
        let mut matched = BTreeSet::new();
        let mut missing = BTreeSet::new();
        let mut score = 0.0f64;
        for term in query_terms {
            let title = document.title_terms.contains(term);
            let tag = document.tag_terms.contains(term);
            let body = document.body_terms.contains(term);
            if title || tag || body {
                matched.insert(term.clone());
                let df = *document_frequency.get(term.as_str()).unwrap_or(&0) as f64;
                let idf = ((documents.len() as f64 + 1.0) / (df + 1.0)).ln() + 1.0;
                let weight = if title {
                    4.0
                } else if tag {
                    3.0
                } else {
                    1.0
                };
                score += idf * weight;
            } else {
                missing.insert(term.clone());
            }
        }
        if !matched.is_empty() {
            hits.push(RecallHit {
                learning_id: document.learning_id.clone(),
                title: document.title.clone(),
                score,
                matched_terms: matched.into_iter().collect(),
                missing_terms: missing.into_iter().collect(),
                tags: document.tags.clone(),
                verification: document.verification.clone(),
            });
        }
    }
    hits.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.learning_id.cmp(&right.learning_id))
    });
    hits.truncate(limit);
    hits
}

fn tokenize_query(text: &str) -> Result<Vec<String>> {
    let terms = tokenize_document(text);
    anyhow::ensure!(
        terms.len() <= MAX_QUERY_TERMS,
        "query contains too many searchable terms"
    );
    Ok(terms)
}

fn tokenize_document(text: &str) -> Vec<String> {
    let mut terms = BTreeSet::new();
    let mut current = String::new();
    for character in text.chars() {
        if character.is_alphanumeric() || character == '_' || character == '-' {
            for lower in character.to_lowercase() {
                current.push(lower);
            }
        } else if !current.is_empty() {
            terms.insert(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        terms.insert(current);
    }
    terms.into_iter().collect()
}

fn validate_relative_root(root: &str) -> Result<()> {
    anyhow::ensure!(
        !root.is_empty() && root.len() <= 4096,
        "knowledge_root is invalid"
    );
    let path = Path::new(root);
    anyhow::ensure!(
        !path.is_absolute(),
        "knowledge_root must be relative to the session cwd"
    );
    for component in path.components() {
        anyhow::ensure!(
            matches!(component, std::path::Component::Normal(_)),
            "knowledge_root may not contain traversal components"
        );
    }
    Ok(())
}

fn ensure_recall_scope(session: &config::Session, path: &Path) -> Result<()> {
    let roots = if session.permitted_directories.is_empty() {
        std::slice::from_ref(&session.cwd)
    } else {
        session.permitted_directories.as_slice()
    };
    anyhow::ensure!(
        roots
            .iter()
            .any(|root| path == root || path.starts_with(root)),
        "recall knowledge root is outside the session roots: {}",
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(root: &Path) -> config::Session {
        let cwd = config::canonical_directory(root).unwrap();
        config::Session {
            id: "recall-test".to_owned(),
            cwd: cwd.clone(),
            permitted_directories: vec![cwd],
            started_at: 1,
            process_id: 2,
            yolo: true,
        }
    }

    fn learning(title: &str, tags: &str, body: &str) -> String {
        format!(
            "---\ntitle: \"{title}\"\ndate: 2026-09-08\ntags: [{tags}]\ndomain: technical\nverification: verified\n---\n\n## Problem\n{body}\n\n## Resolution\nResolved deterministically.\n\n## Reusable lesson\nReuse this bounded learning.\n"
        )
    }

    #[test]
    fn recall_index_is_rebuildable_from_authoritative_markdown() {
        let root = tempfile::tempdir().unwrap();
        let learnings = root.path().join("learnings");
        std::fs::create_dir(&learnings).unwrap();
        std::fs::write(
            learnings.join("wine.md"),
            learning(
                "Wine TLS",
                "wine, tls",
                "GnuTLS fixes certificate validation",
            ),
        )
        .unwrap();
        let session = session(root.path());
        let first = search(&session, "wine tls", None, 5).unwrap();
        let second = search(&session, "wine tls", None, 5).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.index, "rebuilt_per_request");
    }

    #[test]
    fn recall_reports_matched_and_missing_terms() {
        let root = tempfile::tempdir().unwrap();
        let learnings = root.path().join("learnings");
        std::fs::create_dir(&learnings).unwrap();
        std::fs::write(
            learnings.join("wine.md"),
            learning("Wine TLS", "wine, tls", "certificate validation"),
        )
        .unwrap();
        let response = search(&session(root.path()), "wine steam", None, 5).unwrap();
        assert_eq!(response.hits.len(), 1);
        assert_eq!(response.hits[0].matched_terms, vec!["wine"]);
        assert_eq!(response.hits[0].missing_terms, vec!["steam"]);
    }

    #[test]
    fn recall_respects_configured_knowledge_root() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("allowed")).unwrap();
        std::fs::create_dir(root.path().join("other")).unwrap();
        std::fs::write(
            root.path().join("allowed/a.md"),
            learning("Allowed", "alpha", "needle"),
        )
        .unwrap();
        std::fs::write(
            root.path().join("other/b.md"),
            learning("Other", "beta", "needle"),
        )
        .unwrap();
        let response = search(&session(root.path()), "needle", Some("allowed"), 5).unwrap();
        assert_eq!(response.hits.len(), 1);
        assert_eq!(response.hits[0].title, "Allowed");
        assert!(!response.network_required);
    }

    #[test]
    fn recall_rejects_traversal_even_in_yolo_session() {
        let root = tempfile::tempdir().unwrap();
        let session = session(root.path());
        assert!(search(&session, "x", Some("../outside"), 5).is_err());
    }
}
