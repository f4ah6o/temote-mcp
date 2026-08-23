use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::http::{HeaderMap, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
use url::Url;

use crate::approvals::ApprovalSender;
use crate::provider::Identity;

const CODE_TTL: Duration = Duration::from_secs(5 * 60);
const TOKEN_TTL: Duration = Duration::from_secs(15 * 60);
const MAX_CLIENTS: usize = 256;
const MAX_CODES: usize = 256;
const MAX_TOKENS: usize = 1024;
const MAX_PENDING_AUTHORIZATIONS: usize = 64;
const MAX_PENDING_METADATA_FETCHES: usize = 16;
const MAX_CLIENT_METADATA_ADDRESSES: usize = 32;
const METADATA_DNS_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REDIRECT_URIS: usize = 16;
const MAX_URI_BYTES: usize = 2048;
const MAX_OAUTH_STATE_BYTES: usize = 4096;
const MAX_OAUTH_SCOPE_BYTES: usize = 256;
const MAX_OAUTH_TOKEN_VALUE_BYTES: usize = 256;
const MAX_OAUTH_FIXED_PARAMETER_BYTES: usize = 64;
const MAX_OAUTH_CLIENT_NAME_BYTES: usize = 1024;
const MAX_OAUTH_CAPABILITY_VALUES: usize = 8;
const MAX_CLIENT_METADATA_BYTES: usize = 128 * 1024;

#[derive(Clone)]
pub struct LocalOAuth {
    public_url: String,
    resource: String,
    approvals: ApprovalSender,
    approval_slots: Arc<Semaphore>,
    metadata_slots: Arc<Semaphore>,
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    clients: HashMap<String, ClientRegistration>,
    codes: HashMap<String, AuthorizationCode>,
    tokens: HashMap<String, AccessToken>,
}

#[derive(Clone)]
struct ClientRegistration {
    name: String,
    redirect_uris: HashSet<String>,
    last_used_at: Instant,
}

struct AuthorizationCode {
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    resource: String,
    expires_at: Instant,
}

struct AccessToken {
    client_id: String,
    subject: String,
    resource: String,
    expires_at: Instant,
}

#[derive(Debug)]
pub struct OAuthError {
    pub status: StatusCode,
    pub code: &'static str,
    pub description: String,
}

impl OAuthError {
    fn bad_request(code: &'static str, description: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code,
            description: description.into(),
        }
    }

    pub fn json(&self) -> Value {
        json!({
            "error": self.code,
            "error_description": self.description,
        })
    }
}

