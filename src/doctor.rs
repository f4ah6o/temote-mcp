#[cfg(target_os = "linux")]
use std::path::Path;

#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::Result;
#[cfg(target_os = "linux")]
use tokio::process::Command;

use crate::sandbox;

#[cfg(target_os = "linux")]
const BWRAP_INSTALL_HINT: &str =
    "Install bubblewrap (for example: sudo apt install bubblewrap) and make sure it is in PATH.";
const APPARMOR_PROFILE_HINT: &str = "Ubuntu may be blocking unprivileged user namespaces. Try:\n\
sudo apt update\n\
sudo apt install apparmor-profiles apparmor-utils\n\
sudo install -m 0644 /usr/share/apparmor/extra-profiles/bwrap-userns-restrict /etc/apparmor.d/bwrap-userns-restrict\n\
sudo apparmor_parser -r /etc/apparmor.d/bwrap-userns-restrict";

#[derive(Clone, Copy)]
enum Level {
    Pass,
    #[cfg(target_os = "linux")]
    Warn,
    Fail,
}

struct Check {
    level: Level,
    name: String,
    detail: String,
    hint: Option<String>,
}

impl Check {
    fn pass(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            level: Level::Pass,
            name: name.into(),
            detail: detail.into(),
            hint: None,
        }
    }

    #[cfg(target_os = "linux")]
    fn warn(name: impl Into<String>, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            level: Level::Warn,
            name: name.into(),
            detail: detail.into(),
            hint: Some(hint.into()),
        }
    }

    fn fail(name: impl Into<String>, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            level: Level::Fail,
            name: name.into(),
            detail: detail.into(),
            hint: Some(hint.into()),
        }
    }

    fn is_failure(&self) -> bool {
        matches!(self.level, Level::Fail)
    }

    fn print(&self) {
        let label = match self.level {
            Level::Pass => "PASS",
            #[cfg(target_os = "linux")]
            Level::Warn => "WARN",
            Level::Fail => "FAIL",
        };
        println!("[{label}] {}: {}", self.name, self.detail);
        if let Some(hint) = &self.hint {
            for line in hint.lines() {
                println!("       {line}");
            }
        }
    }
}

struct Report {
    checks: Vec<Check>,
}

impl Report {
    fn new() -> Self {
        Self { checks: Vec::new() }
    }

    fn add(&mut self, check: Check) {
        self.checks.push(check);
    }

    fn finish(self) -> Result<()> {
        for check in &self.checks {
            check.print();
        }

        let failures = self
            .checks
            .iter()
            .filter(|check| check.is_failure())
            .count();
        #[cfg(target_os = "linux")]
        let warnings = self
            .checks
            .iter()
            .filter(|check| matches!(check.level, Level::Warn))
            .count();
        #[cfg(not(target_os = "linux"))]
        let warnings = 0;
        println!();
        println!(
            "doctor summary: {} failure(s), {} warning(s)",
            failures, warnings
        );
        if failures == 0 {
            Ok(())
        } else {
            anyhow::bail!("temote-mcp doctor found {failures} failing check(s)")
        }
    }
}

pub async fn run() -> Result<()> {
    println!("temote-mcp doctor");
    println!("platform: {}", std::env::consts::OS);

    let mut report = Report::new();
    check_platform(&mut report);
    #[cfg(target_os = "macos")]
    report.add(Check::pass("sandbox backend", "native macOS Seatbelt"));
    #[cfg(target_os = "linux")]
    report.add(Check::pass("sandbox backend", "Codex Linux sandbox"));

    #[cfg(target_os = "linux")]
    {
        let helper = check_linux_helper(&mut report)?;
        let network_namespace_ok = check_bwrap(&mut report).await;
        check_user_namespace_settings(&mut report, network_namespace_ok);
        if helper {
            check_sandbox_execution(&mut report).await;
            check_sandbox_runtime_environment(&mut report).await;
        }
    }

    #[cfg(target_os = "macos")]
    {
        check_sandbox_execution(&mut report).await;
        check_sandbox_runtime_environment(&mut report).await;
    }

    report.finish()
}

fn check_platform(report: &mut Report) {
    if cfg!(any(target_os = "linux", target_os = "macos")) {
        report.add(Check::pass("platform", "supported local sandbox platform"));
    } else {
        report.add(Check::fail(
            "platform",
            "unsupported operating system",
            "temote-mcp sandbox execution currently supports Linux and macOS only.",
        ));
    }
}

