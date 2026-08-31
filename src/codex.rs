use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use serde_json::{Value, json};
use uuid::Uuid;

const PLUGIN_NAME: &str = "temote-mcp";
const MARKETPLACE: &str = "debug";
const BINARY_HINT: &str = ".temote-mcp-bin";
const INSTALLER_LOCK: &str = ".temote-mcp-plugin.lock";
const TRANSACTION_PREFIX: &str = ".temote-mcp.txn-";
const MAX_CONFIG_BYTES: u64 = 8 * 1024 * 1024;
const SKILL: &str = include_str!("../skills/temote-mcp/SKILL.md");

pub fn run(args: &[String]) -> Result<String, String> {
    match args {
        [] => Ok(usage()),
        [arg] if arg == "--help" || arg == "-h" => Ok(usage()),
        [plugin, action] if plugin == "plugin" && action == "install" => install_current(),
        [plugin, action] if plugin == "plugin" && action == "uninstall" => uninstall_current(),
        [status] if status == "status" => status_current(false),
        [status, flag] if status == "status" && flag == "--json" => status_current(true),
        [diagnose] if diagnose == "diagnose" => diagnose_current(false),
        [diagnose, flag] if diagnose == "diagnose" && flag == "--json" => diagnose_current(true),
        _ => Err(format!(
            "unsupported Codex command: {}\n\n{}",
            args.join(" "),
            usage()
        )),
    }
}

pub fn usage() -> String {
    "Codex plugin integration\n\n\
Usage:\n\
  temote-mcp codex plugin install\n\
  temote-mcp codex plugin uninstall\n\
  temote-mcp codex status [--json]\n\
  temote-mcp codex diagnose [--json]\n\n\
The installed plugin is a thin local router. It pins the exact temote-mcp binary\n\
that performed the install and does not own session lifecycle, sandbox, approval,\n\
OAuth, or ingress policy.\n"
        .to_owned()
}

fn install_current() -> Result<String, String> {
    let codex_home = resolve_codex_home()?;
    let binary = current_binary()?;
    let status = install_at(&codex_home, &binary)?;
    Ok(format!(
        "Installed Temote MCP Codex plugin\nplugin: {}\nbinary: {}\nconfig: {}\n\nRestart an already-running Codex session so its loaded plugin inventory matches disk.\n",
        status.plugin_dir.display(),
        binary.display(),
        status.config_path.display(),
    ))
}

fn uninstall_current() -> Result<String, String> {
    let codex_home = resolve_codex_home()?;
    let result = uninstall_at(&codex_home)?;
    Ok(format!(
        "Uninstalled Temote MCP Codex plugin\nplugin removed: {}\nconfig entry removed: {}\n\nRestart an already-running Codex session so its loaded plugin inventory matches disk.\n",
        result.removed_plugin, result.removed_config
    ))
}

fn status_current(as_json: bool) -> Result<String, String> {
    let codex_home = resolve_codex_home()?;
    let current = current_binary()?;
    let status = inspect(&codex_home, &current)?;
    render_status(&status, as_json)
}

fn diagnose_current(as_json: bool) -> Result<String, String> {
    let codex_home = resolve_codex_home()?;
    let current = current_binary()?;
    let mut status = inspect(&codex_home, &current)?;
    let cli_health = Command::new(&current)
        .args(["mcp", "--help"])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    status.mcp_cli_health = Some(cli_health);
    if !cli_health {
        status
            .problems
            .push("the selected temote-mcp binary did not accept `mcp --help`".to_owned());
    }
    render_status(&status, as_json)
}

fn render_status(status: &Status, as_json: bool) -> Result<String, String> {
    if as_json {
        serde_json::to_string_pretty(&status.to_json()).map_err(|error| error.to_string())
    } else {
        Ok(status.to_text())
    }
}

fn resolve_codex_home() -> Result<PathBuf, String> {
    if let Some(value) = std::env::var_os("CODEX_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(value));
    }
    crate::platform_paths::home_dir()
        .map(|home| home.join(".codex"))
        .ok_or_else(|| {
            "could not determine CODEX_HOME or the current user's home directory".to_owned()
        })
}

fn current_binary() -> Result<PathBuf, String> {
    let path = std::env::current_exe()
        .map_err(|error| format!("could not resolve current temote-mcp binary: {error}"))?;
    canonical_binary(&path)
}

fn canonical_binary(path: &Path) -> Result<PathBuf, String> {
    let path = fs::canonicalize(path).map_err(|error| {
        format!(
            "could not resolve temote-mcp binary {}: {error}",
            path.display()
        )
    })?;
    let metadata = fs::metadata(&path).map_err(|error| {
        format!(
            "could not inspect temote-mcp binary {}: {error}",
            path.display()
        )
    })?;
    if !metadata.is_file() {
        return Err(format!(
            "temote-mcp binary is not a regular file: {}",
            path.display()
        ));
    }
    Ok(path)
}

#[derive(Debug)]
struct InstallStatus {
    plugin_dir: PathBuf,
    config_path: PathBuf,
}

#[derive(Debug)]
struct UninstallStatus {
    removed_plugin: bool,
    removed_config: bool,
}

fn plugin_key() -> String {
    format!("{PLUGIN_NAME}@{MARKETPLACE}")
}

fn plugin_root(codex_home: &Path) -> PathBuf {
    codex_home
        .join("plugins")
        .join("cache")
        .join(MARKETPLACE)
        .join(PLUGIN_NAME)
}

fn plugin_dir(codex_home: &Path) -> PathBuf {
    plugin_root(codex_home).join(env!("CARGO_PKG_VERSION"))
}

