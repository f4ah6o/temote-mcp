//! Typed session source contract for session-first provisioning.
//!
//! S0a is intentionally behavior-preserving: these types do not change the
//! current public `session_start(path=...)` wire contract yet. They provide
//! the seam later provisioning packets will wire into the supervisor.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct RepositoryId {
    host: String,
    owner: String,
    name: String,
}

impl RepositoryId {
    pub(crate) fn parse(source: &str, default_host: &str) -> Result<Self, RepositorySourceError> {
        parse_repository_id(source, default_host)
    }

    /// Construct an already-normalized identity. Unlike source parsing this
    /// never strips `.git`, which can be a legitimate normalized name suffix.
    pub(crate) fn new(host: &str, owner: &str, name: &str) -> Result<Self, RepositorySourceError> {
        validate_component(host, RepositorySourceError::InvalidHost)?;
        validate_component(owner, RepositorySourceError::InvalidOwner)?;
        validate_component(name, RepositorySourceError::InvalidRepository)?;
        let host = host.to_ascii_lowercase();
        let (owner, name) = if host == "github.com" {
            (owner.to_ascii_lowercase(), name.to_ascii_lowercase())
        } else {
            (owner.to_owned(), name.to_owned())
        };
        Ok(Self { host, owner, name })
    }

    pub(crate) fn host(&self) -> &str {
        &self.host
    }
    pub(crate) fn owner(&self) -> &str {
        &self.owner
    }
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn logical_name(&self) -> String {
        format!("{}/{}/{}", self.host, self.owner, self.name)
    }
}

impl<'de> Deserialize<'de> for RepositoryId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            host: String,
            owner: String,
            name: String,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::new(&wire.host, &wire.owner, &wire.name).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VcsPreference {
    #[default]
    Auto,
    Jujutsu,
    Git,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum SessionStartSpec {
    ManagedRepository {
        repository: RepositoryId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base: Option<String>,
        #[serde(default)]
        vcs: VcsPreference,
    },
    ExistingWorkspace {
        logical_path: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RepositorySourceError {
    Empty,
    AbsolutePath,
    QueryOrFragment,
    Credentials,
    UnsupportedScheme,
    InvalidComponentCount,
    Traversal,
    InvalidHost,
    InvalidOwner,
    InvalidRepository,
}

impl fmt::Display for RepositorySourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "repository source is empty",
            Self::AbsolutePath => "repository source must not be an absolute filesystem path",
            Self::QueryOrFragment => "repository source must not contain a query or fragment",
            Self::Credentials => "repository source must not contain credentials",
            Self::UnsupportedScheme => {
                "repository source scheme is unsupported; use owner/repo, host/owner/repo, HTTPS, or git@host:owner/repo"
            }
            Self::InvalidComponentCount => {
                "repository source must identify exactly an owner and repository"
            }
            Self::Traversal => "repository source must not contain dot or parent traversal segments",
            Self::InvalidHost => "repository source host is invalid",
            Self::InvalidOwner => "repository source owner is invalid",
            Self::InvalidRepository => "repository source repository name is invalid",
        })
    }
}

impl std::error::Error for RepositorySourceError {}

fn parse_repository_id(
    source: &str,
    default_host: &str,
) -> Result<RepositoryId, RepositorySourceError> {
    validate_component(default_host, RepositorySourceError::InvalidHost)?;
    if source.is_empty() {
        return Err(RepositorySourceError::Empty);
    }
    if source.trim() != source {
        return Err(RepositorySourceError::InvalidRepository);
    }
    if source.contains('?') || source.contains('#') {
        return Err(RepositorySourceError::QueryOrFragment);
    }

    if let Some(rest) = source.strip_prefix("https://") {
        return parse_https(rest);
    }
    if source.starts_with("http://") || source.contains("://") {
        return Err(RepositorySourceError::UnsupportedScheme);
    }
    if let Some(rest) = source.strip_prefix("git@") {
        return parse_git_ssh(rest);
    }
    if source.contains('@') {
        return Err(RepositorySourceError::Credentials);
    }
    if looks_like_absolute_path(source) {
        return Err(RepositorySourceError::AbsolutePath);
    }
    if source.contains('\\') {
        return Err(RepositorySourceError::InvalidRepository);
    }

    let parts = source.split('/').collect::<Vec<_>>();
    match parts.as_slice() {
        [owner, name] => build_repository_id(default_host, owner, name),
        [host, owner, name] => build_repository_id(host, owner, name),
        _ => Err(RepositorySourceError::InvalidComponentCount),
    }
}

