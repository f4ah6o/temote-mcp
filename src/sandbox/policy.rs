use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

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

#[derive(Debug, Clone)]
pub(super) struct SandboxSpec {
    writable_roots: Vec<PathBuf>,
    read_only_overrides: Vec<PathBuf>,
}

impl SandboxSpec {
    pub(super) fn command(cwd: &Path, writable_roots: &[PathBuf]) -> Result<Self> {
        let cwd = canonical_existing_root(cwd)?;
        let mut roots = Vec::with_capacity(writable_roots.len() + 3);
        roots.push(cwd.clone());
        for root in writable_roots {
            roots.push(canonical_existing_root(root)?);
        }
        roots.push(canonical_existing_root(Path::new("/tmp"))?);
        if let Some(tmpdir) = std::env::var_os("TMPDIR") {
            roots.push(canonical_existing_root(Path::new(&tmpdir))?);
        }
        normalize_roots(&mut roots);
        Ok(Self {
            writable_roots: roots,
            read_only_overrides: Vec::new(),
        })
    }

    pub(super) fn git(
        cwd: &Path,
        writable_roots: &[PathBuf],
        git_metadata_roots: &[PathBuf],
    ) -> Result<Self> {
        let mut spec = Self::command(cwd, writable_roots)?;
        for root in git_metadata_roots {
            let root = canonical_existing_root(root)?;
            spec.writable_roots.push(root.clone());
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

    pub(super) fn writable_roots(&self) -> &[PathBuf] {
        &self.writable_roots
    }

    pub(super) fn read_only_overrides(&self) -> &[PathBuf] {
        &self.read_only_overrides
    }

    pub(super) fn protected_metadata_paths(&self, root: &Path) -> Vec<PathBuf> {
        PROTECTED_METADATA_NAMES
            .iter()
            .map(|name| root.join(name))
            .collect()
    }
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
        canonical.is_absolute(),
        "sandbox root is not absolute: {}",
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
}
