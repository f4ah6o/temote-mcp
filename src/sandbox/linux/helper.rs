// Linux sandbox implementation informed by openai/codex revision
// 20fedafff83f5c681fc62f73b0ca3227e42e3f8b (Apache-2.0).
// See docs/linux-sandbox.md and THIRD_PARTY_NOTICES.md for provenance and local changes.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fmt::Display;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};

use super::policy::{
    LINUX_SANDBOX_POLICY_VERSION, LinuxNetworkPolicy, LinuxPinnedWorkspace, LinuxSandboxPolicy,
};

#[derive(Debug)]
struct HelperArgs {
    policy: LinuxSandboxPolicy,
    command: Vec<String>,
}

fn parse_helper_args<I>(raw: I) -> Result<HelperArgs>
where
    I: IntoIterator<Item = String>,
{
    let raw = raw.into_iter().collect::<Vec<_>>();
    let separator = raw
        .iter()
        .position(|arg| arg == "--")
        .context("Linux sandbox helper requires '--' before the command")?;
    let command = raw[separator + 1..].to_vec();
    validate_command(&command)?;

    let mut args = noargs::RawArgs::new(raw[..separator].iter().cloned());
    args.metadata_mut().app_name = "temote-linux-sandbox";
    args.metadata_mut().help_flag_name = None;
    let policy = noargs::opt("policy")
        .ty("JSON")
        .doc("JSON-serialized Temote Linux sandbox policy")
        .take(&mut args)
        .then(|opt| parse_policy(opt.value()))
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    args.finish()
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;

    Ok(HelperArgs { policy, command })
}

pub(super) fn command_args(policy: &LinuxSandboxPolicy, command: &[String]) -> Result<Vec<String>> {
    validate_command(command)?;
    let policy =
        serde_json::to_string(policy).context("failed to serialize Linux sandbox policy")?;
    let mut args = vec!["--policy".to_owned(), policy, "--".to_owned()];
    args.extend(command.iter().cloned());
    Ok(args)
}

fn capabilities_payload() -> String {
    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "policy_schema": LINUX_SANDBOX_POLICY_VERSION,
    })
    .to_string()
}

pub(super) fn run_main() -> ! {
    let raw: Vec<String> = std::env::args().collect();
    if raw.len() == 2 && raw[1] == "--capabilities" {
        println!("{}", capabilities_payload());
        std::process::exit(0);
    }
    let args = match parse_helper_args(std::env::args()) {
        Ok(args) => args,
        Err(error) => fail(error.context("invalid Linux sandbox helper arguments")),
    };

    let bwrap = match crate::sandbox::svc_acct_bwrap() {
        Ok(path) => path,
        Err(error) => fail(error.context("bubblewrap is required for Linux sandboxing")),
    };
    let seccomp_program = match build_seccomp_filter(args.policy.network) {
        Ok(program) => program,
        Err(error) => fail(error.context("failed to compile Linux seccomp filter")),
    };
    let seccomp_fd = match create_sealed_seccomp_memfd(&seccomp_program) {
        Ok(fd) => fd,
        Err(error) => fail(error.context("failed to prepare Linux seccomp filter fd")),
    };
    let pinned_workspace_fd = match &args.policy.pinned_workspace {
        Some(pinned) => match pin_policy_workspace(pinned) {
            Ok(fd) => Some(fd),
            Err(error) => fail(anyhow::anyhow!(
                "failed to pin the validated managed workspace: {error:#}"
            )),
        },
        None => None,
    };
    let pinned_read_only_paths = match pinned_workspace_fd.as_ref() {
        Some(fd) => match pin_workspace_read_only_paths(&args.policy, fd) {
            Ok(paths) => paths,
            Err(error) => fail(anyhow::anyhow!(
                "failed to pin protected paths in the validated managed workspace: {error:#}"
            )),
        },
        None => Vec::new(),
    };
    let bwrap_args = match build_bwrap_args(
        &args.policy,
        args.command,
        seccomp_fd.as_raw_fd(),
        pinned_workspace_fd.as_ref().map(AsRawFd::as_raw_fd),
        &pinned_read_only_paths,
    ) {
        Ok(args) => args,
        Err(error) => fail(error.context("failed to construct bubblewrap sandbox")),
    };
    // Keep seccomp_fd alive across exec. The memfd intentionally has no CLOEXEC
    // flag; bubblewrap reads and closes the fd before launching the sandboxed
    // command. The fd is sealed read-only before this point.
    exec_absolute(&bwrap, &bwrap_args);
}

fn parse_policy(value: &str) -> std::result::Result<LinuxSandboxPolicy, String> {
    const MAX_POLICY_BYTES: usize = 1024 * 1024;
    if value.len() > MAX_POLICY_BYTES {
        return Err("Linux sandbox policy is too large".to_owned());
    }
    let policy: LinuxSandboxPolicy = serde_json::from_str(value)
        .map_err(|error| format!("invalid Linux sandbox policy JSON: {error}"))?;
    policy
        .validate()
        .map_err(|error| format!("unsafe Linux sandbox policy: {error:#}"))?;
    Ok(policy)
}

fn validate_command(command: &[String]) -> Result<()> {
    anyhow::ensure!(!command.is_empty(), "command must not be empty");
    anyhow::ensure!(
        command
            .iter()
            .all(|argument| !argument.as_bytes().contains(&0)),
        "command contains a NUL byte"
    );
    Ok(())
}

const MAX_PINNED_GIT_POINTER_BYTES: usize = 8192;

#[derive(Debug)]
enum PinnedReadOnlySource {
    Existing(OwnedFd),
    Missing(PathBuf),
}

#[derive(Debug)]
struct PinnedReadOnlyPath {
    target: PathBuf,
    source: PinnedReadOnlySource,
}

