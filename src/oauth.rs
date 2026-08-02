//! OAuth authorization server for the HTTP MCP endpoint.
//!
//! Ported from `shuttle-rs` (`src/oauth.rs`, MIT OR Apache-2.0) with two
//! changes: the SQLite store is replaced by the JSON state file this crate
//! already uses for sessions, and refresh tokens are issued and rotated so
//! remote clients do not have to repeat owner approval every hour.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use url::Url;
use uuid::Uuid;

const MCP_SCOPE: &str = "mcp";
const CODE_TTL: u64 = 10 * 60;
const ACCESS_TOKEN_TTL: u64 = 60 * 60;
const REFRESH_TOKEN_TTL: u64 = 30 * 24 * 60 * 60;
const RATE_WINDOW: u64 = 60;

/// Redirect URIs accepted from dynamic client registration by default.
///
/// A trailing `/` marks a prefix; every other entry must match exactly.
pub const DEFAULT_REDIRECT_PREFIXES: [&str; 3] = [
    "https://chatgpt.com/connector/oauth/",
    "https://chatgpt.com/connector_platform_oauth_redirect",
    "https://claude.ai/api/mcp/auth_callback",
];

#[derive(Clone)]
pub struct OAuthConfig {
    pub public_url: String,
    /// Owner-approval token required by the authorization page.
    pub admin_token: String,
    pub allowed_redirect_prefixes: Vec<String>,
}

impl OAuthConfig {
    pub fn normalize_public_url(public_url: &str) -> String {
        public_url.trim().trim_end_matches('/').to_owned()
    }

    pub fn resource_url(&self) -> String {
        format!("{}/mcp", self.public_url)
    }

    /// Returns true when dynamic client registration may accept `redirect_uri`.
    pub fn allows_redirect(&self, redirect_uri: &str) -> bool {
        self.allowed_redirect_prefixes.iter().any(|allowed| {
            redirect_uri == allowed || (allowed.ends_with('/') && redirect_uri.starts_with(allowed))
        })
    }
}

#[derive(Clone)]
pub struct OAuthStore {
    path: PathBuf,
    state: Arc<Mutex<State>>,
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    clients: HashMap<String, Client>,
    #[serde(default)]
    codes: HashMap<String, Code>,
    /// Keyed by the SHA-256 digest of the bearer token, never the token.
    #[serde(default)]
    access_tokens: HashMap<String, Grant>,
    #[serde(default)]
    refresh_tokens: HashMap<String, Grant>,
    #[serde(default)]
    rate_limits: HashMap<String, RateLimit>,
}

#[derive(Serialize, Deserialize)]
struct Client {
    redirect_uris: Vec<String>,
    client_name: Option<String>,
    created_at: u64,
}

#[derive(Serialize, Deserialize)]
struct Code {
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    scope: String,
    expires_at: u64,
    #[serde(default)]
    used: bool,
}

#[derive(Serialize, Deserialize)]
struct Grant {
    client_id: String,
    scope: String,
    expires_at: u64,
}

#[derive(Serialize, Deserialize)]
struct RateLimit {
    window_started: u64,
    count: u32,
}

