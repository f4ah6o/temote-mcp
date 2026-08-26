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
}

pub(crate) const ONEPASSWORD_MCP: IntegrationSpec = IntegrationSpec {
    id: "onepassword",
    title: "1Password",
    kind: IntegrationKind::McpStdio,
    supports_status: false,
    supports_discover: true,
    supports_resources: true,
};

pub(crate) const ONEPASSWORD_SERVICE_ACCOUNT: IntegrationSpec = IntegrationSpec {
    id: "onepassword-service-account",
    title: "1Password service account",
    kind: IntegrationKind::Command,
    supports_status: true,
    supports_discover: false,
    supports_resources: false,
};

pub(crate) const KINTONE_MCP: IntegrationSpec = IntegrationSpec {
    id: "kintone",
    title: "kintone MCP",
    kind: IntegrationKind::McpStdio,
    supports_status: true,
    supports_discover: true,
    supports_resources: false,
};

pub(crate) const KINTONE_CLI: IntegrationSpec = IntegrationSpec {
    id: "kintone-cli",
    title: "cli-kintone",
    kind: IntegrationKind::Command,
    supports_status: true,
    supports_discover: false,
    supports_resources: false,
};

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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

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
}
