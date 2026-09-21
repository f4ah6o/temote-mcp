// Linux sandbox implementation informed by openai/codex revision
// 20fedafff83f5c681fc62f73b0ca3227e42e3f8b (Apache-2.0).
// See docs/linux-sandbox.md and THIRD_PARTY_NOTICES.md for provenance and local changes.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::sandbox::{PROTECTED_METADATA_NAMES, discover_protected_metadata_paths};

const MAX_ROOTS: usize = 128;
const MAX_READ_ONLY_PATHS: usize = 1024;
/// Network modes supported by the Temote Linux helper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LinuxNetworkPolicy {
    Restricted,
    LocalAgent,
}

/// A verified intermediate launcher symlink that the helper recreates inside
/// the sandbox. `link` is the lexical path that must exist and `target` is its
/// already validated canonical destination; both are re-validated by the helper
/// before bubblewrap arguments are constructed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxReadOnlySymlink {
    pub link: PathBuf,
    pub target: PathBuf,
}

/// A workspace whose repository identity was validated before the helper
/// started.
///
/// The helper opens `path` without following symbolic links, verifies that the
/// opened directory still presents exactly this repository identity, and only
/// then binds that directory descriptor as the workspace (and cwd). The
/// verified entity is therefore the entity the agent runs in, not whatever the
/// path resolves to at bind time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxPinnedWorkspace {
    pub path: PathBuf,
    pub writable: bool,
    pub worktree_root: PathBuf,
    pub metadata_roots: Vec<PathBuf>,
    pub common_dir: PathBuf,
    pub primary_checkout: PathBuf,
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
    pub read_only_roots: Vec<PathBuf>,
    pub read_only_symlinks: Vec<LinuxReadOnlySymlink>,
    pub read_only_scaffold_directories: Vec<PathBuf>,
    pub read_only_files: Vec<PathBuf>,
    pub hidden_roots: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_workspace: Option<LinuxPinnedWorkspace>,
    pub network: LinuxNetworkPolicy,
}

impl LinuxSandboxPolicy {
    pub fn for_command(
        cwd: &Path,
        writable_roots: &[PathBuf],
        git_metadata_roots: &[PathBuf],
    ) -> Result<Self> {
        Self::for_command_with_network(
            cwd,
            writable_roots,
            git_metadata_roots,
            LinuxNetworkPolicy::Restricted,
        )
    }

    /// Ordinary-command profile with an explicit permission-mode-selected
    /// network policy. Filesystem/path containment is identical to
    /// `for_command`; only the network mode changes.
    pub fn for_command_with_network(
        cwd: &Path,
        writable_roots: &[PathBuf],
        git_metadata_roots: &[PathBuf],
        network: LinuxNetworkPolicy,
    ) -> Result<Self> {
        let mut policy = Self::for_scoped_command(cwd, writable_roots, git_metadata_roots, None)?;
        policy.network = network;
        policy.validate()?;
        Ok(policy)
    }

    pub fn for_git_worktree_add(
        cwd: &Path,
        writable_roots: &[PathBuf],
        git_metadata_roots: &[PathBuf],
        protected_worktree_roots: &[PathBuf],
    ) -> Result<Self> {
        Self::for_scoped_command(
            cwd,
            writable_roots,
            git_metadata_roots,
            Some(protected_worktree_roots),
        )
    }

    /// Developer-tool profile: the same workspace/state containment as
    /// `command`, with the operation class selecting whether outbound network
    /// is enabled (`dependency-network`) or denied (`dev-offline`).
    pub fn for_developer_tool(
        cwd: &Path,
        writable_roots: &[PathBuf],
        network_access: bool,
    ) -> Result<Self> {
        let mut policy = Self::for_scoped_command(cwd, writable_roots, &[], None)?;
        if network_access {
            policy.network = LinuxNetworkPolicy::LocalAgent;
        }
        Ok(policy)
    }