fn config_path(codex_home: &Path) -> PathBuf {
    codex_home.join("config.toml")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TransactionPoint {
    AfterLock,
    AfterStageCreate,
    AfterManifestWrite,
    AfterMcpWrite,
    AfterSkillWrite,
    AfterHintWrite,
    AfterStageValidate,
    AfterBundleCommit,
    BeforeConfigCommit,
    AfterConfigTempSync,
    BeforeUninstallBundleRemoval,
}

type TransactionHook<'a> = &'a mut dyn FnMut(TransactionPoint) -> Result<(), String>;

struct InstallerLock {
    #[cfg(unix)]
    file: File,
}

impl InstallerLock {
    fn acquire(codex_home: &Path) -> Result<Self, String> {
        fs::create_dir_all(codex_home).map_err(|error| {
            format!(
                "could not create Codex home {}: {error}",
                codex_home.display()
            )
        })?;
        let path = codex_home.join(INSTALLER_LOCK);
        #[cfg(unix)]
        {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)
                .map_err(|error| {
                    format!(
                        "could not open Codex plugin installer lock {}: {error}",
                        path.display()
                    )
                })?;
            let metadata = file.metadata().map_err(|error| {
                format!(
                    "could not inspect Codex plugin installer lock {}: {error}",
                    path.display()
                )
            })?;
            if !metadata.is_file() {
                return Err(format!(
                    "Codex plugin installer lock is not a regular file: {}",
                    path.display()
                ));
            }
            // SAFETY: flock is called with a valid open file descriptor owned by file.
            let status = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if status != 0 {
                return Err(format!(
                    "another Temote Codex plugin install or uninstall is active: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(Self { file })
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Err(
                "transactional Codex plugin installation is supported only on Unix hosts"
                    .to_owned(),
            )
        }
    }
}

#[cfg(unix)]
impl Drop for InstallerLock {
    fn drop(&mut self) {
        // SAFETY: file remains open for the lifetime of the lock.
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn marketplace_root(codex_home: &Path) -> PathBuf {
    codex_home.join("plugins").join("cache").join(MARKETPLACE)
}

fn transaction_artifacts(codex_home: &Path) -> Result<Vec<PathBuf>, String> {
    let parent = marketplace_root(codex_home);
    let entries = match fs::read_dir(&parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(format!(
                "could not inspect plugin marketplace directory {}: {error}",
                parent.display()
            ));
        }
    };
    let mut artifacts = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "could not inspect plugin marketplace directory {}: {error}",
                parent.display()
            )
        })?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(TRANSACTION_PREFIX))
        {
            artifacts.push(entry.path());
        }
    }
    artifacts.sort();
    Ok(artifacts)
}

fn cleanup_transaction_artifacts(codex_home: &Path) -> Result<(), String> {
    for path in transaction_artifacts(codex_home)? {
        remove_owned_plugin_root(&path)?;
    }
    Ok(())
}

fn validate_owned_plugin_root(path: &Path) -> Result<bool, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(format!(
                "could not inspect plugin directory {}: {error}",
                path.display()
            ));
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "refusing to use symlinked plugin directory: {}",
            path.display()
        ));
    }
    if !metadata.is_dir() {
        return Err(format!(
            "plugin cache path is not a directory: {}",
            path.display()
        ));
    }
    Ok(true)
}

fn create_private_directory(path: &Path) -> Result<(), String> {
    fs::create_dir(path).map_err(|error| {
        format!(
            "could not create staging directory {}: {error}",
            path.display()
        )
    })?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
        format!(
            "could not secure staging directory {}: {error}",
            path.display()
        )
    })?;
    Ok(())
}

fn write_staged_text(path: &Path, content: &str) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    let mut file = options
        .open(path)
        .map_err(|error| format!("could not create staged file {}: {error}", path.display()))?;
    file.write_all(content.as_bytes())
        .map_err(|error| format!("could not write staged file {}: {error}", path.display()))?;
    file.sync_all()
        .map_err(|error| format!("could not sync staged file {}: {error}", path.display()))
}

fn sync_directory(path: &Path) -> Result<(), String> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("could not sync directory {}: {error}", path.display()))
}

struct StagingGuard {
    path: PathBuf,
    armed: bool,
}

