use anyhow::{Context, Result};

pub const HOST_ID_ENV: &str = "TEMOTE_MCP_HOST_ID";
const MAX_HOST_ID_BYTES: usize = 128;

pub fn resolve() -> Result<String> {
    match std::env::var(HOST_ID_ENV) {
        Ok(value) if !value.trim().is_empty() => validate(&value),
        Ok(_) => anyhow::bail!("{HOST_ID_ENV} must not be empty"),
        Err(std::env::VarError::NotPresent) => validate(&os_hostname()?),
        Err(error) => Err(error).context(format!("failed to read {HOST_ID_ENV}")),
    }
}

pub fn validate(value: &str) -> Result<String> {
    anyhow::ensure!(!value.is_empty(), "host ID must not be empty");
    anyhow::ensure!(
        value.len() <= MAX_HOST_ID_BYTES,
        "host ID must be at most {MAX_HOST_ID_BYTES} bytes"
    );
    anyhow::ensure!(
        value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')),
        "host ID may contain only ASCII letters, digits, '.', '_', and '-'"
    );
    anyhow::ensure!(
        value.bytes().any(|byte| byte.is_ascii_alphanumeric()),
        "host ID must contain at least one letter or digit"
    );
    Ok(value.to_owned())
}

#[cfg(unix)]
fn os_hostname() -> Result<String> {
    let mut buffer = [0_u8; 256];
    let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("failed to read OS hostname");
    }
    let length = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    let value = std::str::from_utf8(&buffer[..length]).context("OS hostname is not UTF-8")?;
    anyhow::ensure!(!value.is_empty(), "OS hostname is empty");
    Ok(value.to_owned())
}

#[cfg(not(unix))]
fn os_hostname() -> Result<String> {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .context("failed to determine OS hostname")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_accepts_diagnostic_safe_host_ids() {
        assert_eq!(
            validate("ubuntu1.example_test").unwrap(),
            "ubuntu1.example_test"
        );
    }

    #[test]
    fn validation_rejects_empty_whitespace_and_control_like_values() {
        for value in ["", "host name", "host/one", "host:one", "\n", "---"] {
            assert!(validate(value).is_err(), "value={value:?}");
        }
    }

    #[test]
    fn os_hostname_fallback_is_nonempty_and_valid() {
        let hostname = os_hostname().unwrap();
        assert!(!hostname.is_empty());
        assert!(validate(&hostname).is_ok());
    }
}