impl OAuthStore {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let state = match tokio::fs::read(&path).await {
            Ok(bytes) => serde_json::from_slice(&bytes).context("invalid OAuth state file")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(error) => return Err(error).context("failed to read the OAuth state file"),
        };
        let store = Self {
            path,
            state: Arc::new(Mutex::new(state)),
        };
        store.cleanup_expired().await?;
        Ok(store)
    }

    pub async fn register_client(
        &self,
        request: RegisterRequest,
        config: &OAuthConfig,
    ) -> Result<RegisteredClient> {
        anyhow::ensure!(
            !request.redirect_uris.is_empty(),
            "redirect_uris must contain at least one URI"
        );
        let mut redirect_uris = request.redirect_uris;
        redirect_uris.sort();
        redirect_uris.dedup();
        for uri in &redirect_uris {
            validate_redirect_uri(uri)?;
            anyhow::ensure!(
                config.allows_redirect(uri),
                "redirect URI {uri} is not allowed by this server"
            );
        }
        let client = RegisteredClient {
            client_id: token("lmc"),
            redirect_uris,
            client_name: request.client_name,
        };
        let mut state = self.state.lock().await;
        purge_expired(&mut state);
        enforce_rate_limit(&mut state, "registration", 30)?;
        state.clients.insert(
            client.client_id.clone(),
            Client {
                redirect_uris: client.redirect_uris.clone(),
                client_name: client.client_name.clone(),
                created_at: now(),
            },
        );
        self.persist(&state).await?;
        Ok(client)
    }

    pub async fn client_allows_redirect(&self, client_id: &str, redirect_uri: &str) -> bool {
        let state = self.state.lock().await;
        state
            .clients
            .get(client_id)
            .is_some_and(|client| client.redirect_uris.iter().any(|uri| uri == redirect_uri))
    }

    pub async fn create_code(&self, request: AuthorizeRequest) -> Result<String> {
        anyhow::ensure!(request.response_type == "code", "response_type must be code");
        anyhow::ensure!(
            request.code_challenge_method.as_deref() == Some("S256"),
            "code_challenge_method must be S256"
        );
        let code_challenge = request
            .code_challenge
            .clone()
            .context("missing code_challenge")?;
        validate_redirect_uri(&request.redirect_uri)?;
        anyhow::ensure!(
            self.client_allows_redirect(&request.client_id, &request.redirect_uri)
                .await,
            "unknown client_id or redirect_uri"
        );
        let scope = normalize_scope(request.scope.clone());
        let code = token("lmcc");
        let mut state = self.state.lock().await;
        purge_expired(&mut state);
        enforce_rate_limit(&mut state, &format!("authorize:{}", request.client_id), 30)?;
        state.codes.insert(
            code.clone(),
            Code {
                client_id: request.client_id,
                redirect_uri: request.redirect_uri,
                code_challenge,
                scope,
                expires_at: now() + CODE_TTL,
                used: false,
            },
        );
        self.persist(&state).await?;
        Ok(code)
    }

    pub async fn exchange_code(&self, request: TokenRequest) -> Result<TokenResponse> {
        anyhow::ensure!(
            request.grant_type == "authorization_code",
            "grant_type must be authorization_code"
        );
        let code = request.code.context("missing code")?;
        let verifier = request.code_verifier.context("missing code_verifier")?;
        let mut state = self.state.lock().await;
        purge_expired(&mut state);
        let rate_key = format!("token:{}", request.client_id.as_deref().unwrap_or("unknown"));
        enforce_rate_limit(&mut state, &rate_key, 20)?;

        let stored = state.codes.get(&code).context("invalid code")?;
        anyhow::ensure!(!stored.used, "code already used");
        if let Some(client_id) = request.client_id.as_deref() {
            anyhow::ensure!(stored.client_id == client_id, "invalid client_id");
        }
        if let Some(redirect_uri) = request.redirect_uri.as_deref() {
            anyhow::ensure!(stored.redirect_uri == redirect_uri, "invalid redirect_uri");
        }
        anyhow::ensure!(stored.expires_at > now(), "code expired");
        anyhow::ensure!(
            pkce_s256(&verifier) == stored.code_challenge,
            "invalid code_verifier"
        );

        let client_id = stored.client_id.clone();
        let scope = stored.scope.clone();
        if let Some(stored) = state.codes.get_mut(&code) {
            stored.used = true;
        }
        let response = issue_tokens(&mut state, &client_id, &scope);
        self.persist(&state).await?;
        Ok(response)
    }

    pub async fn refresh(&self, request: TokenRequest) -> Result<TokenResponse> {
        anyhow::ensure!(
            request.grant_type == "refresh_token",
            "grant_type must be refresh_token"
        );
        let refresh_token = request.refresh_token.context("missing refresh_token")?;
        let mut state = self.state.lock().await;
        purge_expired(&mut state);
        let rate_key = format!("token:{}", request.client_id.as_deref().unwrap_or("unknown"));
        enforce_rate_limit(&mut state, &rate_key, 20)?;

        // Rotation: the presented refresh token is consumed even when the rest
        // of the request turns out to be invalid.
        let grant = state
            .refresh_tokens
            .remove(&hash_token(&refresh_token))
            .context("invalid refresh_token")?;
        anyhow::ensure!(grant.expires_at > now(), "refresh_token expired");
        if let Some(client_id) = request.client_id.as_deref() {
            anyhow::ensure!(grant.client_id == client_id, "invalid client_id");
        }
        let response = issue_tokens(&mut state, &grant.client_id, &grant.scope);
        self.persist(&state).await?;
        Ok(response)
    }

    pub async fn validate_access_token(&self, bearer_token: &str) -> bool {
        let state = self.state.lock().await;
        state
            .access_tokens
            .get(&hash_token(bearer_token))
            .is_some_and(|grant| {
                grant.expires_at > now() && grant.scope.split_whitespace().any(|s| s == MCP_SCOPE)
            })
    }

    /// Revokes an access or refresh token. Returns true when something was
    /// removed; RFC 7009 still expects a success response either way.
    pub async fn revoke_token(&self, bearer_token: &str) -> Result<bool> {
        let digest = hash_token(bearer_token);
        let mut state = self.state.lock().await;
        let access_removed = state.access_tokens.remove(&digest).is_some();
        let refresh_removed = state.refresh_tokens.remove(&digest).is_some();
        self.persist(&state).await?;
        Ok(access_removed || refresh_removed)
    }

    pub async fn cleanup_expired(&self) -> Result<()> {
        let mut state = self.state.lock().await;
        purge_expired(&mut state);
        self.persist(&state).await
    }

    async fn persist(&self, state: &State) -> Result<()> {
        let temporary = self
            .path
            .with_extension(format!("json.{}.tmp", std::process::id()));
        tokio::fs::write(&temporary, serde_json::to_vec_pretty(state)?).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600)).await?;
        }
        tokio::fs::rename(&temporary, &self.path).await?;
        Ok(())
    }
}