/// Opens the validated workspace without following symbolic links and verifies
/// that the opened directory entity still presents exactly the validated
/// repository identity.
///
/// The descriptor intentionally has no `O_CLOEXEC` flag: bubblewrap inherits it
/// and uses it as the `--bind-fd` source, so the workspace and writable scope
/// are bound from the verified entity instead of the (re-resolved) path.
fn pin_policy_workspace(pinned: &LinuxPinnedWorkspace) -> Result<OwnedFd> {
    let path = pinned
        .path
        .to_str()
        .context("pinned workspace path is not valid UTF-8")?;
    let path =
        CString::new(path.as_bytes()).context("pinned workspace path contains a NUL byte")?;
    // SAFETY: `path` is a valid NUL-terminated path and the returned descriptor
    // is owned by this process. `O_PATH | O_NOFOLLOW` pins the directory entity
    // itself and refuses a symbolic link at the workspace path.
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("cannot open pinned workspace {}", pinned.path.display()));
    }
    // SAFETY: `fd` is a freshly opened descriptor owned by this scope.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    verify_pinned_workspace(&fd, pinned)?;
    Ok(fd)
}

fn pin_workspace_read_only_paths(
    policy: &LinuxSandboxPolicy,
    workspace_fd: &OwnedFd,
) -> Result<Vec<PinnedReadOnlyPath>> {
    let pinned = policy
        .pinned_workspace
        .as_ref()
        .context("protected path pinning requires a pinned workspace policy")?;
    let mut paths = Vec::new();
    for target in &policy.read_only_paths {
        if !target.starts_with(&pinned.path) || target == &pinned.path {
            continue;
        }
        let relative = target
            .strip_prefix(&pinned.path)
            .context("protected path escaped the pinned workspace")?;
        let source = pin_relative_workspace_path(workspace_fd, &pinned.path, relative)
            .with_context(|| format!("cannot pin protected path {}", target.display()))?;
        paths.push(PinnedReadOnlyPath {
            target: target.clone(),
            source,
        });
    }
    Ok(paths)
}

fn pin_relative_workspace_path(
    workspace_fd: &OwnedFd,
    workspace: &Path,
    relative: &Path,
) -> Result<PinnedReadOnlySource> {
    let components = relative
        .components()
        .map(|component| match component {
            std::path::Component::Normal(name) => Ok(name.to_owned()),
            _ => anyhow::bail!("protected workspace path is not relative and normalized"),
        })
        .collect::<Result<Vec<_>>>()?;
    anyhow::ensure!(
        !components.is_empty(),
        "protected workspace path must name a descendant"
    );

    let mut opened = Vec::<OwnedFd>::new();
    let mut current_fd = workspace_fd.as_raw_fd();
    let mut absolute = workspace.to_owned();
    for (index, component) in components.iter().enumerate() {
        absolute.push(component);
        let final_component = index + 1 == components.len();
        let flags = if final_component {
            libc::O_PATH
        } else {
            libc::O_PATH | libc::O_DIRECTORY
        };
        let Some(next) = openat_component_optional(current_fd, component, flags)? else {
            return Ok(PinnedReadOnlySource::Missing(absolute));
        };
        if final_component {
            let metadata = fstat(next.as_raw_fd())?;
            anyhow::ensure!(
                metadata.st_mode & libc::S_IFMT != libc::S_IFLNK,
                "protected workspace path became a symbolic link: {}",
                absolute.display()
            );
            return Ok(PinnedReadOnlySource::Existing(next));
        }
        opened.push(next);
        current_fd = opened
            .last()
            .context("protected path descriptor disappeared")?
            .as_raw_fd();
    }
    unreachable!("non-empty protected path has a final component")
}

fn openat_component_optional(
    directory: i32,
    name: &std::ffi::OsStr,
    flags: i32,
) -> Result<Option<OwnedFd>> {
    let name = CString::new(name.as_bytes()).context("descriptor name contains a NUL byte")?;
    // SAFETY: `directory` is a live directory descriptor, `name` is a valid
    // NUL-terminated relative component and the returned descriptor is owned
    // by this scope.
    let fd = unsafe { libc::openat(directory, name.as_ptr(), flags | libc::O_NOFOLLOW) };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error).context("cannot open protected workspace path component");
    }
    // SAFETY: `fd` is a freshly opened descriptor owned by this scope.
    Ok(Some(unsafe { OwnedFd::from_raw_fd(fd) }))
}

fn verify_pinned_workspace(fd: &OwnedFd, pinned: &LinuxPinnedWorkspace) -> Result<()> {
    let metadata = fstat(fd.as_raw_fd())?;
    anyhow::ensure!(
        metadata.st_mode & libc::S_IFMT == libc::S_IFDIR,
        "pinned workspace is not a directory: {}",
        pinned.path.display()
    );
    let dot_git = openat_no_follow(fd.as_raw_fd(), ".git", libc::O_PATH)?;
    let dot_git_metadata = fstat(dot_git.as_raw_fd())?;
    match dot_git_metadata.st_mode & libc::S_IFMT {
        libc::S_IFDIR => {
            anyhow::ensure!(
                pinned.worktree_root == pinned.primary_checkout,
                "pinned workspace is not the validated primary checkout: {}",
                pinned.path.display()
            );
            let dot_git_path = read_descriptor_path(dot_git.as_raw_fd())?;
            let canonical = std::fs::canonicalize(&dot_git_path)
                .with_context(|| format!("cannot resolve {}", dot_git_path.display()))?;
            anyhow::ensure!(
                canonical == pinned.common_dir,
                "pinned workspace common Git directory changed: {}",
                canonical.display()
            );
        }
        libc::S_IFREG => {
            anyhow::ensure!(
                pinned.worktree_root != pinned.primary_checkout,
                "pinned workspace is not the validated linked worktree: {}",
                pinned.path.display()
            );
            let contents = openat_no_follow(fd.as_raw_fd(), ".git", libc::O_RDONLY)?;
            let contents = read_descriptor(&contents, MAX_PINNED_GIT_POINTER_BYTES)?;
            let contents = String::from_utf8(contents)
                .context("pinned workspace .git pointer is not valid UTF-8")?;
            let pointer = parse_gitdir_pointer(&contents)?;
            let workspace_dir = read_descriptor_path(fd.as_raw_fd())?;
            let private = canonical_pointer_target(&workspace_dir.join(".git"), &pointer)?;
            let expected_private = pinned
                .metadata_roots
                .iter()
                .find(|root| {
                    root.parent().and_then(Path::file_name)
                        == Some(std::ffi::OsStr::new("worktrees"))
                })
                .context("pinned workspace has no validated private metadata root")?;
            anyhow::ensure!(
                &private == expected_private,
                "pinned workspace belongs to a different repository identity: {}",
                private.display()
            );
            let commondir_file = private.join("commondir");
            let commondir_pointer =
                std::fs::read_to_string(&commondir_file).with_context(|| {
                    format!(
                        "cannot read Git common directory pointer {}",
                        commondir_file.display()
                    )
                })?;
            let commondir_pointer = commondir_pointer
                .lines()
                .next()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .context("Git common directory pointer is empty")?;
            let commondir =
                canonical_pointer_target(&commondir_file, Path::new(commondir_pointer))?;
            anyhow::ensure!(
                commondir == pinned.common_dir,
                "pinned workspace common Git directory changed: {}",
                commondir.display()
            );
        }
        _ => anyhow::bail!(
            "pinned workspace .git metadata is neither a directory nor a file: {}",
            pinned.path.display()
        ),
    }
    Ok(())
}

