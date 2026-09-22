use noprop::TestCaseContext;
use std::path::PathBuf;
use std::sync::OnceLock;

pub const DEFAULT_CASES: usize = 1024;
const DEFAULT_SEED: u64 = 0x5445_4D4F_5445_0001;

/// Return a process-private root for unit tests that exercise production path defaults.
///
/// Cargo runs the library and binary unit suites in separate processes, so the PID and
/// random suffix isolate concurrent test executables without mutating process-wide
/// environment variables. The short `/tmp` name also leaves enough room for the Unix
/// socket path limit when a maximum-length session ID is appended.
pub fn private_process_root() -> Result<PathBuf, String> {
    static ROOT: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    ROOT.get_or_init(create_private_process_root).clone()
}

fn create_private_process_root() -> Result<PathBuf, String> {
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
                // macOS spells /tmp as /private/tmp; keep this test fixture
                // canonical so production path authority remains unchanged.
                return std::fs::canonicalize(&path).map_err(|error| {
                    format!("failed to canonicalize private process root {path:?}: {error}")
                });
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

#[test]
fn test_isolation_process_root_is_stable_private_and_short() {
    let root = private_process_root().unwrap();
    assert_eq!(private_process_root().unwrap(), root);
    let canonical_tmp = std::fs::canonicalize("/tmp").unwrap();
    assert_eq!(root.parent(), Some(canonical_tmp.as_path()));
    assert_eq!(root, std::fs::canonicalize(&root).unwrap());
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
fn test_isolation_process_root_nested_state_paths_are_canonical_and_private() {
    let root = private_process_root().unwrap();
    let unique = uuid::Uuid::new_v4().to_string();
    let subtree = root.join("state").join(unique);
    let state_path = subtree.join("temote-mcp");

    std::fs::create_dir_all(&state_path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&subtree, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&state_path, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            std::fs::metadata(&subtree).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&state_path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    assert_eq!(subtree, std::fs::canonicalize(&subtree).unwrap());
    assert_eq!(state_path, std::fs::canonicalize(&state_path).unwrap());
    std::fs::remove_dir_all(subtree).unwrap();
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
