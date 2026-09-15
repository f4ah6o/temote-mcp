use noprop::TestCaseContext;
use std::path::PathBuf;
use std::sync::OnceLock;

pub const DEFAULT_CASES: usize = 1024;
pub const PRIVATE_PROCESS_ROOT_ENV: &str = "TEMOTE_TEST_PRIVATE_PROCESS_ROOT";
const DEFAULT_SEED: u64 = 0x5445_4D4F_5445_0001;

/// Return a process-private root for unit tests that exercise production path defaults.
///
/// Cargo runs the library and binary unit suites in separate processes, so the PID and
/// random suffix isolate concurrent test executables without mutating process-wide
/// environment variables. The short `/tmp` name also leaves enough room for the Unix
/// socket path limit when a maximum-length session ID is appended. Intentional re-exec
/// fixtures may pass `PRIVATE_PROCESS_ROOT_ENV` to their child with `Command::env`.
pub fn private_process_root() -> Result<PathBuf, String> {
    static ROOT: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    ROOT.get_or_init(create_private_process_root).clone()
}

fn create_private_process_root() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os(PRIVATE_PROCESS_ROOT_ENV) {
        return validate_private_process_root(PathBuf::from(path));
    }
    for _ in 0..16 {
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let path = PathBuf::from(format!("/tmp/tm{:x}-{}", std::process::id(), &nonce[..6]));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&path) {
            Ok(()) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mode = std::fs::metadata(&path)
                        .map_err(|error| {
                            format!(
                                "failed to inspect private test root {}: {error}",
                                path.display()
                            )
                        })?
                        .permissions()
                        .mode()
                        & 0o777;
                    if mode != 0o700 {
                        return Err(format!(
                            "private test root {} has mode {mode:04o}, expected 0700",
                            path.display()
                        ));
                    }
                }
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "failed to create private test root {}: {error}",
                    path.display()
                ));
            }
        }
    }
    Err("failed to allocate a unique private test root".to_owned())
}

fn validate_private_process_root(path: PathBuf) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!(
            "shared private test root must be absolute: {}",
            path.display()
        ));
    }
    let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
        format!(
            "failed to inspect shared private test root {}: {error}",
            path.display()
        )
    })?;
    if !metadata.file_type().is_dir() {
        return Err(format!(
            "shared private test root is not a directory: {}",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(format!(
                "shared private test root is not owned by the current user: {}",
                path.display()
            ));
        }
        let mode = metadata.permissions().mode() & 0o777;
        if mode != 0o700 {
            return Err(format!(
                "shared private test root {} has mode {mode:04o}, expected 0700",
                path.display()
            ));
        }
    }
    std::fs::canonicalize(&path).map_err(|error| {
        format!(
            "failed to resolve shared private test root {}: {error}",
            path.display()
        )
    })
}

#[test]
fn test_isolation_process_root_is_stable_private_and_short() {
    let root = private_process_root().unwrap();
    assert_eq!(private_process_root().unwrap(), root);
    assert_eq!(root.parent(), Some(std::path::Path::new("/tmp")));
    assert!(
        root.as_os_str().len() <= 32,
        "test root is too long: {root:?}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(root).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}

#[test]
fn shared_process_root_validation_rejects_relative_public_and_symlink_paths() {
    assert!(validate_private_process_root(PathBuf::from("relative")).is_err());

    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let public = tempfile::tempdir().unwrap();
        std::fs::set_permissions(public.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(validate_private_process_root(public.path().to_owned()).is_err());

        let private = tempfile::tempdir().unwrap();
        std::fs::set_permissions(private.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            validate_private_process_root(private.path().to_owned()).unwrap(),
            std::fs::canonicalize(private.path()).unwrap()
        );
        let link = public.path().join("private-link");
        symlink(private.path(), &link).unwrap();
        assert!(validate_private_process_root(link).is_err());
    }
}

pub fn seed(salt: u64) -> u64 {
    let base = match std::env::var("TEMOTE_PBT_SEED") {
        Ok(raw) => {
            if let Some(hex) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
                u64::from_str_radix(hex, 16).expect("TEMOTE_PBT_SEED must be a u64")
            } else {
                raw.parse().expect("TEMOTE_PBT_SEED must be a u64")
            }
        }
        Err(_) => DEFAULT_SEED,
    };
    base ^ salt
}

pub fn run(
    salt: u64,
    cases: usize,
    property: impl Fn(&mut TestCaseContext) -> noprop::TestResult,
) -> noprop::TestResult {
    noprop::Runner::new(seed(salt)).run(cases, property)?;
    Ok(())
}

#[allow(dead_code)]
pub fn ascii_string(ctx: &mut TestCaseContext, max_len: usize) -> String {
    let len = noprop::sample_usize_in(ctx, 0..=max_len);
    (0..len)
        .map(|_| char::from(noprop::sample_u8(ctx) & 0x7f))
        .collect()
}

pub fn safe_component(ctx: &mut TestCaseContext) -> String {
    let len = noprop::sample_usize_in(ctx, 1..=12);
    (0..len)
        .map(|_| match noprop::sample_u8(ctx) % 36 {
            value @ 0..=9 => char::from(b'0' + value),
            value => char::from(b'a' + (value - 10)),
        })
        .collect()
}