#[cfg(target_os = "linux")]
fn check_linux_helper(report: &mut Report) -> Result<bool> {
    let executable =
        std::env::current_exe().context("could not determine temote-mcp executable")?;
    let directory = executable
        .parent()
        .context("temote-mcp executable has no parent directory")?;
    let helper = directory.join("codex-linux-sandbox");
    if helper.is_file() {
        report.add(Check::pass(
            "sandbox helper",
            format!("{}", helper.display()),
        ));
        Ok(true)
    } else {
        report.add(Check::fail(
            "sandbox helper",
            format!("missing {}", helper.display()),
            "Install temote-mcp with `cargo install --path . --locked` so the helper is installed beside it.",
        ));
        Ok(false)
    }
}

#[cfg(target_os = "linux")]
async fn check_bwrap(report: &mut Report) -> bool {
    let version = Command::new("bwrap").arg("--version").output().await;
    match version {
        Ok(output) if output.status.success() => {
            report.add(Check::pass(
                "bubblewrap",
                display_output(&output).unwrap_or_else(|| "available".to_owned()),
            ));
        }
        Ok(output) => {
            report.add(Check::fail(
                "bubblewrap",
                format!("--version exited with {}", output.status),
                BWRAP_INSTALL_HINT,
            ));
            return false;
        }
        Err(error) => {
            report.add(Check::fail(
                "bubblewrap",
                format!("cannot execute bwrap: {error}"),
                BWRAP_INSTALL_HINT,
            ));
            return false;
        }
    }

    match Command::new("bwrap")
        .args([
            "--unshare-user",
            "--unshare-net",
            "--ro-bind",
            "/",
            "/",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--",
            "/bin/true",
        ])
        .output()
        .await
    {
        Ok(output) if output.status.success() => {
            report.add(Check::pass(
                "network namespace",
                "bwrap can create the isolated loopback namespace",
            ));
            true
        }
        Ok(output) => {
            let detail =
                display_output(&output).unwrap_or_else(|| format!("exited with {}", output.status));
            let hint = if contains_loopback_permission_error(&output) {
                APPARMOR_PROFILE_HINT
            } else {
                "Run the bwrap namespace probe manually and check the host's user-namespace and network-namespace policy."
            };
            report.add(Check::fail("network namespace", detail, hint));
            false
        }
        Err(error) => {
            report.add(Check::fail(
                "network namespace",
                format!("cannot execute bwrap: {error}"),
                BWRAP_INSTALL_HINT,
            ));
            false
        }
    }
}

