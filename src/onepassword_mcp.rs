use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::child_env;
#[cfg(test)]
use crate::line_protocol::session_probe_means_stopped;
use crate::line_protocol::{
    ChildMcp, integration, validate_child_resource_uri, validate_child_tool_call,
};
use crate::{approvals, config};

const MAX_CACHED_CLIENTS: usize = 64;

pub(crate) fn integration_spec() -> &'static integration::IntegrationSpec {
    &integration::ONEPASSWORD_MCP
}

fn onepassword_child_command(executable: &Path, cwd: &Path) -> Command {
    let mut command = Command::new(executable);
    command
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    child_env::scrub_sensitive(&mut command, &[]);
    command
}

fn clients() -> &'static Mutex<HashMap<String, ChildMcp>> {
    static CLIENTS: OnceLock<Mutex<HashMap<String, ChildMcp>>> = OnceLock::new();
    CLIENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub async fn discover(session: &config::Session) -> Result<Value> {
    let (resources, tools) = {
        let mut clients = clients().lock().await;
        ensure_client(&mut clients, session).await?;
        let client = clients
            .get_mut(&session.id)
            .context("1Password MCP client disappeared")?;
        let resources = match client.request("resources/list", json!({})).await {
            Ok(value) => value,
            Err(error) => {
                clients.remove(&session.id);
                return Err(error);
            }
        };
        let tools = match client.request("tools/list", json!({})).await {
            Ok(value) => value,
            Err(error) => {
                clients.remove(&session.id);
                return Err(error);
            }
        };
        (resources, tools)
    };
    approvals::activity(&session.id, "Discovered 1Password MCP capabilities", None).await;
    Ok(json!({"resources": resources["resources"], "tools": tools["tools"]}))
}

pub async fn read_resource(session: &config::Session, uri: &str) -> Result<Value> {
    validate_child_resource_uri(uri, "1password://").context("invalid 1Password resource URI")?;
    let result = {
        let mut clients = clients().lock().await;
        ensure_client(&mut clients, session).await?;
        let client = clients
            .get_mut(&session.id)
            .context("1Password MCP client disappeared")?;
        let listed = client.request("resources/list", json!({})).await;
        let listed = match listed {
            Ok(value) => value,
            Err(error) => {
                clients.remove(&session.id);
                return Err(error);
            }
        };
        let allowed = listed["resources"].as_array().is_some_and(|resources| {
            resources
                .iter()
                .any(|resource| resource["uri"].as_str() == Some(uri))
        });
        anyhow::ensure!(allowed, "unknown 1Password MCP resource: {uri}");
        match client.request("resources/read", json!({"uri": uri})).await {
            Ok(value) => value,
            Err(error) => {
                clients.remove(&session.id);
                return Err(error);
            }
        }
    };
    approvals::activity(
        &session.id,
        "Read 1Password MCP resource",
        Some(format!("└ {uri}")),
    )
    .await;
    Ok(result)
}

pub async fn call_tool(
    session: &config::Session,
    tool_name: &str,
    arguments: Value,
) -> Result<Value> {
    validate_child_tool_call(tool_name, &arguments).context("invalid 1Password MCP tool call")?;
    enforce_path_boundary(session, tool_name, &arguments)?;

    let descriptor = {
        let mut clients = clients().lock().await;
        ensure_client(&mut clients, session).await?;
        let client = clients
            .get_mut(&session.id)
            .context("1Password MCP client disappeared")?;
        let listed = match client.request("tools/list", json!({})).await {
            Ok(value) => value,
            Err(error) => {
                clients.remove(&session.id);
                return Err(error);
            }
        };
        listed["tools"]
            .as_array()
            .and_then(|tools| {
                tools
                    .iter()
                    .find(|tool| tool["name"].as_str() == Some(tool_name))
                    .cloned()
            })
            .with_context(|| format!("unknown 1Password MCP tool: {tool_name}"))?
    };

    let read_only = descriptor
        .pointer("/annotations/readOnlyHint")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !read_only
        && !approvals::request(
            &session.id,
            "onepassword_mcp_call",
            safe_call_summary(tool_name, &arguments),
            session.cwd.clone(),
        )
        .await?
    {
        anyhow::bail!("user denied 1Password MCP tool call")
    }

    let result = {
        let mut clients = clients().lock().await;
        ensure_client(&mut clients, session).await?;
        let client = clients
            .get_mut(&session.id)
            .context("1Password MCP client disappeared")?;
        match client
            .request(
                "tools/call",
                json!({"name": tool_name, "arguments": arguments}),
            )
            .await
        {
            Ok(value) => value,
            Err(error) => {
                clients.remove(&session.id);
                return Err(error);
            }
        }
    };
    approvals::activity(
        &session.id,
        format!("Called 1Password MCP tool {tool_name}"),
        None,
    )
    .await;
    Ok(result)
}

