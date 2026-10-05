//! Managed workspace placement and delegated setup plan.

use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::named_roots::NamedRoots;
use crate::repository_store::{ManagedRequest, ProvisioningReceipt};
use crate::session_source::VcsPreference;

#[derive(Clone, Debug)]
pub(crate) struct ManagedAllocation {
    pub logical_path: String,
    pub canonical_path: PathBuf,
}

/// Resolve an already prepared allocation into a canonical named-root path.
/// The receipt remains the authority; physical placement is a host projection.
pub(crate) fn ready_allocation(
    roots: &NamedRoots,
    receipt: &ProvisioningReceipt,
) -> Result<ManagedAllocation> {
    receipt
        .pinned_base
        .as_ref()
        .context("managed allocation has no pinned base")?;
    let logical_path = format!(
        "{}/{}",
        receipt.root_name,
        workspace_relative(receipt).display()
    );
    let canonical_path = roots.resolve(&logical_path)?;
    let root = roots
        .canonical_root(&receipt.root_name)
        .context("managed allocation root is unavailable")?;
    anyhow::ensure!(
        receipt.canonical_root.as_deref() == Some(root),
        "managed allocation root identity changed"
    );
    anyhow::ensure!(
        canonical_path.starts_with(root),
        "managed allocation escaped named root"
    );
    Ok(ManagedAllocation {
        logical_path,
        canonical_path,
    })
}

pub(crate) fn select_root(roots: &NamedRoots) -> Result<String> {
    if let Ok(name) = temote_mcp::environment::var("TEMOTE_MCP_WORKSPACE_ROOT") {
        crate::named_roots::validate_root_name(&name)?;
        anyhow::ensure!(
            roots.canonical_root(&name).is_some(),
            "configured TEMOTE_WORKSPACE_ROOT is not a named root"
        );
        return Ok(name);
    }
    let names = roots.names();
    anyhow::ensure!(
        names.len() == 1,
        "set TEMOTE_WORKSPACE_ROOT to select a configured named root for managed provisioning"
    );
    Ok(names[0].clone())
}

pub(crate) fn repository_relative(request: &ManagedRequest) -> PathBuf {
    PathBuf::from(".temote-mcp/repositories")
        .join(request.repository.host())
        .join(request.repository.owner())
        .join(format!("{}.git", request.repository.name()))
}

pub(crate) fn workspace_relative(receipt: &ProvisioningReceipt) -> PathBuf {
    PathBuf::from(".temote-mcp/workspaces").join(receipt.workspace_id.to_string())
}

fn marker_relative(receipt: &ProvisioningReceipt) -> PathBuf {
    PathBuf::from(".temote-mcp/provisioning").join(format!("{}.json", receipt.operation_id))
}