fn openat_no_follow(directory: i32, name: &str, flags: i32) -> Result<OwnedFd> {
    let name = CString::new(name.as_bytes()).context("descriptor name contains a NUL byte")?;
    // SAFETY: `directory` is a live directory descriptor, `name` is a valid
    // NUL-terminated relative path and the returned descriptor is owned here.
    let fd = unsafe { libc::openat(directory, name.as_ptr(), flags | libc::O_NOFOLLOW) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("cannot open workspace metadata");
    }
    // SAFETY: `fd` is a freshly opened descriptor owned by this scope.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn fstat(fd: i32) -> Result<libc::stat> {
    // SAFETY: the zeroed `stat` is fully initialized by a successful `fstat`.
    let mut metadata = unsafe { std::mem::zeroed::<libc::stat>() };
    let result = unsafe { libc::fstat(fd, &mut metadata) };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("cannot inspect workspace descriptor");
    }
    Ok(metadata)
}

fn read_descriptor(fd: &OwnedFd, maximum: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 1024];
    loop {
        // SAFETY: `buffer` is a valid writable buffer for its length.
        let read = unsafe { libc::read(fd.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
        if read < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("cannot read workspace metadata");
        }
        if read == 0 {
            break;
        }
        anyhow::ensure!(
            bytes.len() + read as usize <= maximum,
            "workspace metadata exceeds {maximum} bytes"
        );
        bytes.extend_from_slice(&buffer[..read as usize]);
    }
    Ok(bytes)
}

fn read_descriptor_path(fd: i32) -> Result<PathBuf> {
    let link = PathBuf::from(format!("/proc/self/fd/{fd}"));
    let target = std::fs::read_link(&link)
        .with_context(|| format!("cannot resolve descriptor path {}", link.display()))?;
    let text = target.to_string_lossy();
    let path = text.strip_suffix(" (deleted)").unwrap_or(&text);
    Ok(PathBuf::from(path))
}

fn parse_gitdir_pointer(contents: &str) -> Result<PathBuf> {
    let mut lines = contents.lines();
    let pointer = lines
        .next()
        .and_then(|line| line.strip_prefix("gitdir:"))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .context("pinned workspace .git pointer has no gitdir path")?;
    anyhow::ensure!(
        lines.all(|line| line.trim().is_empty()),
        "pinned workspace .git pointer has unexpected extra content"
    );
    Ok(PathBuf::from(pointer))
}

fn canonical_pointer_target(pointer_file: &Path, value: &Path) -> Result<PathBuf> {
    let target = if value.is_absolute() {
        value.to_owned()
    } else {
        pointer_file
            .parent()
            .context("pinned workspace .git pointer has no parent")?
            .join(value)
    };
    std::fs::canonicalize(&target)
        .with_context(|| format!("cannot resolve Git directory {}", target.display()))
}