#[derive(Debug, Deserialize)]
pub struct RegistrationRequest {
    pub redirect_uris: Vec<String>,
    #[serde(default)]
    pub client_name: Option<String>,
    #[serde(default)]
    pub application_type: Option<String>,
    #[serde(default)]
    pub grant_types: Vec<String>,
    #[serde(default)]
    pub response_types: Vec<String>,
    #[serde(default)]
    pub token_endpoint_auth_method: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AuthorizeRequest {
    pub response_type: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub code_challenge_method: String,
    pub resource: String,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TokenRequest {
    pub grant_type: String,
    pub code: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub code_verifier: String,
    pub resource: String,
}

#[derive(Debug, Deserialize)]
struct ClientMetadataDocument {
    client_id: String,
    redirect_uris: Vec<String>,
    #[serde(default)]
    client_name: Option<String>,
    #[serde(default)]
    grant_types: Vec<String>,
    #[serde(default)]
    response_types: Vec<String>,
    #[serde(default)]
    token_endpoint_auth_method: Option<String>,
}

impl LocalOAuth {
    pub fn new(public_url: String, approvals: ApprovalSender) -> Self {
        Self::with_limits(
            public_url,
            approvals,
            MAX_PENDING_AUTHORIZATIONS,
            MAX_PENDING_METADATA_FETCHES,
        )
    }

    fn with_limits(
        public_url: String,
        approvals: ApprovalSender,
        max_pending_authorizations: usize,
        max_pending_metadata_fetches: usize,
    ) -> Self {
        let resource = format!("{public_url}/mcp");
        Self {
            public_url,
            resource,
            approvals,
            approval_slots: Arc::new(Semaphore::new(max_pending_authorizations)),
            metadata_slots: Arc::new(Semaphore::new(max_pending_metadata_fetches)),
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    pub fn protected_resource_metadata(&self) -> Value {
        json!({
            "resource": self.resource,
            "authorization_servers": [self.public_url],
            "bearer_methods_supported": ["header"],
            "scopes_supported": ["mcp"],
        })
    }

    pub fn authorization_server_metadata(&self) -> Value {
        json!({
            "issuer": self.public_url,
            "authorization_endpoint": format!("{}/authorize", self.public_url),
            "token_endpoint": format!("{}/token", self.public_url),
            "registration_endpoint": format!("{}/register", self.public_url),
            "response_types_supported": ["code"],
            "grant_types_supported": ["authorization_code"],
            "code_challenge_methods_supported": ["S256"],
            "token_endpoint_auth_methods_supported": ["none"],
            "scopes_supported": ["mcp"],
            "client_id_metadata_document_supported": true,
        })
    }

    pub fn resource_metadata_url(&self) -> String {
        format!("{}/.well-known/oauth-protected-resource", self.public_url)
    }

    pub async fn register(
        &self,
        request: RegistrationRequest,
    ) -> std::result::Result<Value, OAuthError> {
        validate_registration_request_bounds(&request)?;
        if request.redirect_uris.is_empty() || request.redirect_uris.len() > MAX_REDIRECT_URIS {
            return Err(OAuthError::bad_request(
                "invalid_client_metadata",
                format!("redirect_uris must contain 1..={MAX_REDIRECT_URIS} entries"),
            ));
        }
        if request
            .application_type
            .as_deref()
            .is_some_and(|value| value != "native" && value != "web")
        {
            return Err(OAuthError::bad_request(
                "invalid_client_metadata",
                "application_type must be native or web",
            ));
        }
        if request
            .token_endpoint_auth_method
            .as_deref()
            .is_some_and(|value| value != "none")
        {
            return Err(OAuthError::bad_request(
                "invalid_client_metadata",
                "only public clients with token_endpoint_auth_method=none are supported",
            ));
        }
        if request
            .grant_types
            .iter()
            .any(|value| value != "authorization_code")
        {
            return Err(OAuthError::bad_request(
                "invalid_client_metadata",
                "only authorization_code is supported",
            ));
        }
        if request.response_types.iter().any(|value| value != "code") {
            return Err(OAuthError::bad_request(
                "invalid_client_metadata",
                "only response_type=code is supported",
            ));
        }

        let mut redirect_uris = HashSet::with_capacity(request.redirect_uris.len());
        for redirect_uri in request.redirect_uris {
            validate_redirect_uri(&redirect_uri)?;
            redirect_uris.insert(redirect_uri);
        }
        let name = request
            .client_name
            .unwrap_or_else(|| "MCP client".to_owned())
            .trim()
            .chars()
            .take(200)
            .collect::<String>();
        let name = if name.is_empty() {
            "MCP client".to_owned()
        } else {
            name
        };

        let client_id = format!("temote-{}", random_token().map_err(internal_error)?);
        let mut state = self.state.lock().await;
        cleanup(&mut state);
        insert_client_registration(
            &mut state,
            client_id.clone(),
            ClientRegistration {
                name: name.clone(),
                redirect_uris: redirect_uris.clone(),
                last_used_at: Instant::now(),
            },
            MAX_CLIENTS,
        )?;
        Ok(json!({
            "client_id": client_id,
            "client_name": name,
            "redirect_uris": redirect_uris.into_iter().collect::<Vec<_>>(),
            "application_type": request.application_type.unwrap_or_else(|| "native".to_owned()),
            "grant_types": ["authorization_code"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        }))
    }

    pub async fn authorize(
        &self,
        request: AuthorizeRequest,
    ) -> std::result::Result<String, OAuthError> {
        validate_authorize_request_lengths(&request)?;
        if request.response_type != "code" {
            return Err(OAuthError::bad_request(
                "unsupported_response_type",
                "only response_type=code is supported",
            ));
        }
        if request.code_challenge_method != "S256" {
            return Err(OAuthError::bad_request(
                "invalid_request",
                "PKCE S256 is mandatory",
            ));
        }
        validate_code_challenge(&request.code_challenge)?;
        self.validate_resource(&request.resource)?;
        validate_scope(request.scope.as_deref())?;

        let client = self
            .resolve_client(&request.client_id, &request.redirect_uri)
            .await?;
        if !client.redirect_uris.contains(&request.redirect_uri) {
            return Err(OAuthError::bad_request(
                "invalid_request",
                "redirect_uri does not exactly match the registered URI",
            ));
        }

        let _approval_permit = self
            .approval_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                OAuthError::bad_request(
                    "temporarily_unavailable",
                    "too many OAuth authorization requests are waiting for local approval",
                )
            })?;
        let detail = format!(
            "Authorize OAuth client {:?}\nclient_id: {}\nredirect: {}\nresource: {}\nscope: mcp",
            client.name, request.client_id, request.redirect_uri, request.resource
        );
        let allowed = crate::approvals::request_supervisor_approval(
            &self.approvals,
            "oauth_authorize",
            detail,
        )
        .await
        .map_err(internal_error)?;

        if !allowed {
            return redirect_error(
                &request.redirect_uri,
                "access_denied",
                request.state.as_deref(),
                &self.public_url,
            );
        }

        let code = random_token().map_err(internal_error)?;
        {
            let mut state = self.state.lock().await;
            cleanup(&mut state);
            insert_authorization_code(
                &mut state,
                code.clone(),
                AuthorizationCode {
                    client_id: request.client_id,
                    redirect_uri: request.redirect_uri.clone(),
                    code_challenge: request.code_challenge,
                    resource: request.resource,
                    expires_at: Instant::now() + CODE_TTL,
                },
                MAX_CODES,
            )?;
        }
        redirect_success(
            &request.redirect_uri,
            &code,
            request.state.as_deref(),
            &self.public_url,
        )
    }

    pub async fn token(&self, request: TokenRequest) -> std::result::Result<Value, OAuthError> {
        validate_token_request_lengths(&request)?;
        if request.grant_type != "authorization_code" {
            return Err(OAuthError::bad_request(
                "unsupported_grant_type",
                "only authorization_code is supported",
            ));
        }
        self.validate_resource(&request.resource)?;
        let challenge = pkce_challenge(&request.code_verifier)?;
        let token = random_token().map_err(internal_error)?;
        let now = Instant::now();
        {
            let mut state = self.state.lock().await;
            redeem_authorization_code(
                &mut state,
                &request,
                &challenge,
                token.clone(),
                now,
                MAX_TOKENS,
            )?;
        }
        Ok(json!({
            "access_token": token,
            "token_type": "Bearer",
            "expires_in": TOKEN_TTL.as_secs(),
            "scope": "mcp",
        }))
    }

    async fn resolve_client(
        &self,
        client_id: &str,
        redirect_uri: &str,
    ) -> std::result::Result<ClientRegistration, OAuthError> {
        {
            let mut state = self.state.lock().await;
            cleanup(&mut state);
            if let Some(client) = state.clients.get_mut(client_id) {
                client.last_used_at = Instant::now();
                return Ok(client.clone());
            }
        }
        self.fetch_client_metadata(client_id, redirect_uri).await
    }

    async fn fetch_client_metadata(
        &self,
        client_id: &str,
        redirect_uri: &str,
    ) -> std::result::Result<ClientRegistration, OAuthError> {
        let url = Url::parse(client_id).map_err(|_| {
            OAuthError::bad_request(
                "unauthorized_client",
                "client_id is neither a registered client nor a valid HTTPS metadata document URL",
            )
        })?;
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || url.port_or_known_default() != Some(443)
        {
            return Err(OAuthError::bad_request(
                "unauthorized_client",
                "client metadata document client_id must be an HTTPS URL on port 443 without credentials or fragment",
            ));
        }
        let host = url.host_str().ok_or_else(|| {
            OAuthError::bad_request("unauthorized_client", "client metadata URL has no host")
        })?;
        if host.parse::<IpAddr>().is_ok() || host.eq_ignore_ascii_case("localhost") {
            return Err(OAuthError::bad_request(
                "unauthorized_client",
                "client metadata document host must be a public DNS name",
            ));
        }
        let _metadata_permit = self.acquire_metadata_fetch_slot()?;

        let resolved =
            tokio::time::timeout(METADATA_DNS_TIMEOUT, tokio::net::lookup_host((host, 443)))
                .await
                .map_err(|_| {
                    OAuthError::bad_request(
                        "unauthorized_client",
                        "timed out resolving client metadata host",
                    )
                })?
                .map_err(|error| {
                    OAuthError::bad_request(
                        "unauthorized_client",
                        format!("cannot resolve client metadata host: {error}"),
                    )
                })?;
        let addresses = resolved
            .take(MAX_CLIENT_METADATA_ADDRESSES + 1)
            .collect::<Vec<_>>();
        if addresses.len() > MAX_CLIENT_METADATA_ADDRESSES {
            return Err(OAuthError::bad_request(
                "unauthorized_client",
                "client metadata host resolves to too many addresses",
            ));
        }
        if addresses.is_empty() || addresses.iter().any(|address| !is_public_ip(address.ip())) {
            return Err(OAuthError::bad_request(
                "unauthorized_client",
                "client metadata host resolves to a non-public address",
            ));
        }
        let pinned = SocketAddr::new(addresses[0].ip(), 443);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .resolve(host, pinned)
            .user_agent(format!("temote-mcp/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| {
                OAuthError::bad_request(
                    "unauthorized_client",
                    format!("cannot create client metadata fetcher: {error}"),
                )
            })?;
        let response = client.get(client_id).send().await.map_err(|error| {
            OAuthError::bad_request(
                "unauthorized_client",
                format!("cannot fetch client metadata document: {error}"),
            )
        })?;
        if !response.status().is_success() {
            return Err(OAuthError::bad_request(
                "unauthorized_client",
                format!(
                    "client metadata document returned HTTP {}",
                    response.status()
                ),
            ));
        }
        let bytes = read_client_metadata_body(response).await?;
        let metadata: ClientMetadataDocument = serde_json::from_slice(&bytes).map_err(|error| {
            OAuthError::bad_request(
                "unauthorized_client",
                format!("client metadata document is invalid JSON: {error}"),
            )
        })?;
        if metadata.client_id != client_id {
            return Err(OAuthError::bad_request(
                "unauthorized_client",
                "client metadata document client_id does not exactly match its URL",
            ));
        }
        if metadata.redirect_uris.is_empty() || metadata.redirect_uris.len() > MAX_REDIRECT_URIS {
            return Err(OAuthError::bad_request(
                "unauthorized_client",
                format!(
                    "client metadata redirect_uris must contain 1..={MAX_REDIRECT_URIS} entries"
                ),
            ));
        }
        if metadata
            .token_endpoint_auth_method
            .as_deref()
            .is_some_and(|value| value != "none")
            || metadata
                .grant_types
                .iter()
                .any(|value| value != "authorization_code")
            || metadata.response_types.iter().any(|value| value != "code")
        {
            return Err(OAuthError::bad_request(
                "unauthorized_client",
                "client metadata requests unsupported OAuth capabilities",
            ));
        }
        let mut redirect_uris = HashSet::with_capacity(metadata.redirect_uris.len());
        for uri in metadata.redirect_uris {
            validate_redirect_uri(&uri).map_err(|error| OAuthError {
                status: error.status,
                code: "unauthorized_client",
                description: error.description,
            })?;
            redirect_uris.insert(uri);
        }
        if !redirect_uris.contains(redirect_uri) {
            return Err(OAuthError::bad_request(
                "invalid_request",
                "redirect_uri does not exactly match the client metadata document",
            ));
        }
        let name = metadata
            .client_name
            .unwrap_or_else(|| client_id.to_owned())
            .chars()
            .take(200)
            .collect();
        Ok(ClientRegistration {
            name,
            redirect_uris,
            last_used_at: Instant::now(),
        })
    }

    pub async fn authenticate(&self, headers: &HeaderMap) -> Result<Identity> {
        let value = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .context("missing Authorization header")?;
        let (scheme, token) = value
            .split_once(' ')
            .context("invalid Authorization header")?;
        anyhow::ensure!(
            scheme.eq_ignore_ascii_case("Bearer"),
            "Authorization scheme must be Bearer"
        );
        let token = token.trim();
        validate_bearer_token(token)?;

        let mut state = self.state.lock().await;
        cleanup(&mut state);
        let access = state
            .tokens
            .get(token)
            .context("Bearer token is invalid or expired")?;
        anyhow::ensure!(
            access.expires_at > Instant::now(),
            "Bearer token is expired"
        );
        anyhow::ensure!(
            access.resource == self.resource,
            "Bearer token resource is invalid"
        );
        Ok(Identity {
            provider: "local-oauth",
            subject: Some(access.subject.clone()),
            display_principal: Some(format!("local owner via {}", access.client_id)),
            email: None,
        })
    }

    fn acquire_metadata_fetch_slot(&self) -> std::result::Result<OwnedSemaphorePermit, OAuthError> {
        self.metadata_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                OAuthError::bad_request(
                    "temporarily_unavailable",
                    "too many client metadata fetches are in progress",
                )
            })
    }

    fn validate_resource(&self, resource: &str) -> std::result::Result<(), OAuthError> {
        if resource != self.resource {
            return Err(OAuthError::bad_request(
                "invalid_target",
                "resource must exactly match this MCP endpoint",
            ));
        }
        Ok(())
    }
}

async fn read_client_metadata_body(
    mut response: reqwest::Response,
) -> std::result::Result<Vec<u8>, OAuthError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_CLIENT_METADATA_BYTES as u64)
    {
        return Err(client_metadata_too_large());
    }
    let capacity = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or_default()
        .min(MAX_CLIENT_METADATA_BYTES);
    let mut body = Vec::with_capacity(capacity);
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        OAuthError::bad_request(
            "unauthorized_client",
            format!("cannot read client metadata document: {error}"),
        )
    })? {
        append_bounded_metadata_chunk(&mut body, &chunk, MAX_CLIENT_METADATA_BYTES)?;
    }
    Ok(body)
}