fn issue_tokens(state: &mut State, client_id: &str, scope: &str) -> TokenResponse {
    let access_token = token("lmca");
    let refresh_token = token("lmcr");
    let now = now();
    state.access_tokens.insert(
        hash_token(&access_token),
        Grant {
            client_id: client_id.to_owned(),
            scope: scope.to_owned(),
            expires_at: now + ACCESS_TOKEN_TTL,
        },
    );
    state.refresh_tokens.insert(
        hash_token(&refresh_token),
        Grant {
            client_id: client_id.to_owned(),
            scope: scope.to_owned(),
            expires_at: now + REFRESH_TOKEN_TTL,
        },
    );
    TokenResponse {
        access_token,
        token_type: "Bearer",
        expires_in: ACCESS_TOKEN_TTL,
        refresh_token,
        scope: scope.to_owned(),
    }
}

fn purge_expired(state: &mut State) {
    let now = now();
    state.codes.retain(|_, code| code.expires_at > now);
    state.access_tokens.retain(|_, grant| grant.expires_at > now);
    state.refresh_tokens.retain(|_, grant| grant.expires_at > now);
    state
        .rate_limits
        .retain(|_, limit| limit.window_started + RATE_WINDOW > now);
}

fn enforce_rate_limit(state: &mut State, key: &str, max_requests: u32) -> Result<()> {
    let now = now();
    let limit = state.rate_limits.entry(key.to_owned()).or_insert(RateLimit {
        window_started: now,
        count: 0,
    });
    if limit.window_started + RATE_WINDOW <= now {
        limit.window_started = now;
        limit.count = 0;
    }
    anyhow::ensure!(limit.count < max_requests, "rate limit exceeded");
    limit.count += 1;
    Ok(())
}

