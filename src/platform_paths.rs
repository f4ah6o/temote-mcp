use std::env;
use std::ffi::OsString;
#[cfg(any(not(target_os = "macos"), test))]
use std::path::Path;
use std::path::PathBuf;

/// Resolve the current user's home directory without pulling in a platform-path crate.
///
/// On Unix, preserve the previous `dirs` behavior: prefer a non-empty `HOME`, then
/// fall back to the passwd database for the current uid.
pub fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(system_home_dir)
}

/// Resolve Temote's platform config base using the subset of `dirs` semantics it
/// previously depended on.
#[cfg(feature = "network")]
pub fn config_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        home_dir().map(|home| home.join("Library/Application Support"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        xdg_or_home(
            env::var_os("XDG_CONFIG_HOME"),
            home_dir().as_deref(),
            ".config",
        )
    }
}

/// Resolve the state base. macOS intentionally has no dedicated state directory,
/// matching `dirs`; callers can fall back to local data when appropriate.
pub fn state_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        None
    }
    #[cfg(not(target_os = "macos"))]
    {
        xdg_or_home(
            env::var_os("XDG_STATE_HOME"),
            home_dir().as_deref(),
            ".local/state",
        )
    }
}

/// Resolve the local-data base used only as the state fallback on platforms without
/// a dedicated state directory.
pub fn data_local_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        home_dir().map(|home| home.join("Library/Application Support"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        xdg_or_home(
            env::var_os("XDG_DATA_HOME"),
            home_dir().as_deref(),
            ".local/share",
        )
    }
}

#[cfg(any(not(target_os = "macos"), test))]
fn xdg_or_home(value: Option<OsString>, home: Option<&Path>, fallback: &str) -> Option<PathBuf> {
    absolute_path(value).or_else(|| home.map(|home| home.join(fallback)))
}

#[cfg(any(not(target_os = "macos"), test))]
fn absolute_path(value: Option<OsString>) -> Option<PathBuf> {
    let path = PathBuf::from(value?);
    path.is_absolute().then_some(path)
}

#[cfg(unix)]
fn system_home_dir() -> Option<PathBuf> {
    use std::ffi::CStr;
    use std::mem;
    use std::os::unix::ffi::OsStringExt;
    use std::ptr;

    // Match the previous `dirs-sys` fallback closely. POSIX allows sysconf to
    // report no fixed bound, in which case a modest passwd buffer is conventional.
    let size = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let size = if size < 0 { 512 } else { size as usize };
    let mut buffer = vec![0_u8; size.max(1)];
    let mut passwd: libc::passwd = unsafe { mem::zeroed() };
    let mut result = ptr::null_mut();
    let status = unsafe {
        libc::getpwuid_r(
            libc::getuid(),
            &mut passwd,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() || passwd.pw_dir.is_null() {
        return None;
    }
    let bytes = unsafe { CStr::from_ptr(passwd.pw_dir) }.to_bytes();
    if bytes.is_empty() {
        None
    } else {
        Some(PathBuf::from(OsString::from_vec(bytes.to_vec())))
    }
}

#[cfg(not(unix))]
fn system_home_dir() -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_paths_require_absolute_values_and_fall_back_to_home() {
        let home = Path::new("/home/alice");
        assert_eq!(
            xdg_or_home(
                Some(OsString::from("/custom/config")),
                Some(home),
                ".config"
            ),
            Some(PathBuf::from("/custom/config"))
        );
        assert_eq!(
            xdg_or_home(
                Some(OsString::from("relative/config")),
                Some(home),
                ".config"
            ),
            Some(home.join(".config"))
        );
        assert_eq!(
            xdg_or_home(Some(OsString::new()), Some(home), ".config"),
            Some(home.join(".config"))
        );
        assert_eq!(
            xdg_or_home(Some(OsString::from("relative")), None, ".config"),
            None
        );
    }

    #[cfg(all(target_os = "macos", feature = "network"))]
    #[test]
    fn macos_config_and_local_data_share_application_support_semantics() {
        let home = home_dir().expect("macOS test environment should resolve a home directory");
        let expected = home.join("Library/Application Support");
        assert_eq!(config_dir(), Some(expected.clone()));
        assert_eq!(data_local_dir(), Some(expected));
        assert_eq!(state_dir(), None);
    }
}
