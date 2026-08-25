use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

const PLUGIN_NAME: &str = "temote-mcp";
const MARKETPLACE: &str = "debug";
const BINARY_HINT: &str = ".temote-mcp-bin";
const PLUGIN_MANIFEST: &str = include_str!("../.codex-plugin/plugin.json");
const SKILL: &str = include_str!("../skills/temote-mcp/SKILL.md");

pub fn run(args: &[String]) -> Result<String, String> {
    match args {
        [] => Ok(usage()),
        [arg] if arg == "--help" || arg == "-h" => Ok(usage()),
        [plugin, action] if plugin == "plugin" && action == "install" => install_current(),
        [plugin, action] if plugin == "plugin" && action == "uninstall" => uninstall_current(),
        [status] if status == "status" => status_current(false),
        [status, json] if status == "status" && json == "--json" => status_current(true),
        [diagnose] if diagnose == "diagnose" => diagnose_current(false),
        [diagnose, json] if diagnose == "diagnose" && json == "--json" => diagnose_current(true),
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
    if as_json {
        serde_json::to_string_pretty(&status.to_json()).map_err(|error| error.to_string())
    } else {
        Ok(status.to_text())
    }
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
        .ok_or_else(|| "could not determine CODEX_HOME or the current user's home directory".to_owned())
}

fn current_binary() -> Result<PathBuf, String> {
    let path = std::env::current_exe().map_err(|error| format!("could not resolve current temote-mcp binary: {error}"))?;
    canonical_binary(&path)
}

fn canonical_binary(path: &Path) -> Result<PathBuf, String> {
    let path = fs::canonicalize(path)
        .map_err(|error| format!("could not resolve temote-mcp binary {}: {error}", path.display()))?;
    let metadata = fs::metadata(&path)
        .map_err(|error| format!("could not inspect temote-mcp binary {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("temote-mcp binary is not a regular file: {}", path.display()));
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

fn install_at(codex_home: &Path, binary: &Path) -> Result<InstallStatus, String> {
    let binary = canonical_binary(binary)?;
    let root = plugin_root(codex_home);
    remove_owned_plugin_root(&root)?;
    let target = plugin_dir(codex_home);
    fs::create_dir_all(target.join(".codex-plugin"))
        .map_err(|error| format!("could not create plugin directory {}: {error}", target.display()))?;
    fs::create_dir_all(target.join("skills").join("temote-mcp"))
        .map_err(|error| format!("could not create plugin skill directory {}: {error}", target.display()))?;

    let manifest = rendered_manifest()?;
    write_text(&target.join(".codex-plugin").join("plugin.json"), &manifest)?;
    write_text(&target.join(".mcp.json"), &rendered_mcp_config(&binary)?)?;
    write_text(
        &target.join("skills").join("temote-mcp").join("SKILL.md"),
        SKILL,
    )?;
    write_text(&target.join(BINARY_HINT), &format!("{}\n", binary.display()))?;

    let config = config_path(codex_home);
    set_config_enabled(&config, true)?;
    Ok(InstallStatus {
        plugin_dir: target,
        config_path: config,
    })
}

fn uninstall_at(codex_home: &Path) -> Result<UninstallStatus, String> {
    let root = plugin_root(codex_home);
    let removed_plugin = remove_owned_plugin_root(&root)?;
    let removed_config = set_config_enabled(&config_path(codex_home), false)?;
    Ok(UninstallStatus {
        removed_plugin,
        removed_config,
    })
}

fn remove_owned_plugin_root(path: &Path) -> Result<bool, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("could not inspect plugin directory {}: {error}", path.display())),
    };
    if metadata.file_type().is_symlink() {
        return Err(format!("refusing to remove symlinked plugin directory: {}", path.display()));
    }
    if !metadata.is_dir() {
        return Err(format!("plugin cache path is not a directory: {}", path.display()));
    }
    fs::remove_dir_all(path)
        .map_err(|error| format!("could not remove plugin directory {}: {error}", path.display()))?;
    Ok(true)
}

fn rendered_manifest() -> Result<String, String> {
    let mut value: Value = serde_json::from_str(PLUGIN_MANIFEST)
        .map_err(|error| format!("embedded Codex plugin manifest is invalid: {error}"))?;
    value["version"] = Value::String(env!("CARGO_PKG_VERSION").to_owned());
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

fn write_text(path: &Path, content: &str) -> Result<(), String> {
    fs::write(path, content)
        .map_err(|error| format!("could not write {}: {error}", path.display()))
}

fn section_header() -> String {
    format!("[plugins.\"{}\"]", plugin_key())
}

fn set_config_enabled(path: &Path, enabled: bool) -> Result<bool, String> {
    let existing = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("could not read Codex config {}: {error}", path.display())),
    };
    let (mut updated, removed) = remove_plugin_config_section(&existing);
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
    if updated == existing {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("could not create Codex config directory {}: {error}", parent.display()))?;
    }
    fs::write(path, updated)
        .map_err(|error| format!("could not update Codex config {}: {error}", path.display()))?;
    Ok(removed || enabled)
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

    if !input.ends_with('\n') {
        let tail = input.rsplit_once('\n').map_or(input, |(_, tail)| tail);
        if !tail.is_empty() && !output.ends_with(tail) && !skipping {
            output.push_str(tail);
        }
    }

    while output.ends_with("\n\n\n") {
        output.pop();
    }
    (output, removed)
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
            "problems": self.problems,
            "healthy": self.problems.is_empty(),
        })
    }

    fn to_text(&self) -> String {
        let mut text = format!(
            "Codex home: {}\nPlugin key: {}\nPlugin dir: {}\nEnabled: {}\nInstalled: {}\nCurrent binary: {}\nPinned binary: {}\nBinary matches: {}\nMCP command matches: {}\n",
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
    let target = plugin_dir(codex_home);
    let config = config_path(codex_home);
    let header = section_header();
    let config_text = fs::read_to_string(&config).unwrap_or_default();
    let enabled = config_section_enabled(&config_text, &header);
    let manifest_path = target.join(".codex-plugin").join("plugin.json");
    let installed = manifest_path.is_file();
    let manifest_version = fs::read_to_string(&manifest_path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.get("version").and_then(Value::as_str).map(str::to_owned));
    let binary_hint_path = target.join(BINARY_HINT);
    let binary_hint = fs::read_to_string(&binary_hint_path)
        .ok()
        .map(|value| PathBuf::from(value.trim()))
        .filter(|path| !path.as_os_str().is_empty());
    let binary_matches = binary_hint.as_deref() == Some(current_binary);
    let mcp_command = read_mcp_command(&target.join(".mcp.json"));
    let mcp_command_matches = mcp_command.as_deref() == Some(current_binary);

    let mut problems = Vec::new();
    if !enabled {
        problems.push("the Temote plugin is not enabled in Codex config".to_owned());
    }
    if !installed {
        problems.push("the Temote plugin manifest is missing from the Codex cache".to_owned());
    }
    if installed && manifest_version.as_deref() != Some(env!("CARGO_PKG_VERSION")) {
        problems.push("the installed plugin version does not match this temote-mcp binary".to_owned());
    }
    if binary_hint.is_none() {
        problems.push("the installed plugin has no pinned temote-mcp binary hint".to_owned());
    } else if !binary_matches {
        problems.push("the installed plugin is pinned to a different temote-mcp binary".to_owned());
    }
    if mcp_command.is_none() {
        problems.push("the installed plugin MCP command is missing or invalid".to_owned());
    } else if !mcp_command_matches {
        problems.push("the installed plugin MCP command does not use this exact temote-mcp binary".to_owned());
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
        assert!(installed.plugin_dir.join(".codex-plugin/plugin.json").is_file());
        assert!(installed.plugin_dir.join("skills/temote-mcp/SKILL.md").is_file());
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
}