fn append_bounded_metadata_chunk(
    body: &mut Vec<u8>,
    chunk: &[u8],
    max_bytes: usize,
) -> std::result::Result<(), OAuthError> {
    if body.len().saturating_add(chunk.len()) > max_bytes {
        return Err(client_metadata_too_large());
    }
    body.extend_from_slice(chunk);
    Ok(())
}

fn client_metadata_too_large() -> OAuthError {
    OAuthError::bad_request(
        "unauthorized_client",
        "client metadata document is too large",
    )
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_ipv4(ip),
        IpAddr::V6(ip) => is_public_ipv6(ip),
    }
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _d] = ip.octets();
    if a == 0
        || a == 10
        || a == 127
        || a >= 224
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 192 && b == 88 && c == 99)
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
    {
        return false;
    }
    true
}

fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(ipv4) = ip.to_ipv4() {
        return is_public_ipv4(ipv4);
    }
    let segments = ip.segments();

    // RFC 6052 well-known NAT64 prefix. Preserve IPv4 SSRF policy for the
    // embedded address rather than treating the translator prefix as public.
    if segments[0] == 0x0064
        && segments[1] == 0xff9b
        && segments[2..6].iter().all(|segment| *segment == 0)
    {
        let ipv4 = Ipv4Addr::new(
            (segments[6] >> 8) as u8,
            segments[6] as u8,
            (segments[7] >> 8) as u8,
            segments[7] as u8,
        );
        return is_public_ipv4(ipv4);
    }

    let local_nat64 = segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2] == 0x0001;
    let discard_only = segments[0] == 0x0100
        && segments[1] == 0
        && segments[2] == 0
        && matches!(segments[3], 0 | 1);
    let ietf_protocol_space = segments[0] == 0x2001 && (segments[1] & 0xfe00) == 0;
    let six_to_four = segments[0] == 0x2002;
    let documentation_3fff = segments[0] == 0x3fff && (segments[1] & 0xf000) == 0;
    let srv6_sid = segments[0] == 0x5f00;

    if ip.is_unspecified()
        || ip.is_loopback()
        || (segments[0] & 0xff00) == 0xff00
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] & 0xffc0) == 0xfec0
        || local_nat64
        || discard_only
        || ietf_protocol_space
        || six_to_four
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        || documentation_3fff
        || srv6_sid
    {
        return false;
    }
    true
}

fn insert_client_registration(
    state: &mut State,
    client_id: String,
    registration: ClientRegistration,
    max_clients: usize,
) -> std::result::Result<(), OAuthError> {
    if state.clients.contains_key(&client_id) {
        return Err(internal_error(anyhow::anyhow!(
            "generated duplicate local OAuth client id"
        )));
    }
    if !make_client_capacity_available(state, max_clients) {
        return Err(OAuthError::bad_request(
            "temporarily_unavailable",
            "local OAuth client registry is full of active registrations",
        ));
    }
    state.clients.insert(client_id, registration);
    Ok(())
}

fn insert_authorization_code(
    state: &mut State,
    code: String,
    authorization: AuthorizationCode,
    max_codes: usize,
) -> std::result::Result<(), OAuthError> {
    if state.codes.contains_key(&code) {
        return Err(internal_error(anyhow::anyhow!(
            "generated duplicate local OAuth authorization code"
        )));
    }
    if state.codes.len() >= max_codes {
        return Err(OAuthError::bad_request(
            "temporarily_unavailable",
            "too many pending authorization codes",
        ));
    }
    state.codes.insert(code, authorization);
    Ok(())
}

fn make_client_capacity_available(state: &mut State, max_clients: usize) -> bool {
    if max_clients == 0 {
        return false;
    }
    while state.clients.len() >= max_clients {
        let candidate = state
            .clients
            .iter()
            .filter(|(client_id, _)| {
                !state
                    .codes
                    .values()
                    .any(|code| code.client_id.as_str() == client_id.as_str())
                    && !state
                        .tokens
                        .values()
                        .any(|token| token.client_id.as_str() == client_id.as_str())
            })
            .min_by(|(left_id, left), (right_id, right)| {
                left.last_used_at
                    .cmp(&right.last_used_at)
                    .then_with(|| left_id.cmp(right_id))
            })
            .map(|(client_id, _)| client_id.clone());
        let Some(client_id) = candidate else {
            return false;
        };
        state.clients.remove(&client_id);
    }
    true
}

fn cleanup(state: &mut State) {
    cleanup_at(state, Instant::now());
}

fn cleanup_at(state: &mut State, now: Instant) {
    state.codes.retain(|_, code| code.expires_at > now);
    state.tokens.retain(|_, token| token.expires_at > now);
}