    fn for_scoped_command(
        cwd: &Path,
        writable_roots: &[PathBuf],
        git_metadata_roots: &[PathBuf],
        protected_worktree_roots: Option<&[PathBuf]>,
    ) -> Result<Self> {
        let cwd = canonical_existing_directory(cwd, "sandbox cwd")?;
        let mut writable = vec![cwd.clone()];
        writable.extend(
            writable_roots
                .iter()
                .map(|path| canonical_existing_directory(path, "writable root"))
                .collect::<Result<Vec<_>>>()?,
        );
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

        // Protected metadata under ordinary writable roots keeps its read-only
        // mask. Metadata roots themselves are handled below: the whole
        // metadata root is bound read-only so missing protected entries stay
        // missing (no mount-point artifacts, no empty placeholder changing Git
        // semantics) and only the exact writable paths Git needs are re-exposed.
        let mut read_only_paths = Vec::new();
        for root in &writable {
            for name in PROTECTED_METADATA_NAMES {
                let path = root.join(name);
                if canonical_git_roots.iter().any(|git_root| git_root == &path) {
                    continue;
                }
                read_only_paths.push(path);
            }
        }

        // `cwd` and explicit writable roots remain writable.
        let mut read_only_roots = Vec::new();
        let mut writable_metadata = Vec::new();
        for git_root in &canonical_git_roots {
            if is_linked_worktree_metadata_root(git_root) {
                // The selected worktree's private metadata directory is
                // writable as a whole so HEAD/index lock+rename persist; its
                // pointer files stay read-only.
                writable_metadata.push(git_root.clone());
                read_only_paths.extend([git_root.join("gitdir"), git_root.join("commondir")]);
            } else {
                read_only_roots.push(git_root.clone());
                // The common repository directory is read-only. Only the paths
                // that structured mutation needs to create or update stay
                // writable; they are host-backed directory entries, so Git's
                // lock-then-rename protocol persists to the host.
                for suffix in ["refs/heads", "objects", "logs"] {
                    let path = git_root.join(suffix);
                    if path.is_dir() {
                        writable_metadata.push(path);
                    }
                }
                if protected_worktree_roots.is_some() {
                    let worktrees = git_root.join("worktrees");
                    if worktrees.is_dir() {
                        writable_metadata.push(worktrees);
                    }
                }
                read_only_paths.push(git_root.join("objects/info"));
                read_only_paths.push(git_root.join("objects/pack"));
            }
        }
        writable.extend(writable_metadata);

        if let Some(protected_worktree_roots) = protected_worktree_roots {
            for protected in protected_worktree_roots {
                let protected =
                    canonical_existing_directory(protected, "protected worktree metadata root")?;
                anyhow::ensure!(
                    canonical_git_roots.iter().any(|common| {
                        protected.parent() == Some(common.join("worktrees").as_path())
                    }),
                    "protected worktree metadata root is not a direct child of a validated common Git worktrees directory: {}",
                    protected.display()
                );
                read_only_paths.push(protected);
            }
        }

        normalize_paths(&mut writable);
        normalize_paths(&mut read_only_paths);
        normalize_paths(&mut read_only_roots);
        let policy = Self {
            version: 1,
            cwd,
            writable_roots: writable,
            temporary_roots,
            read_only_paths,
            read_only_roots,
            read_only_symlinks: Vec::new(),
            read_only_scaffold_directories: Vec::new(),
            read_only_files: Vec::new(),
            hidden_roots: Vec::new(),
            pinned_workspace: None,
            network: LinuxNetworkPolicy::Restricted,
        };
        policy.validate()?;
        Ok(policy)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn for_local_agent(
        cwd: &Path,
        writable_roots: &[PathBuf],
        temporary_roots: &[PathBuf],
        read_only_paths: &[PathBuf],
        read_only_roots: &[PathBuf],
        read_only_symlinks: &[crate::sandbox::LocalAgentSymlink],
        read_only_scaffold_directories: &[PathBuf],
        read_only_files: &[PathBuf],
        hidden_roots: &[PathBuf],
        expected_repository: Option<&crate::sandbox::WorkspaceRepositoryIdentity>,
    ) -> Result<Self> {
        let cwd = canonical_existing_directory(cwd, "sandbox cwd")?;
        let mut writable = writable_roots
            .iter()
            .map(|path| canonical_existing_directory(path, "writable root"))
            .collect::<Result<Vec<_>>>()?;
        normalize_paths(&mut writable);

        let mut temporary = temporary_roots
            .iter()
            .map(|path| canonical_existing_directory(path, "temporary root"))
            .collect::<Result<Vec<_>>>()?;
        normalize_paths(&mut temporary);

        let mut read_only = read_only_paths
            .iter()
            .map(|path| {
                std::fs::canonicalize(path)
                    .with_context(|| format!("cannot resolve read-only path {}", path.display()))
            })
            .collect::<Result<Vec<_>>>()?;
        for root in &writable {
            read_only.extend(discover_protected_metadata_paths(root)?);
        }
        normalize_paths(&mut read_only);

        let mut visible_roots = read_only_roots
            .iter()
            .map(|path| canonical_existing_directory(path, "read-only root"))
            .collect::<Result<Vec<_>>>()?;
        normalize_paths(&mut visible_roots);
        let mut hidden = hidden_roots
            .iter()
            .map(|path| canonical_existing_directory(path, "hidden root"))
            .collect::<Result<Vec<_>>>()?;
        normalize_paths(&mut hidden);
        let mut read_only_symlinks = read_only_symlinks
            .iter()
            .map(|symlink| LinuxReadOnlySymlink {
                link: symlink.link.clone(),
                target: symlink.target.clone(),
            })
            .collect::<Vec<_>>();
        read_only_symlinks.sort_by_key(|symlink| symlink.link.clone());
        read_only_symlinks.dedup();
        let mut scaffold_directories = read_only_scaffold_directories.to_vec();
        normalize_paths(&mut scaffold_directories);
        let mut files = read_only_files.to_vec();
        normalize_paths(&mut files);

        let pinned_workspace = match expected_repository {
            Some(expected) => {
                anyhow::ensure!(
                    expected.worktree_root == cwd,
                    "pinned workspace identity does not match the local agent cwd"
                );
                Some(LinuxPinnedWorkspace {
                    path: cwd.clone(),
                    writable: writable.contains(&cwd),
                    worktree_root: expected.worktree_root.clone(),
                    metadata_roots: expected.metadata_roots.clone(),
                    common_dir: expected.common_dir.clone(),
                    primary_checkout: expected.primary_checkout.clone(),
                })
            }
            None => None,
        };
        let policy = Self {
            version: 1,
            cwd,
            writable_roots: writable,
            temporary_roots: temporary,
            read_only_paths: read_only,
            read_only_roots: visible_roots,
            read_only_symlinks,
            read_only_scaffold_directories: scaffold_directories,
            read_only_files: files,
            hidden_roots: hidden,
            pinned_workspace,
            network: LinuxNetworkPolicy::LocalAgent,
        };
        policy.validate()?;
        Ok(policy)
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.version == 1,
            "unsupported Linux sandbox policy version"
        );
        if let Some(pinned) = &self.pinned_workspace {
            anyhow::ensure!(
                pinned.path == self.cwd,
                "pinned workspace path must equal the sandbox cwd: {}",
                pinned.path.display()
            );
            anyhow::ensure!(
                pinned.worktree_root == pinned.path,
                "pinned workspace root must be the sandbox cwd: {}",
                pinned.worktree_root.display()
            );
            anyhow::ensure!(
                !pinned.metadata_roots.is_empty(),
                "pinned workspace metadata roots must not be empty"
            );
            anyhow::ensure!(
                pinned.common_dir.is_absolute()
                    && pinned.primary_checkout.is_absolute()
                    && pinned.metadata_roots.iter().all(|root| root.is_absolute()),
                "pinned workspace identity paths must be absolute"
            );
            let writable = self.writable_roots.iter().any(|root| root == &pinned.path);
            let read_only = self.read_only_roots.iter().any(|root| root == &pinned.path);
            anyhow::ensure!(
                (pinned.writable && writable) || (!pinned.writable && read_only),
                "pinned workspace path must appear as its matching sandbox root: {}",
                pinned.path.display()
            );
        }
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
            self.read_only_roots.len() <= MAX_ROOTS,
            "too many read-only roots"
        );
        anyhow::ensure!(
            self.read_only_symlinks.len() <= MAX_ROOTS,
            "too many read-only symlinks"
        );
        anyhow::ensure!(
            self.read_only_scaffold_directories.len() <= MAX_ROOTS,
            "too many read-only scaffold directories"
        );
        anyhow::ensure!(
            self.read_only_files.len() <= MAX_ROOTS,
            "too many read-only files"
        );
        anyhow::ensure!(
            self.hidden_roots.len() <= MAX_ROOTS,
            "too many hidden roots"
        );
        if self.network == LinuxNetworkPolicy::Restricted {
            anyhow::ensure!(
                self.writable_roots.iter().any(|root| root == &self.cwd),
                "sandbox cwd must be a writable root"
            );
        }