impl StagingGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn directory_entry_names(path: &Path) -> Result<Vec<String>, String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        format!(
            "could not inspect staged directory {}: {error}",
            path.display()
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "staged path is not a real directory: {}",
            path.display()
        ));
    }
    let mut names = fs::read_dir(path)
        .map_err(|error| {
            format!(
                "could not read staged directory {}: {error}",
                path.display()
            )
        })?
        .map(|entry| {
            entry
                .map_err(|error| {
                    format!(
                        "could not read staged directory {}: {error}",
                        path.display()
                    )
                })?
                .file_name()
                .into_string()
                .map_err(|_| {
                    format!(
                        "staged directory contains a non-UTF-8 entry: {}",
                        path.display()
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    names.sort();
    Ok(names)
}

fn validate_regular_staged_file(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("could not inspect staged file {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "staged path is not a regular file: {}",
            path.display()
        ));
    }
    Ok(())
}

fn validate_staged_bundle(stage_root: &Path, binary: &Path) -> Result<(), String> {
    let version = env!("CARGO_PKG_VERSION");
    if directory_entry_names(stage_root)? != [version.to_owned()] {
        return Err("staged plugin root contains unexpected entries".to_owned());
    }
    let target = stage_root.join(version);
    if directory_entry_names(&target)?
        != [
            ".codex-plugin".to_owned(),
            ".mcp.json".to_owned(),
            BINARY_HINT.to_owned(),
            "skills".to_owned(),
        ]
    {
        return Err("staged plugin bundle contains unexpected entries".to_owned());
    }
    let manifest_dir = target.join(".codex-plugin");
    if directory_entry_names(&manifest_dir)? != ["plugin.json".to_owned()] {
        return Err("staged plugin manifest directory contains unexpected entries".to_owned());
    }
    let skills_dir = target.join("skills");
    if directory_entry_names(&skills_dir)? != [PLUGIN_NAME.to_owned()] {
        return Err("staged plugin skills directory contains unexpected entries".to_owned());
    }
    let skill_dir = skills_dir.join(PLUGIN_NAME);
    if directory_entry_names(&skill_dir)? != ["SKILL.md".to_owned()] {
        return Err("staged plugin skill contains unexpected entries".to_owned());
    }

    let manifest_path = manifest_dir.join("plugin.json");
    let mcp_path = target.join(".mcp.json");
    let skill_path = skill_dir.join("SKILL.md");
    let hint_path = target.join(BINARY_HINT);
    for path in [&manifest_path, &mcp_path, &skill_path, &hint_path] {
        validate_regular_staged_file(path)?;
    }

    let manifest: Value = serde_json::from_str(
        &fs::read_to_string(&manifest_path)
            .map_err(|error| format!("could not read staged manifest: {error}"))?,
    )
    .map_err(|error| format!("staged plugin manifest is invalid: {error}"))?;
    if manifest["name"].as_str() != Some(PLUGIN_NAME)
        || manifest["version"].as_str() != Some(version)
    {
        return Err("staged plugin manifest identity is invalid".to_owned());
    }
    if read_mcp_command(&mcp_path).as_deref() != Some(binary) {
        return Err("staged MCP command does not pin the selected binary".to_owned());
    }
    if fs::read_to_string(&skill_path)
        .map_err(|error| format!("could not read staged skill: {error}"))?
        != SKILL
    {
        return Err("staged skill content is invalid".to_owned());
    }
    if fs::read_to_string(&hint_path)
        .map_err(|error| format!("could not read staged binary hint: {error}"))?
        .trim()
        != binary.to_string_lossy()
    {
        return Err("staged binary hint does not pin the selected binary".to_owned());
    }
    Ok(())
}

fn build_staged_bundle(
    codex_home: &Path,
    binary: &Path,
    hook: TransactionHook<'_>,
) -> Result<StagingGuard, String> {
    let parent = marketplace_root(codex_home);
    fs::create_dir_all(&parent).map_err(|error| {
        format!(
            "could not create plugin marketplace directory {}: {error}",
            parent.display()
        )
    })?;
    let stage_root = parent.join(format!("{TRANSACTION_PREFIX}{}", Uuid::new_v4()));
    create_private_directory(&stage_root)?;
    let guard = StagingGuard {
        path: stage_root.clone(),
        armed: true,
    };
    hook(TransactionPoint::AfterStageCreate)?;

    let target = stage_root.join(env!("CARGO_PKG_VERSION"));
    fs::create_dir(&target)
        .and_then(|_| fs::create_dir(target.join(".codex-plugin")))
        .and_then(|_| fs::create_dir(target.join("skills")))
        .and_then(|_| fs::create_dir(target.join("skills").join(PLUGIN_NAME)))
        .map_err(|error| format!("could not create staged plugin tree: {error}"))?;

    write_staged_text(
        &target.join(".codex-plugin").join("plugin.json"),
        &rendered_manifest()?,
    )?;
    hook(TransactionPoint::AfterManifestWrite)?;
    write_staged_text(&target.join(".mcp.json"), &rendered_mcp_config(binary)?)?;
    hook(TransactionPoint::AfterMcpWrite)?;
    write_staged_text(
        &target.join("skills").join(PLUGIN_NAME).join("SKILL.md"),
        SKILL,
    )?;
    hook(TransactionPoint::AfterSkillWrite)?;
    write_staged_text(
        &target.join(BINARY_HINT),
        &format!("{}\n", binary.display()),
    )?;
    hook(TransactionPoint::AfterHintWrite)?;

    validate_staged_bundle(&stage_root, binary)?;
    for directory in [
        target.join(".codex-plugin"),
        target.join("skills").join(PLUGIN_NAME),
        target.join("skills"),
        target,
        stage_root.clone(),
    ] {
        sync_directory(&directory)?;
    }
    hook(TransactionPoint::AfterStageValidate)?;
    Ok(guard)
}

#[cfg(target_os = "linux")]
fn atomic_exchange(left: &Path, right: &Path) -> Result<(), String> {
    let left = std::ffi::CString::new(left.as_os_str().as_bytes())
        .map_err(|_| "plugin path contains a NUL byte".to_owned())?;
    let right = std::ffi::CString::new(right.as_os_str().as_bytes())
        .map_err(|_| "plugin path contains a NUL byte".to_owned())?;
    // SAFETY: both C strings are valid and point to sibling paths for the duration of the call.
    let status = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            left.as_ptr(),
            libc::AT_FDCWD,
            right.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(format!(
            "could not atomically exchange plugin directories: {}",
            std::io::Error::last_os_error()
        ))
    }
}

#[cfg(target_os = "macos")]
fn atomic_exchange(left: &Path, right: &Path) -> Result<(), String> {
    let left = std::ffi::CString::new(left.as_os_str().as_bytes())
        .map_err(|_| "plugin path contains a NUL byte".to_owned())?;
    let right = std::ffi::CString::new(right.as_os_str().as_bytes())
        .map_err(|_| "plugin path contains a NUL byte".to_owned())?;
    // SAFETY: both C strings are valid and point to sibling paths for the duration of the call.
    let status = unsafe { libc::renamex_np(left.as_ptr(), right.as_ptr(), libc::RENAME_SWAP) };
    if status == 0 {
        Ok(())
    } else {
        Err(format!(
            "could not atomically exchange plugin directories: {}",
            std::io::Error::last_os_error()
        ))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn atomic_exchange(_left: &Path, _right: &Path) -> Result<(), String> {
    Err("atomic plugin directory exchange is unavailable on this platform".to_owned())
}

struct BundlePlacement {
    root: PathBuf,
    alternate: PathBuf,
    replaced_existing: bool,
}

fn place_staged_bundle(mut stage: StagingGuard, root: &Path) -> Result<BundlePlacement, String> {
    let existed = validate_owned_plugin_root(root)?;
    if existed {
        atomic_exchange(root, &stage.path)?;
    } else {
        fs::rename(&stage.path, root).map_err(|error| {
            format!(
                "could not commit staged plugin directory {}: {error}",
                root.display()
            )
        })?;
    }
    let alternate = stage.path.clone();
    stage.disarm();
    let placement = BundlePlacement {
        root: root.to_path_buf(),
        alternate,
        replaced_existing: existed,
    };
    if let Err(error) = sync_directory(root.parent().unwrap()) {
        return match rollback_bundle(&placement) {
            Ok(()) => Err(error),
            Err(rollback) => Err(format!("{error}; plugin rollback also failed: {rollback}")),
        };
    }
    Ok(placement)
}

fn rollback_bundle(placement: &BundlePlacement) -> Result<(), String> {
    if placement.replaced_existing {
        atomic_exchange(&placement.root, &placement.alternate)?;
        remove_owned_plugin_root(&placement.alternate)?;
    } else {
        remove_owned_plugin_root(&placement.root)?;
    }
    sync_directory(placement.root.parent().unwrap())
}

fn finish_bundle_commit(placement: &BundlePlacement) -> Result<(), String> {
    if placement.replaced_existing {
        remove_owned_plugin_root(&placement.alternate)?;
        sync_directory(placement.root.parent().unwrap())?;
    }
    Ok(())
}

fn install_at(codex_home: &Path, binary: &Path) -> Result<InstallStatus, String> {
    let mut no_failure = |_| Ok(());
    install_at_with(codex_home, binary, &mut no_failure)
}

fn install_at_with(
    codex_home: &Path,
    binary: &Path,
    hook: TransactionHook<'_>,
) -> Result<InstallStatus, String> {
    let binary = canonical_binary(binary)?;
    let _lock = InstallerLock::acquire(codex_home)?;
    hook(TransactionPoint::AfterLock)?;

    let root = plugin_root(codex_home);
    validate_owned_plugin_root(&root)?;
    cleanup_transaction_artifacts(codex_home)?;
    let config = config_path(codex_home);
    let config_snapshot = read_config_snapshot(&config)?;
    let staged = build_staged_bundle(codex_home, &binary, hook)?;
    let placement = place_staged_bundle(staged, &root)?;

    if let Err(error) = hook(TransactionPoint::AfterBundleCommit) {
        return match rollback_bundle(&placement) {
            Ok(()) => Err(error),
            Err(rollback) => Err(format!("{error}; plugin rollback also failed: {rollback}")),
        };
    }

    let (updated_config, _) = updated_config_text(&config_snapshot.text, true);
    if let Err(error) = hook(TransactionPoint::BeforeConfigCommit)
        .and_then(|_| commit_config_snapshot(&config, &config_snapshot, &updated_config, hook))
    {
        return match rollback_bundle(&placement) {
            Ok(()) => Err(error),
            Err(rollback) => Err(format!("{error}; plugin rollback also failed: {rollback}")),
        };
    }

    finish_bundle_commit(&placement)?;
    Ok(InstallStatus {
        plugin_dir: plugin_dir(codex_home),
        config_path: config,
    })
}

fn uninstall_at(codex_home: &Path) -> Result<UninstallStatus, String> {
    let mut no_failure = |_| Ok(());
    uninstall_at_with(codex_home, &mut no_failure)
}

fn uninstall_at_with(
    codex_home: &Path,
    hook: TransactionHook<'_>,
) -> Result<UninstallStatus, String> {
    let _lock = InstallerLock::acquire(codex_home)?;
    hook(TransactionPoint::AfterLock)?;
    cleanup_transaction_artifacts(codex_home)?;

    let config = config_path(codex_home);
    let snapshot = read_config_snapshot(&config)?;
    let (updated, removed_config) = updated_config_text(&snapshot.text, false);
    commit_config_snapshot(&config, &snapshot, &updated, hook)?;
    hook(TransactionPoint::BeforeUninstallBundleRemoval)?;

    let removed_plugin = remove_owned_plugin_root(&plugin_root(codex_home))?;
    Ok(UninstallStatus {
        removed_plugin,
        removed_config,
    })
}

fn remove_owned_plugin_root(path: &Path) -> Result<bool, String> {
    if !validate_owned_plugin_root(path)? {
        return Ok(false);
    }
    fs::remove_dir_all(path).map_err(|error| {
        format!(
            "could not remove plugin directory {}: {error}",
            path.display()
        )
    })?;
    Ok(true)
}

fn rendered_manifest() -> Result<String, String> {
    let value = json!({
        "name": PLUGIN_NAME,
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Use Temote MCP local sessions, files, commands, Git, and host integrations from Codex.",
        "author": {
            "name": "f4ah6o",
            "url": "https://github.com/f4ah6o"
        },
        "homepage": "https://github.com/f4ah6o/temote-mcp",
        "repository": "https://github.com/f4ah6o/temote-mcp",
        "license": "MIT AND Apache-2.0",
        "keywords": ["codex", "mcp", "local-mcp", "temote-mcp"],
        "skills": "./skills/",
        "mcpServers": "./.mcp.json",
        "interface": {
            "displayName": "Temote MCP",
            "shortDescription": "Use a local machine safely through Temote MCP.",
            "longDescription": "Connect Codex to Temote MCP local sessions while keeping session lifecycle, named-root resolution, sandboxing, approvals, and host integration policy inside the native temote-mcp binary.",
            "developerName": "f4ah6o",
            "category": "Developer Tools",
            "capabilities": ["Interactive", "Read", "Write", "Shell"],
            "websiteURL": "https://github.com/f4ah6o/temote-mcp",
            "defaultPrompt": [
                "Show the available Temote sessions and summarize their state.",
                "Use Temote MCP to work on the matching local repository session.",
                "Diagnose why the selected Temote session cannot run the requested task."
            ]
        }
    });
    serde_json::to_string_pretty(&value)
        .map(|mut text| {
            text.push('\n');
            text
        })
        .map_err(|error| error.to_string())
}

fn rendered_mcp_config(binary: &Path) -> Result<String, String> {
    let value = json!({
        "mcpServers": {
            "temoteMcp": {
                "cwd": ".",
                "command": binary,
                "args": ["mcp"]
            }
        }
    });
    serde_json::to_string_pretty(&value)
        .map(|mut text| {
            text.push('\n');
            text
        })
        .map_err(|error| error.to_string())
}

#[derive(Clone, Debug)]
struct ConfigSnapshot {
    exists: bool,
    text: String,
    #[cfg(unix)]
    mode: Option<u32>,
}

impl ConfigSnapshot {
    fn matches(&self, other: &Self) -> bool {
        self.exists == other.exists && self.text == other.text && {
            #[cfg(unix)]
            {
                self.mode == other.mode
            }
            #[cfg(not(unix))]
            {
                true
            }
        }
    }
}

fn validate_owned_config_section(input: &str) -> Result<(), String> {
    let header = section_header();
    let mut in_target = false;
    let mut section_count = 0usize;
    let mut enabled_count = 0usize;
    for line in input.lines() {
        let trimmed = line.trim();
        let section = trimmed.starts_with('[') && trimmed.ends_with(']');
        if section {
            in_target = trimmed == header;
            if in_target {
                section_count += 1;
            }
            continue;
        }
        if in_target && !trimmed.is_empty() && !trimmed.starts_with('#') {
            if matches!(trimmed, "enabled = true" | "enabled = false") {
                enabled_count += 1;
            } else {
                return Err(format!(
                    "Temote plugin config section contains an unsupported entry: {trimmed}"
                ));
            }
        }
    }
    if section_count > 1 {
        return Err("Codex config contains duplicate Temote plugin sections".to_owned());
    }
    if section_count == 1 && enabled_count != 1 {
        return Err(
            "Temote plugin config section must contain exactly one enabled setting".to_owned(),
        );
    }
    Ok(())
}

fn read_config_snapshot(path: &Path) -> Result<ConfigSnapshot, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ConfigSnapshot {
                exists: false,
                text: String::new(),
                #[cfg(unix)]
                mode: None,
            });
        }
        Err(error) => {
            return Err(format!(
                "could not inspect Codex config {}: {error}",
                path.display()
            ));
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "refusing to replace symlinked Codex config: {}",
            path.display()
        ));
    }
    if !metadata.is_file() {
        return Err(format!(
            "Codex config is not a regular file: {}",
            path.display()
        ));
    }
    if metadata.len() > MAX_CONFIG_BYTES {
        return Err(format!(
            "Codex config exceeds {MAX_CONFIG_BYTES} bytes: {}",
            path.display()
        ));
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let file = options
        .open(path)
        .map_err(|error| format!("could not read Codex config {}: {error}", path.display()))?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not read Codex config {}: {error}", path.display()))?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(format!(
            "Codex config exceeds {MAX_CONFIG_BYTES} bytes: {}",
            path.display()
        ));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| format!("Codex config is not valid UTF-8: {}", path.display()))?;
    validate_owned_config_section(&text)?;
    Ok(ConfigSnapshot {
        exists: true,
        text,
        #[cfg(unix)]
        mode: Some(metadata.permissions().mode() & 0o777),
    })
}