async fn ensure_client(
    clients: &mut HashMap<String, ChildMcp>,
    session: &config::Session,
) -> Result<()> {
    retain_live_entries(clients, |client| !client.watcher_finished());
    if clients.contains_key(&session.id) {
        return Ok(());
    }
    anyhow::ensure!(
        client_capacity_available(false, clients.len()),
        "1Password MCP client limit reached ({MAX_CACHED_CLIENTS})"
    );
    let executable = executable_path()?;
    let command = onepassword_child_command(&executable, &session.cwd);
    clients.insert(
        session.id.clone(),
        ChildMcp::spawn(
            command,
            "1Password MCP",
            integration_spec().title,
            Some(session.id.clone()),
        )
        .await?,
    );
    Ok(())
}

fn retain_live_entries<T>(entries: &mut HashMap<String, T>, mut is_live: impl FnMut(&T) -> bool) {
    entries.retain(|_, entry| is_live(entry));
}

fn client_capacity_available(existing: bool, live_count: usize) -> bool {
    existing || live_count < MAX_CACHED_CLIENTS
}

fn enforce_path_boundary(
    session: &config::Session,
    tool_name: &str,
    arguments: &Value,
) -> Result<()> {
    if tool_name != "create_local_env_file" {
        return Ok(());
    }
    let mount_path = arguments
        .get("mountPath")
        .and_then(Value::as_str)
        .context("create_local_env_file requires mountPath")?;
    config::resolve_write_path(session, Path::new(mount_path))?;
    Ok(())
}