#[cfg(target_os = "linux")]
fn check_user_namespace_settings(report: &mut Report, network_namespace_ok: bool) {
    check_positive_sysctl(
        report,
        "/proc/sys/user/max_user_namespaces",
        "user namespaces",
        "Enable unprivileged user namespaces or run temote-mcp on a host that permits them.",
    );
    check_positive_sysctl(
        report,
        "/proc/sys/kernel/unprivileged_userns_clone",
        "unprivileged user namespaces",
        "Set kernel.unprivileged_userns_clone to 1 or use the distribution's supported user-namespace configuration.",
    );

    let path = Path::new("/proc/sys/kernel/apparmor_restrict_unprivileged_userns");
    match std::fs::read_to_string(path) {
        Ok(value) if value.trim() == "1" && network_namespace_ok => report.add(Check::pass(
            "AppArmor userns policy",
            "restriction value is 1; bwrap compatibility check passed",
        )),
        Ok(value) if value.trim() == "1" => report.add(Check::warn(
            "AppArmor userns policy",
            "unprivileged user namespaces are restricted (1)",
            APPARMOR_PROFILE_HINT,
        )),
        Ok(value) => report.add(Check::pass(
            "AppArmor userns policy",
            format!("restriction value is {}", value.trim()),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => report.add(Check::pass(
            "AppArmor userns policy",
            "kernel setting is not present",
        )),
        Err(error) => report.add(Check::warn(
            "AppArmor userns policy",
            format!("could not read {path:?}: {error}"),
            "Check the host's AppArmor and user-namespace policy if bwrap fails.",
        )),
    }
}

#[cfg(target_os = "linux")]
fn check_positive_sysctl(report: &mut Report, path: &str, name: &str, hint: &str) {
    match std::fs::read_to_string(path) {
        Ok(value) => match value.trim().parse::<u64>() {
            Ok(value) if value > 0 => report.add(Check::pass(name, format!("{path}={value}"))),
            Ok(value) => report.add(Check::fail(name, format!("{path}={value}"), hint)),
            Err(error) => report.add(Check::warn(
                name,
                format!("{path} is not numeric: {error}"),
                hint,
            )),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            report.add(Check::pass(name, format!("{path} is not present")))
        }
        Err(error) => report.add(Check::warn(
            name,
            format!("could not read {path}: {error}"),
            hint,
        )),
    }
}

async fn check_sandbox_execution(report: &mut Report) {
    let cwd = match std::env::current_dir().and_then(std::fs::canonicalize) {
        Ok(path) => path,
        Err(error) => {
            report.add(Check::fail(
                "sandbox execution",
                format!("cannot resolve current directory: {error}"),
                "Run doctor from an existing directory that the current user can read.",
            ));
            return;
        }
    };

    let roots = vec![cwd.clone()];
    #[cfg(target_os = "macos")]
    let true_executable = "/usr/bin/true";
    #[cfg(not(target_os = "macos"))]
    let true_executable = "/bin/true";
    match sandbox::run(&[true_executable.to_owned()], &cwd, &roots, None).await {
        Ok(output) if output.status == 0 => report.add(Check::pass(
            "sandbox execution",
            "a temote-mcp sandboxed command completed successfully",
        )),
        Ok(output) => {
            let detail = if output.stderr.trim().is_empty() {
                format!("sandboxed command exited with status {}", output.status)
            } else {
                output.stderr.trim().to_owned()
            };
            let hint = if contains_loopback_permission_error_text(&detail) {
                APPARMOR_PROFILE_HINT
            } else {
                "Fix the lower-level sandbox check above, then restart temote-mcp."
            };
            report.add(Check::fail("sandbox execution", detail, hint));
        }
        Err(error) => {
            let detail = format!("{error:#}");
            let hint = if contains_loopback_permission_error_text(&detail) {
                APPARMOR_PROFILE_HINT
            } else {
                "Fix the lower-level sandbox check above, then restart temote-mcp."
            };
            report.add(Check::fail("sandbox execution", detail, hint));
        }
    }
}

async fn check_sandbox_runtime_environment(report: &mut Report) {
    let cwd = match std::env::current_dir().and_then(std::fs::canonicalize) {
        Ok(path) => path,
        Err(error) => {
            report.add(Check::fail(
                "sandbox runtime environment",
                format!("cannot resolve current directory: {error}"),
                "Run doctor from an existing directory that the current user can read.",
            ));
            return;
        }
    };

    let command = vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        "test -n \"$HOME\" || { echo 'HOME is unset' >&2; exit 10; }; test -d \"$HOME\" || { echo 'HOME is not a directory' >&2; exit 11; }; temp=$(mktemp /tmp/temote-mcp-doctor.XXXXXX) || { echo '/tmp is not writable' >&2; exit 12; }; rm -f \"$temp\" || { echo 'cannot remove temporary file from /tmp' >&2; exit 13; }".to_owned(),
    ];
    match sandbox::run(&command, &cwd, std::slice::from_ref(&cwd), None).await {
        Ok(output) if output.status == 0 => report.add(Check::pass(
            "sandbox runtime environment",
            "HOME and /tmp are available to sandboxed commands",
        )),
        Ok(output) => {
            let detail = command_output_detail(&output);
            report.add(Check::fail(
                "sandbox runtime environment",
                detail,
                "Run `just install` to update temote-mcp, then restart it; shell commands need HOME and a writable temporary directory.",
            ));
        }
        Err(error) => report.add(Check::fail(
            "sandbox runtime environment",
            format!("{error:#}"),
            "Run `just install` to update temote-mcp, then restart it; shell commands need HOME and a writable temporary directory.",
        )),
    }
}

fn command_output_detail(output: &sandbox::Output) -> String {
    let stderr = output.stderr.trim();
    if !stderr.is_empty() {
        stderr.to_owned()
    } else if output.stdout.trim().is_empty() {
        format!("sandboxed command exited with status {}", output.status)
    } else {
        output.stdout.trim().to_owned()
    }
}

#[cfg(target_os = "linux")]
fn display_output(output: &std::process::Output) -> Option<String> {
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if !stderr.is_empty() {
        Some(stderr)
    } else if !stdout.is_empty() {
        Some(stdout)
    } else {
        None
    }
}

#[cfg(target_os = "linux")]
fn contains_loopback_permission_error(output: &std::process::Output) -> bool {
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    contains_loopback_permission_error_text(&text)
}

fn contains_loopback_permission_error_text(text: &str) -> bool {
    text.contains("RTM_NEWADDR")
        || (text.contains("loopback") && text.contains("Operation not permitted"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_the_bwrap_loopback_failure() {
        assert!(contains_loopback_permission_error_text(
            "bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted\n"
        ));
    }

    #[test]
    fn ignores_unrelated_bwrap_failures() {
        assert!(!contains_loopback_permission_error_text(
            "bwrap: Can't find source path /missing\n"
        ));
    }
}