fn updated_config_text(existing: &str, enabled: bool) -> (String, bool) {
    let (mut updated, removed) = remove_plugin_config_section(existing);
    if enabled {
        if !updated.is_empty() && !updated.ends_with('\n') {
            updated.push('\n');
        }
        if !updated.is_empty() && !updated.ends_with("\n\n") {
            updated.push('\n');
        }
        updated.push_str(&section_header());
        updated.push_str("\nenabled = true\n");
    }
    (updated, removed)
}

struct TemporaryFileGuard {
    path: PathBuf,
    armed: bool,
}

impl Drop for TemporaryFileGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn commit_config_snapshot(
    path: &Path,
    expected: &ConfigSnapshot,
    updated: &str,
    hook: TransactionHook<'_>,
) -> Result<bool, String> {
    if updated == expected.text {
        return Ok(false);
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("Codex config has no parent directory: {}", path.display()))?;
    fs::create_dir_all(parent).map_err(|error| {
        format!(
            "could not create Codex config directory {}: {error}",
            parent.display()
        )
    })?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");
    let temporary = parent.join(format!(".{file_name}.temote-{}.tmp", Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    let mut file = options.open(&temporary).map_err(|error| {
        format!(
            "could not create temporary Codex config {}: {error}",
            temporary.display()
        )
    })?;
    let mut guard = TemporaryFileGuard {
        path: temporary.clone(),
        armed: true,
    };
    file.write_all(updated.as_bytes()).map_err(|error| {
        format!(
            "could not write temporary Codex config {}: {error}",
            temporary.display()
        )
    })?;
    #[cfg(unix)]
    file.set_permissions(fs::Permissions::from_mode(expected.mode.unwrap_or(0o600)))
        .map_err(|error| {
            format!(
                "could not preserve Codex config permissions {}: {error}",
                temporary.display()
            )
        })?;
    file.sync_all().map_err(|error| {
        format!(
            "could not sync temporary Codex config {}: {error}",
            temporary.display()
        )
    })?;
    hook(TransactionPoint::AfterConfigTempSync)?;

    let current = read_config_snapshot(path)?;
    if !current.matches(expected) {
        return Err(format!(
            "Codex config changed concurrently; refusing to overwrite {}",
            path.display()
        ));
    }
    fs::rename(&temporary, path).map_err(|error| {
        format!(
            "could not atomically update Codex config {}: {error}",
            path.display()
        )
    })?;
    guard.armed = false;
    // The file contents were flushed before the atomic rename. A directory fsync
    // failure cannot safely be reported as an uncommitted config update.
    let _ = sync_directory(parent);
    Ok(true)
}

#[cfg(test)]
fn set_config_enabled(path: &Path, enabled: bool) -> Result<bool, String> {
    let snapshot = read_config_snapshot(path)?;
    let (updated, removed) = updated_config_text(&snapshot.text, enabled);
    let changed = updated != snapshot.text;
    let mut no_failure = |_| Ok(());
    commit_config_snapshot(path, &snapshot, &updated, &mut no_failure)?;
    Ok(changed && (removed || enabled))
}

fn section_header() -> String {
    format!("[plugins.\"{}\"]", plugin_key())
}

fn remove_plugin_config_section(input: &str) -> (String, bool) {
    let header = section_header();
    let mut output = String::with_capacity(input.len());
    let mut skipping = false;
    let mut removed = false;

    for line in input.split_inclusive('\n') {
        let trimmed = line.trim();
        let section = trimmed.starts_with('[') && trimmed.ends_with(']');
        if section {
            if trimmed == header {
                skipping = true;
                removed = true;
                continue;
            }
            if skipping {
                skipping = false;
            }
        }
        if !skipping {
            output.push_str(line);
        }
    }

    while output.ends_with("\n\n\n") {
        output.pop();
    }
    (output, removed)
}

fn stale_plugin_versions(codex_home: &Path) -> Result<Vec<PathBuf>, String> {
    let root = plugin_root(codex_home);
    if !validate_owned_plugin_root(&root)? {
        return Ok(Vec::new());
    }
    let mut stale = Vec::new();
    for entry in fs::read_dir(&root).map_err(|error| {
        format!(
            "could not inspect plugin versions {}: {error}",
            root.display()
        )
    })? {
        let entry = entry.map_err(|error| {
            format!(
                "could not inspect plugin versions {}: {error}",
                root.display()
            )
        })?;
        if entry.file_name() != std::ffi::OsStr::new(env!("CARGO_PKG_VERSION")) {
            stale.push(entry.path());
        }
    }
    stale.sort();
    Ok(stale)
}

#[derive(Debug)]
struct Status {
    codex_home: PathBuf,
    config_path: PathBuf,
    plugin_dir: PathBuf,
    plugin_key: String,
    enabled: bool,
    installed: bool,
    manifest_version: Option<String>,
    binary_hint_path: PathBuf,
    binary_hint: Option<PathBuf>,
    current_binary: PathBuf,
    binary_matches: bool,
    mcp_command: Option<PathBuf>,
    mcp_command_matches: bool,
    mcp_cli_health: Option<bool>,
    transaction_artifacts: Vec<PathBuf>,
    stale_versions: Vec<PathBuf>,
    dangling_config: bool,
    disabled_bundle: bool,
    problems: Vec<String>,
}

impl Status {
    fn to_json(&self) -> Value {
        json!({
            "codex_home": self.codex_home,
            "config_path": self.config_path,
            "plugin_key": self.plugin_key,
            "plugin_dir": self.plugin_dir,
            "enabled": self.enabled,
            "installed": self.installed,
            "manifest_version": self.manifest_version,
            "package_version": env!("CARGO_PKG_VERSION"),
            "binary_hint_path": self.binary_hint_path,
            "binary_hint": self.binary_hint,
            "current_binary": self.current_binary,
            "binary_matches": self.binary_matches,
            "mcp_command": self.mcp_command,
            "mcp_command_matches": self.mcp_command_matches,
            "mcp_cli_health": self.mcp_cli_health,
            "transaction_artifacts": self.transaction_artifacts,
            "stale_versions": self.stale_versions,
            "dangling_config": self.dangling_config,
            "disabled_bundle": self.disabled_bundle,
            "problems": self.problems,
            "healthy": self.problems.is_empty(),
        })
    }

    fn to_text(&self) -> String {
        let mut text = format!(
            "Codex home: {}\nPlugin key: {}\nPlugin dir: {}\nEnabled: {}\nInstalled: {}\nCurrent binary: {}\nPinned binary: {}\nBinary matches: {}\nMCP command matches: {}\nTransaction artifacts: {}\nStale versions: {}\nDangling config: {}\nDisabled bundle: {}\n",
            self.codex_home.display(),
            self.plugin_key,
            self.plugin_dir.display(),
            self.enabled,
            self.installed,
            self.current_binary.display(),
            self.binary_hint
                .as_deref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "<missing>".to_owned()),
            self.binary_matches,
            self.mcp_command_matches,
            self.transaction_artifacts.len(),
            self.stale_versions.len(),
            self.dangling_config,
            self.disabled_bundle,
        );
        if let Some(health) = self.mcp_cli_health {
            text.push_str(&format!("MCP CLI health: {health}\n"));
        }
        if self.problems.is_empty() {
            text.push_str("Status: healthy\n");
        } else {
            text.push_str("Status: attention required\n");
            for problem in &self.problems {
                text.push_str("- ");
                text.push_str(problem);
                text.push('\n');
            }
        }
        text
    }
}

fn inspect(codex_home: &Path, current_binary: &Path) -> Result<Status, String> {
    let root = plugin_root(codex_home);
    let root_present = validate_owned_plugin_root(&root)?;
    let target = plugin_dir(codex_home);
    let config = config_path(codex_home);
    let header = section_header();
    let config_snapshot = read_config_snapshot(&config)?;
    let enabled = config_section_enabled(&config_snapshot.text, &header);

    let manifest_path = target.join(".codex-plugin").join("plugin.json");
    let installed = manifest_path.is_file();
    let manifest_version = fs::read_to_string(&manifest_path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| {
            value
                .get("version")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });

    let binary_hint_path = target.join(BINARY_HINT);
    let binary_hint = fs::read_to_string(&binary_hint_path)
        .ok()
        .map(|value| PathBuf::from(value.trim()))
        .filter(|path| !path.as_os_str().is_empty());
    let binary_matches = binary_hint.as_deref() == Some(current_binary);
    let mcp_command = read_mcp_command(&target.join(".mcp.json"));
    let mcp_command_matches = mcp_command.as_deref() == Some(current_binary);
    let transaction_artifacts = transaction_artifacts(codex_home)?;
    let stale_versions = stale_plugin_versions(codex_home)?;
    let dangling_config = enabled && !installed;
    let disabled_bundle = !enabled && root_present;

    let mut problems = Vec::new();
    if !transaction_artifacts.is_empty() {
        problems.push(format!(
            "{} recoverable plugin transaction artifact(s) remain",
            transaction_artifacts.len()
        ));
    }
    if !stale_versions.is_empty() {
        problems.push(format!(
            "{} stale Temote plugin version(s) remain",
            stale_versions.len()
        ));
    }
    if dangling_config {
        problems.push(
            "the enabled Temote plugin config points to a missing or incomplete bundle".to_owned(),
        );
    }
    if disabled_bundle {
        problems.push("a disabled Temote plugin bundle remains as cleanup debt".to_owned());
    }
    if !enabled {
        problems.push("the Temote plugin is not enabled in Codex config".to_owned());
    }
    if !installed {
        problems.push("the Temote plugin manifest is missing from the Codex cache".to_owned());
    }
    if installed && manifest_version.as_deref() != Some(env!("CARGO_PKG_VERSION")) {
        problems
            .push("the installed plugin version does not match this temote-mcp binary".to_owned());
    }
    if binary_hint.is_none() {
        problems.push("the installed plugin has no pinned temote-mcp binary hint".to_owned());
    } else if !binary_matches {
        problems.push("the installed plugin is pinned to a different temote-mcp binary".to_owned());
    }
    if mcp_command.is_none() {
        problems.push("the installed plugin MCP command is missing or invalid".to_owned());
    } else if !mcp_command_matches {
        problems.push(
            "the installed plugin MCP command does not use this exact temote-mcp binary".to_owned(),
        );
    }

    Ok(Status {
        codex_home: codex_home.to_path_buf(),
        config_path: config,
        plugin_dir: target,
        plugin_key: plugin_key(),
        enabled,
        installed,
        manifest_version,
        binary_hint_path,
        binary_hint,
        current_binary: current_binary.to_path_buf(),
        binary_matches,
        mcp_command,
        mcp_command_matches,
        mcp_cli_health: None,
        transaction_artifacts,
        stale_versions,
        dangling_config,
        disabled_bundle,
        problems,
    })
}

fn config_section_enabled(input: &str, header: &str) -> bool {
    let mut in_target = false;
    for line in input.lines() {
        let trimmed = line.trim();
        let section = trimmed.starts_with('[') && trimmed.ends_with(']');
        if section {
            in_target = trimmed == header;
            continue;
        }
        if in_target && trimmed == "enabled = true" {
            return true;
        }
    }
    false
}

fn read_mcp_command(path: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value
        .get("mcpServers")?
        .get("temoteMcp")?
        .get("command")?
        .as_str()
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_binary(root: &Path) -> PathBuf {
        let path = root.join("temote-mcp-test-bin");
        fs::write(&path, b"test").unwrap();
        path
    }

    #[test]
    fn config_section_updates_preserve_unrelated_settings() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.toml");
        fs::write(
            &config,
            "model = \"gpt\"\n\n[plugins.\"other@example\"]\nenabled = true\n",
        )
        .unwrap();

        assert!(set_config_enabled(&config, true).unwrap());
        let enabled = fs::read_to_string(&config).unwrap();
        assert!(enabled.contains("model = \"gpt\""));
        assert!(enabled.contains("[plugins.\"other@example\"]"));
        assert!(enabled.contains(&section_header()));
        assert!(config_section_enabled(&enabled, &section_header()));

        assert!(set_config_enabled(&config, false).unwrap());
        let disabled = fs::read_to_string(&config).unwrap();
        assert!(disabled.contains("model = \"gpt\""));
        assert!(disabled.contains("[plugins.\"other@example\"]"));
        assert!(!disabled.contains(&section_header()));
    }

    #[test]
    fn install_pins_exact_binary_in_hint_and_mcp_config() {
        let root = tempfile::tempdir().unwrap();
        let codex_home = root.path().join("codex");
        let binary = dummy_binary(root.path());
        let canonical = fs::canonicalize(&binary).unwrap();

        let installed = install_at(&codex_home, &binary).unwrap();
        assert!(
            installed
                .plugin_dir
                .join(".codex-plugin/plugin.json")
                .is_file()
        );
        assert!(
            installed
                .plugin_dir
                .join("skills/temote-mcp/SKILL.md")
                .is_file()
        );
        assert_eq!(
            fs::read_to_string(installed.plugin_dir.join(BINARY_HINT))
                .unwrap()
                .trim(),
            canonical.to_string_lossy()
        );
        assert_eq!(
            read_mcp_command(&installed.plugin_dir.join(".mcp.json")).as_deref(),
            Some(canonical.as_path())
        );

        let status = inspect(&codex_home, &canonical).unwrap();
        assert!(status.enabled);
        assert!(status.installed);
        assert!(status.binary_matches);
        assert!(status.mcp_command_matches);
        assert!(status.problems.is_empty(), "{:?}", status.problems);
    }

    #[test]
    fn reinstall_removes_stale_versions_before_writing_current_version() {
        let root = tempfile::tempdir().unwrap();
        let codex_home = root.path().join("codex");
        let stale = plugin_root(&codex_home).join("old-version");
        fs::create_dir_all(&stale).unwrap();
        fs::write(stale.join("stale"), b"x").unwrap();
        let binary = dummy_binary(root.path());

        install_at(&codex_home, &binary).unwrap();
        assert!(!stale.exists());
        assert!(plugin_dir(&codex_home).exists());
    }

    #[cfg(unix)]
    #[test]
    fn install_refuses_symlinked_owned_plugin_root() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let codex_home = root.path().join("codex");
        let actual = root.path().join("actual");
        fs::create_dir_all(&actual).unwrap();
        let plugin_root = plugin_root(&codex_home);
        fs::create_dir_all(plugin_root.parent().unwrap()).unwrap();
        symlink(&actual, &plugin_root).unwrap();
        let binary = dummy_binary(root.path());

        let error = install_at(&codex_home, &binary).unwrap_err();
        assert!(error.contains("symlinked plugin directory"), "{error}");
    }

    #[test]
    fn failed_reinstall_boundaries_preserve_previous_bundle_and_config() {
        let points = [
            TransactionPoint::AfterLock,
            TransactionPoint::AfterStageCreate,
            TransactionPoint::AfterManifestWrite,
            TransactionPoint::AfterMcpWrite,
            TransactionPoint::AfterSkillWrite,
            TransactionPoint::AfterHintWrite,
            TransactionPoint::AfterStageValidate,
            TransactionPoint::AfterBundleCommit,
            TransactionPoint::BeforeConfigCommit,
            TransactionPoint::AfterConfigTempSync,
        ];
        for point in points {
            let root = tempfile::tempdir().unwrap();
            let codex_home = root.path().join("codex");
            let old_binary = root.path().join("temote-old");
            let new_binary = root.path().join("temote-new");
            fs::write(&old_binary, b"old").unwrap();
            fs::write(&new_binary, b"new").unwrap();
            install_at(&codex_home, &old_binary).unwrap();
            if point == TransactionPoint::AfterConfigTempSync {
                set_config_enabled(&config_path(&codex_home), false).unwrap();
            }
            let old_hint = fs::read_to_string(plugin_dir(&codex_home).join(BINARY_HINT)).unwrap();
            let old_config = fs::read_to_string(config_path(&codex_home)).unwrap();

            let mut fail_at = |current| {
                if current == point {
                    Err(format!("injected failure at {point:?}"))
                } else {
                    Ok(())
                }
            };
            let error = install_at_with(&codex_home, &new_binary, &mut fail_at).unwrap_err();
            assert!(error.contains("injected failure"), "{point:?}: {error}");
            assert_eq!(
                fs::read_to_string(plugin_dir(&codex_home).join(BINARY_HINT)).unwrap(),
                old_hint,
                "{point:?}"
            );
            assert_eq!(
                fs::read_to_string(config_path(&codex_home)).unwrap(),
                old_config,
                "{point:?}"
            );
            assert!(
                transaction_artifacts(&codex_home).unwrap().is_empty(),
                "{point:?}"
            );

            let installed = install_at(&codex_home, &new_binary).unwrap();
            assert_eq!(
                fs::read_to_string(installed.plugin_dir.join(BINARY_HINT))
                    .unwrap()
                    .trim(),
                fs::canonicalize(&new_binary).unwrap().to_string_lossy()
            );
            assert!(
                inspect(&codex_home, &fs::canonicalize(&new_binary).unwrap())
                    .unwrap()
                    .problems
                    .is_empty(),
                "{point:?}"
            );
        }
    }

    #[test]
    fn concurrent_unrelated_config_mutation_is_not_overwritten() {
        let root = tempfile::tempdir().unwrap();
        let codex_home = root.path().join("codex");
        let old_binary = root.path().join("temote-old");
        let new_binary = root.path().join("temote-new");
        fs::write(&old_binary, b"old").unwrap();
        fs::write(&new_binary, b"new").unwrap();
        install_at(&codex_home, &old_binary).unwrap();
        let config = config_path(&codex_home);
        set_config_enabled(&config, false).unwrap();
        let old_hint = fs::read_to_string(plugin_dir(&codex_home).join(BINARY_HINT)).unwrap();
        let concurrent = format!(
            "{}\n[plugins.\"other@example\"]\nenabled = true\n",
            fs::read_to_string(&config).unwrap()
        );
        let mut mutate = |point| {
            if point == TransactionPoint::AfterConfigTempSync {
                fs::write(&config, &concurrent).unwrap();
            }
            Ok(())
        };

        let error = install_at_with(&codex_home, &new_binary, &mut mutate).unwrap_err();
        assert!(error.contains("changed concurrently"), "{error}");
        assert_eq!(fs::read_to_string(&config).unwrap(), concurrent);
        assert_eq!(
            fs::read_to_string(plugin_dir(&codex_home).join(BINARY_HINT)).unwrap(),
            old_hint
        );
        assert!(transaction_artifacts(&codex_home).unwrap().is_empty());
    }

    #[test]
    fn failed_uninstall_disables_config_before_leaving_cleanup_debt() {
        let root = tempfile::tempdir().unwrap();
        let codex_home = root.path().join("codex");
        let binary = dummy_binary(root.path());
        let canonical = fs::canonicalize(&binary).unwrap();
        install_at(&codex_home, &binary).unwrap();
        let mut fail = |point| {
            if point == TransactionPoint::BeforeUninstallBundleRemoval {
                Err("injected bundle removal failure".to_owned())
            } else {
                Ok(())
            }
        };

        let error = uninstall_at_with(&codex_home, &mut fail).unwrap_err();
        assert!(error.contains("bundle removal failure"));
        assert!(plugin_root(&codex_home).is_dir());
        let config = fs::read_to_string(config_path(&codex_home)).unwrap();
        assert!(!config_section_enabled(&config, &section_header()));
        let status = inspect(&codex_home, &canonical).unwrap();
        assert!(status.disabled_bundle);
        assert!(!status.dangling_config);
    }

    #[cfg(unix)]
    #[test]
    fn config_updates_preserve_permissions_and_refuse_symlinks() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.toml");
        fs::write(&config, "model = \"gpt\"\n").unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o640)).unwrap();
        set_config_enabled(&config, true).unwrap();
        assert_eq!(
            fs::metadata(&config).unwrap().permissions().mode() & 0o777,
            0o640
        );

        let codex_home = root.path().join("codex");
        fs::create_dir_all(&codex_home).unwrap();
        let actual = root.path().join("actual-config.toml");
        fs::write(&actual, "model = \"safe\"\n").unwrap();
        symlink(&actual, config_path(&codex_home)).unwrap();
        let binary = dummy_binary(&codex_home);
        let error = install_at(&codex_home, &binary).unwrap_err();
        assert!(error.contains("symlinked Codex config"), "{error}");
        assert_eq!(fs::read_to_string(&actual).unwrap(), "model = \"safe\"\n");
        assert!(!plugin_root(&codex_home).exists());
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_and_non_regular_configs_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let unreadable = root.path().join("unreadable.toml");
        fs::write(&unreadable, "model = \"gpt\"\n").unwrap();
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
        let error = read_config_snapshot(&unreadable).unwrap_err();
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(error.contains("could not read Codex config"), "{error}");

        let directory = root.path().join("config-directory");
        fs::create_dir(&directory).unwrap();
        let error = read_config_snapshot(&directory).unwrap_err();
        assert!(error.contains("not a regular file"), "{error}");
    }

    #[test]
    fn malformed_owned_config_is_rejected_without_rewrite() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.toml");
        let malformed = format!(
            "{}\nenabled = true\n\n{}\nenabled = false\n",
            section_header(),
            section_header()
        );
        fs::write(&config, &malformed).unwrap();

        let error = set_config_enabled(&config, true).unwrap_err();
        assert!(
            error.contains("duplicate Temote plugin sections"),
            "{error}"
        );
        assert_eq!(fs::read_to_string(&config).unwrap(), malformed);
    }

    #[test]
    fn status_reports_recoverable_transaction_dangling_and_stale_states() {
        let root = tempfile::tempdir().unwrap();
        let codex_home = root.path().join("codex");
        fs::create_dir_all(&codex_home).unwrap();
        let binary = dummy_binary(root.path());
        let canonical = fs::canonicalize(&binary).unwrap();
        fs::write(
            config_path(&codex_home),
            format!("{}\nenabled = true\n", section_header()),
        )
        .unwrap();
        fs::create_dir_all(plugin_root(&codex_home).join("old-version")).unwrap();
        fs::create_dir_all(
            marketplace_root(&codex_home).join(format!("{TRANSACTION_PREFIX}orphan")),
        )
        .unwrap();

        let status = inspect(&codex_home, &canonical).unwrap();
        assert!(status.dangling_config);
        assert_eq!(status.transaction_artifacts.len(), 1);
        assert_eq!(status.stale_versions.len(), 1);
        let json = status.to_json();
        assert_eq!(json["dangling_config"], true);
        assert_eq!(json["transaction_artifacts"].as_array().unwrap().len(), 1);
        assert_eq!(json["stale_versions"].as_array().unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_installer_lock_fails_safely() {
        let root = tempfile::tempdir().unwrap();
        let codex_home = root.path().join("codex");
        let _held = InstallerLock::acquire(&codex_home).unwrap();
        let error = match InstallerLock::acquire(&codex_home) {
            Ok(_) => panic!("second installer lock unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(error.contains("another Temote Codex plugin"), "{error}");
    }
}