fn redeem_authorization_code(
    state: &mut State,
    request: &TokenRequest,
    challenge: &str,
    token: String,
    now: Instant,
    token_limit: usize,
) -> std::result::Result<(), OAuthError> {
    cleanup_at(state, now);
    if state.tokens.len() >= token_limit {
        return Err(OAuthError::bad_request(
            "temporarily_unavailable",
            "too many active local OAuth tokens",
        ));
    }

    // A temporary capacity failure above does not consume the code, so the client can retry.
    // A binding failure does consume it, preventing verifier/redirect probing and replay.
    let binding_valid = {
        let code = state.codes.get(&request.code).ok_or_else(|| {
            OAuthError::bad_request(
                "invalid_grant",
                "authorization code is invalid, expired, or already used",
            )
        })?;
        code.expires_at > now
            && code.client_id == request.client_id
            && code.redirect_uri == request.redirect_uri
            && code.resource == request.resource
            && code.code_challenge == challenge
    };
    if !binding_valid {
        state.codes.remove(&request.code);
        return Err(OAuthError::bad_request(
            "invalid_grant",
            "authorization code binding or PKCE verification failed",
        ));
    }
    // A generated-token collision is an internal, retryable failure. Do not consume a
    // valid authorization code until a unique token can be committed atomically.
    if state.tokens.contains_key(&token) {
        return Err(internal_error(anyhow::anyhow!(
            "generated duplicate local OAuth access token"
        )));
    }
    state.codes.remove(&request.code);
    state.tokens.insert(
        token,
        AccessToken {
            client_id: request.client_id.clone(),
            subject: "local-owner".to_owned(),
            resource: request.resource.clone(),
            expires_at: now + TOKEN_TTL,
        },
    );
    Ok(())
}

fn validate_registration_request_bounds(
    request: &RegistrationRequest,
) -> std::result::Result<(), OAuthError> {
    if let Some(name) = request.client_name.as_deref() {
        validate_client_metadata_length(name, MAX_OAUTH_CLIENT_NAME_BYTES, "client_name")?;
    }
    for (name, value) in [
        ("application_type", request.application_type.as_deref()),
        (
            "token_endpoint_auth_method",
            request.token_endpoint_auth_method.as_deref(),
        ),
    ] {
        if let Some(value) = value {
            validate_client_metadata_length(value, MAX_OAUTH_FIXED_PARAMETER_BYTES, name)?;
        }
    }
    for (name, values) in [
        ("grant_types", &request.grant_types),
        ("response_types", &request.response_types),
    ] {
        if values.len() > MAX_OAUTH_CAPABILITY_VALUES {
            return Err(OAuthError::bad_request(
                "invalid_client_metadata",
                format!("{name} contains too many values"),
            ));
        }
        for value in values {
            validate_client_metadata_length(value, MAX_OAUTH_FIXED_PARAMETER_BYTES, name)?;
        }
    }
    Ok(())
}

fn validate_client_metadata_length(
    value: &str,
    max_bytes: usize,
    name: &str,
) -> std::result::Result<(), OAuthError> {
    if value.len() > max_bytes {
        return Err(OAuthError::bad_request(
            "invalid_client_metadata",
            format!("{name} exceeds {max_bytes} bytes"),
        ));
    }
    Ok(())
}

fn validate_parameter_length(
    value: &str,
    max_bytes: usize,
    name: &str,
) -> std::result::Result<(), OAuthError> {
    if value.len() > max_bytes {
        return Err(OAuthError::bad_request(
            "invalid_request",
            format!("{name} exceeds {max_bytes} bytes"),
        ));
    }
    Ok(())
}

fn validate_authorize_request_lengths(
    request: &AuthorizeRequest,
) -> std::result::Result<(), OAuthError> {
    validate_parameter_length(
        &request.response_type,
        MAX_OAUTH_FIXED_PARAMETER_BYTES,
        "response_type",
    )?;
    validate_parameter_length(&request.client_id, MAX_URI_BYTES, "client_id")?;
    validate_parameter_length(&request.redirect_uri, MAX_URI_BYTES, "redirect_uri")?;
    validate_parameter_length(
        &request.code_challenge,
        MAX_OAUTH_TOKEN_VALUE_BYTES,
        "code_challenge",
    )?;
    validate_parameter_length(
        &request.code_challenge_method,
        MAX_OAUTH_FIXED_PARAMETER_BYTES,
        "code_challenge_method",
    )?;
    validate_parameter_length(&request.resource, MAX_URI_BYTES, "resource")?;
    if let Some(state) = request.state.as_deref() {
        validate_parameter_length(state, MAX_OAUTH_STATE_BYTES, "state")?;
    }
    if let Some(scope) = request.scope.as_deref() {
        validate_parameter_length(scope, MAX_OAUTH_SCOPE_BYTES, "scope")?;
    }
    Ok(())
}

fn validate_token_request_lengths(request: &TokenRequest) -> std::result::Result<(), OAuthError> {
    validate_parameter_length(
        &request.grant_type,
        MAX_OAUTH_FIXED_PARAMETER_BYTES,
        "grant_type",
    )?;
    validate_parameter_length(&request.code, MAX_OAUTH_TOKEN_VALUE_BYTES, "code")?;
    validate_parameter_length(&request.client_id, MAX_URI_BYTES, "client_id")?;
    validate_parameter_length(&request.redirect_uri, MAX_URI_BYTES, "redirect_uri")?;
    validate_parameter_length(
        &request.code_verifier,
        MAX_OAUTH_TOKEN_VALUE_BYTES,
        "code_verifier",
    )?;
    validate_parameter_length(&request.resource, MAX_URI_BYTES, "resource")?;
    Ok(())
}

fn validate_scope(scope: Option<&str>) -> std::result::Result<(), OAuthError> {
    if scope.is_some_and(|scope| scope.split_ascii_whitespace().any(|value| value != "mcp")) {
        return Err(OAuthError::bad_request(
            "invalid_scope",
            "only the mcp scope is supported",
        ));
    }
    Ok(())
}

fn validate_redirect_uri(value: &str) -> std::result::Result<(), OAuthError> {
    if value.len() > MAX_URI_BYTES {
        return Err(OAuthError::bad_request(
            "invalid_client_metadata",
            "redirect URI is too long",
        ));
    }
    let parsed = Url::parse(value).map_err(|_| {
        OAuthError::bad_request("invalid_client_metadata", "redirect URI is invalid")
    })?;
    if parsed.fragment().is_some() || !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(OAuthError::bad_request(
            "invalid_client_metadata",
            "redirect URI must not contain credentials or a fragment",
        ));
    }
    let secure = parsed.scheme() == "https";
    let loopback_http = parsed.scheme() == "http"
        && parsed.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
        });
    if !secure && !loopback_http {
        return Err(OAuthError::bad_request(
            "invalid_client_metadata",
            "redirect URI must use HTTPS, except HTTP loopback redirects",
        ));
    }
    Ok(())
}

fn validate_code_challenge(value: &str) -> std::result::Result<(), OAuthError> {
    let valid = value.len() == 43
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    if !valid {
        return Err(OAuthError::bad_request(
            "invalid_request",
            "code_challenge must be an unpadded base64url SHA-256 value",
        ));
    }
    Ok(())
}

fn validate_bearer_token(token: &str) -> Result<()> {
    anyhow::ensure!(!token.is_empty(), "Bearer token is empty");
    anyhow::ensure!(
        token.len() <= MAX_OAUTH_TOKEN_VALUE_BYTES,
        "Bearer token exceeds {MAX_OAUTH_TOKEN_VALUE_BYTES} bytes"
    );
    Ok(())
}

fn pkce_challenge(verifier: &str) -> std::result::Result<String, OAuthError> {
    let valid = (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~'));
    if !valid {
        return Err(OAuthError::bad_request(
            "invalid_grant",
            "code_verifier is invalid",
        ));
    }
    Ok(URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())))
}

fn random_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("secure random generation failed: {error}"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn internal_error(error: anyhow::Error) -> OAuthError {
    OAuthError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: "server_error",
        description: format!("local OAuth operation failed: {error:#}"),
    }
}

fn redirect_success(
    redirect_uri: &str,
    code: &str,
    state: Option<&str>,
    issuer: &str,
) -> std::result::Result<String, OAuthError> {
    redirect_with_params(
        redirect_uri,
        [
            ("code", Some(code)),
            ("state", state),
            ("iss", Some(issuer)),
        ],
    )
}