        validate_existing_directory(&self.cwd, "sandbox cwd")?;
        validate_unique_existing_directories(&self.writable_roots, "writable root")?;
        validate_unique_existing_directories(&self.temporary_roots, "temporary root")?;
        validate_unique_existing_directories(&self.read_only_roots, "read-only root")?;
        validate_unique_existing_directories(&self.hidden_roots, "hidden root")?;

        for root in &self.read_only_roots {
            anyhow::ensure!(
                !self.writable_roots.iter().any(|writable| writable == root),
                "read-only root cannot be a writable root: {}",
                root.display()
            );
        }
        for hidden in &self.hidden_roots {
            anyhow::ensure!(
                hidden != Path::new("/"),
                "hidden root cannot be the filesystem root"
            );
            for visible in self
                .writable_roots
                .iter()
                .chain(self.temporary_roots.iter())
                .chain(self.read_only_roots.iter())
            {
                anyhow::ensure!(
                    hidden != visible && !hidden.starts_with(visible),
                    "hidden root is inside a visible root: {}",
                    hidden.display()
                );
            }
        }

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

        let mut symlink_links = BTreeSet::new();
        for symlink in &self.read_only_symlinks {
            validate_absolute_clean_path(&symlink.link, "read-only symlink link")?;
            validate_absolute_clean_path(&symlink.target, "read-only symlink target")?;
            anyhow::ensure!(
                symlink.target != Path::new("/"),
                "read-only symlink target cannot be the filesystem root: {}",
                symlink.link.display()
            );
            anyhow::ensure!(
                !is_protected_metadata_location(&symlink.link)
                    && !is_protected_metadata_location(&symlink.target),
                "read-only symlink crosses protected metadata: {}",
                symlink.link.display()
            );
            anyhow::ensure!(
                !self
                    .writable_roots
                    .iter()
                    .chain(self.temporary_roots.iter())
                    .chain(self.read_only_roots.iter())
                    .any(|root| symlink.link.starts_with(root)),
                "read-only symlink is inside a visible root: {}",
                symlink.link.display()
            );
            let parent = symlink
                .link
                .parent()
                .context("read-only symlink has no parent")?;
            anyhow::ensure!(
                parent.is_dir(),
                "read-only symlink parent is not a directory: {}",
                symlink.link.display()
            );
            validate_no_symlink_components(parent)?;
            let metadata = std::fs::symlink_metadata(&symlink.link).with_context(|| {
                format!(
                    "cannot inspect read-only symlink {}",
                    symlink.link.display()
                )
            })?;
            anyhow::ensure!(
                metadata.file_type().is_symlink(),
                "read-only path is no longer a symlink: {}",
                symlink.link.display()
            );
            let canonical = std::fs::canonicalize(&symlink.link).with_context(|| {
                format!(
                    "cannot resolve read-only symlink {}",
                    symlink.link.display()
                )
            })?;
            anyhow::ensure!(
                canonical == symlink.target,
                "read-only symlink target changed: {}",
                symlink.link.display()
            );
            anyhow::ensure!(
                symlink_links.insert(&symlink.link),
                "duplicate read-only symlink link: {}",
                symlink.link.display()
            );
        }