fn build_bwrap_args(
    policy: &LinuxSandboxPolicy,
    command: Vec<String>,
    seccomp_fd: i32,
    pinned_workspace_fd: Option<i32>,
    pinned_read_only_paths: &[PinnedReadOnlyPath],
) -> Result<Vec<String>> {
    policy.validate()?;
    if policy.pinned_workspace.is_some() {
        anyhow::ensure!(
            pinned_workspace_fd.is_some(),
            "pinned workspace policy requires a verified directory descriptor"
        );
    } else {
        anyhow::ensure!(
            pinned_workspace_fd.is_none(),
            "a pinned workspace descriptor requires a pinned workspace policy"
        );
    }
    let mut args = vec![
        "--new-session".to_owned(),
        "--die-with-parent".to_owned(),
        "--ro-bind".to_owned(),
        "/".to_owned(),
        "/".to_owned(),
        "--dev".to_owned(),
        "/dev".to_owned(),
        "--proc".to_owned(),
        "/proc".to_owned(),
        "--unshare-user".to_owned(),
        "--unshare-pid".to_owned(),
    ];
    if policy.network == LinuxNetworkPolicy::Restricted {
        args.push("--unshare-net".to_owned());
    }
    args.extend(["--seccomp".to_owned(), seccomp_fd.to_string()]);

    let mut hidden_roots = policy.hidden_roots.clone();
    hidden_roots.sort_by_key(|path| path_depth(path));
    hidden_roots.dedup();
    for root in &hidden_roots {
        args.push("--tmpfs".to_owned());
        args.push(path_to_string(root)?);
    }

    let mut writable_roots = policy.writable_roots.clone();
    writable_roots.extend(policy.temporary_roots.iter().cloned());
    writable_roots.sort_by_key(|path| path_depth(path));
    writable_roots.dedup();
    let mut visible_roots = writable_roots.clone();
    visible_roots.extend(policy.read_only_roots.iter().cloned());
    visible_roots.sort_by_key(|path| path_depth(path));
    visible_roots.dedup();
    let hidden_symlinks = policy
        .read_only_symlinks
        .iter()
        .filter(|symlink| {
            hidden_roots
                .iter()
                .any(|hidden| symlink.link.starts_with(hidden))
        })
        .collect::<Vec<_>>();
    let mut symlink_scaffold_paths = policy.read_only_scaffold_directories.clone();
    symlink_scaffold_paths.extend(
        hidden_symlinks
            .iter()
            .filter_map(|symlink| symlink.link.parent().map(Path::to_owned))
            .collect::<Vec<_>>(),
    );
    symlink_scaffold_paths.extend(
        policy
            .read_only_files
            .iter()
            .filter_map(|file| file.parent().map(Path::to_owned))
            .collect::<Vec<_>>(),
    );
    for symlink in &hidden_symlinks {
        let target = if std::fs::metadata(&symlink.target)
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false)
        {
            symlink.target.clone()
        } else {
            symlink
                .target
                .parent()
                .context("read-only symlink target has no parent")?
                .to_owned()
        };
        symlink_scaffold_paths.push(target);
    }
    append_namespace_directory_scaffolding(
        &mut args,
        &hidden_roots,
        &visible_roots,
        &symlink_scaffold_paths,
    )?;
    // Mounts are emitted shallowest-first: a later mount of a descendant wins
    // over an earlier ancestor mount, so writable subpaths inside a read-only
    // metadata root stay writable while the rest of the root stays read-only.
    let pinned_path = policy
        .pinned_workspace
        .as_ref()
        .map(|pinned| pinned.path.clone());
    let mut ordered_binds: Vec<(PathBuf, bool)> = writable_roots
        .into_iter()
        .map(|root| (root, true))
        .chain(
            policy
                .read_only_roots
                .iter()
                .map(|root| (root.clone(), false)),
        )
        .collect();
    ordered_binds.sort_by(|left, right| {
        path_depth(&left.0)
            .cmp(&path_depth(&right.0))
            .then_with(|| left.1.cmp(&right.1))
    });
    for (root, writable) in ordered_binds {
        if pinned_path.as_deref() == Some(root.as_path()) {
            continue;
        }
        let flag = if writable { "--bind" } else { "--ro-bind" };
        append_pair(&mut args, flag, &root, &root)?;
    }
    if let (Some(fd), Some(pinned)) = (pinned_workspace_fd, policy.pinned_workspace.as_ref()) {
        // The verified directory descriptor is the only source for the
        // workspace binding: a path swap after verification cannot redirect
        // the cwd or the writable scope to another directory entity.
        let flag = if pinned.writable {
            "--bind-fd"
        } else {
            "--ro-bind-fd"
        };
        args.push(flag.to_owned());
        args.push(fd.to_string());
        args.push(path_to_string(&pinned.path)?);
    }
    for file in &policy.read_only_files {
        append_pair(&mut args, "--ro-bind", file, file)?;
    }

    // Protected paths below a pinned workspace use descriptor-pinned sources,
    // never a re-resolved host pathname. Missing paths are masked only when the
    // effective parent mount is writable. A missing path below an actually
    // read-only mount needs no placeholder, preserving Git absence semantics
    // while still protecting missing top-level metadata and descendants
    // re-exposed by writable overlays.
    let mut masked_paths = Vec::new();
    let mut read_only_paths = policy.read_only_paths.clone();
    read_only_paths.sort_by_key(|path| path_depth(path));
    for path in read_only_paths {
        if masked_paths
            .iter()
            .any(|ancestor: &PathBuf| path.starts_with(ancestor))
        {
            continue;
        }

        if let Some(pinned) = pinned_read_only_paths
            .iter()
            .find(|pinned| pinned.target == path)
        {
            match &pinned.source {
                PinnedReadOnlySource::Existing(fd) => {
                    append_read_only_fd(&mut args, fd.as_raw_fd(), &path)?;
                    masked_paths.push(path);
                }
                PinnedReadOnlySource::Missing(mask) => {
                    if effective_mount_is_read_only(policy, mask) {
                        continue;
                    }
                    append_verified_missing_mask(&mut args, mask)?;
                    masked_paths.push(mask.clone());
                }
            }
            continue;
        }

        if path.exists() {
            append_read_only_mask(&mut args, &path)?;
            masked_paths.push(path);
            continue;
        }
        let mask = first_missing_component(&path).unwrap_or(path);
        if effective_mount_is_read_only(policy, &mask) {
            continue;
        }
        append_missing_mask(&mut args, &mask)?;
        masked_paths.push(mask);
    }

    // Recreate only verified intermediate launcher symlinks that were hidden
    // by the tmpfs overlay. Their parents and canonical targets were
    // scaffolded above, and policy validation prevents unrelated paths from
    // becoming visible.
    for symlink in hidden_symlinks {
        append_pair(&mut args, "--symlink", &symlink.target, &symlink.link)?;
    }

    args.push("--chdir".to_owned());
    args.push(path_to_string(&policy.cwd)?);
    args.push("--".to_owned());
    args.extend(command);
    Ok(args)
}

fn append_namespace_directory_scaffolding(
    args: &mut Vec<String>,
    hidden_roots: &[PathBuf],
    visible_roots: &[PathBuf],
    extra_paths: &[PathBuf],
) -> Result<()> {
    let mut directories = Vec::new();
    for visible in visible_roots.iter().chain(extra_paths.iter()) {
        let Some(hidden) = hidden_roots
            .iter()
            .filter(|hidden| visible.starts_with(hidden))
            .max_by_key(|hidden| path_depth(hidden))
        else {
            continue;
        };
        let relative = visible
            .strip_prefix(hidden)
            .context("visible sandbox root is not below its hidden root")?;
        let mut current = hidden.clone();
        for component in relative.components() {
            current.push(component.as_os_str());
            directories.push(current.clone());
        }
    }
    directories.sort_by_key(|path| path_depth(path));
    directories.dedup();
    for directory in directories {
        args.push("--dir".to_owned());
        args.push(path_to_string(&directory)?);
    }
    Ok(())
}

fn append_pair(args: &mut Vec<String>, flag: &str, source: &Path, target: &Path) -> Result<()> {
    args.push(flag.to_owned());
    args.push(path_to_string(source)?);
    args.push(path_to_string(target)?);
    Ok(())
}

fn append_read_only_fd(args: &mut Vec<String>, fd: i32, target: &Path) -> Result<()> {
    args.push("--ro-bind-fd".to_owned());
    args.push(fd.to_string());
    args.push(path_to_string(target)?);
    Ok(())
}

fn append_read_only_mask(args: &mut Vec<String>, path: &Path) -> Result<()> {
    anyhow::ensure!(
        path.exists(),
        "read-only mask target must exist: {}",
        path.display()
    );
    append_pair(args, "--ro-bind", path, path)
}

fn append_missing_mask(args: &mut Vec<String>, path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .context("missing read-only path has no parent")?;
    anyhow::ensure!(
        parent.is_dir(),
        "missing read-only path parent is not a directory: {}",
        parent.display()
    );
    append_verified_missing_mask(args, path)
}

