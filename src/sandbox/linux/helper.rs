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
    LinuxNetworkPolicy, LinuxSandboxPolicy, is_linked_worktree_metadata_root,
    missing_path_is_directory,
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

pub(super) fn run_main() -> ! {
    let args = match parse_helper_args(std::env::args()) {
        Ok(args) => args,
        Err(error) => fail(error.context("invalid Linux sandbox helper arguments")),
    };

    let bwrap = match crate::sandbox::trusted_service_account_bwrap() {
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
    let bwrap_args = match build_bwrap_args(&args.policy, args.command, seccomp_fd.as_raw_fd()) {
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

fn build_bwrap_args(
    policy: &LinuxSandboxPolicy,
    command: Vec<String>,
    seccomp_fd: i32,
) -> Result<Vec<String>> {
    policy.validate()?;
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
    for root in writable_roots {
        append_pair(&mut args, "--bind", &root, &root)?;
    }
    for root in &policy.read_only_roots {
        append_pair(&mut args, "--ro-bind", root, root)?;
    }
    for file in &policy.read_only_files {
        append_pair(&mut args, "--ro-bind", file, file)?;
    }

    let mut masked_paths = Vec::new();
    let mut read_only_paths = policy.read_only_paths.clone();
    read_only_paths.sort_by_key(|path| path_depth(path));
    for path in read_only_paths {
        let mask = first_missing_component(&path).unwrap_or(path);
        if masked_paths
            .iter()
            .any(|ancestor: &PathBuf| mask.starts_with(ancestor))
        {
            continue;
        }
        append_read_only_mask(&mut args, &mask)?;
        masked_paths.push(mask);
    }

    // The common Git metadata root protects its entire `worktrees` directory.
    // A validated linked worktree has one private metadata directory nested
    // below that read-only mount. Re-overlay only that validated private root
    // as writable, then restore every narrower read-only mask below it. This
    // keeps sibling worktree metadata protected while allowing Git to create
    // index.lock and other per-worktree state.
    let mut linked_worktree_roots = policy
        .writable_roots
        .iter()
        .filter(|root| is_linked_worktree_metadata_root(root))
        .cloned()
        .collect::<Vec<_>>();
    linked_worktree_roots.sort_by_key(|path| path_depth(path));
    for root in linked_worktree_roots {
        append_pair(&mut args, "--bind", &root, &root)?;
        let mut remasked = Vec::<PathBuf>::new();
        let mut descendants = policy
            .read_only_paths
            .iter()
            .filter(|path| path.starts_with(&root) && *path != &root)
            .cloned()
            .collect::<Vec<_>>();
        descendants.sort_by_key(|path| path_depth(path));
        for path in descendants {
            let mask = first_missing_component(&path).unwrap_or(path);
            if remasked.iter().any(|ancestor| mask.starts_with(ancestor)) {
                continue;
            }
            append_read_only_mask(&mut args, &mask)?;
            remasked.push(mask);
        }
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

fn append_read_only_mask(args: &mut Vec<String>, path: &Path) -> Result<()> {
    if path.exists() {
        append_pair(args, "--ro-bind", path, path)
    } else {
        append_missing_mask(args, path)
    }
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
    if missing_path_is_directory(path) {
        args.extend([
            "--tmpfs".to_owned(),
            path_to_string(path)?,
            "--remount-ro".to_owned(),
            path_to_string(path)?,
        ]);
    } else {
        // A bind of the host /dev/null prevents creation of a missing metadata
        // file without exposing a writable mountpoint. `--dev-bind` is
        // required: the read-only bind mounts of bubblewrap are `nodev`, so
        // reading a plain `--ro-bind /dev/null` mask fails with EACCES instead
        // of yielding the empty content Git expects from optional files such
        // as `packed-refs` or `shallow`.
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
        // The network-enabled local-agent profile has no inherited IPC file
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
        let args = build_bwrap_args(&policy, vec!["/bin/true".to_owned()], 42).unwrap();

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
    fn local_agent_policy_keeps_network_and_uses_only_explicit_temp_roots() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let temp = root.path().join("tmp");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&temp).unwrap();
        let hidden = root.path().to_path_buf();
        let policy = LinuxSandboxPolicy::for_local_agent(
            &workspace,
            &[],
            std::slice::from_ref(&temp),
            &[],
            std::slice::from_ref(&workspace),
            &[],
            &[],
            &[],
            std::slice::from_ref(&hidden),
        )
        .unwrap();
        let args = build_bwrap_args(&policy, vec!["/bin/true".to_owned()], 42).unwrap();

        assert!(!args.iter().any(|arg| arg == "--unshare-net"));
        assert!(
            args.windows(2)
                .any(|window| window == ["--tmpfs", root.path().to_str().unwrap()])
        );
        assert!(
            args.windows(2)
                .any(|window| window == ["--dir", workspace.to_str().unwrap()])
        );
        assert!(args.windows(3).any(|window| {
            window
                == [
                    "--ro-bind",
                    workspace.to_str().unwrap(),
                    workspace.to_str().unwrap(),
                ]
        }));
        assert!(args.windows(3).any(|window| {
            window == ["--bind", temp.to_str().unwrap(), temp.to_str().unwrap()]
        }));
        assert!(
            !args
                .windows(3)
                .any(|window| window == ["--bind", "/tmp", "/tmp"])
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
            build_bwrap_args(&restricted, vec!["/bin/true".to_owned()], 42).unwrap();
        let development_args =
            build_bwrap_args(&development, vec!["/bin/true".to_owned()], 42).unwrap();
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

    #[test]
    #[cfg(unix)]
    fn local_agent_policy_recreates_only_verified_symlinks() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let hidden = root.path().join("home");
        let bin = hidden.join("bin");
        let version = hidden.join("0.3.1");
        let scratch = hidden.join("scratch");
        let current = hidden.join("current");
        let metadata_file = hidden.join("bins/codex.json");
        let unrelated_file = hidden.join("bins/unrelated.json");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(version.join("bin")).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir(&scratch).unwrap();
        std::fs::create_dir_all(metadata_file.parent().unwrap()).unwrap();
        std::fs::write(&metadata_file, b"verified").unwrap();
        std::fs::write(&unrelated_file, b"must not be mounted").unwrap();
        std::fs::write(scratch.join("secret"), b"must not be mounted").unwrap();
        std::fs::write(version.join("bin/vp"), b"#!/bin/sh\n").unwrap();
        symlink(Path::new("../current/bin/vp"), bin.join("codex")).unwrap();
        symlink(Path::new("0.3.1"), &current).unwrap();
        let canonical_version = std::fs::canonicalize(&version).unwrap();
        let hidden = std::fs::canonicalize(&hidden).unwrap();

        let symlinks = [crate::sandbox::LocalAgentSymlink {
            link: hidden.join("current"),
            target: canonical_version.clone(),
        }];
        let hidden = std::fs::canonicalize(&hidden).unwrap();
        let policy = LinuxSandboxPolicy::for_local_agent(
            &workspace,
            &[],
            &[],
            &[],
            std::slice::from_ref(&bin),
            &symlinks,
            std::slice::from_ref(&scratch),
            std::slice::from_ref(&metadata_file),
            std::slice::from_ref(&hidden),
        )
        .unwrap();
        let args = build_bwrap_args(&policy, vec!["/bin/true".to_owned()], 42).unwrap();

        assert!(args.windows(3).any(|window| {
            window
                == [
                    "--symlink",
                    canonical_version.to_str().unwrap(),
                    hidden.join("current").to_str().unwrap(),
                ]
        }));
        assert!(
            args.windows(2)
                .any(|window| { window == ["--dir", hidden.join("0.3.1").to_str().unwrap()] })
        );
        assert!(
            args.windows(2)
                .any(|window| { window == ["--dir", scratch.to_str().unwrap()] })
        );
        assert!(!args.windows(3).any(|window| {
            window
                == [
                    "--ro-bind",
                    scratch.to_str().unwrap(),
                    scratch.to_str().unwrap(),
                ]
        }));
        assert!(!args.iter().any(|argument| argument == "secret"));
        assert!(args.windows(3).any(|window| {
            window
                == [
                    "--ro-bind",
                    metadata_file.to_str().unwrap(),
                    metadata_file.to_str().unwrap(),
                ]
        }));
        assert!(
            !args
                .iter()
                .any(|argument| argument == unrelated_file.to_str().unwrap())
        );
        assert!(
            !args
                .iter()
                .any(|argument| Path::new(argument) == hidden.join("bin/codex"))
        );
    }
}