fn safe_call_summary(tool_name: &str, arguments: &Value) -> String {
    let mut keys = arguments
        .as_object()
        .map(|object| object.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    keys.sort();
    format!(
        "tool: {tool_name}\nargument keys: {}",
        if keys.is_empty() {
            "(none)".to_owned()
        } else {
            keys.join(", ")
        }
    )
}

fn executable_path() -> Result<PathBuf> {
    if let Some(name) = integration_spec().executable_override_env
        && let Ok(value) = std::env::var(name)
    {
        let path = PathBuf::from(value);
        anyhow::ensure!(path.is_absolute(), "{name} must be an absolute path");
        anyhow::ensure!(
            path.is_file(),
            "1Password MCP executable not found: {}",
            path.display()
        );
        return Ok(path);
    }

    #[cfg(target_os = "macos")]
    let path = PathBuf::from("/Applications/1Password.app/Contents/MacOS/1password-mcp");
    #[cfg(target_os = "linux")]
    let path = PathBuf::from("/opt/1Password/1password-mcp");
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    anyhow::bail!("1Password MCP support is currently available on macOS and Linux hosts");

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        anyhow::ensure!(
            path.is_file(),
            "1Password MCP executable not found at {}; enable the Temote MCP server in 1Password Developer settings",
            path.display()
        );
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;

    #[test]
    fn registry_spec_is_the_onepassword_mcp_source_of_truth() {
        assert_eq!(integration_spec(), &integration::ONEPASSWORD_MCP);
        assert_eq!(integration_spec().id, "onepassword");
    }

    #[test]
    fn onepassword_child_command_scrubs_temote_credentials() {
        use std::ffi::OsStr;

        let mut command = onepassword_child_command(Path::new("op-mcp"), Path::new("."));
        let sensitive_names = child_env::sensitive_environment_names();
        for name in &sensitive_names {
            command.env(name, "sentinel");
        }
        child_env::scrub_sensitive(&mut command, &[]);
        let envs = command.as_std().get_envs().collect::<Vec<_>>();
        for name in &sensitive_names {
            let value = envs
                .iter()
                .find(|(key, _)| *key == OsStr::new(name))
                .map(|(_, value)| *value);
            assert_eq!(value, Some(None), "credential leaked: {name}");
        }
    }

    #[test]
    fn generated_client_registry_reaps_finished_entries_and_respects_capacity() -> noprop::TestResult
    {
        test_support::run(0x4f50_434c_4945_4e54, 512, |ctx| {
            let count = noprop::sample_usize_in(ctx, 0..=MAX_CACHED_CLIENTS + 16);
            let mut entries = HashMap::new();
            let mut expected_live = 0usize;
            for index in 0..count {
                let finished = noprop::sample_bool(ctx);
                entries.insert(format!("client-{index}"), finished);
                if !finished {
                    expected_live += 1;
                }
            }

            retain_live_entries(&mut entries, |finished| !*finished);
            assert_eq!(entries.len(), expected_live);
            assert!(entries.values().all(|finished| !*finished));

            let existing = noprop::sample_bool(ctx);
            assert_eq!(
                client_capacity_available(existing, entries.len()),
                existing || entries.len() < MAX_CACHED_CLIENTS
            );
            Ok(())
        })
    }

    #[test]
    fn generated_session_watcher_stops_only_on_explicit_inactive() -> noprop::TestResult {
        test_support::run(0x3150_5741_5443_4845, 512, |ctx| {
            let choice = noprop::sample_usize_in(ctx, 0..3);
            let probe = match choice {
                0 => Ok(true),
                1 => Ok(false),
                _ => Err(anyhow::anyhow!("probe failure")),
            };
            assert_eq!(session_probe_means_stopped(&probe), choice == 1);
            Ok(())
        })
    }

    #[test]
    fn approval_summary_never_contains_argument_values() {
        let summary = safe_call_summary(
            "append_variables",
            &json!({
                "accountId": "account-secret-ish",
                "environmentId": "environment-secret-ish",
                "variables": [{"name": "TOKEN", "value": "super-secret", "concealed": true}]
            }),
        );
        assert!(summary.contains("accountId"));
        assert!(summary.contains("environmentId"));
        assert!(summary.contains("variables"));
        assert!(!summary.contains("super-secret"));
        assert!(!summary.contains("TOKEN"));
        assert!(!summary.contains("account-secret-ish"));
    }

    #[test]
    fn local_env_file_mounts_stay_inside_normal_session_roots() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = config::canonical_directory(root.path()).unwrap();
        let session = config::Session {
            id: "onepassword-test".to_owned(),
            cwd: root.clone(),
            permitted_directories: vec![root],
            started_at: 0,
            process_id: 0,
            yolo: false,
        };
        assert!(
            enforce_path_boundary(
                &session,
                "create_local_env_file",
                &json!({"mountPath": "inside.env"}),
            )
            .is_ok()
        );
        assert!(
            enforce_path_boundary(
                &session,
                "create_local_env_file",
                &json!({"mountPath": outside.path().join("outside.env")}),
            )
            .is_err()
        );
    }

    #[test]
    fn generated_approval_summaries_expose_keys_but_never_values() -> noprop::TestResult {
        test_support::run(0x4f50_5355_4d4d_4152, test_support::DEFAULT_CASES, |ctx| {
            let key_a = format!("key_{}", test_support::safe_component(ctx));
            let key_b = format!("key_{}", test_support::safe_component(ctx));
            let value_a = format!(
                "value-{}-{}",
                test_support::safe_component(ctx),
                noprop::sample_u64(ctx)
            );
            let value_b = format!(
                "value-{}-{}",
                test_support::safe_component(ctx),
                noprop::sample_u64(ctx)
            );
            let arguments = json!({
                key_a.clone(): value_a.clone(),
                key_b.clone(): {"nested": value_b.clone()}
            });
            let summary = safe_call_summary("generated_tool", &arguments);
            assert!(summary.contains(&key_a));
            assert!(summary.contains(&key_b));
            assert!(
                !summary.contains(&value_a),
                "leaked {value_a:?} in {summary:?}"
            );
            assert!(
                !summary.contains(&value_b),
                "leaked {value_b:?} in {summary:?}"
            );

            let mut expected_keys = [key_a, key_b];
            expected_keys.sort();
            assert!(
                summary.ends_with(&expected_keys.join(", ")),
                "summary={summary:?}"
            );
            Ok(())
        })
    }

    #[cfg(unix)]
    #[test]
    fn generated_local_env_mounts_fail_closed_on_outside_and_symlink_paths() -> noprop::TestResult {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("root");
        let outside = fixture.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        symlink(&outside, root.join("escape")).unwrap();
        let root = config::canonical_directory(&root).unwrap();
        let session = config::Session {
            id: "onepassword-pbt".to_owned(),
            cwd: root.clone(),
            permitted_directories: vec![root],
            started_at: 0,
            process_id: 0,
            yolo: false,
        };

        test_support::run(0x4f50_4d4f_554e_5401, 512, |ctx| {
            let leaf = format!("{}.env", test_support::safe_component(ctx));
            assert!(
                enforce_path_boundary(
                    &session,
                    "create_local_env_file",
                    &json!({"mountPath": leaf}),
                )
                .is_ok()
            );

            let escaped = if noprop::sample_bool(ctx) {
                outside.join(format!("{}.env", test_support::safe_component(ctx)))
            } else {
                PathBuf::from(format!("escape/{}.env", test_support::safe_component(ctx)))
            };
            assert!(
                enforce_path_boundary(
                    &session,
                    "create_local_env_file",
                    &json!({"mountPath": escaped}),
                )
                .is_err()
            );
            Ok(())
        })
    }
}
