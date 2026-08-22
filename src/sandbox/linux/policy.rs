// Linux sandbox implementation informed by openai/codex revision
// 20fedafff83f5c681fc62f73b0ca3227e42e3f8b (Apache-2.0).
// See docs/linux-sandbox.md and THIRD_PARTY_NOTICES.md for provenance and local changes.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const MAX_ROOTS: usize = 128;
const MAX_READ_ONLY_PATHS: usize = 1024;
const PROTECTED_METADATA_NAMES: &[&str] = &[".git", ".agents", ".codex"];
const GIT_READ_ONLY_PATHS: &[&str] = &[
    "config",
    "hooks",
    "info",
    "attributes",
    "description",
    "packed-refs",
    "shallow",
    "worktrees",
    "refs/tags",
    "refs/remotes",
    "objects/info",
    "objects/pack",
];

/// The only network mode supported by the Temote Linux helper.
///
/// Keeping this as a one-variant enum makes a serialized policy explicit while
/// ensuring that adding a future network mode requires an intentional helper
/// implementation instead of silently widening an old policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LinuxNetworkPolicy {
    Restricted,
}

/// Minimal Temote-specific policy passed across the helper process boundary.
///
/// This is deliberately not a compatibility representation of Codex's
/// permission API. The parent constructs this closed set of canonical paths;
/// the helper validates it again before constructing bubblewrap arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxSandboxPolicy {
    pub version: u8,
    pub cwd: PathBuf,
    pub writable_roots: Vec<PathBuf>,
    pub temporary_roots: Vec<PathBuf>,
    pub read_only_paths: Vec<PathBuf>,
    pub network: LinuxNetworkPolicy,
}

impl LinuxSandboxPolicy {
    pub fn for_command(
        cwd: &Path,
        writable_roots: &[PathBuf],
        git_metadata_roots: &[PathBuf],
    ) -> Result<Self> {
        let cwd = canonical_existing_directory(cwd, "sandbox cwd")?;
        let mut writable = vec![cwd.clone()];
        writable.extend(
            writable_roots
                .iter()
                .map(|path| canonical_existing_directory(path, "writable root"))
                .collect::<Result<Vec<_>>>()?,
        );
        writable.extend(
            git_metadata_roots
                .iter()
                .map(|path| canonical_existing_directory(path, "Git metadata root"))
                .collect::<Result<Vec<_>>>()?,
        );
        normalize_paths(&mut writable);
        let canonical_git_roots = git_metadata_roots
            .iter()
            .map(|path| canonical_existing_directory(path, "Git metadata root"))
            .collect::<Result<Vec<_>>>()?;

        let mut temporary_roots = vec![canonical_existing_directory(
            Path::new("/tmp"),
            "temporary root /tmp",
        )?];
        if let Some(tmpdir) = std::env::var_os("TMPDIR") {
            temporary_roots.push(canonical_existing_directory(Path::new(&tmpdir), "TMPDIR")?);
        }
        normalize_paths(&mut temporary_roots);

        let mut read_only_paths = Vec::new();
        for root in &writable {
            for name in PROTECTED_METADATA_NAMES {
                let path = root.join(name);
                // A validated run_git operation may write the repository's
                // own metadata root. Its narrower config/hooks/etc. masks are
                // added below instead.
                if canonical_git_roots.iter().any(|git_root| git_root == &path) {
                    continue;
                }
                read_only_paths.push(path);
            }
        }

        for git_root in &canonical_git_roots {
            if is_linked_worktree_metadata_root(git_root) {
                read_only_paths.extend([git_root.join("gitdir"), git_root.join("commondir")]);
            } else {
                read_only_paths.extend(
                    GIT_READ_ONLY_PATHS
                        .iter()
                        .map(|suffix| git_root.join(suffix)),
                );
            }
        }

        normalize_paths(&mut read_only_paths);
        let policy = Self {
            version: 1,
            cwd,
            writable_roots: writable,
            temporary_roots,
            read_only_paths,
            network: LinuxNetworkPolicy::Restricted,
        };
        policy.validate()?;
        Ok(policy)
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.version == 1,
            "unsupported Linux sandbox policy version"
        );
        anyhow::ensure!(
            self.writable_roots.len() <= MAX_ROOTS,
            "too many writable roots"
        );
        anyhow::ensure!(
            self.temporary_roots.len() <= MAX_ROOTS,
            "too many temporary roots"
        );
        anyhow::ensure!(
            self.read_only_paths.len() <= MAX_READ_ONLY_PATHS,
            "too many read-only paths"
        );
        anyhow::ensure!(
            self.writable_roots.iter().any(|root| root == &self.cwd),
            "sandbox cwd must be a writable root"
        );
        anyhow::ensure!(
            self.network == LinuxNetworkPolicy::Restricted,
            "unsupported Linux network policy"
        );