fn validate_redirect_uri(value: &str) -> Result<()> {
    anyhow::ensure!(
        !value.contains('*'),
        "redirect URI wildcards are not allowed"
    );
    let parsed = Url::parse(value).context("redirect URI is malformed")?;
    anyhow::ensure!(
        parsed.fragment().is_none() && parsed.username().is_empty() && parsed.password().is_none(),
        "redirect URI must not contain a fragment or userinfo"
    );
    let host = parsed
        .host_str()
        .context("redirect URI must include a host")?;
    let loopback = matches!(parsed.host(), Some(url::Host::Ipv4(ip)) if ip.is_loopback())
        || matches!(parsed.host(), Some(url::Host::Ipv6(ip)) if ip.is_loopback())
        || host.eq_ignore_ascii_case("localhost");
    anyhow::ensure!(
        parsed.scheme() == "https" || (parsed.scheme() == "http" && loopback),
        "redirect URI must use HTTPS except for loopback URIs"
    );
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    #[serde(default)]
    pub redirect_uris: Vec<String>,
    pub client_name: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RegisteredClient {
    pub client_id: String,
    pub redirect_uris: Vec<String>,
    pub client_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthorizeRequest {
    pub response_type: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub state: Option<String>,
    pub scope: Option<String>,
    pub code_challenge: Option<String>,
    pub code_challenge_method: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AuthorizeForm {
    pub admin_token: String,
    pub response_type: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub state: Option<String>,
    pub scope: Option<String>,
    pub code_challenge: Option<String>,
    pub code_challenge_method: Option<String>,
}

impl From<AuthorizeForm> for AuthorizeRequest {
    fn from(form: AuthorizeForm) -> Self {
        Self {
            response_type: form.response_type,
            client_id: form.client_id,
            redirect_uri: form.redirect_uri,
            state: form.state,
            scope: form.scope,
            code_challenge: form.code_challenge,
            code_challenge_method: form.code_challenge_method,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokenRequest {
    pub grant_type: String,
    pub client_id: Option<String>,
    pub redirect_uri: Option<String>,
    pub code: Option<String>,
    pub code_verifier: Option<String>,
    pub refresh_token: Option<String>,
}

/// RFC 7009 revocation request. `token_type_hint` is accepted and ignored,
/// like every other unknown field.
#[derive(Debug, Deserialize)]
pub struct RevokeRequest {
    pub token: String,
}

#[derive(Debug, Serialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: &'static str,
    pub expires_in: u64,
    pub refresh_token: String,
    pub scope: String,
}

pub fn authorization_server_metadata(config: &OAuthConfig) -> Value {
    json!({
        "issuer": config.public_url,
        "authorization_endpoint": format!("{}/oauth/authorize", config.public_url),
        "token_endpoint": format!("{}/oauth/token", config.public_url),
        "registration_endpoint": format!("{}/oauth/register", config.public_url),
        "revocation_endpoint": format!("{}/oauth/revoke", config.public_url),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": [MCP_SCOPE],
    })
}

pub fn protected_resource_metadata(config: &OAuthConfig) -> Value {
    json!({
        "resource": config.resource_url(),
        "authorization_servers": [config.public_url],
        "scopes_supported": [MCP_SCOPE],
        "bearer_methods_supported": ["header"],
    })
}

/// Builds the OAuth 2.0 authorization-code redirect URL (RFC 6749 §4.1.2).
///
/// `code` and `state` are percent-encoded so reserved characters in opaque
/// client state cannot change the query structure.
pub fn authorize_redirect(redirect_uri: &str, code: &str, state: Option<&str>) -> String {
    let mut target = format!(
        "{}{}code={}",
        redirect_uri,
        if redirect_uri.contains('?') { "&" } else { "?" },
        query_component(code)
    );
    if let Some(state) = state {
        target.push_str("&state=");
        target.push_str(&query_component(state));
    }
    target
}

fn query_component(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

fn normalize_scope(scope: Option<String>) -> String {
    let scope = scope.unwrap_or_else(|| MCP_SCOPE.to_owned());
    if scope.split_whitespace().any(|scope| scope == MCP_SCOPE) {
        scope
    } else {
        MCP_SCOPE.to_owned()
    }
}

fn token(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4().simple())
}

fn hash_token(value: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(value.as_bytes()))
}

fn pkce_s256(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    const VERIFIER: &str = "abc123abc123abc123abc123abc123abc123abc123abc123";

    fn config() -> OAuthConfig {
        OAuthConfig {
            public_url: "https://localmcp.example.test".to_owned(),
            admin_token: "admin".to_owned(),
            allowed_redirect_prefixes: DEFAULT_REDIRECT_PREFIXES
                .iter()
                .map(|prefix| (*prefix).to_owned())
                .collect(),
        }
    }

    async fn store(directory: &tempfile::TempDir) -> OAuthStore {
        OAuthStore::open(directory.path().join("oauth.json"))
            .await
            .unwrap()
    }

    async fn registered_client(store: &OAuthStore) -> RegisteredClient {
        store
            .register_client(
                RegisterRequest {
                    redirect_uris: vec!["https://chatgpt.com/connector/oauth/abc".to_owned()],
                    client_name: Some("ChatGPT".to_owned()),
                },
                &config(),
            )
            .await
            .unwrap()
    }

    fn authorize(client_id: &str) -> AuthorizeRequest {
        AuthorizeRequest {
            response_type: "code".to_owned(),
            client_id: client_id.to_owned(),
            redirect_uri: "https://chatgpt.com/connector/oauth/abc".to_owned(),
            state: None,
            scope: Some("mcp".to_owned()),
            code_challenge: Some(pkce_s256(VERIFIER)),
            code_challenge_method: Some("S256".to_owned()),
        }
    }

    fn exchange(client_id: &str, code: String) -> TokenRequest {
        TokenRequest {
            grant_type: "authorization_code".to_owned(),
            client_id: Some(client_id.to_owned()),
            redirect_uri: Some("https://chatgpt.com/connector/oauth/abc".to_owned()),
            code: Some(code),
            code_verifier: Some(VERIFIER.to_owned()),
            refresh_token: None,
        }
    }

    #[tokio::test]
    async fn metadata_uses_public_url() {
        let config = config();
        assert_eq!(
            protected_resource_metadata(&config)["resource"],
            "https://localmcp.example.test/mcp"
        );
        assert_eq!(
            authorization_server_metadata(&config)["token_endpoint"],
            "https://localmcp.example.test/oauth/token"
        );
    }

    #[test]
    fn authorize_redirect_encodes_state_as_query_component() {
        assert_eq!(
            authorize_redirect(
                "https://chatgpt.com/connector/oauth/abc",
                "lmcc_1",
                Some("opaque=value+with/special&fragment#part")
            ),
            "https://chatgpt.com/connector/oauth/abc?code=lmcc_1&state=opaque%3Dvalue%2Bwith%2Fspecial%26fragment%23part"
        );
    }

    #[test]
    fn redirect_allowlist_matches_prefixes_and_exact_uris() {
        let config = config();
        assert!(config.allows_redirect("https://chatgpt.com/connector/oauth/abc"));
        assert!(config.allows_redirect("https://chatgpt.com/connector_platform_oauth_redirect"));
        assert!(!config.allows_redirect("https://chatgpt.com/connector_platform_oauth_redirect2"));
        assert!(!config.allows_redirect("https://evil.example.test/callback"));
    }

    #[tokio::test]
    async fn code_exchange_validates_pkce_and_issues_tokens() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(&directory).await;
        let client = registered_client(&store).await;
        let code = store.create_code(authorize(&client.client_id)).await.unwrap();

        let token = store
            .exchange_code(exchange(&client.client_id, code))
            .await
            .unwrap();

        assert!(store.validate_access_token(&token.access_token).await);
        assert!(!store.validate_access_token("lmca_unknown").await);
    }

    #[tokio::test]
    async fn code_exchange_rejects_reuse_and_bad_verifier() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(&directory).await;
        let client = registered_client(&store).await;
        let code = store.create_code(authorize(&client.client_id)).await.unwrap();

        let mut wrong = exchange(&client.client_id, code.clone());
        wrong.code_verifier = Some("wrong-verifier-wrong-verifier-wrong".to_owned());
        assert!(
            store
                .exchange_code(wrong)
                .await
                .unwrap_err()
                .to_string()
                .contains("invalid code_verifier")
        );

        store
            .exchange_code(exchange(&client.client_id, code.clone()))
            .await
            .unwrap();
        assert!(
            store
                .exchange_code(exchange(&client.client_id, code))
                .await
                .unwrap_err()
                .to_string()
                .contains("code already used")
        );
    }

    #[tokio::test]
    async fn refresh_tokens_rotate() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(&directory).await;
        let client = registered_client(&store).await;
        let code = store.create_code(authorize(&client.client_id)).await.unwrap();
        let first = store
            .exchange_code(exchange(&client.client_id, code))
            .await
            .unwrap();

        let refresh = TokenRequest {
            grant_type: "refresh_token".to_owned(),
            client_id: Some(client.client_id.clone()),
            redirect_uri: None,
            code: None,
            code_verifier: None,
            refresh_token: Some(first.refresh_token.clone()),
        };
        let second = store.refresh(refresh.clone()).await.unwrap();

        assert_ne!(first.access_token, second.access_token);
        assert_ne!(first.refresh_token, second.refresh_token);
        assert!(store.validate_access_token(&second.access_token).await);
        assert!(
            store
                .refresh(refresh)
                .await
                .unwrap_err()
                .to_string()
                .contains("invalid refresh_token")
        );
    }

    #[tokio::test]
    async fn registration_enforces_redirect_policy() {
        let directory = tempfile::tempdir().unwrap();
        let store = store(&directory).await;
        for redirect_uri in [
            "http://chatgpt.com/connector/oauth/abc",
            "https://chatgpt.com/connector/oauth/abc#fragment",
            "https://user:password@chatgpt.com/connector/oauth/abc",
            "https://chatgpt.com/connector/oauth/*",
            "https://evil.example.test/callback",
            "not a url",
        ] {
            assert!(
                store
                    .register_client(
                        RegisterRequest {
                            redirect_uris: vec![redirect_uri.to_owned()],
                            client_name: None,
                        },
                        &config()
                    )
                    .await
                    .is_err(),
                "{redirect_uri} should be rejected"
            );
        }
    }

    #[tokio::test]
    async fn tokens_are_hashed_at_rest_and_revocable() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("oauth.json");
        let store = OAuthStore::open(&path).await.unwrap();
        let client = registered_client(&store).await;
        let code = store.create_code(authorize(&client.client_id)).await.unwrap();
        let token = store
            .exchange_code(exchange(&client.client_id, code))
            .await
            .unwrap();

        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(!contents.contains(&token.access_token));
        assert!(!contents.contains(&token.refresh_token));

        assert!(store.revoke_token(&token.access_token).await.unwrap());
        assert!(!store.validate_access_token(&token.access_token).await);
        assert!(!store.revoke_token(&token.access_token).await.unwrap());
    }

    #[tokio::test]
    async fn state_survives_reopening() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("oauth.json");
        let token = {
            let store = OAuthStore::open(&path).await.unwrap();
            let client = registered_client(&store).await;
            let code = store.create_code(authorize(&client.client_id)).await.unwrap();
            store
                .exchange_code(exchange(&client.client_id, code))
                .await
                .unwrap()
        };

        let reopened = OAuthStore::open(&path).await.unwrap();
        assert!(reopened.validate_access_token(&token.access_token).await);
    }
}