fn append_verified_missing_mask(args: &mut Vec<String>, path: &Path) -> Result<()> {
    if missing_path_is_directory(path) {
        args.extend([
            "--tmpfs".to_owned(),
            path_to_string(path)?,
            "--remount-ro".to_owned(),
            path_to_string(path)?,
        ]);
    } else {
        append_pair(args, "--dev-bind", Path::new("/dev/null"), path)?;
    }
    Ok(())
}

fn first_missing_component(path: &Path) -> Option<PathBuf> {
    let mut current = PathBuf::from("/");
    for component in path.components() {
        let std::path::Component::Normal(name) = component else {
            continue;
        };
        current.push(name);
        if !current.exists() {
            return Some(current);
        }
    }
    None
}

fn missing_path_is_directory(path: &Path) -> bool {
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

fn effective_mount_is_read_only(policy: &LinuxSandboxPolicy, path: &Path) -> bool {
    let mut effective = None::<(usize, bool)>;
    for root in &policy.read_only_roots {
        if path.starts_with(root) {
            let depth = path_depth(root);
            if effective
                .is_none_or(|(current, writable)| depth > current || (depth == current && writable))
            {
                effective = Some((depth, false));
            }
        }
    }
    for root in policy
        .writable_roots
        .iter()
        .chain(policy.temporary_roots.iter())
    {
        if path.starts_with(root) {
            let depth = path_depth(root);
            if effective.is_none_or(|(current, _)| depth >= current) {
                effective = Some((depth, true));
            }
        }
    }
    matches!(effective, Some((_, false)))
}

fn path_to_string(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .with_context(|| format!("sandbox path is not valid UTF-8: {}", path.display()))
}

fn path_depth(path: &Path) -> usize {
    path.components().count()
}

const LOCAL_AGENT_BLOCKING_STREAM_SOCKETPAIR_TYPE: u64 =
    (libc::SOCK_STREAM | libc::SOCK_CLOEXEC) as u64;
const LOCAL_AGENT_NONBLOCKING_STREAM_SOCKETPAIR_TYPE: u64 =
    (libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK) as u64;

#[cfg(test)]
fn local_agent_stream_socketpair_type_allowed(socket_type: u64) -> bool {
    matches!(
        socket_type,
        LOCAL_AGENT_BLOCKING_STREAM_SOCKETPAIR_TYPE
            | LOCAL_AGENT_NONBLOCKING_STREAM_SOCKETPAIR_TYPE
    )
}

fn exec_absolute(program: &Path, args: &[String]) -> ! {
    let program = match CString::new(program.as_os_str().as_bytes()) {
        Ok(program) => program,
        Err(error) => fail(anyhow::anyhow!("invalid executable path: {error}")),
    };
    let c_args = match args
        .iter()
        .map(|argument| CString::new(argument.as_bytes()))
        .collect::<std::result::Result<Vec<_>, _>>()
    {
        Ok(args) => args,
        Err(error) => fail(anyhow::anyhow!("invalid sandbox argument: {error}")),
    };
    let mut pointers = c_args
        .iter()
        .map(|argument| argument.as_ptr())
        .collect::<Vec<_>>();
    pointers.push(std::ptr::null());

    // SAFETY: all pointers refer to live NUL-terminated strings and the final
    // null pointer terminates argv. On success execv does not return.
    unsafe { libc::execv(program.as_ptr(), pointers.as_ptr()) };
    let error = std::io::Error::last_os_error();
    fail(anyhow::anyhow!("failed to exec bubblewrap: {error}"));
}

fn build_seccomp_filter(network: LinuxNetworkPolicy) -> Result<BpfProgram> {
    fn deny_syscall(rules: &mut BTreeMap<i64, Vec<SeccompRule>>, syscall: i64) {
        rules.insert(syscall, Vec::new());
    }

    let mut rules = BTreeMap::new();
    deny_syscall(&mut rules, libc::SYS_ptrace);
    deny_syscall(&mut rules, libc::SYS_process_vm_readv);
    deny_syscall(&mut rules, libc::SYS_process_vm_writev);
    deny_syscall(&mut rules, libc::SYS_io_uring_setup);
    deny_syscall(&mut rules, libc::SYS_io_uring_enter);
    deny_syscall(&mut rules, libc::SYS_io_uring_register);

    if network == LinuxNetworkPolicy::Restricted {
        for syscall in [
            libc::SYS_connect,
            libc::SYS_accept,
            libc::SYS_accept4,
            libc::SYS_bind,
            libc::SYS_listen,
            libc::SYS_getpeername,
            libc::SYS_getsockname,
            libc::SYS_shutdown,
            libc::SYS_sendto,
            libc::SYS_sendmmsg,
            libc::SYS_recvmmsg,
            libc::SYS_getsockopt,
            libc::SYS_setsockopt,
        ] {
            deny_syscall(&mut rules, syscall);
        }

        // The restricted command profile can still use Unix-domain sockets
        // for local subprocess management; all other socket families are
        // rejected before a command can create one.
        let unix_only_rule = SeccompRule::new(vec![SeccompCondition::new(
            0,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Ne,
            libc::AF_UNIX as u64,
        )?])?;
        rules.insert(libc::SYS_socket, vec![unix_only_rule.clone()]);
        rules.insert(libc::SYS_socketpair, vec![unix_only_rule]);
    } else {
        // The network-enabled development profile has no inherited IPC file
        // descriptors and must not create path-based Unix sockets, which could
        // reach host control sockets. Runtimes use connected unnamed AF_UNIX
        // socketpairs for signal handling and child-process stdio. libuv may
        // request either a blocking CLOEXEC stream pair and set O_NONBLOCK
        // later, or request NONBLOCK atomically. Keep socket(AF_UNIX) denied and
        // allow only those two connected stream-pair forms with protocol 0.
        let unix_rule = SeccompRule::new(vec![SeccompCondition::new(
            0,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Eq,
            libc::AF_UNIX as u64,
        )?])?;
        rules.insert(libc::SYS_socket, vec![unix_rule.clone()]);
        let invalid_domain_rule = SeccompRule::new(vec![SeccompCondition::new(
            0,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Ne,
            libc::AF_UNIX as u64,
        )?])?;
        let invalid_type_rule = SeccompRule::new(vec![
            SeccompCondition::new(
                1,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::Ne,
                LOCAL_AGENT_BLOCKING_STREAM_SOCKETPAIR_TYPE,
            )?,
            SeccompCondition::new(
                1,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::Ne,
                LOCAL_AGENT_NONBLOCKING_STREAM_SOCKETPAIR_TYPE,
            )?,
        ])?;
        let invalid_protocol_rule = SeccompRule::new(vec![SeccompCondition::new(
            2,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Ne,
            0,
        )?])?;
        rules.insert(
            libc::SYS_socketpair,
            vec![
                invalid_domain_rule,
                invalid_type_rule,
                invalid_protocol_rule,
            ],
        );
    }

    let architecture = if cfg!(target_arch = "x86_64") {
        TargetArch::x86_64
    } else if cfg!(target_arch = "aarch64") {
        TargetArch::aarch64
    } else {
        anyhow::bail!("unsupported Linux seccomp architecture")
    };
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        architecture,
    )?;
    filter.try_into().map_err(Into::into)
}

