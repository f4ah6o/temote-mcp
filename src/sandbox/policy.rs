use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::sandbox::{PROTECTED_METADATA_NAMES, discover_protected_metadata_paths};

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

const GIT_WORKTREE_ADD_READ_ONLY_PATHS: &[&str] = &[
    "config",
    "hooks",
    "info",
    "attributes",
    "description",
    "packed-refs",
    "shallow",
    "refs/tags",
    "refs/remotes",
    "objects/info",
    "objects/pack",
];

#[derive(Debug, Clone)]
pub(super) struct SandboxSpec {
    writable_roots: Vec<PathBuf>,
    read_only_overrides: Vec<PathBuf>,
    read_only_roots: Vec<PathBuf>,
    read_only_symlinks: Vec<PathBuf>,
    read_only_scaffold_directories: Vec<PathBuf>,
    read_only_files: Vec<PathBuf>,
    hidden_roots: Vec<PathBuf>,
    discovered_protected_metadata_paths: Vec<PathBuf>,
    network_access: bool,
}

impl SandboxSpec {
    pub(super) fn command(
        cwd: &Path,
        writable_roots: &[PathBuf],
        network_access: bool,
    ) -> Result<Self> {
        Self::scoped_command(cwd, writable_roots, network_access)
    }

    /// Developer-tool profile: workspace plus narrowly scoped tool
    /// cache/state roots, top-level protected metadata masks, and an explicit
    /// network capability selected by the dev-tool operation class.
    pub(super) fn developer_tool(
        cwd: &Path,
        writable_roots: &[PathBuf],
        network_access: bool,
    ) -> Result<Self> {
        Self::scoped_command(cwd, writable_roots, network_access)
    }