        validate_existing_directory(&self.cwd, "sandbox cwd")?;
        validate_unique_existing_directories(&self.writable_roots, "writable root")?;
        validate_unique_existing_directories(&self.temporary_roots, "temporary root")?;

        for path in &self.read_only_paths {
            validate_absolute_clean_path(path, "read-only path")?;
            anyhow::ensure!(
                self.writable_roots
                    .iter()
                    .any(|root| path.starts_with(root) && path != root),
                "read-only path is outside writable roots: {}",
                path.display()
            );
            anyhow::ensure!(
                !self.writable_roots.iter().any(|root| root == path),
                "read-only path cannot be a writable root: {}",
                path.display()
            );
            validate_no_symlink_components(path)?;
        }

        Ok(())
    }
}

fn canonical_existing_directory(path: &Path, label: &str) -> Result<PathBuf> {
    let canonical = std::fs::canonicalize(path)
        .with_context(|| format!("cannot resolve {label} {}", path.display()))?;
    anyhow::ensure!(
        canonical.is_absolute() && canonical.is_dir(),
        "{label} is not an absolute directory: {}",
        path.display()
    );
    validate_absolute_clean_path(&canonical, label)?;
    Ok(canonical)
}

fn validate_existing_directory(path: &Path, label: &str) -> Result<()> {
    let canonical = std::fs::canonicalize(path)
        .with_context(|| format!("cannot resolve {label} {}", path.display()))?;
    anyhow::ensure!(
        canonical == path && canonical.is_dir(),
        "{label} is not canonical and existing: {}",
        path.display()
    );
    validate_absolute_clean_path(path, label)
}

fn validate_unique_existing_directories(paths: &[PathBuf], label: &str) -> Result<()> {
    let mut seen = BTreeSet::new();
    for path in paths {
        validate_existing_directory(path, label)?;
        anyhow::ensure!(seen.insert(path), "duplicate {label}: {}", path.display());
    }
    Ok(())
}

fn validate_absolute_clean_path(path: &Path, label: &str) -> Result<()> {
    anyhow::ensure!(
        path.is_absolute(),
        "{label} is not absolute: {}",
        path.display()
    );
    anyhow::ensure!(
        !path.as_os_str().as_encoded_bytes().contains(&0),
        "{label} contains a NUL byte"
    );
    anyhow::ensure!(
        path.components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_))),
        "{label} is not normalized: {}",
        path.display()
    );
    Ok(())
}

fn validate_no_symlink_components(path: &Path) -> Result<()> {
    let mut current = PathBuf::from("/");
    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        current.push(name);
        let metadata = match std::fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "cannot inspect read-only path component {}",
                        current.display()
                    )
                });
            }
        };
        anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "read-only path crosses a symlink: {}",
            current.display()
        );
    }
    Ok(())
}

fn normalize_paths(paths: &mut Vec<PathBuf>) {
    paths.sort();
    paths.dedup();
}

pub(super) fn is_linked_worktree_metadata_root(path: &Path) -> bool {
    path.parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == "worktrees")
        && path.join("gitdir").is_file()
        && path.join("commondir").is_file()
}

/// Whether a missing protected path should be materialized as a directory
/// mask rather than an empty read-only file.
pub fn missing_path_is_directory(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    matches!(
        name,
        ".git"
            | ".agents"
            | ".codex"
            | "hooks"
            | "info"
            | "objects"
            | "refs"
            | "worktrees"
            | "tags"
            | "remotes"
            | "pack"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_only_the_temote_policy_shape() {
        let root = tempfile::tempdir().unwrap();
        let policy = LinuxSandboxPolicy::for_command(root.path(), &[], &[]).unwrap();
        let value: serde_json::Value = serde_json::to_value(policy).unwrap();

        assert_eq!(value["version"], 1);
        assert_eq!(value["network"], "restricted");
        assert!(value.get("permission_profile").is_none());
        assert!(value.get("entries").is_none());
    }

    #[test]
    fn rejects_unknown_fields_and_noncanonical_roots() {
        let root = tempfile::tempdir().unwrap();
        let policy = LinuxSandboxPolicy::for_command(root.path(), &[], &[]).unwrap();
        let mut value = serde_json::to_value(policy).unwrap();
        value["unexpected"] = serde_json::Value::Bool(true);
        assert!(serde_json::from_value::<LinuxSandboxPolicy>(value).is_err());

        let mut policy = LinuxSandboxPolicy::for_command(root.path(), &[], &[]).unwrap();
        policy.cwd = root.path().join(".");
        assert!(policy.validate().is_err());
    }

    #[test]
    fn rejects_read_only_symlink_paths() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        let protected = root.path().join(".git");
        std::fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, &protected).unwrap();

        let error = LinuxSandboxPolicy::for_command(root.path(), &[], &[]).unwrap_err();
        assert!(error.to_string().contains("symlink"));
    }
}