fn create_sealed_seccomp_memfd(program: &BpfProgram) -> Result<OwnedFd> {
    anyhow::ensure!(!program.is_empty(), "seccomp program must not be empty");
    let name = CString::new("temote-seccomp")?;
    // Do not use MFD_CLOEXEC: bubblewrap must inherit this fd and consumes it
    // through --seccomp FD. MFD_ALLOW_SEALING lets us make the bytecode
    // immutable before exec.
    let raw_fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_ALLOW_SEALING) };
    anyhow::ensure!(
        raw_fd >= 0,
        "memfd_create failed: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: raw_fd was freshly returned by memfd_create and ownership is
    // transferred exactly once into OwnedFd.
    let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };

    let byte_len = program
        .len()
        .checked_mul(std::mem::size_of_val(&program[0]))
        .context("seccomp program byte length overflow")?;
    // seccompiler's sock_filter is #[repr(C)] with the kernel's 8-byte layout.
    // SAFETY: program is initialized contiguous memory and byte_len exactly
    // spans its elements; the resulting slice is only used for writing.
    let bytes = unsafe { std::slice::from_raw_parts(program.as_ptr().cast::<u8>(), byte_len) };
    let mut file = std::fs::File::from(fd);
    file.write_all(bytes)
        .context("failed to write seccomp bytecode")?;
    file.flush().context("failed to flush seccomp bytecode")?;
    let raw_fd = file.as_raw_fd();
    anyhow::ensure!(
        unsafe { libc::lseek(raw_fd, 0, libc::SEEK_SET) } == 0,
        "failed to rewind seccomp memfd: {}",
        std::io::Error::last_os_error()
    );
    let seals = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
    anyhow::ensure!(
        unsafe { libc::fcntl(raw_fd, libc::F_ADD_SEALS, seals) } == 0,
        "failed to seal seccomp memfd: {}",
        std::io::Error::last_os_error()
    );
    Ok(file.into())
}