fn redirect_error(
    redirect_uri: &str,
    error: &str,
    state: Option<&str>,
    issuer: &str,
) -> std::result::Result<String, OAuthError> {
    redirect_with_params(
        redirect_uri,
        [
            ("error", Some(error)),
            ("state", state),
            ("iss", Some(issuer)),
        ],
    )
}

fn redirect_with_params<const N: usize>(
    redirect_uri: &str,
    params: [(&str, Option<&str>); N],
) -> std::result::Result<String, OAuthError> {
    let mut url = Url::parse(redirect_uri)
        .map_err(|_| OAuthError::bad_request("invalid_request", "redirect URI is invalid"))?;
    {
        let mut query = url.query_pairs_mut();
        for (name, value) in params {
            if let Some(value) = value {
                query.append_pair(name, value);
            }
        }
    }
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approvals;
    use crate::test_support;

    fn oauth() -> (LocalOAuth, approvals::ApprovalReceiver) {
        let (sender, receiver) = approvals::approval_channel();
        (
            LocalOAuth::new("https://node.example.ts.net".to_owned(), sender),
            receiver,
        )
    }

    async fn registered(oauth: &LocalOAuth) -> String {
        oauth
            .register(RegistrationRequest {
                redirect_uris: vec!["http://127.0.0.1:9876/callback".to_owned()],
                client_name: Some("test client".to_owned()),
                application_type: Some("native".to_owned()),
                grant_types: vec!["authorization_code".to_owned()],
                response_types: vec!["code".to_owned()],
                token_endpoint_auth_method: Some("none".to_owned()),
            })
            .await
            .unwrap()["client_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn generated_registration_bounds_match_reference_model() -> noprop::TestResult {
        crate::test_support::run(0x4f41_5554_4852_4547, 512, |ctx| {
            let field = noprop::sample_usize_in(ctx, 0..5);
            let mut request = RegistrationRequest {
                redirect_uris: vec!["http://127.0.0.1:9876/callback".to_owned()],
                client_name: Some("client".to_owned()),
                application_type: Some("native".to_owned()),
                grant_types: vec!["authorization_code".to_owned()],
                response_types: vec!["code".to_owned()],
                token_endpoint_auth_method: Some("none".to_owned()),
            };
            let expected = match field {
                0 => {
                    let len = noprop::sample_usize_in(
                        ctx,
                        MAX_OAUTH_CLIENT_NAME_BYTES.saturating_sub(2)
                            ..=MAX_OAUTH_CLIENT_NAME_BYTES + 2,
                    );
                    request.client_name = Some("x".repeat(len));
                    len <= MAX_OAUTH_CLIENT_NAME_BYTES
                }
                1 => {
                    let len = noprop::sample_usize_in(
                        ctx,
                        MAX_OAUTH_FIXED_PARAMETER_BYTES.saturating_sub(2)
                            ..=MAX_OAUTH_FIXED_PARAMETER_BYTES + 2,
                    );
                    request.application_type = Some("x".repeat(len));
                    len <= MAX_OAUTH_FIXED_PARAMETER_BYTES
                }
                2 => {
                    let len = noprop::sample_usize_in(
                        ctx,
                        MAX_OAUTH_FIXED_PARAMETER_BYTES.saturating_sub(2)
                            ..=MAX_OAUTH_FIXED_PARAMETER_BYTES + 2,
                    );
                    request.token_endpoint_auth_method = Some("x".repeat(len));
                    len <= MAX_OAUTH_FIXED_PARAMETER_BYTES
                }
                3 => {
                    let count = noprop::sample_usize_in(
                        ctx,
                        MAX_OAUTH_CAPABILITY_VALUES.saturating_sub(2)
                            ..=MAX_OAUTH_CAPABILITY_VALUES + 2,
                    );
                    request.grant_types = vec!["authorization_code".to_owned(); count];
                    count <= MAX_OAUTH_CAPABILITY_VALUES
                }
                _ => {
                    let count = noprop::sample_usize_in(
                        ctx,
                        MAX_OAUTH_CAPABILITY_VALUES.saturating_sub(2)
                            ..=MAX_OAUTH_CAPABILITY_VALUES + 2,
                    );
                    request.response_types = vec!["code".to_owned(); count];
                    count <= MAX_OAUTH_CAPABILITY_VALUES
                }
            };
            assert_eq!(
                validate_registration_request_bounds(&request).is_ok(),
                expected,
                "field={field}"
            );
            Ok(())
        })
    }

    #[test]
    fn generated_authorize_parameter_lengths_match_bounds() -> noprop::TestResult {
        crate::test_support::run(0x4f41_5554_484c_454e, 512, |ctx| {
            let field = noprop::sample_usize_in(ctx, 0..8);
            let max = match field {
                0 | 4 => MAX_OAUTH_FIXED_PARAMETER_BYTES,
                1 | 2 | 5 => MAX_URI_BYTES,
                3 => MAX_OAUTH_TOKEN_VALUE_BYTES,
                6 => MAX_OAUTH_STATE_BYTES,
                _ => MAX_OAUTH_SCOPE_BYTES,
            };
            let len = noprop::sample_usize_in(ctx, max.saturating_sub(2)..=max + 2);
            let value = "x".repeat(len);
            let mut request = AuthorizeRequest {
                response_type: "code".to_owned(),
                client_id: "client".to_owned(),
                redirect_uri: "http://127.0.0.1:9876/callback".to_owned(),
                code_challenge: "A".repeat(43),
                code_challenge_method: "S256".to_owned(),
                resource: "https://node.example.ts.net/mcp".to_owned(),
                state: None,
                scope: Some("mcp".to_owned()),
            };
            match field {
                0 => request.response_type = value,
                1 => request.client_id = value,
                2 => request.redirect_uri = value,
                3 => request.code_challenge = value,
                4 => request.code_challenge_method = value,
                5 => request.resource = value,
                6 => request.state = Some(value),
                _ => request.scope = Some(value),
            }
            assert_eq!(
                validate_authorize_request_lengths(&request).is_ok(),
                len <= max,
                "field={field} len={len} max={max}"
            );
            Ok(())
        })
    }

    #[test]
    fn generated_token_parameter_lengths_match_bounds() -> noprop::TestResult {
        crate::test_support::run(0x4f41_5554_4854_4c45, 512, |ctx| {
            let field = noprop::sample_usize_in(ctx, 0..6);
            let max = match field {
                0 => MAX_OAUTH_FIXED_PARAMETER_BYTES,
                1 | 4 => MAX_OAUTH_TOKEN_VALUE_BYTES,
                _ => MAX_URI_BYTES,
            };
            let len = noprop::sample_usize_in(ctx, max.saturating_sub(2)..=max + 2);
            let value = "x".repeat(len);
            let mut request = TokenRequest {
                grant_type: "authorization_code".to_owned(),
                code: "code".to_owned(),
                client_id: "client".to_owned(),
                redirect_uri: "http://127.0.0.1:9876/callback".to_owned(),
                code_verifier: "a".repeat(43),
                resource: "https://node.example.ts.net/mcp".to_owned(),
            };
            match field {
                0 => request.grant_type = value,
                1 => request.code = value,
                2 => request.client_id = value,
                3 => request.redirect_uri = value,
                4 => request.code_verifier = value,
                _ => request.resource = value,
            }
            assert_eq!(
                validate_token_request_lengths(&request).is_ok(),
                len <= max,
                "field={field} len={len} max={max}"
            );
            Ok(())
        })
    }

    #[test]
    fn redirect_validation_is_exact_and_https_or_loopback_only() {
        assert!(validate_redirect_uri("https://client.example/callback").is_ok());
        assert!(validate_redirect_uri("http://127.0.0.1:1234/callback").is_ok());
        assert!(validate_redirect_uri("http://localhost:1234/callback").is_ok());
        assert!(validate_redirect_uri("http://client.example/callback").is_err());
        assert!(validate_redirect_uri("https://client.example/callback#fragment").is_err());
    }

    #[test]
    fn pkce_s256_matches_rfc7636_example() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_challenge(verifier).unwrap(),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[tokio::test]
    async fn code_is_single_use_and_bound_to_redirect_client_resource_and_pkce() {
        let (oauth, mut approvals) = oauth();
        let client_id = registered(&oauth).await;
        let verifier = "a".repeat(43);
        let challenge = pkce_challenge(&verifier).unwrap();
        let authorize = AuthorizeRequest {
            response_type: "code".to_owned(),
            client_id: client_id.clone(),
            redirect_uri: "http://127.0.0.1:9876/callback".to_owned(),
            code_challenge: challenge,
            code_challenge_method: "S256".to_owned(),
            resource: "https://node.example.ts.net/mcp".to_owned(),
            state: Some("opaque-state".to_owned()),
            scope: Some("mcp".to_owned()),
        };
        let authorize_task = {
            let oauth = oauth.clone();
            tokio::spawn(async move { oauth.authorize(authorize).await })
        };
        let prompt = approvals.recv().await.unwrap();
        assert_eq!(prompt.request.operation, "oauth_authorize");
        prompt.respond(true);
        let redirect = authorize_task.await.unwrap().unwrap();
        let redirect = Url::parse(&redirect).unwrap();
        let params = redirect
            .query_pairs()
            .into_owned()
            .collect::<HashMap<_, _>>();
        assert_eq!(
            params.get("state").map(String::as_str),
            Some("opaque-state")
        );
        assert_eq!(
            params.get("iss").map(String::as_str),
            Some("https://node.example.ts.net")
        );
        let code = params.get("code").unwrap().clone();

        let token = oauth
            .token(TokenRequest {
                grant_type: "authorization_code".to_owned(),
                code: code.clone(),
                client_id: client_id.clone(),
                redirect_uri: "http://127.0.0.1:9876/callback".to_owned(),
                code_verifier: verifier.clone(),
                resource: "https://node.example.ts.net/mcp".to_owned(),
            })
            .await
            .unwrap();
        assert_eq!(token["token_type"], "Bearer");

        let replay = oauth
            .token(TokenRequest {
                grant_type: "authorization_code".to_owned(),
                code,
                client_id,
                redirect_uri: "http://127.0.0.1:9876/callback".to_owned(),
                code_verifier: verifier,
                resource: "https://node.example.ts.net/mcp".to_owned(),
            })
            .await
            .unwrap_err();
        assert_eq!(replay.code, "invalid_grant");
    }

    #[test]
    fn client_metadata_ssrf_guard_accepts_public_and_rejects_private_addresses() {
        assert!(is_public_ip("8.8.8.8".parse().unwrap()));
        assert!(is_public_ip("2606:4700:4700::1111".parse().unwrap()));
        for value in [
            "127.0.0.1",
            "10.0.0.1",
            "100.64.0.1",
            "169.254.1.1",
            "172.16.0.1",
            "192.168.0.1",
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "64:ff9b::7f00:1",
            "64:ff9b:1::1",
            "100::1",
            "100:0:0:1::1",
            "2001:2::1",
            "2001:db8::1",
            "2002:0808:0808::1",
            "3fff::1",
            "5f00::1",
        ] {
            assert!(!is_public_ip(value.parse().unwrap()), "accepted {value}");
        }
    }

    #[test]
    fn generated_metadata_chunk_budget_never_overreads() -> noprop::TestResult {
        crate::test_support::run(0x4f41_5554_4842_4f44, 1024, |ctx| {
            let limit = noprop::sample_usize_in(ctx, 0..=1024);
            let chunk_count = noprop::sample_usize_in(ctx, 0..=16);
            let chunk_lengths = (0..chunk_count)
                .map(|_| noprop::sample_usize_in(ctx, 0..=256))
                .collect::<Vec<_>>();
            let mut body = Vec::new();
            let mut accepted = 0usize;
            let mut rejected = false;

            for length in chunk_lengths {
                let before = body.len();
                let chunk = vec![b'x'; length];
                let result = append_bounded_metadata_chunk(&mut body, &chunk, limit);
                if accepted.saturating_add(length) <= limit {
                    result.unwrap();
                    accepted += length;
                    assert_eq!(body.len(), accepted);
                } else {
                    assert!(result.is_err());
                    assert_eq!(body.len(), before, "rejected chunk was partially retained");
                    rejected = true;
                    break;
                }
                assert!(body.len() <= limit);
            }
            assert_eq!(body.len(), accepted);
            if rejected {
                assert!(body.len() <= limit);
            }
            Ok(())
        })
    }

    #[test]
    fn generated_well_known_nat64_inherits_ipv4_publicness() -> noprop::TestResult {
        crate::test_support::run(0x4f41_5554_484e_4154, 1024, |ctx| {
            let raw = noprop::sample_u32(ctx);
            let octets = raw.to_be_bytes();
            let ipv4 = Ipv4Addr::from(octets);
            let ipv6 = Ipv6Addr::new(
                0x0064,
                0xff9b,
                0,
                0,
                0,
                0,
                u16::from_be_bytes([octets[0], octets[1]]),
                u16::from_be_bytes([octets[2], octets[3]]),
            );
            assert_eq!(
                is_public_ipv6(ipv6),
                is_public_ipv4(ipv4),
                "NAT64 classification diverged for {ipv4} via {ipv6}"
            );
            Ok(())
        })
    }

    #[test]
    fn generated_bearer_token_lengths_match_authentication_bound() -> noprop::TestResult {
        test_support::run(0x4f41_5554_4842_4541, 512, |ctx| {
            let length = match noprop::sample_usize_in(ctx, 0..=5) {
                0 => 0,
                1 => 1,
                2 => MAX_OAUTH_TOKEN_VALUE_BYTES - 1,
                3 => MAX_OAUTH_TOKEN_VALUE_BYTES,
                4 => MAX_OAUTH_TOKEN_VALUE_BYTES + 1,
                _ => noprop::sample_usize_in(ctx, 0..=MAX_OAUTH_TOKEN_VALUE_BYTES + 64),
            };
            let token = "t".repeat(length);
            assert_eq!(
                validate_bearer_token(&token).is_ok(),
                length > 0 && length <= MAX_OAUTH_TOKEN_VALUE_BYTES,
                "length={length}"
            );
            Ok(())
        })
    }

    #[tokio::test]
    async fn oversized_bearer_token_is_rejected_before_lookup() {
        let (oauth, _approvals) = oauth();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {}", "t".repeat(MAX_OAUTH_TOKEN_VALUE_BYTES + 1))
                .parse()
                .unwrap(),
        );
        let error = oauth.authenticate(&headers).await.unwrap_err().to_string();
        assert!(error.contains("exceeds"));
        assert!(oauth.state.lock().await.tokens.is_empty());
    }

    #[tokio::test]
    async fn expired_and_wrong_resource_bearer_tokens_are_rejected() {
        let (oauth, _approvals) = oauth();
        {
            let mut state = oauth.state.lock().await;
            state.tokens.insert(
                "expired-token".to_owned(),
                AccessToken {
                    client_id: "client".to_owned(),
                    subject: "local-owner".to_owned(),
                    resource: "https://node.example.ts.net/mcp".to_owned(),
                    expires_at: Instant::now() - Duration::from_secs(1),
                },
            );
            state.tokens.insert(
                "wrong-resource-token".to_owned(),
                AccessToken {
                    client_id: "client".to_owned(),
                    subject: "local-owner".to_owned(),
                    resource: "https://other.example/mcp".to_owned(),
                    expires_at: Instant::now() + Duration::from_secs(60),
                },
            );
        }

        for token in ["expired-token", "wrong-resource-token"] {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::AUTHORIZATION,
                format!("Bearer {token}").parse().unwrap(),
            );
            assert!(
                oauth.authenticate(&headers).await.is_err(),
                "accepted {token}"
            );
        }
    }

    #[tokio::test]
    async fn registered_redirect_uri_requires_exact_match_before_approval() {
        let (oauth, mut approvals) = oauth();
        let client_id = registered(&oauth).await;
        let request = AuthorizeRequest {
            response_type: "code".to_owned(),
            client_id,
            redirect_uri: "http://127.0.0.1:9876/callback?unexpected=1".to_owned(),
            code_challenge: "A".repeat(43),
            code_challenge_method: "S256".to_owned(),
            resource: "https://node.example.ts.net/mcp".to_owned(),
            state: None,
            scope: Some("mcp".to_owned()),
        };
        let error = oauth.authorize(request).await.unwrap_err();
        assert_eq!(error.code, "invalid_request");
        assert!(approvals.try_recv().is_err());
    }

    #[test]
    fn generated_redirect_variants_match_only_exact_registration() -> noprop::TestResult {
        crate::test_support::run(0x4f41_5554_4852_4544, 256, |ctx| {
            let host = format!("{}.example.test", crate::test_support::safe_component(ctx));
            let registered = format!("https://{host}/callback");
            let candidate = match noprop::sample_usize_in(ctx, 0..=5) {
                0 => registered.clone(),
                1 => format!("{registered}/"),
                2 => format!("{registered}?x=1"),
                3 => format!("https://{host}/Callback"),
                4 => format!("https://{host}:443/callback"),
                _ => format!("https://other-{host}/callback"),
            };
            let allowed = HashSet::from([registered.clone()]);
            assert_eq!(allowed.contains(&candidate), candidate == registered);
            Ok(())
        })
    }

    #[test]
    fn generated_metadata_fetch_slots_stay_bounded() -> noprop::TestResult {
        crate::test_support::run(0x4f41_5554_484d_4554, 256, |ctx| {
            let limit = noprop::sample_usize_in(ctx, 1..=8);
            let releases = noprop::sample_usize_in(ctx, 0..=limit);
            let (sender, _approvals) = approvals::approval_channel();
            let oauth =
                LocalOAuth::with_limits("https://node.example.ts.net".to_owned(), sender, 1, limit);
            let mut permits = Vec::with_capacity(limit);
            for _ in 0..limit {
                permits.push(oauth.acquire_metadata_fetch_slot().unwrap());
            }
            let error = oauth.acquire_metadata_fetch_slot().unwrap_err();
            assert_eq!(error.code, "temporarily_unavailable");

            for _ in 0..releases {
                permits.pop();
            }
            let mut recovered = Vec::with_capacity(releases);
            for _ in 0..releases {
                recovered.push(oauth.acquire_metadata_fetch_slot().unwrap());
            }
            assert!(oauth.acquire_metadata_fetch_slot().is_err());
            drop(recovered);
            drop(permits);
            assert!(oauth.acquire_metadata_fetch_slot().is_ok());
            Ok(())
        })
    }

    #[test]
    fn generated_authorization_approval_queue_stays_bounded() -> noprop::TestResult {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        crate::test_support::run(0x4f41_5554_4841_5051, 32, |ctx| {
            let limit = noprop::sample_usize_in(ctx, 1..=4);
            runtime.block_on(async {
                let (sender, mut approvals) = approvals::approval_channel();
                let oauth = LocalOAuth::with_limits(
                    "https://node.example.ts.net".to_owned(),
                    sender,
                    limit,
                    MAX_PENDING_METADATA_FETCHES,
                );
                let client_id = registered(&oauth).await;
                let make_request = |state: String| AuthorizeRequest {
                    response_type: "code".to_owned(),
                    client_id: client_id.clone(),
                    redirect_uri: "http://127.0.0.1:9876/callback".to_owned(),
                    code_challenge: "A".repeat(43),
                    code_challenge_method: "S256".to_owned(),
                    resource: "https://node.example.ts.net/mcp".to_owned(),
                    state: Some(state),
                    scope: Some("mcp".to_owned()),
                };

                let mut waiting = Vec::with_capacity(limit);
                for index in 0..limit {
                    let oauth = oauth.clone();
                    let request = make_request(format!("waiting-{index}"));
                    waiting.push(tokio::spawn(async move { oauth.authorize(request).await }));
                }
                let mut prompts = Vec::with_capacity(limit);
                for _ in 0..limit {
                    prompts.push(
                        tokio::time::timeout(Duration::from_secs(1), approvals.recv())
                            .await
                            .expect("OAuth approval prompt was not delivered")
                            .expect("approval channel closed unexpectedly"),
                    );
                }

                let excess = oauth
                    .authorize(make_request("excess".to_owned()))
                    .await
                    .unwrap_err();
                assert_eq!(excess.code, "temporarily_unavailable");
                assert!(
                    approvals.try_recv().is_err(),
                    "over-capacity OAuth request reached approval queue"
                );

                for prompt in prompts {
                    prompt.respond(false);
                }
                for task in waiting {
                    let redirect = task.await.unwrap().unwrap();
                    assert!(redirect.contains("error=access_denied"));
                }

                let oauth_for_recovery = oauth.clone();
                let recovery_request = make_request("recovery".to_owned());
                let recovery =
                    tokio::spawn(
                        async move { oauth_for_recovery.authorize(recovery_request).await },
                    );
                let prompt = tokio::time::timeout(Duration::from_secs(1), approvals.recv())
                    .await
                    .expect("OAuth approval capacity did not recover")
                    .expect("approval channel closed unexpectedly");
                prompt.respond(false);
                assert!(
                    recovery
                        .await
                        .unwrap()
                        .unwrap()
                        .contains("error=access_denied")
                );
            });
            Ok(())
        })
    }

    #[test]
    fn generated_client_id_collisions_do_not_evict_existing_registrations() -> noprop::TestResult {
        crate::test_support::run(0x4f41_5554_4843_4c49, 256, |ctx| {
            let max_clients = noprop::sample_usize_in(ctx, 1..=8);
            let collision = noprop::sample_bool(ctx);
            let now = Instant::now();
            let mut state = State::default();
            for index in 0..max_clients {
                state.clients.insert(
                    format!("client-{index}"),
                    ClientRegistration {
                        name: format!("existing-{index}"),
                        redirect_uris: HashSet::from(["http://127.0.0.1:9876/callback".to_owned()]),
                        last_used_at: now + Duration::from_secs(index as u64),
                    },
                );
            }
            let candidate = if collision {
                "client-0".to_owned()
            } else {
                "new-client".to_owned()
            };
            let existing_names = state
                .clients
                .iter()
                .map(|(id, client)| (id.clone(), client.name.clone()))
                .collect::<HashMap<_, _>>();
            let result = insert_client_registration(
                &mut state,
                candidate.clone(),
                ClientRegistration {
                    name: "new-registration".to_owned(),
                    redirect_uris: HashSet::from(["http://127.0.0.1:9876/callback".to_owned()]),
                    last_used_at: now + Duration::from_secs(60),
                },
                max_clients,
            );

            if collision {
                let error = result.unwrap_err();
                assert_eq!(error.code, "server_error");
                assert_eq!(state.clients.len(), max_clients);
                for (id, name) in existing_names {
                    assert_eq!(
                        state.clients.get(&id).map(|client| &client.name),
                        Some(&name)
                    );
                }
            } else {
                result.unwrap();
                assert_eq!(state.clients.len(), max_clients);
                assert_eq!(
                    state
                        .clients
                        .get(&candidate)
                        .map(|client| client.name.as_str()),
                    Some("new-registration")
                );
            }
            Ok(())
        })
    }

    #[test]
    fn generated_authorization_code_collisions_preserve_existing_codes() -> noprop::TestResult {
        crate::test_support::run(0x4f41_5554_4843_4f44, 256, |ctx| {
            let max_codes = noprop::sample_usize_in(ctx, 1..=8);
            let occupancy = noprop::sample_usize_in(ctx, 0..=max_codes);
            let collision = occupancy > 0 && noprop::sample_bool(ctx);
            let now = Instant::now();
            let mut state = State::default();
            for index in 0..occupancy {
                state.codes.insert(
                    format!("code-{index}"),
                    AuthorizationCode {
                        client_id: format!("existing-client-{index}"),
                        redirect_uri: "http://127.0.0.1:9876/callback".to_owned(),
                        code_challenge: "A".repeat(43),
                        resource: "https://node.example.ts.net/mcp".to_owned(),
                        expires_at: now + Duration::from_secs(60),
                    },
                );
            }
            let candidate = if collision {
                "code-0".to_owned()
            } else {
                "new-code".to_owned()
            };
            let old_client = state
                .codes
                .get(&candidate)
                .map(|code| code.client_id.clone());
            let result = insert_authorization_code(
                &mut state,
                candidate.clone(),
                AuthorizationCode {
                    client_id: "new-client".to_owned(),
                    redirect_uri: "http://127.0.0.1:9876/callback".to_owned(),
                    code_challenge: "B".repeat(43),
                    resource: "https://node.example.ts.net/mcp".to_owned(),
                    expires_at: now + Duration::from_secs(60),
                },
                max_codes,
            );

            if collision {
                let error = result.unwrap_err();
                assert_eq!(error.code, "server_error");
                assert_eq!(state.codes.len(), occupancy);
                assert_eq!(
                    state.codes.get(&candidate).map(|code| &code.client_id),
                    old_client.as_ref()
                );
            } else if occupancy == max_codes {
                let error = result.unwrap_err();
                assert_eq!(error.code, "temporarily_unavailable");
                assert_eq!(state.codes.len(), occupancy);
                assert!(!state.codes.contains_key(&candidate));
            } else {
                result.unwrap();
                assert_eq!(state.codes.len(), occupancy + 1);
                assert_eq!(
                    state
                        .codes
                        .get(&candidate)
                        .map(|code| code.client_id.as_str()),
                    Some("new-client")
                );
            }
            Ok(())
        })
    }

    #[test]
    fn generated_client_capacity_evicts_oldest_inactive_registration() -> noprop::TestResult {
        crate::test_support::run(0x4f41_5554_4843_4c49, 256, |ctx| {
            let max_clients = noprop::sample_usize_in(ctx, 1..=8);
            let protected_mask = noprop::sample_u64(ctx);
            let now = Instant::now();
            let mut state = State::default();
            let mut protected = HashSet::new();
            let mut inactive = Vec::new();

            for index in 0..max_clients {
                let client_id = format!("client-{index}");
                state.clients.insert(
                    client_id.clone(),
                    ClientRegistration {
                        name: client_id.clone(),
                        redirect_uris: HashSet::from(["http://127.0.0.1:9876/callback".to_owned()]),
                        last_used_at: now + Duration::from_nanos(index as u64),
                    },
                );
                if protected_mask & (1 << index) != 0 {
                    protected.insert(client_id.clone());
                    if index % 2 == 0 {
                        state.codes.insert(
                            format!("code-{index}"),
                            AuthorizationCode {
                                client_id,
                                redirect_uri: "http://127.0.0.1:9876/callback".to_owned(),
                                code_challenge: "A".repeat(43),
                                resource: "https://node.example.ts.net/mcp".to_owned(),
                                expires_at: now + Duration::from_secs(60),
                            },
                        );
                    } else {
                        state.tokens.insert(
                            format!("token-{index}"),
                            AccessToken {
                                client_id,
                                subject: "local-owner".to_owned(),
                                resource: "https://node.example.ts.net/mcp".to_owned(),
                                expires_at: now + Duration::from_secs(60),
                            },
                        );
                    }
                } else {
                    inactive.push(client_id);
                }
            }

            let available = make_client_capacity_available(&mut state, max_clients);
            if let Some(expected_evicted) = inactive.first() {
                assert!(available);
                assert_eq!(state.clients.len(), max_clients - 1);
                assert!(!state.clients.contains_key(expected_evicted));
                assert!(
                    protected
                        .iter()
                        .all(|client_id| state.clients.contains_key(client_id)),
                    "active OAuth client was evicted"
                );
            } else {
                assert!(!available);
                assert_eq!(state.clients.len(), max_clients);
            }
            Ok(())
        })
    }

    #[test]
    fn generated_token_capacity_preserves_retryable_codes() -> noprop::TestResult {
        crate::test_support::run(0x4f41_5554_4854_4f4b, 256, |ctx| {
            let token_limit = noprop::sample_usize_in(ctx, 1..=8);
            let occupancy = noprop::sample_usize_in(ctx, 0..=token_limit);
            let binding_valid = noprop::sample_bool(ctx);
            let request_token_collision = noprop::sample_bool(ctx);
            let now = Instant::now();
            let verifier = "a".repeat(43);
            let challenge = pkce_challenge(&verifier).unwrap();
            let code_value = format!("code-{:x}", noprop::sample_u64(ctx));
            let client_id = "client".to_owned();
            let registered_redirect = "http://127.0.0.1:9876/callback".to_owned();
            let resource = "https://node.example.ts.net/mcp".to_owned();
            let request = TokenRequest {
                grant_type: "authorization_code".to_owned(),
                code: code_value.clone(),
                client_id: client_id.clone(),
                redirect_uri: if binding_valid {
                    registered_redirect.clone()
                } else {
                    "http://127.0.0.1:9876/other".to_owned()
                },
                code_verifier: verifier,
                resource: resource.clone(),
            };
            let mut state = State::default();
            state.codes.insert(
                code_value.clone(),
                AuthorizationCode {
                    client_id,
                    redirect_uri: registered_redirect,
                    code_challenge: challenge.clone(),
                    resource,
                    expires_at: now + Duration::from_secs(60),
                },
            );
            for index in 0..occupancy {
                state.tokens.insert(
                    format!("existing-{index}"),
                    AccessToken {
                        client_id: "client".to_owned(),
                        subject: "local-owner".to_owned(),
                        resource: "https://node.example.ts.net/mcp".to_owned(),
                        expires_at: now + Duration::from_secs(60),
                    },
                );
            }

            let collision = request_token_collision && occupancy > 0;
            let issued_token = if collision {
                "existing-0".to_owned()
            } else {
                "issued-token".to_owned()
            };
            let result = redeem_authorization_code(
                &mut state,
                &request,
                &challenge,
                issued_token.clone(),
                now,
                token_limit,
            );
            if occupancy == token_limit {
                let error = result.unwrap_err();
                assert_eq!(error.code, "temporarily_unavailable");
                assert!(state.codes.contains_key(&code_value));
                assert_eq!(state.tokens.len(), occupancy);
            } else if !binding_valid {
                let error = result.unwrap_err();
                assert_eq!(error.code, "invalid_grant");
                assert!(!state.codes.contains_key(&code_value));
                assert_eq!(state.tokens.len(), occupancy);
            } else if collision {
                let error = result.unwrap_err();
                assert_eq!(error.code, "server_error");
                assert!(state.codes.contains_key(&code_value));
                assert_eq!(state.tokens.len(), occupancy);
            } else {
                result.unwrap();
                assert!(!state.codes.contains_key(&code_value));
                assert_eq!(state.tokens.len(), occupancy + 1);
                assert!(state.tokens.contains_key(&issued_token));
            }
            Ok(())
        })
    }

    #[tokio::test]
    async fn wrong_resource_and_pkce_downgrade_are_rejected() {
        let (oauth, _approvals) = oauth();
        let client_id = registered(&oauth).await;
        let request = AuthorizeRequest {
            response_type: "code".to_owned(),
            client_id,
            redirect_uri: "http://127.0.0.1:9876/callback".to_owned(),
            code_challenge: "A".repeat(43),
            code_challenge_method: "plain".to_owned(),
            resource: "https://wrong.example/mcp".to_owned(),
            state: None,
            scope: Some("mcp".to_owned()),
        };
        assert!(oauth.authorize(request).await.is_err());
    }
}
