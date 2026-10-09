use tokio::process::Command;

pub const SENSITIVE_ENV_NAMES: &[&str] = &[
    "OP_SERVICE_ACCOUNT_TOKEN",
    "OP_CONNECT_HOST",
    "OP_CONNECT_TOKEN",
    "OPENAI_ADMIN_KEY",
    "CONTROL_PLANE_API_KEY",
    "OPENAI_API_KEY",
    "CLOUDFLARE_API_TOKEN",
    "TEMOTE_MCP_CLOUDFLARE_API_TOKEN",
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    "TEMOTE_MCP_GATEWAY_HOST_TOKEN",
    "TEMOTE_MCP_GATEWAY_ACCESS_CLIENT_ID",
    "TEMOTE_MCP_GATEWAY_ACCESS_CLIENT_SECRET",
    "TEMOTE_MCP_EVENTS_SENDER_BEARER",
    "HOST_TOKENS_JSON",
    "HOST_TOKEN",
    "CLIENT_TOKEN",
    "EVENT_SENDER_BEARER",
    "EVENT_SENDER_ACCESS_CLIENT_ID",
    "EVENT_SENDER_ACCESS_CLIENT_SECRET",
    "FABRIC_INTERACTION_SECRET",
    "KINTONE_USERNAME",
    "KINTONE_PASSWORD",
    "KINTONE_API_TOKEN",
    "KINTONE_BASIC_AUTH_USERNAME",
    "KINTONE_BASIC_AUTH_PASSWORD",
    "KINTONE_PFX_FILE_PASSWORD",
];

pub fn scrub_sensitive(command: &mut Command, keep: &[&str]) {
    for name in SENSITIVE_ENV_NAMES {
        if !keep.contains(name) {
            for alias in temote_mcp::environment::names(std::ffi::OsStr::new(name)) {
                command.env_remove(alias);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;
    use std::ffi::OsStr;

    fn env_value(command: &Command, name: &str) -> Option<Option<String>> {
        command
            .as_std()
            .get_envs()
            .find(|(key, _)| *key == OsStr::new(name))
            .map(|(_, value)| value.map(|value| value.to_string_lossy().into_owned()))
    }

    #[test]
    fn generated_sensitive_child_environment_is_removed_except_explicit_keep() -> noprop::TestResult
    {
        test_support::run(0x4348_494c_4445_4e56, 512, |ctx| {
            let keep_index = noprop::sample_usize_in(ctx, 0..=SENSITIVE_ENV_NAMES.len());
            let keep = if keep_index == SENSITIVE_ENV_NAMES.len() {
                None
            } else {
                Some(SENSITIVE_ENV_NAMES[keep_index])
            };
            let mut command = Command::new("child");
            for name in SENSITIVE_ENV_NAMES {
                for alias in temote_mcp::environment::names(OsStr::new(name)) {
                    command.env(alias, "sentinel");
                }
            }
            let keep_values = keep.into_iter().collect::<Vec<_>>();
            scrub_sensitive(&mut command, &keep_values);
            for name in SENSITIVE_ENV_NAMES {
                let expected = if Some(*name) == keep {
                    Some(Some("sentinel".to_owned()))
                } else {
                    Some(None)
                };
                for alias in temote_mcp::environment::names(OsStr::new(name)) {
                    assert_eq!(
                        env_value(&command, alias.to_str().unwrap()),
                        expected,
                        "name={alias:?} keep={keep:?}"
                    );
                }
            }
            Ok(())
        })
    }
}