fn parse_https(rest: &str) -> Result<RepositoryId, RepositorySourceError> {
    let (authority, path) = rest
        .split_once('/')
        .ok_or(RepositorySourceError::InvalidComponentCount)?;
    if authority.contains('@') {
        return Err(RepositorySourceError::Credentials);
    }
    if authority.contains(':') {
        return Err(RepositorySourceError::InvalidHost);
    }
    let parts = path.split('/').collect::<Vec<_>>();
    match parts.as_slice() {
        [owner, name] => build_repository_id(authority, owner, name),
        _ => Err(RepositorySourceError::InvalidComponentCount),
    }
}

fn parse_git_ssh(rest: &str) -> Result<RepositoryId, RepositorySourceError> {
    let (host, path) = rest
        .split_once(':')
        .ok_or(RepositorySourceError::InvalidComponentCount)?;
    if host.contains('@') {
        return Err(RepositorySourceError::Credentials);
    }
    let parts = path.split('/').collect::<Vec<_>>();
    match parts.as_slice() {
        [owner, name] => build_repository_id(host, owner, name),
        _ => Err(RepositorySourceError::InvalidComponentCount),
    }
}

fn build_repository_id(
    host: &str,
    owner: &str,
    name: &str,
) -> Result<RepositoryId, RepositorySourceError> {
    validate_component(host, RepositorySourceError::InvalidHost)?;
    validate_component(owner, RepositorySourceError::InvalidOwner)?;

    validate_component(name, RepositorySourceError::InvalidRepository)?;
    let name = name.strip_suffix(".git").unwrap_or(name);
    validate_component(name, RepositorySourceError::InvalidRepository)?;
    RepositoryId::new(host, owner, name)
}

fn validate_component(
    component: &str,
    error: RepositorySourceError,
) -> Result<(), RepositorySourceError> {
    if component == "." || component == ".." {
        return Err(RepositorySourceError::Traversal);
    }
    if !component
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
        || !component.as_bytes().iter().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, b'.' | b'_' | b'-')
        })
    {
        return Err(error);
    }
    Ok(())
}