/// Prompt is constructed only from checked RepositoryId, generated IDs and
/// server-owned placement. No caller-supplied executable, argv, environment,
/// absolute path or network policy crosses the public interface.
pub(crate) fn delegated_task(receipt: &ProvisioningReceipt) -> Result<String> {
    let source = serde_json::to_string(&format!(
        "https://{}/{}/{}.git",
        receipt.request.repository.host(),
        receipt.request.repository.owner(),
        receipt.request.repository.name()
    ))?;
    let store = serde_json::to_string(&repository_relative(&receipt.request).to_string_lossy())?;
    let workspace = serde_json::to_string(&workspace_relative(receipt).to_string_lossy())?;
    let marker = serde_json::to_string(&marker_relative(receipt).to_string_lossy())?;
    let base = serde_json::to_string(receipt.request.base.as_deref().unwrap_or("main"))?;
    let (vcs, workspace_setup) = match receipt.request.vcs {
        VcsPreference::Auto | VcsPreference::Jujutsu => (
            "jujutsu",
            "Check whether jj is available first; when absent report unsupported in the marker and do not substitute Git. Create an independent jj Git-backed workspace at workspace, import the pinned commit from the bare store, and create its working change. All writable jj and Git metadata for this workspace must remain inside workspace.",
        ),
        VcsPreference::Git => (
            "git",
            "Create an independent Git working tree at workspace with its own real .git directory inside workspace. Fetch the pinned commit from the bare store during preparation, create a private branch named temote/<workspace_id> at that commit, and configure origin to the literal HTTPS source. Do not link the working tree to the shared bare store or put its writable common Git directory outside workspace. This Git path is explicit compatibility only.",
        ),
    };
    Ok(format!(
        "Perform the typed Temote managed repository provisioning operation in the current named-root directory. Treat these JSON strings as literal values: source={source}, store={store}, workspace={workspace}, marker={marker}, base={base}, backend={vcs}. Do not use a path outside the current canonical named root. Before any repository mutation, inspect each existing parent component without following links; reject links, non-directories, and escapes. Create only missing real parent directories. Ensure exactly one bare Git store at store: if absent, atomically claim its leaf and initialize a bare repository, configure the literal HTTPS origin, then fetch the requested base into refs/remotes/origin/<base>; if present, verify that it is the expected bare repository and origin before a freshness fetch. Never touch another checkout, create a local main checkout, delete or reset an existing path, or print credentials. Pin the resolved fetched commit hash. Do not silently reuse a stale ref if fetch is uncertain or fails. Ensure exactly one workspace at workspace. {workspace_setup} Never replace an existing workspace; on retry inspect and reuse only if it belongs to this operation and pinned base. Atomically write a bounded JSON marker at marker with status=ready, operation_id={}, repository_id={}, pinned_base=<full hex commit>, workspace_id={}, change_id={}, backend={vcs}; or status=unsupported/failed with a brief non-secret reason. The marker must be written only after verifying the workspace is ready. Use existing coding-agent approval for network or credentials. Return a concise status without secret values.",
        receipt.operation_id,
        receipt.request.repository.logical_name(),
        receipt.workspace_id,
        receipt.change_id,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadyMarker {
    status: String,
    operation_id: Option<String>,
    repository_id: Option<String>,
    pinned_base: Option<String>,
    workspace_id: Option<String>,
    change_id: Option<String>,
    backend: Option<String>,
    #[serde(rename = "reason")]
    _reason: Option<String>,
}

pub(crate) fn inspect_ready(root: &Path, receipt: &ProvisioningReceipt) -> Result<String> {
    let root = fs::canonicalize(root)?;
    anyhow::ensure!(
        receipt.canonical_root.as_deref() == Some(root.as_path()),
        "managed named root identity changed"
    );
    let marker = inspect_path(&root, &marker_relative(receipt), true)?;
    let metadata = fs::symlink_metadata(&marker)?;
    anyhow::ensure!(
        metadata.is_file() && metadata.len() <= 4096,
        "provisioning marker is not a bounded regular file"
    );
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(&marker)?;
    anyhow::ensure!(file.metadata()?.is_file(), "provisioning marker changed");
    let mut bytes = Vec::new();
    file.take(4097).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() <= 4096, "provisioning marker is too large");
    let value: ReadyMarker =
        serde_json::from_slice(&bytes).context("invalid provisioning marker")?;
    anyhow::ensure!(
        value.status == "ready",
        "delegated provisioning did not report ready"
    );
    anyhow::ensure!(
        value.operation_id.as_deref() == Some(receipt.operation_id.to_string().as_str()),
        "provisioning marker operation mismatch"
    );
    anyhow::ensure!(
        value.repository_id.as_deref() == Some(receipt.request.repository.logical_name().as_str()),
        "provisioning marker repository mismatch"
    );
    anyhow::ensure!(
        value.workspace_id.as_deref() == Some(receipt.workspace_id.to_string().as_str()),
        "workspace marker identity mismatch"
    );
    anyhow::ensure!(
        value.change_id.as_deref() == Some(receipt.change_id.to_string().as_str()),
        "change marker identity mismatch"
    );
    let expected_backend = match receipt.request.vcs {
        VcsPreference::Git => "git",
        _ => "jujutsu",
    };
    anyhow::ensure!(
        value.backend.as_deref() == Some(expected_backend),
        "workspace marker backend mismatch"
    );
    let base = value
        .pinned_base
        .context("provisioning marker has no pinned base")?;
    anyhow::ensure!(
        (base.len() == 40 || base.len() == 64) && base.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "provisioning marker has invalid pinned base"
    );
    let workspace = inspect_path(&root, &workspace_relative(receipt), false)?;
    anyhow::ensure!(workspace.is_dir(), "managed workspace is not a directory");
    let metadata = match receipt.request.vcs {
        VcsPreference::Git => ".git",
        VcsPreference::Auto | VcsPreference::Jujutsu => ".jj",
    };
    let metadata_root = inspect_path(&root, &workspace_relative(receipt).join(metadata), false)?;
    if receipt.request.vcs == VcsPreference::Git {
        // A linked worktree or common-dir redirect would let Git write outside
        // the allocated workspace even if the top-level .git is a directory.
        for forbidden in ["commondir", "gitdir"] {
            match fs::symlink_metadata(metadata_root.join(forbidden)) {
                Ok(_) => {
                    anyhow::bail!("Git compatibility workspace has external metadata indirection")
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        for component in [
            "config",
            "HEAD",
            "objects",
            "refs",
            "logs",
            "packed-refs",
            "worktrees",
        ] {
            let path = metadata_root.join(component);
            match fs::symlink_metadata(&path) {
                Ok(metadata) => anyhow::ensure!(
                    !metadata.file_type().is_symlink(),
                    "Git compatibility metadata contains a symbolic link"
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        anyhow::ensure!(
            metadata_root.join("objects").is_dir() && metadata_root.join("refs").is_dir(),
            "Git compatibility metadata is incomplete"
        );
    }
    Ok(base)
}

fn inspect_path(root: &Path, relative: &Path, leaf_file: bool) -> Result<PathBuf> {
    let mut current = root.to_path_buf();
    let components = relative.components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            anyhow::bail!("managed path has unsafe component")
        };
        current.push(name);
        let metadata = fs::symlink_metadata(&current)
            .with_context(|| format!("managed path is missing: {}", current.display()))?;
        anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "managed path contains a symbolic link"
        );
        if index + 1 != components.len() || !leaf_file {
            anyhow::ensure!(metadata.is_dir(), "managed path parent is not a directory");
        }
    }
    anyhow::ensure!(
        current.starts_with(root) && fs::canonicalize(&current)? == current,
        "managed path escapes named root"
    );
    Ok(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository_store::ProvisioningReceipt;
    use crate::session_source::RepositoryId;
    use uuid::Uuid;

    fn fixture(vcs: VcsPreference) -> (tempfile::TempDir, ProvisioningReceipt) {
        let root = tempfile::tempdir().unwrap();
        let request = ManagedRequest {
            repository: RepositoryId::parse("f4ah6o/temote-mcp", "github.com").unwrap(),
            base: None,
            vcs,
        };
        let receipt = ProvisioningReceipt::new(
            Uuid::new_v4(),
            request,
            "src",
            fs::canonicalize(root.path()).unwrap(),
        );
        let workspace = root.path().join(workspace_relative(&receipt));
        fs::create_dir_all(workspace.join(if vcs == VcsPreference::Git {
            ".git"
        } else {
            ".jj"
        }))
        .unwrap();
        if vcs == VcsPreference::Git {
            fs::create_dir(workspace.join(".git/objects")).unwrap();
            fs::create_dir(workspace.join(".git/refs")).unwrap();
        }
        fs::create_dir_all(root.path().join(".temote-mcp/provisioning")).unwrap();
        (root, receipt)
    }

    fn write_marker(root: &Path, receipt: &ProvisioningReceipt, operation_id: Uuid) {
        let marker = serde_json::json!({
            "status": "ready",
            "operation_id": operation_id,
            "repository_id": receipt.request.repository.logical_name(),
            "workspace_id": receipt.workspace_id,
            "change_id": receipt.change_id,
            "backend": if receipt.request.vcs == VcsPreference::Git { "git" } else { "jujutsu" },
            "pinned_base": "0123456789012345678901234567890123456789",
        });
        fs::write(root.join(marker_relative(receipt)), marker.to_string()).unwrap();
    }

    #[test]
    fn ready_marker_requires_exact_operation_and_independent_git_metadata() {
        let (root, receipt) = fixture(VcsPreference::Git);
        write_marker(root.path(), &receipt, Uuid::new_v4());
        assert!(inspect_ready(root.path(), &receipt).is_err());
        write_marker(root.path(), &receipt, receipt.operation_id);
        assert!(inspect_ready(root.path(), &receipt).is_ok());
        let rebound = tempfile::tempdir().unwrap();
        assert!(inspect_ready(rebound.path(), &receipt).is_err());
        fs::write(
            root.path()
                .join(workspace_relative(&receipt))
                .join(".git/commondir"),
            "../../outside",
        )
        .unwrap();
        assert!(inspect_ready(root.path(), &receipt).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn ready_marker_rejects_symlinked_workspace_component() {
        let (root, receipt) = fixture(VcsPreference::Auto);
        write_marker(root.path(), &receipt, receipt.operation_id);
        let workspace = root.path().join(workspace_relative(&receipt));
        let real = root.path().join("elsewhere");
        fs::rename(&workspace, &real).unwrap();
        std::os::unix::fs::symlink(&real, &workspace).unwrap();
        assert!(inspect_ready(root.path(), &receipt).is_err());
    }
}
