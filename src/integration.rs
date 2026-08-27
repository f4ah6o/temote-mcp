use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IntegrationKind {
    McpStdio,
    Command,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct IntegrationSpec {
    pub id: &'static str,
    pub title: &'static str,
    pub kind: IntegrationKind,
    pub supports_status: bool,
    pub supports_discover: bool,
    pub supports_resources: bool,
    pub captured_env: &'static [&'static str],
    pub secret_env: &'static [&'static str],
    pub executable_override_env: Option<&'static str>,
}

impl IntegrationSpec {
    pub(crate) fn capture_environment(
        &self,
        source: &BTreeMap<String, String>,
    ) -> BTreeMap<String, String> {
        self.captured_env
            .iter()
            .filter_map(|name| {
                source
                    .get(*name)
                    .cloned()
                    .filter(|value| !value.is_empty())
                    .map(|value| ((*name).to_owned(), value))
            })
            .collect()
    }

    pub(crate) fn executable_override<'a>(
        &self,
        source: &'a BTreeMap<String, String>,
    ) -> Option<&'a str> {
        self.executable_override_env
            .and_then(|name| source.get(name))
            .map(String::as_str)
            .filter(|value| !value.is_empty())
    }
}

const RUNTIME_ENV: &[&str] = &["PATH", "HOME", "TMPDIR", "LANG", "LC_ALL"];

const KINTONE_MCP_CAPTURED_ENV: &[&str] = &[
    "KINTONE_BASE_URL",
    "KINTONE_USERNAME",
    "KINTONE_PASSWORD",
    "KINTONE_API_TOKEN",
    "KINTONE_BASIC_AUTH_USERNAME",
    "KINTONE_BASIC_AUTH_PASSWORD",
    "KINTONE_PFX_FILE_PATH",
    "KINTONE_PFX_FILE_PASSWORD",
    "KINTONE_ATTACHMENTS_DIR",
    "HTTPS_PROXY",
    "https_proxy",
    "PATH",
    "HOME",
    "TMPDIR",
    "LANG",
    "LC_ALL",
];

const KINTONE_CLI_CAPTURED_ENV: &[&str] = &[
    "KINTONE_BASE_URL",
    "KINTONE_USERNAME",
    "KINTONE_PASSWORD",
    "KINTONE_API_TOKEN",
    "KINTONE_BASIC_AUTH_USERNAME",
    "KINTONE_BASIC_AUTH_PASSWORD",
    "KINTONE_GUEST_SPACE_ID",
    "HTTPS_PROXY",
    "https_proxy",
    "PATH",
    "HOME",
    "TMPDIR",
    "LANG",
    "LC_ALL",
];

const KINTONE_SECRET_ENV: &[&str] = &[
    "KINTONE_USERNAME",
    "KINTONE_PASSWORD",
    "KINTONE_API_TOKEN",
    "KINTONE_BASIC_AUTH_USERNAME",
    "KINTONE_BASIC_AUTH_PASSWORD",
    "KINTONE_PFX_FILE_PASSWORD",
];

const ONEPASSWORD_SERVICE_ACCOUNT_CAPTURED_ENV: &[&str] = &[
    "OP_SERVICE_ACCOUNT_TOKEN",
    "PATH",
    "HOME",
    "TMPDIR",
    "LANG",
    "LC_ALL",
];

pub(crate) const ONEPASSWORD_MCP: IntegrationSpec = IntegrationSpec {
    id: "onepassword",
    title: "1Password",
    kind: IntegrationKind::McpStdio,
    supports_status: false,
    supports_discover: true,
    supports_resources: true,
    captured_env: RUNTIME_ENV,
    secret_env: &[],
    executable_override_env: Some("TEMOTE_MCP_ONEPASSWORD_MCP"),
};

pub(crate) const ONEPASSWORD_SERVICE_ACCOUNT: IntegrationSpec = IntegrationSpec {
    id: "onepassword-service-account",
    title: "1Password service account",
    kind: IntegrationKind::Command,
    supports_status: true,
    supports_discover: false,
    supports_resources: false,
    captured_env: ONEPASSWORD_SERVICE_ACCOUNT_CAPTURED_ENV,
    secret_env: &["OP_SERVICE_ACCOUNT_TOKEN"],
    executable_override_env: None,
};

pub(crate) const KINTONE_MCP: IntegrationSpec = IntegrationSpec {
    id: "kintone",
    title: "kintone MCP",
    kind: IntegrationKind::McpStdio,
    supports_status: true,
    supports_discover: true,
    supports_resources: false,
    captured_env: KINTONE_MCP_CAPTURED_ENV,
    secret_env: KINTONE_SECRET_ENV,
    executable_override_env: Some("TEMOTE_MCP_KINTONE_MCP"),
};