fn fail(error: impl Display) -> ! {
    eprintln!("temote-linux-sandbox: {error}");
    std::process::exit(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_payload_reports_running_policy_schema() {
        let payload: serde_json::Value = serde_json::from_str(&capabilities_payload()).unwrap();
        assert_eq!(payload["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(
            payload["policy_schema"],
            serde_json::json!(LINUX_SANDBOX_POLICY_VERSION)
        );
    }

    fn run_git_fixture(cwd: &Path, args: &[&str]) {
        let status = std::process::Command::new("/usr/bin/git")
            .args(args)
            .current_dir(cwd)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed in {}", cwd.display());
    }

    fn linked_worktree_identity(path: &Path) -> crate::sandbox::WorkspaceRepositoryIdentity {
        crate::sandbox::WorkspaceRepositoryIdentity::for_workspace(path).unwrap()
    }

    fn pinned_workspace_policy(
        target: &Path,
        temporary: &Path,
        identity: &crate::sandbox::WorkspaceRepositoryIdentity,
    ) -> LinuxSandboxPolicy {
        LinuxSandboxPolicy {
            version: LINUX_SANDBOX_POLICY_VERSION,
            cwd: target.to_path_buf(),
            writable_roots: vec![target.to_path_buf(), temporary.to_path_buf()],
            temporary_roots: vec![temporary.to_path_buf()],
            read_only_paths: Vec::new(),
            read_only_roots: Vec::new(),
            read_only_symlinks: Vec::new(),
            read_only_scaffold_directories: Vec::new(),
            read_only_files: Vec::new(),
            hidden_roots: Vec::new(),
            pinned_workspace: Some(LinuxPinnedWorkspace {
                path: target.to_path_buf(),
                writable: true,
                worktree_root: identity.worktree_root.clone(),
                metadata_roots: identity.metadata_roots.clone(),
                common_dir: identity.common_dir.clone(),
                primary_checkout: identity.primary_checkout.clone(),
            }),
            network: LinuxNetworkPolicy::Restricted,
        }
    }

    #[test]
    fn missing_protected_paths_are_masked_only_when_the_effective_mount_is_writable() {
        let fixture = tempfile::tempdir().unwrap();
        let workspace = fixture.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let workspace = std::fs::canonicalize(workspace).unwrap();

        let policy = LinuxSandboxPolicy::for_command(&workspace, &[], &[]).unwrap();
        let args = build_bwrap_args(&policy, vec!["/bin/true".to_owned()], 42, None, &[]).unwrap();
        for name in [".git", ".agents", ".codex"] {
            let target = workspace.join(name).display().to_string();
            assert!(
                args.contains(&target),
                "missing top-level protected path was omitted: {name}"
            );
        }

        let git = workspace.join(".git");
        let objects = git.join("objects");
        std::fs::create_dir_all(&objects).unwrap();
        let git = std::fs::canonicalize(git).unwrap();
        let policy =
            LinuxSandboxPolicy::for_command(&workspace, &[], std::slice::from_ref(&git)).unwrap();
        let args = build_bwrap_args(&policy, vec!["/bin/true".to_owned()], 42, None, &[]).unwrap();
        for name in ["info", "pack"] {
            let target = objects.join(name).display().to_string();
            assert!(
                args.contains(&target),
                "missing protected object child was omitted below writable objects: {name}"
            );
        }
        assert!(
            !effective_mount_is_read_only(&policy, &objects.join("info")),
            "writable objects overlay must remain the effective mount"
        );
        assert!(
            effective_mount_is_read_only(&policy, &git.join("shallow")),
            "missing entries outside writable overlays must remain protected by the read-only Git root"
        );
    }

    #[test]
    fn pinned_workspace_protected_overlays_keep_the_pinned_source_after_path_swap() {
        let fixture = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(fixture.path()).unwrap();
        let repository_a = base.join("repo-a");
        let repository_b = base.join("repo-b");
        for repository in [&repository_a, &repository_b] {
            std::fs::create_dir(repository).unwrap();
            run_git_fixture(repository, &["init", "--quiet"]);
            std::fs::write(repository.join("tracked.txt"), "base\n").unwrap();
            run_git_fixture(repository, &["add", "tracked.txt"]);
            run_git_fixture(
                repository,
                &[
                    "-c",
                    "user.name=temote-mcp test",
                    "-c",
                    "user.email=temote-mcp@example.invalid",
                    "commit",
                    "--quiet",
                    "-m",
                    "base",
                ],
            );
        }

        let target = base.join("target");
        run_git_fixture(
            &repository_a,
            &[
                "worktree",
                "add",
                "--quiet",
                target.to_str().unwrap(),
                "-b",
                "feature",
            ],
        );
        std::fs::create_dir(target.join(".agents")).unwrap();
        let identity = linked_worktree_identity(&target);
        let temporary = base.join("tmp");
        std::fs::create_dir(&temporary).unwrap();
        let mut policy = pinned_workspace_policy(&target, &temporary, &identity);
        policy.read_only_paths = vec![
            target.join(".git"),
            target.join(".agents"),
            target.join(".codex"),
        ];
        policy.read_only_paths.sort();
        policy.validate().unwrap();

        let pinned_fd = pin_policy_workspace(policy.pinned_workspace.as_ref().unwrap()).unwrap();
        let pinned_paths = pin_workspace_read_only_paths(&policy, &pinned_fd).unwrap();
        assert!(matches!(
            pinned_paths
                .iter()
                .find(|entry| entry.target == target.join(".codex"))
                .map(|entry| &entry.source),
            Some(PinnedReadOnlySource::Missing(_))
        ));

        std::fs::rename(&target, base.join("moved-a")).unwrap();
        run_git_fixture(
            &repository_b,
            &[
                "worktree",
                "add",
                "--quiet",
                target.to_str().unwrap(),
                "-b",
                "other",
            ],
        );
        std::fs::create_dir(target.join(".agents")).unwrap();
        std::fs::create_dir(target.join(".codex")).unwrap();

        let args = build_bwrap_args(
            &policy,
            vec!["/bin/true".to_owned()],
            42,
            Some(pinned_fd.as_raw_fd()),
            &pinned_paths,
        )
        .unwrap();

        for name in [".git", ".agents"] {
            let target_path = target.join(name);
            let pinned = pinned_paths
                .iter()
                .find(|entry| entry.target == target_path)
                .unwrap();
            let PinnedReadOnlySource::Existing(fd) = &pinned.source else {
                panic!("expected existing pinned source for {name}");
            };
            let target_text = target_path.display().to_string();
            assert!(
                args.windows(3).any(|window| {
                    window[0] == "--ro-bind-fd"
                        && window[1] == fd.as_raw_fd().to_string()
                        && window[2] == target_text
                }),
                "protected overlay did not use its pinned descriptor: {name}"
            );
        }
        let missing_target = target.join(".codex").display().to_string();
        assert!(
            args.windows(2)
                .any(|window| window[0] == "--tmpfs" && window[1] == missing_target),
            "metadata absent from the pinned workspace must stay a protected missing entry"
        );
        assert!(
            !args.windows(3).any(|window| {
                window[0] == "--ro-bind"
                    && window[1] == missing_target
                    && window[2] == missing_target
            }),
            "swapped workspace metadata must not become a protected overlay source"
        );
        assert!(
            pin_policy_workspace(policy.pinned_workspace.as_ref().unwrap()).is_err(),
            "pinning the substituted worktree must fail closed"
        );
    }

    /// R1: the verified directory descriptor is the bound workspace even after
    /// the host path is swapped to another valid repository worktree, and
    /// pinning that path after the swap fails closed.
    #[test]
    #[ignore = "host acceptance: requires production-shaped bubblewrap/userns support"]
    fn pinned_workspace_descriptor_survives_a_path_swap_host_acceptance() {
        let fixture = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(fixture.path()).unwrap();
        let repository_a = base.join("repo-a");
        let repository_b = base.join("repo-b");
        for repository in [&repository_a, &repository_b] {
            std::fs::create_dir(repository).unwrap();
            run_git_fixture(repository, &["init", "--quiet"]);
            std::fs::write(repository.join("tracked.txt"), "base\n").unwrap();
            run_git_fixture(repository, &["add", "tracked.txt"]);
            run_git_fixture(
                repository,
                &[
                    "-c",
                    "user.name=temote-mcp test",
                    "-c",
                    "user.email=temote-mcp@example.invalid",
                    "commit",
                    "--quiet",
                    "-m",
                    "base",
                ],
            );
        }
        let target = base.join("target");
        run_git_fixture(
            &repository_a,
            &[
                "worktree",
                "add",
                "--quiet",
                target.to_str().unwrap(),
                "-b",
                "feature",
            ],
        );
        std::fs::write(target.join("marker.txt"), "pinned-a").unwrap();
        let identity = linked_worktree_identity(&target);
        let temporary = base.join("tmp");
        std::fs::create_dir(&temporary).unwrap();
        let policy = pinned_workspace_policy(&target, &temporary, &identity);
        policy.validate().unwrap();

        let pinned_fd = pin_policy_workspace(policy.pinned_workspace.as_ref().unwrap())
            .expect("pin the validated workspace");
        let seccomp = build_seccomp_filter(policy.network).unwrap();
        let seccomp_fd = create_sealed_seccomp_memfd(&seccomp).unwrap();
        let args = build_bwrap_args(
            &policy,
            vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "cat marker.txt".to_owned(),
            ],
            seccomp_fd.as_raw_fd(),
            Some(pinned_fd.as_raw_fd()),
            &[],
        )
        .unwrap();

        // Swap the target path to another repository's valid worktree *after*
        // the verified descriptor was pinned.
        std::fs::rename(&target, base.join("moved-a")).unwrap();
        run_git_fixture(
            &repository_b,
            &[
                "worktree",
                "add",
                "--quiet",
                target.to_str().unwrap(),
                "-b",
                "other",
            ],
        );
        std::fs::write(target.join("marker.txt"), "swapped-b").unwrap();

        let bwrap = crate::sandbox::svc_acct_bwrap().unwrap();
        let output = std::process::Command::new(bwrap)
            .args(&args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "pinned sandbox failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout), "pinned-a");

        // Pinning the swapped path is refused: the entity no longer presents
        // the validated repository identity.
        assert!(
            pin_policy_workspace(policy.pinned_workspace.as_ref().unwrap()).is_err(),
            "pinning another repository's worktree must fail closed"
        );
    }

    #[test]
    fn generated_local_agent_socketpair_type_allowlist_matches_reference() -> noprop::TestResult {
        crate::test_support::run(0x534f_434b_5041_4952, 1024, |ctx| {
            let socket_type = noprop::sample_u32(ctx) as u64;
            let expected = socket_type == LOCAL_AGENT_BLOCKING_STREAM_SOCKETPAIR_TYPE
                || socket_type == LOCAL_AGENT_NONBLOCKING_STREAM_SOCKETPAIR_TYPE;
            assert_eq!(
                local_agent_stream_socketpair_type_allowed(socket_type),
                expected,
                "unexpected allowlist decision for socket type {socket_type:#x}"
            );
            Ok(())
        })
    }

    #[test]
    fn local_agent_socketpair_type_allowlist_is_exact() {
        assert!(local_agent_stream_socketpair_type_allowed(
            LOCAL_AGENT_BLOCKING_STREAM_SOCKETPAIR_TYPE
        ));
        assert!(local_agent_stream_socketpair_type_allowed(
            LOCAL_AGENT_NONBLOCKING_STREAM_SOCKETPAIR_TYPE
        ));
        assert!(!local_agent_stream_socketpair_type_allowed(
            libc::SOCK_STREAM as u64
        ));
        assert!(!local_agent_stream_socketpair_type_allowed(
            (libc::SOCK_DGRAM | libc::SOCK_CLOEXEC) as u64
        ));
        assert!(!local_agent_stream_socketpair_type_allowed(
            (libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK | libc::SOCK_DGRAM)
                as u64
        ));
    }

    #[test]
    fn local_agent_seccomp_filter_compiles_with_runtime_socketpair_allowlist() {
        let program = build_seccomp_filter(LinuxNetworkPolicy::LocalAgent).unwrap();
        assert!(!program.is_empty());
    }

    #[test]
    fn malformed_policy_and_helper_misuse_fail_closed() {
        assert!(parse_policy("not-json").is_err());
        assert!(validate_command(&[]).is_err());
        assert!(validate_command(&["contains\0nul".to_owned()]).is_err());
        assert!(
            parse_helper_args(
                [
                    "temote-linux-sandbox",
                    "--apply-seccomp",
                    "--policy",
                    "{}",
                    "--",
                    "/bin/true",
                ]
                .into_iter()
                .map(str::to_owned)
            )
            .is_err()
        );
    }

    #[test]
    fn helper_terminator_keeps_child_options_out_of_noargs() {
        let root = tempfile::tempdir().unwrap();
        let policy = LinuxSandboxPolicy::for_command(root.path(), &[], &[]).unwrap();
        let policy = serde_json::to_string(&policy).unwrap();
        let parsed = parse_helper_args([
            "temote-linux-sandbox".to_owned(),
            "--policy".to_owned(),
            policy,
            "--".to_owned(),
            "/bin/echo".to_owned(),
            "--policy".to_owned(),
            "child-value".to_owned(),
        ])
        .unwrap();
        assert_eq!(parsed.command, ["/bin/echo", "--policy", "child-value"]);
    }

    #[test]
    fn bwrap_policy_has_read_only_root_writable_temp_and_isolated_network() {
        let root = tempfile::tempdir().unwrap();
        let policy = LinuxSandboxPolicy::for_command(root.path(), &[], &[]).unwrap();
        let args = build_bwrap_args(&policy, vec!["/bin/true".to_owned()], 42, None, &[]).unwrap();

        assert!(
            args.windows(3)
                .any(|window| window == ["--ro-bind", "/", "/"])
        );
        assert!(args.windows(3).any(|window| {
            window
                == [
                    "--bind",
                    root.path().to_str().unwrap(),
                    root.path().to_str().unwrap(),
                ]
        }));
        assert!(args.iter().any(|arg| arg == "--unshare-net"));
        assert!(args.windows(2).any(|window| window == ["--seccomp", "42"]));
        assert!(
            args.windows(2)
                .any(|window| window == ["--chdir", root.path().to_str().unwrap()])
        );
    }

    #[test]
    fn ordinary_command_network_policy_controls_network_namespace_and_seccomp() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();

        let restricted = LinuxSandboxPolicy::for_command_with_network(
            &workspace,
            &[],
            &[],
            LinuxNetworkPolicy::Restricted,
        )
        .unwrap();
        let development = LinuxSandboxPolicy::for_command_with_network(
            &workspace,
            &[],
            &[],
            LinuxNetworkPolicy::LocalAgent,
        )
        .unwrap();
        assert_eq!(restricted.network, LinuxNetworkPolicy::Restricted);
        assert_eq!(development.network, LinuxNetworkPolicy::LocalAgent);

        let restricted_args =
            build_bwrap_args(&restricted, vec!["/bin/true".to_owned()], 42, None, &[]).unwrap();
        let development_args =
            build_bwrap_args(&development, vec!["/bin/true".to_owned()], 42, None, &[]).unwrap();
        assert!(restricted_args.iter().any(|arg| arg == "--unshare-net"));
        assert!(!development_args.iter().any(|arg| arg == "--unshare-net"));
        assert!(
            !build_seccomp_filter(LinuxNetworkPolicy::Restricted)
                .unwrap()
                .is_empty()
        );
        assert!(
            !build_seccomp_filter(LinuxNetworkPolicy::LocalAgent)
                .unwrap()
                .is_empty()
        );
    }
}