        for directory in &self.read_only_scaffold_directories {
            validate_absolute_clean_path(directory, "read-only scaffold directory")?;
            anyhow::ensure!(
                !is_protected_metadata_location(directory),
                "read-only scaffold directory is inside protected metadata: {}",
                directory.display()
            );
            anyhow::ensure!(
                directory != Path::new("/"),
                "read-only scaffold directory cannot be the filesystem root"
            );
            validate_no_symlink_components(directory)?;
            validate_existing_directory(directory, "read-only scaffold directory")?;
            anyhow::ensure!(
                !self
                    .writable_roots
                    .iter()
                    .chain(self.temporary_roots.iter())
                    .chain(self.read_only_roots.iter())
                    .any(|root| directory.starts_with(root)),
                "read-only scaffold directory is inside a visible root: {}",
                directory.display()
            );
        }

        for file in &self.read_only_files {
            validate_absolute_clean_path(file, "read-only file")?;
            anyhow::ensure!(
                !is_protected_metadata_location(file),
                "read-only file is inside protected metadata: {}",
                file.display()
            );
            let metadata = std::fs::symlink_metadata(file)
                .with_context(|| format!("cannot inspect read-only file {}", file.display()))?;
            anyhow::ensure!(
                metadata.file_type().is_file(),
                "read-only path is not a regular file: {}",
                file.display()
            );
            anyhow::ensure!(
                std::fs::canonicalize(file).with_context(|| {
                    format!("cannot resolve read-only file {}", file.display())
                })? == *file,
                "read-only file is not canonical: {}",
                file.display()
            );
            let parent = file.parent().context("read-only file has no parent")?;
            validate_existing_directory(parent, "read-only file parent")?;
            validate_no_symlink_components(parent)?;
            anyhow::ensure!(
                !self
                    .writable_roots
                    .iter()
                    .chain(self.temporary_roots.iter())
                    .chain(self.read_only_roots.iter())
                    .any(|root| file.starts_with(root)),
                "read-only file is inside a visible root: {}",
                file.display()
            );
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

fn is_protected_metadata_location(path: &Path) -> bool {
    path.components().any(|component| {
        let Component::Normal(name) = component else {
            return false;
        };
        matches!(name.to_str(), Some(".git" | ".agents" | ".codex"))
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{
        PROTECTED_METADATA_NAMES, ProtectedMetadataScanLimits,
        discover_protected_metadata_paths_with_limits,
    };
    use crate::test_support;

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
    fn command_policy_keeps_top_level_masks_without_recursive_scan() {
        let fixture = tempfile::tempdir().unwrap();
        let workspace = fixture.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        for index in 0..3 {
            std::fs::create_dir(workspace.join(format!("entry-{index}"))).unwrap();
        }
        let workspace = std::fs::canonicalize(workspace).unwrap();

        assert!(
            discover_protected_metadata_paths_with_limits(
                &workspace,
                ProtectedMetadataScanLimits {
                    max_entries: 2,
                    max_depth: 64,
                    max_paths: 1024,
                }
            )
            .is_err(),
            "fixture must exceed the injected local-agent scan budget"
        );

        let policy = LinuxSandboxPolicy::for_command(&workspace, &[], &[]).unwrap();
        for name in PROTECTED_METADATA_NAMES {
            assert!(policy.read_only_paths.contains(&workspace.join(name)));
        }
    }

    #[test]
    fn local_agent_policy_can_keep_a_read_only_cwd() {
        let root = tempfile::tempdir().unwrap();
        let temp = root.path().join("tmp");
        std::fs::create_dir(&temp).unwrap();
        let policy = LinuxSandboxPolicy::for_local_agent(
            root.path(),
            &[],
            std::slice::from_ref(&temp),
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
        )
        .unwrap();

        assert!(!policy.writable_roots.contains(&policy.cwd));
        assert_eq!(policy.network, LinuxNetworkPolicy::LocalAgent);
        assert!(policy.validate().is_ok());
    }

    #[test]
    fn local_agent_policy_masks_nested_metadata_files_and_directories() {
        let fixture = tempfile::tempdir().unwrap();
        let workspace = fixture.path().join("workspace");
        std::fs::create_dir_all(workspace.join("nested/.git")).unwrap();
        std::fs::create_dir_all(workspace.join("nested/.agents")).unwrap();
        std::fs::create_dir_all(workspace.join("nested/deep")).unwrap();
        std::fs::write(workspace.join("nested/deep/.codex"), b"metadata").unwrap();
        std::fs::create_dir_all(workspace.join("ordinary")).unwrap();
        std::fs::write(workspace.join("ordinary/.git"), b"gitdir: linked").unwrap();

        let workspace = std::fs::canonicalize(workspace).unwrap();
        let policy = LinuxSandboxPolicy::for_local_agent(
            &workspace,
            std::slice::from_ref(&workspace),
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
        )
        .unwrap();

        for expected in [
            workspace.join("nested/.git"),
            workspace.join("nested/.agents"),
            workspace.join("nested/deep/.codex"),
            workspace.join("ordinary/.git"),
        ] {
            assert!(
                policy.read_only_paths.contains(&expected),
                "missing nested protected path {expected:?}"
            );
        }
    }

    #[test]
    fn rejects_unknown_fields_and_noncanonical_roots() {
        let root = tempfile::tempdir().unwrap();
        let policy = LinuxSandboxPolicy::for_command(root.path(), &[], &[]).unwrap();
        let mut value = serde_json::to_value(policy).unwrap();
        value["unexpected"] = serde_json::Value::Bool(true);
        assert!(serde_json::from_value::<LinuxSandboxPolicy>(value).is_err());

        let mut policy = LinuxSandboxPolicy::for_command(root.path(), &[], &[]).unwrap();
        let child = root.path().join("child");
        std::fs::create_dir(&child).unwrap();
        policy.cwd = child.join("..");
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

    #[test]
    fn generated_policy_normalizes_roots_and_preserves_protection() -> noprop::TestResult {
        let fixture = tempfile::tempdir().unwrap();
        let cwd = fixture.path().join("workspace");
        let git = cwd.join(".git");
        std::fs::create_dir_all(&git).unwrap();
        let cwd = std::fs::canonicalize(cwd).unwrap();
        let git = std::fs::canonicalize(git).unwrap();

        test_support::run(0x4c49_4e55_5850_4f4c, 512, |ctx| {
            let count = noprop::sample_usize_in(ctx, 0..=6);
            let mut requested = Vec::new();
            for _ in 0..count {
                let root = fixture.path().join(test_support::safe_component(ctx));
                std::fs::create_dir_all(&root).unwrap();
                requested.push(root.clone());
                if noprop::sample_bool(ctx) {
                    requested.push(root);
                }
            }
            let policy =
                LinuxSandboxPolicy::for_command(&cwd, &requested, std::slice::from_ref(&git))
                    .unwrap();

            assert!(policy.validate().is_ok());
            assert!(policy.writable_roots.contains(&cwd));
            // The whole metadata root is read-only; only the paths Git must
            // update stay writable, and missing protected entries keep no
            // placeholder at all.
            assert!(policy.read_only_roots.contains(&git));
            assert!(!policy.writable_roots.contains(&git));
            for suffix in ["refs/heads", "objects", "logs"] {
                let path = git.join(suffix);
                assert_eq!(
                    policy.writable_roots.contains(&path),
                    path.is_dir(),
                    "unexpected writable Git path {suffix}"
                );
            }
            assert!(
                policy
                    .writable_roots
                    .windows(2)
                    .all(|pair| pair[0] < pair[1])
            );
            assert!(policy.read_only_paths.contains(&git.join("objects/info")));
            assert!(policy.read_only_paths.contains(&git.join("objects/pack")));
            Ok(())
        })
    }

    #[test]
    fn worktree_add_policy_opens_parent_but_masks_existing_sibling_metadata() {
        let fixture = tempfile::tempdir().unwrap();
        let cwd = fixture.path().join("workspace");
        let git = cwd.join(".git");
        let sibling = git.join("worktrees").join("sibling");
        std::fs::create_dir_all(&sibling).unwrap();
        let cwd = std::fs::canonicalize(&cwd).unwrap();
        let git = std::fs::canonicalize(&git).unwrap();
        let sibling = std::fs::canonicalize(&sibling).unwrap();

        let policy = LinuxSandboxPolicy::for_git_worktree_add(
            &cwd,
            std::slice::from_ref(&cwd),
            std::slice::from_ref(&git),
            std::slice::from_ref(&sibling),
        )
        .unwrap();

        assert!(policy.validate().is_ok());
        // The metadata root itself is read-only; only `worktrees` (to create
        // the new private metadata) is writable, and existing sibling metadata
        // stays read-only.
        assert!(policy.read_only_roots.contains(&git));
        assert!(policy.writable_roots.contains(&git.join("worktrees")));
        assert!(!policy.read_only_paths.contains(&git.join("worktrees")));
        assert!(policy.read_only_paths.contains(&sibling));
        assert!(!policy.writable_roots.contains(&git.join("config")));
        assert!(!policy.writable_roots.contains(&git.join("hooks")));
    }

    #[test]
    fn generated_invalid_policy_mutations_fail_closed() -> noprop::TestResult {
        let fixture = tempfile::tempdir().unwrap();
        let cwd = fixture.path().join("workspace");
        let extra = fixture.path().join("extra");
        std::fs::create_dir(&cwd).unwrap();
        std::fs::create_dir(&extra).unwrap();
        let cwd = std::fs::canonicalize(cwd).unwrap();
        let extra = std::fs::canonicalize(extra).unwrap();

        test_support::run(0x4c49_4e55_584d_5554, 512, |ctx| {
            let mut policy =
                LinuxSandboxPolicy::for_command(&cwd, std::slice::from_ref(&extra), &[]).unwrap();
            match noprop::sample_usize_in(ctx, 0..4) {
                0 => policy.version = policy.version.saturating_add(1),
                1 => policy.writable_roots.retain(|root| root != &cwd),
                2 => policy.writable_roots.push(extra.clone()),
                _ => policy
                    .read_only_paths
                    .push(fixture.path().join("outside-mask")),
            }
            assert!(
                policy.validate().is_err(),
                "invalid policy mutation was accepted: {policy:?}"
            );
            Ok(())
        })
    }
}
