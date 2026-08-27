use std::collections::BTreeSet;

use tokio::process::Command;

use crate::line_protocol::integration;

pub const SENSITIVE_ENV_NAMES: &[&str] = &[
    "OPENAI_ADMIN_KEY",
    "CONTROL_PLANE_API_KEY",
    "OPENAI_API_KEY",
    "CLOUDFLARE_API_TOKEN",
    "TEMOTE_MCP_CLOUDFLARE_API_TOKEN",
    "TEMOTE_MCP_GATEWAY_HOST_TOKEN",
    "TEMOTE_MCP_GATEWAY_ACCESS_CLIENT_ID",
    "TEMOTE_MCP_GATEWAY_ACCESS_CLIENT_SECRET",
];

pub fn sensitive_environment_names() -> Vec<&'static str> {
    let mut names = SENSITIVE_ENV_NAMES
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    names.extend(integration::secret_environment_names());
    names.into_iter().collect()
}

pub fn scrub_sensitive(command: &mut Command, keep: &[&str]) {
    for name in sensitive_environment_names() {
        if !keep.contains(&name) {
            command.env_remove(name);
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
        let names = sensitive_environment_names();
        test_support::run(0x4348_494c_4445_4e56, 512, |ctx| {
            let keep_index = noprop::sample_usize_in(ctx, 0..=names.len());
            let keep = if keep_index == names.len() {
                None
            } else {
                Some(names[keep_index])
            };
            let mut command = Command::new("child");
            for name in &names {
                command.env(name, "sentinel");
            }
            let keep_values = keep.into_iter().collect::<Vec<_>>();
            scrub_sensitive(&mut command, &keep_values);
            for name in &names {
                let expected = if Some(*name) == keep {
                    Some(Some("sentinel".to_owned()))
                } else {
                    Some(None)
                };
                assert_eq!(
                    env_value(&command, name),
                    expected,
                    "name={name} keep={keep:?}"
                );
            }
            Ok(())
        })
    }

    #[test]
    fn integration_secrets_are_in_the_scrub_set() {
        let names = sensitive_environment_names();
        assert!(names.contains(&"OP_SERVICE_ACCOUNT_TOKEN"));
        assert!(names.contains(&"KINTONE_API_TOKEN"));
        assert!(names.contains(&"KINTONE_PASSWORD"));
    }
}