fn looks_like_absolute_path(source: &str) -> bool {
    source.starts_with('/')
        || source.starts_with("\\\\")
        || (source.len() >= 3
            && source.as_bytes()[1] == b':'
            && matches!(source.as_bytes()[2], b'/' | b'\\'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;

    #[test]
    fn parses_owner_repo_with_supplied_default_host() {
        let parsed = RepositoryId::parse("f4ah6o/temote-mcp", "github.com").unwrap();
        assert_eq!(
            parsed,
            RepositoryId::new("github.com", "f4ah6o", "temote-mcp").unwrap()
        );
    }

    #[test]
    fn parses_explicit_host_owner_repo() {
        let parsed = RepositoryId::parse("git.example.test/Team/Repo", "github.com").unwrap();
        assert_eq!(parsed.host, "git.example.test");
        assert_eq!(parsed.owner, "Team");
        assert_eq!(parsed.name, "Repo");
    }

    #[test]
    fn parses_https_source() {
        let parsed =
            RepositoryId::parse("https://github.com/f4ah6o/temote-mcp", "unused.test").unwrap();
        assert_eq!(parsed.host, "github.com");
        assert_eq!(parsed.owner, "f4ah6o");
        assert_eq!(parsed.name, "temote-mcp");
    }

    #[test]
    fn strips_terminal_dot_git_suffix() {
        let parsed =
            RepositoryId::parse("https://github.com/f4ah6o/temote-mcp.git", "unused.test").unwrap();
        assert_eq!(parsed.name, "temote-mcp");
    }

    #[test]
    fn parses_git_at_ssh_source() {
        let parsed =
            RepositoryId::parse("git@github.com:f4ah6o/temote-mcp.git", "unused.test").unwrap();
        assert_eq!(parsed.host, "github.com");
        assert_eq!(parsed.owner, "f4ah6o");
        assert_eq!(parsed.name, "temote-mcp");
    }

    #[test]
    fn normalizes_github_identity_case() {
        let parsed =
            RepositoryId::parse("https://GitHub.COM/F4AH6O/Temote-MCP", "unused.test").unwrap();
        assert_eq!(parsed.host, "github.com");
        assert_eq!(parsed.owner, "f4ah6o");
        assert_eq!(parsed.name, "temote-mcp");
    }

    #[test]
    fn rejects_one_component_source() {
        assert_eq!(
            RepositoryId::parse("temote-mcp", "github.com").unwrap_err(),
            RepositorySourceError::InvalidComponentCount
        );
    }

    #[test]
    fn rejects_extra_path_components() {
        assert_eq!(
            RepositoryId::parse("github.com/a/b/c", "github.com").unwrap_err(),
            RepositorySourceError::InvalidComponentCount
        );
    }

    #[test]
    fn rejects_parent_traversal() {
        assert_eq!(
            RepositoryId::parse("owner/../repo", "github.com").unwrap_err(),
            RepositorySourceError::Traversal
        );
    }

    #[test]
    fn rejects_absolute_filesystem_paths() {
        assert_eq!(
            RepositoryId::parse("/tmp/temote-mcp", "github.com").unwrap_err(),
            RepositorySourceError::AbsolutePath
        );
        assert_eq!(
            RepositoryId::parse(r"C:\\src\\temote-mcp", "github.com").unwrap_err(),
            RepositorySourceError::AbsolutePath
        );
    }

    #[test]
    fn rejects_https_query_and_fragment() {
        for source in [
            "https://github.com/f4ah6o/temote-mcp?ref=main",
            "https://github.com/f4ah6o/temote-mcp#readme",
        ] {
            assert_eq!(
                RepositoryId::parse(source, "github.com").unwrap_err(),
                RepositorySourceError::QueryOrFragment
            );
        }
    }

    #[test]
    fn rejects_credential_bearing_https_url() {
        assert_eq!(
            RepositoryId::parse("https://token@github.com/f4ah6o/temote-mcp", "github.com")
                .unwrap_err(),
            RepositorySourceError::Credentials
        );
    }

    #[test]
    fn managed_repository_round_trips_through_serde() {
        let spec = SessionStartSpec::ManagedRepository {
            repository: RepositoryId::parse("f4ah6o/temote-mcp", "github.com").unwrap(),
            base: Some("main".to_owned()),
            vcs: VcsPreference::Jujutsu,
        };
        let encoded = serde_json::to_string(&spec).unwrap();
        let decoded: SessionStartSpec = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, spec);
    }

    #[test]
    fn existing_workspace_round_trips_through_serde() {
        let spec = SessionStartSpec::ExistingWorkspace {
            logical_path: "src/temote-mcp".to_owned(),
        };
        let encoded = serde_json::to_string(&spec).unwrap();
        let decoded: SessionStartSpec = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, spec);
    }

    #[test]
    fn managed_repository_serialization_has_no_physical_or_delivery_fields() {
        let spec = SessionStartSpec::ManagedRepository {
            repository: RepositoryId::parse("f4ah6o/temote-mcp", "github.com").unwrap(),
            base: Some("main".to_owned()),
            vcs: VcsPreference::Auto,
        };
        let value = serde_json::to_value(spec).unwrap();
        let object = value.as_object().unwrap();
        assert_eq!(
            object.get("kind").and_then(serde_json::Value::as_str),
            Some("managed_repository")
        );
        for forbidden in ["path", "store", "store_path", "workspace", "branch"] {
            assert!(
                !object.contains_key(forbidden),
                "{forbidden} leaked into start spec"
            );
        }
    }

    #[test]
    fn rejects_forbidden_raw_components_and_invalid_default_host() {
        for source in [
            "github.com/f4ah6o/repo%2Fother",
            "github.com/f4ah6o/repo%5Cother",
            "github.com/f4ah6o/repo%20name",
            "ow\\ner/repo",
            "owner/re\\po",
            " owner/repo",
            "owner/repo ",
            "owner/re po",
            "owner/re\tpo",
            "owner/re\npo",
            "owner/re\0po",
            "owner/\u{7f}repo",
            "owner/.git",
            "https://github.com/./repo",
            "https://github.com/../repo",
            "git@github.com:owner/..",
            "https://github.com/owner/..%2Fother",
        ] {
            assert!(
                RepositoryId::parse(source, "github.com").is_err(),
                "{source:?}"
            );
        }
        for host in [
            "bad/host",
            "bad%2Fhost",
            "bad\\host",
            " bad",
            "bad\0host",
            ".host",
        ] {
            assert!(RepositoryId::parse("owner/repo", host).is_err(), "{host:?}");
            assert!(
                RepositoryId::parse("github.com/owner/repo", host).is_err(),
                "{host:?}"
            );
        }
    }

    #[test]
    fn checked_constructor_and_serde_preserve_normalized_suffix() {
        let identity = RepositoryId::parse("OWNER/Repo.git.git", "GitHub.COM").unwrap();
        assert_eq!(identity.name, "repo.git");
        assert_eq!(
            identity,
            RepositoryId::new("GitHub.COM", "OWNER", "Repo.git").unwrap()
        );
        let encoded = serde_json::to_string(&identity).unwrap();
        assert_eq!(
            serde_json::from_str::<RepositoryId>(&encoded).unwrap(),
            identity
        );
        for value in [
            serde_json::json!({"host":"github.com","owner":"o","name":"bad%2Fname"}),
            serde_json::json!({"host":"bad/host","owner":"o","name":"repo"}),
            serde_json::json!({"host":"github.com","owner":".","name":"repo"}),
            serde_json::json!({"host":"github.com","owner":"o","name":"repo","extra":1}),
        ] {
            assert!(serde_json::from_value::<RepositoryId>(value).is_err());
        }
    }

    #[test]
    fn generated_notations_normalize_and_round_trip() -> noprop::TestResult {
        test_support::run(0x5245_504f_4944_0001, 512, |ctx| {
            let owner = format!("a{}", test_support::safe_component(ctx));
            let name = format!("r{}", test_support::safe_component(ctx));
            let expected = RepositoryId::new("github.com", &owner, &name).unwrap();
            for source in [
                format!("{owner}/{name}"),
                format!("github.com/{owner}/{name}"),
                format!("https://github.com/{owner}/{name}.git"),
                format!("git@github.com:{owner}/{name}.git"),
            ] {
                let parsed = RepositoryId::parse(&source, "GitHub.COM").unwrap();
                assert_eq!(parsed, expected);
                for component in [&parsed.host, &parsed.owner, &parsed.name] {
                    assert!(
                        component
                            .as_bytes()
                            .first()
                            .is_some_and(u8::is_ascii_alphanumeric)
                    );
                    assert!(
                        component
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
                    );
                }
                let encoded = serde_json::to_string(&parsed).unwrap();
                assert_eq!(
                    serde_json::from_str::<RepositoryId>(&encoded).unwrap(),
                    parsed
                );
            }
            Ok(())
        })
    }

    #[test]
    fn generated_malformed_source_is_never_repaired() -> noprop::TestResult {
        test_support::run(0x5245_504f_4944_0002, 512, |ctx| {
            let stem = format!("r{}", test_support::safe_component(ctx));
            let invalid = ["%2F", "%5C", "%20", "\\", " ", "\t", "\n", "\0", "@"];
            let inserted = invalid[noprop::sample_usize_in(ctx, 0..=invalid.len() - 1)];
            let source = format!("owner/{stem}{inserted}repo");
            assert!(
                RepositoryId::parse(&source, "github.com").is_err(),
                "{source:?}"
            );
            Ok(())
        })
    }
}