    fn scoped_command(
        cwd: &Path,
        writable_roots: &[PathBuf],
        network_access: bool,
    ) -> Result<Self> {
        let cwd = canonical_existing_root(cwd)?;
        let mut roots = Vec::with_capacity(writable_roots.len() + 3);
        roots.push(cwd.clone());
        for root in writable_roots {
            roots.push(canonical_existing_root(root)?);
        }
        normalize_roots(&mut roots);
        roots.push(canonical_existing_root(Path::new("/tmp"))?);
        if let Some(tmpdir) = std::env::var_os("TMPDIR") {
            roots.push(canonical_existing_root(Path::new(&tmpdir))?);
        }
        normalize_roots(&mut roots);
        Ok(Self {
            writable_roots: roots,
            read_only_overrides: Vec::new(),
            read_only_roots: Vec::new(),
            read_only_symlinks: Vec::new(),
            read_only_scaffold_directories: Vec::new(),
            read_only_files: Vec::new(),
            hidden_roots: Vec::new(),
            discovered_protected_metadata_paths: Vec::new(),
            network_access,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn local_agent(
        cwd: &Path,
        writable_roots: &[PathBuf],
        temporary_roots: &[PathBuf],
        read_only_paths: &[PathBuf],
        read_only_roots: &[PathBuf],
        read_only_symlinks: &[crate::sandbox::LocalAgentSymlink],
        read_only_scaffold_directories: &[PathBuf],
        read_only_files: &[PathBuf],
        hidden_roots: &[PathBuf],
    ) -> Result<Self> {
        let _cwd = canonical_existing_root(cwd)?;
        let mut writable = writable_roots
            .iter()
            .map(|root| canonical_existing_root(root))
            .collect::<Result<Vec<_>>>()?;
        normalize_roots(&mut writable);
        let discovered_protected_metadata_paths =
            discover_local_agent_metadata_for_roots(&writable)?;
        let mut roots = Vec::with_capacity(writable.len() + temporary_roots.len());
        roots.extend(writable);
        roots.extend(
            temporary_roots
                .iter()
                .map(|root| canonical_existing_root(root))
                .collect::<Result<Vec<_>>>()?,
        );
        normalize_roots(&mut roots);
        let mut read_only_overrides = read_only_paths
            .iter()
            .map(|path| {
                std::fs::canonicalize(path)
                    .with_context(|| format!("cannot resolve read-only path {}", path.display()))
            })
            .collect::<Result<Vec<_>>>()?;
        normalize_paths(&mut read_only_overrides);
        let mut visible_roots = read_only_roots
            .iter()
            .map(|root| canonical_existing_root(root))
            .collect::<Result<Vec<_>>>()?;
        normalize_roots(&mut visible_roots);
        let mut hidden = hidden_roots
            .iter()
            .map(|root| canonical_existing_root(root))
            .collect::<Result<Vec<_>>>()?;
        normalize_roots(&mut hidden);
        let mut read_only_symlinks = read_only_symlinks
            .iter()
            .map(|symlink| symlink.link.clone())
            .collect::<Vec<_>>();
        normalize_paths(&mut read_only_symlinks);
        for link in &read_only_symlinks {
            anyhow::ensure!(
                link.is_absolute(),
                "read-only symlink is not absolute: {}",
                link.display()
            );
            anyhow::ensure!(
                !roots
                    .iter()
                    .chain(visible_roots.iter())
                    .any(|root| link.starts_with(root)),
                "read-only symlink is inside a visible root: {}",
                link.display()
            );
            let canonical = std::fs::canonicalize(link)
                .with_context(|| format!("cannot resolve read-only symlink {}", link.display()))?;
            anyhow::ensure!(
                canonical.is_dir(),
                "read-only symlink target is not a directory: {}",
                link.display()
            );
        }
        let mut scaffold_directories = read_only_scaffold_directories.to_vec();
        normalize_paths(&mut scaffold_directories);
        let mut files = read_only_files.to_vec();
        normalize_paths(&mut files);
        for file in &files {
            anyhow::ensure!(
                file.is_absolute(),
                "read-only file is not absolute: {}",
                file.display()
            );
            anyhow::ensure!(
                !roots
                    .iter()
                    .chain(visible_roots.iter())
                    .any(|root| file.starts_with(root)),
                "read-only file is inside a visible root: {}",
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
                std::fs::canonicalize(file)
                    .with_context(|| format!("cannot resolve read-only file {}", file.display()))?
                    == *file,
                "read-only file is not canonical: {}",
                file.display()
            );
        }
        for root in &visible_roots {
            anyhow::ensure!(
                !roots.iter().any(|writable| writable == root),
                "read-only root cannot be a writable root: {}",
                root.display()
            );
        }
        for hidden_root in &hidden {
            anyhow::ensure!(
                hidden_root != Path::new("/"),
                "hidden root cannot be the filesystem root"
            );
            for visible in roots.iter().chain(visible_roots.iter()) {
                anyhow::ensure!(
                    hidden_root != visible && !hidden_root.starts_with(visible),
                    "hidden root is inside a visible root: {}",
                    hidden_root.display()
                );
            }
        }
        Ok(Self {
            writable_roots: roots,
            read_only_overrides,
            read_only_roots: visible_roots,
            read_only_symlinks,
            read_only_scaffold_directories: scaffold_directories,
            read_only_files: files,
            hidden_roots: hidden,
            discovered_protected_metadata_paths,
            network_access: true,
        })
    }

    pub(super) fn git(
        cwd: &Path,
        writable_roots: &[PathBuf],
        git_metadata_roots: &[PathBuf],
    ) -> Result<Self> {
        // Git profiles are always network-restricted; their callers pass
        // `CommandNetworkPolicy::Restricted` on every platform.
        let mut spec = Self::command(cwd, writable_roots, false)?;
        for root in git_metadata_roots {
            let root = canonical_existing_root(root)?;
            spec.writable_roots.push(root.clone());
            spec.discovered_protected_metadata_paths
                .retain(|path| path != &root);
            if root.join("gitdir").is_file() {
                spec.read_only_overrides.push(root.join("gitdir"));
                spec.read_only_overrides.push(root.join("commondir"));
            } else {
                spec.read_only_overrides
                    .extend(GIT_READ_ONLY_PATHS.iter().map(|suffix| root.join(suffix)));
            }
        }
        normalize_roots(&mut spec.writable_roots);
        normalize_paths(&mut spec.read_only_overrides);
        Ok(spec)
    }

    pub(super) fn git_worktree_add(
        cwd: &Path,
        writable_roots: &[PathBuf],
        git_metadata_roots: &[PathBuf],
        protected_worktree_roots: &[PathBuf],
    ) -> Result<Self> {
        // Git profiles are always network-restricted; their callers pass
        // `CommandNetworkPolicy::Restricted` on every platform.
        let mut spec = Self::command(cwd, writable_roots, false)?;
        let mut common_git_roots = Vec::new();
        for root in git_metadata_roots {
            let root = canonical_existing_root(root)?;
            spec.writable_roots.push(root.clone());
            spec.discovered_protected_metadata_paths
                .retain(|path| path != &root);
            if root.join("gitdir").is_file() {
                spec.read_only_overrides.push(root.join("gitdir"));
                spec.read_only_overrides.push(root.join("commondir"));
            } else {
                common_git_roots.push(root.clone());
                spec.read_only_overrides.extend(
                    GIT_WORKTREE_ADD_READ_ONLY_PATHS
                        .iter()
                        .map(|suffix| root.join(suffix)),
                );
            }
        }
        for protected in protected_worktree_roots {
            let protected = canonical_existing_root(protected)?;
            anyhow::ensure!(
                common_git_roots.iter().any(|common| {
                    protected.parent() == Some(common.join("worktrees").as_path())
                }),
                "protected worktree metadata root is not a direct child of a validated common Git worktrees directory: {}",
                protected.display()
            );
            spec.read_only_overrides.push(protected);
        }
        normalize_roots(&mut spec.writable_roots);
        normalize_paths(&mut spec.read_only_overrides);
        Ok(spec)
    }

    pub(super) fn writable_roots(&self) -> &[PathBuf] {
        &self.writable_roots
    }

    pub(super) fn read_only_overrides(&self) -> &[PathBuf] {
        &self.read_only_overrides
    }

    pub(super) fn read_only_roots(&self) -> &[PathBuf] {
        &self.read_only_roots
    }

    pub(super) fn read_only_symlinks(&self) -> &[PathBuf] {
        &self.read_only_symlinks
    }

    pub(super) fn read_only_scaffold_directories(&self) -> &[PathBuf] {
        &self.read_only_scaffold_directories
    }

    pub(super) fn read_only_files(&self) -> &[PathBuf] {
        &self.read_only_files
    }

    pub(super) fn hidden_roots(&self) -> &[PathBuf] {
        &self.hidden_roots
    }

    pub(super) fn network_access(&self) -> bool {
        self.network_access
    }

    pub(super) fn protected_metadata_paths(&self, root: &Path) -> Vec<PathBuf> {
        let mut paths = PROTECTED_METADATA_NAMES
            .iter()
            .map(|name| root.join(name))
            .collect::<Vec<_>>();
        paths.extend(
            self.discovered_protected_metadata_paths
                .iter()
                .filter(|path| path.starts_with(root))
                .cloned(),
        );
        normalize_paths(&mut paths);
        paths
    }
}

fn discover_local_agent_metadata_for_roots(roots: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for root in roots {
        paths.extend(discover_protected_metadata_paths(root)?);
    }
    normalize_paths(&mut paths);
    Ok(paths)
}

fn canonical_existing_root(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let canonical = std::fs::canonicalize(&path)
        .with_context(|| format!("cannot resolve sandbox root {}", path.display()))?;
    anyhow::ensure!(
        canonical.is_absolute() && canonical.is_dir(),
        "sandbox root is not an absolute directory: {}",
        canonical.display()
    );
    anyhow::ensure!(
        canonical.to_str().is_some(),
        "sandbox root is not valid UTF-8: {}",
        canonical.display()
    );
    Ok(canonical)
}

fn normalize_roots(paths: &mut Vec<PathBuf>) {
    paths.sort();
    paths.dedup();
}

fn normalize_paths(paths: &mut Vec<PathBuf>) {
    paths.sort();
    paths.dedup();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{
        ProtectedMetadataScanLimits, discover_protected_metadata_paths_with_limits,
    };
    use crate::test_support;

    #[test]
    fn command_spec_rejects_regular_file_roots() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("not-a-directory");
        std::fs::write(&file, b"x").unwrap();
        assert!(SandboxSpec::command(root.path(), &[file], false).is_err());
    }

    #[test]
    fn command_spec_keeps_top_level_masks_without_recursive_scan() {
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

        let spec = SandboxSpec::command(&workspace, &[], false).unwrap();
        for name in PROTECTED_METADATA_NAMES {
            assert!(
                spec.protected_metadata_paths(&workspace)
                    .contains(&workspace.join(name))
            );
        }
    }

    #[test]
    fn generated_command_spec_normalizes_duplicate_roots() -> noprop::TestResult {
        let fixture = tempfile::tempdir().unwrap();
        let cwd = fixture.path().join("cwd");
        std::fs::create_dir(&cwd).unwrap();
        let cwd = std::fs::canonicalize(&cwd).unwrap();
        let roots = (0..6)
            .map(|index| {
                let path = fixture.path().join(format!("root-{index}"));
                std::fs::create_dir(&path).unwrap();
                std::fs::canonicalize(path).unwrap()
            })
            .collect::<Vec<_>>();

        test_support::run(0x5341_4e44_5350_4543, 512, |ctx| {
            let count = noprop::sample_usize_in(ctx, 0..=12);
            let requested = (0..count)
                .map(|_| roots[noprop::sample_usize_in(ctx, 0..roots.len())].clone())
                .collect::<Vec<_>>();
            let spec = SandboxSpec::command(&cwd, &requested, false).unwrap();

            assert!(spec.writable_roots().contains(&cwd));
            assert!(
                spec.writable_roots()
                    .windows(2)
                    .all(|pair| pair[0] < pair[1]),
                "writable roots are not sorted and unique: {:?}",
                spec.writable_roots()
            );
            for root in &requested {
                assert!(spec.writable_roots().contains(root));
            }
            Ok(())
        })
    }

    #[test]
    fn git_spec_keeps_sensitive_metadata_read_only() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let git = workspace.join(".git");
        std::fs::create_dir_all(&git).unwrap();

        let spec = SandboxSpec::git(
            &workspace,
            std::slice::from_ref(&workspace),
            std::slice::from_ref(&git),
        )
        .unwrap();

        let git = std::fs::canonicalize(&git).unwrap();
        assert!(spec.writable_roots().contains(&git));
        assert!(spec.read_only_overrides().contains(&git.join("config")));
        assert!(spec.read_only_overrides().contains(&git.join("hooks")));
        assert!(!spec.read_only_overrides().contains(&git.join("index")));
        assert!(!spec.read_only_overrides().contains(&git.join("objects")));
    }

    #[test]
    fn git_worktree_add_spec_opens_parent_but_masks_existing_sibling_metadata() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let git = workspace.join(".git");
        let sibling = git.join("worktrees").join("sibling");
        std::fs::create_dir_all(&sibling).unwrap();

        let spec = SandboxSpec::git_worktree_add(
            &workspace,
            std::slice::from_ref(&workspace),
            std::slice::from_ref(&git),
            std::slice::from_ref(&sibling),
        )
        .unwrap();

        let git = std::fs::canonicalize(&git).unwrap();
        let sibling = std::fs::canonicalize(&sibling).unwrap();
        assert!(spec.writable_roots().contains(&git));
        assert!(!spec.read_only_overrides().contains(&git.join("worktrees")));
        assert!(spec.read_only_overrides().contains(&sibling));
        assert!(spec.read_only_overrides().contains(&git.join("config")));
        assert!(spec.read_only_overrides().contains(&git.join("hooks")));
    }
}