pub(crate) const KINTONE_CLI: IntegrationSpec = IntegrationSpec {
    id: "kintone-cli",
    title: "cli-kintone",
    kind: IntegrationKind::Command,
    supports_status: true,
    supports_discover: false,
    supports_resources: false,
    captured_env: KINTONE_CLI_CAPTURED_ENV,
    secret_env: KINTONE_SECRET_ENV,
    executable_override_env: Some("TEMOTE_MCP_KINTONE_CLI"),
};

// Security boundary: integration specs are host-owned and static. Never expose a
// client-controlled registration path that can inject arbitrary commands or env.
const INTEGRATIONS: &[IntegrationSpec] = &[
    ONEPASSWORD_MCP,
    ONEPASSWORD_SERVICE_ACCOUNT,
    KINTONE_MCP,
    KINTONE_CLI,
];

pub(crate) fn all() -> &'static [IntegrationSpec] {
    INTEGRATIONS
}

pub(crate) fn get(id: &str) -> Option<&'static IntegrationSpec> {
    INTEGRATIONS.iter().find(|integration| integration.id == id)
}

pub(crate) fn captured_start_environment_names() -> Vec<&'static str> {
    let mut names = BTreeSet::new();
    for integration in all() {
        names.extend(integration.captured_env.iter().copied());
        if let Some(name) = integration.executable_override_env {
            names.insert(name);
        }
    }
    names.into_iter().collect()
}

pub(crate) fn secret_environment_names() -> Vec<&'static str> {
    let mut names = BTreeSet::new();
    for integration in all() {
        names.extend(integration.secret_env.iter().copied());
    }
    names.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integration_ids_are_unique_and_stable() {
        let ids = all()
            .iter()
            .map(|integration| integration.id)
            .collect::<BTreeSet<_>>();
        assert_eq!(ids.len(), all().len());
        assert!(ids.contains("onepassword"));
        assert!(ids.contains("onepassword-service-account"));
        assert!(ids.contains("kintone"));
        assert!(ids.contains("kintone-cli"));
    }

    #[test]
    fn registry_lookup_returns_declared_specs_only() {
        for integration in all() {
            assert_eq!(get(integration.id), Some(integration));
        }
        assert_eq!(get("unknown"), None);
    }

    #[test]
    fn capture_environment_is_allow_listed_and_drops_empty_values() {
        let source = [
            ("KINTONE_BASE_URL".to_owned(), "https://example.invalid".to_owned()),
            ("KINTONE_API_TOKEN".to_owned(), "secret".to_owned()),
            ("HOME".to_owned(), "/tmp/home".to_owned()),
            ("UNRELATED_SECRET".to_owned(), "must-not-pass".to_owned()),
            ("LANG".to_owned(), String::new()),
        ]
        .into_iter()
        .collect();
        let captured = KINTONE_MCP.capture_environment(&source);
        assert_eq!(captured.get("KINTONE_API_TOKEN").map(String::as_str), Some("secret"));
        assert_eq!(captured.get("HOME").map(String::as_str), Some("/tmp/home"));
        assert!(!captured.contains_key("UNRELATED_SECRET"));
        assert!(!captured.contains_key("LANG"));
    }

    #[test]
    fn captured_start_environment_union_covers_integration_credentials_and_overrides() {
        let names = captured_start_environment_names()
            .into_iter()
            .collect::<BTreeSet<_>>();
        for required in [
            "OP_SERVICE_ACCOUNT_TOKEN",
            "KINTONE_BASE_URL",
            "KINTONE_USERNAME",
            "KINTONE_PASSWORD",
            "KINTONE_API_TOKEN",
            "KINTONE_BASIC_AUTH_USERNAME",
            "KINTONE_BASIC_AUTH_PASSWORD",
            "KINTONE_PFX_FILE_PATH",
            "KINTONE_PFX_FILE_PASSWORD",
            "KINTONE_ATTACHMENTS_DIR",
            "KINTONE_GUEST_SPACE_ID",
            "TEMOTE_MCP_KINTONE_MCP",
            "TEMOTE_MCP_KINTONE_CLI",
            "TEMOTE_MCP_ONEPASSWORD_MCP",
            "PATH",
            "HOME",
            "TMPDIR",
            "LANG",
            "LC_ALL",
        ] {
            assert!(names.contains(required), "missing {required}");
        }
    }

    #[test]
    fn secret_environment_union_never_classifies_runtime_values_as_secrets() {
        let names = secret_environment_names()
            .into_iter()
            .collect::<BTreeSet<_>>();
        assert!(names.contains("OP_SERVICE_ACCOUNT_TOKEN"));
        assert!(names.contains("KINTONE_API_TOKEN"));
        assert!(names.contains("KINTONE_PASSWORD"));
        for runtime in RUNTIME_ENV {
            assert!(!names.contains(runtime));
        }
    }
}
