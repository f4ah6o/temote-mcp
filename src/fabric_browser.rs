//! Browser-enrolled Fabric credentials and OAuth protocol.
//!
//! Secret-bearing state is kept behind `CredentialStore`; tests use an in-memory
//! fake and never touch an operating-system credential provider.

use std::collections::{BTreeMap, BTreeSet};
#[cfg(target_os = "linux")]
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::digest::{SHA256, digest};
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

#[cfg(test)]
use std::sync::Mutex;

const STORE_SERVICE: &str = "com.temote.fabric.browser";
const RECORD_VERSION: u32 = 1;
const MAX_RECORD_BYTES: usize = 32 * 1024;
const MAX_HTTP_BYTES: usize = 64 * 1024;
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const LOOPBACK_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Clone, Debug)]
pub struct BrowserConnectOptions {
    pub gateway_url: String,
    pub host_id: Option<String>,
    pub issuer: String,
    pub client_id: String,
    pub permitted_origins: Vec<String>,
    pub resource: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialRecord {
    pub schema_version: u32,
    pub version: u64,
    pub gateway_origin: String,
    pub issuer: String,
    pub oauth_client_id: String,
    pub oauth_authorization_endpoint: String,
    pub oauth_token_endpoint: String,
    pub owner_key: String,
    pub email: String,
    pub host_id: String,
    pub grant_id: String,
    pub grant_secret: String,
    pub grant_generation: u64,
    /// Unix epoch seconds, normalized from the Worker’s millisecond response.
    pub grant_expires_at: u64,
    pub attempt_id: String,
    pub reserve_operation_id: String,
    pub expected_generation: u64,
    pub activation_operation_id: String,
    pub cancel_operation_id: Option<String>,
    pub revoke_operation_id: Option<String>,
    pub oauth_access_token: String,
    pub oauth_refresh_token: String,
    /// Unix epoch seconds from OAuth `expires_in`.
    pub access_expires_at: u64,
    pub refresh_in_flight: bool,
    pub refresh_attempt_id: Option<String>,
    pub refresh_outcome_unknown: bool,
    pub refresh_commit_pending: bool,
    pub status: CredentialStatus,
    pub supervisor_boot_generation: String,
    pub root_registry_fingerprint: String,
    pub root_fingerprints: BTreeMap<String, String>,
    pub approved_roots: Vec<String>,
    pub local_hmac_key: String,
}

/// The signed authorization snapshot returned in a browser Host long-poll.
/// This is a transport envelope, not a client-provided MCP argument.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkerAuthSnapshot {
    pub mode: String,
    pub owner_key: String,
    pub grant_id: String,
    pub grant_generation: u64,
    pub approved_roots: Vec<String>,
}

/// Worker-captured instance for one routed session operation. It travels in a
/// separate internal envelope and is checked against the Supervisor's live
/// session before any task, evidence, job, or lifecycle side effect.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BrowserSessionBinding {
    pub session_id: String,
    pub session_instance: String,
}

/// Local-only root identity returned by the lifecycle Supervisor's private
/// control socket. `canonical_path` never leaves the process boundary.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalRootIdentity {
    pub name: String,
    pub canonical_path: PathBuf,
    pub device: u64,
    pub inode: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalSupervisorInventory {
    pub host_id: String,
    pub boot_generation: String,
    pub control_protocol: u64,
    pub roots: Vec<LocalRootIdentity>,
}

/// Private Link-to-Supervisor authority. Its local HMAC key and root
/// fingerprints only cross the owner-only control socket; they are never part
/// of an MCP request, a Worker payload, or user-facing output.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FabricAuthority {
    pub schema_version: u32,
    pub host_id: String,
    pub owner_key: String,
    pub grant_id: String,
    pub grant_generation: u64,
    pub approved_roots: Vec<String>,
    pub supervisor_boot_generation: String,
    pub root_registry_fingerprint: String,
    pub root_fingerprints: BTreeMap<String, String>,
    pub local_hmac_key: String,
    pub proof: String,
}

impl std::fmt::Debug for FabricAuthority {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FabricAuthority")
            .field("host_id", &self.host_id)
            .field("grant_generation", &self.grant_generation)
            .field("approved_roots", &self.approved_roots)
            .field("proof", &"[REDACTED]")
            .field("local_hmac_key", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl CredentialRecord {
    pub fn validate_for_link(&self, gateway_origin: &str) -> Result<()> {
        anyhow::ensure!(
            self.schema_version == RECORD_VERSION,
            "browser credential schema is unsupported"
        );
        anyhow::ensure!(
            self.status == CredentialStatus::Active,
            "browser enrollment is not active"
        );
        anyhow::ensure!(
            self.gateway_origin == gateway_origin,
            "browser credential belongs to another Fabric origin"
        );
        anyhow::ensure!(
            !self.refresh_in_flight
                && !self.refresh_outcome_unknown
                && !self.refresh_commit_pending,
            "OAuth refresh outcome is uncertain; reconnect interactively"
        );
        anyhow::ensure!(
            !self.oauth_access_token.is_empty() && !self.oauth_refresh_token.is_empty(),
            "browser OAuth credentials are incomplete"
        );
        anyhow::ensure!(
            !self.oauth_client_id.is_empty()
                && !self.oauth_authorization_endpoint.is_empty()
                && !self.oauth_token_endpoint.is_empty(),
            "browser OAuth configuration pin is incomplete"
        );
        anyhow::ensure!(
            !self.grant_secret.is_empty() && self.grant_generation > 0,
            "browser Host grant is incomplete"
        );
        anyhow::ensure!(
            self.grant_expires_at > unix_now(),
            "browser Host grant is expired; reconnect to confirm a new enrollment"
        );
        anyhow::ensure!(
            !self.approved_roots.is_empty(),
            "browser enrollment has no approved roots"
        );
        anyhow::ensure!(
            self.approved_roots
                .iter()
                .all(|name| self.root_fingerprints.contains_key(name)),
            "approved root identity is unavailable"
        );
        Ok(())
    }
}

pub fn inventory_root_fingerprints(
    inventory: &LocalSupervisorInventory,
    local_hmac_key: &str,
) -> Result<BTreeMap<String, String>> {
    let key = decode_hmac_key(local_hmac_key)?;
    let mut fingerprints = BTreeMap::new();
    for root in &inventory.roots {
        crate::named_roots::validate_root_name(&root.name)?;
        anyhow::ensure!(
            root.canonical_path.is_absolute(),
            "Supervisor root identity is not canonical"
        );
        let metadata = std::fs::metadata(&root.canonical_path)
            .context("Supervisor root identity is no longer available")?;
        anyhow::ensure!(
            metadata.is_dir(),
            "Supervisor named root is not a directory"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            anyhow::ensure!(
                metadata.dev() == root.device && metadata.ino() == root.inode,
                "Supervisor root identity changed during browser enrollment"
            );
        }
        let message =
            root_identity_message(&root.name, &root.canonical_path, root.device, root.inode)?;
        let signature = hmac::sign(&key, &message);
        anyhow::ensure!(
            fingerprints
                .insert(root.name.clone(), hex_encode(signature.as_ref()))
                .is_none(),
            "Supervisor root inventory contains duplicate names"
        );
    }
    anyhow::ensure!(
        !fingerprints.is_empty(),
        "the running Supervisor has no configured named roots"
    );
    Ok(fingerprints)
}

pub fn root_registry_fingerprint(
    local_hmac_key: &str,
    fingerprints: &BTreeMap<String, String>,
) -> Result<String> {
    let key = decode_hmac_key(local_hmac_key)?;
    let bytes = serde_json::to_vec(fingerprints)?;
    Ok(hex_encode(hmac::sign(&key, &bytes).as_ref()))
}

pub fn make_fabric_authority(
    record: &CredentialRecord,
    snapshot: &WorkerAuthSnapshot,
    inventory: &LocalSupervisorInventory,
) -> Result<FabricAuthority> {
    anyhow::ensure!(
        snapshot.mode == "browser",
        "Worker did not authorize browser Host mode"
    );
    anyhow::ensure!(
        record.host_id == inventory.host_id,
        "credential Host identity differs from the running Supervisor"
    );
    anyhow::ensure!(
        snapshot.owner_key == record.owner_key && snapshot.grant_id == record.grant_id,
        "Worker Host authorization belongs to a different owner or grant"
    );
    anyhow::ensure!(
        snapshot.grant_generation == record.grant_generation && snapshot.grant_generation > 0,
        "Worker Host authorization generation changed; reconnect required"
    );
    let mut worker_roots = snapshot.approved_roots.clone();
    worker_roots.sort();
    anyhow::ensure!(
        worker_roots == record.approved_roots && !worker_roots.is_empty(),
        "Worker Host authorization root scope changed; reconnect required"
    );
    anyhow::ensure!(
        inventory.control_protocol >= crate::session_control::CONTROL_PROTOCOL_VERSION,
        "running Supervisor lacks browser enrollment admission support; upgrade and restart it before connecting"
    );
    let root_fingerprints = inventory_root_fingerprints(inventory, &record.local_hmac_key)?;
    anyhow::ensure!(
        worker_roots
            .iter()
            .all(|root| root_fingerprints.contains_key(root)),
        "an approved named root is unavailable in the running Supervisor"
    );
    let current_registry_fingerprint =
        root_registry_fingerprint(&record.local_hmac_key, &root_fingerprints)?;
    anyhow::ensure!(
        record.root_registry_fingerprint == current_registry_fingerprint,
        "Supervisor named-root inventory changed; explicitly reapprove the current root scope"
    );
    anyhow::ensure!(
        record
            .approved_roots
            .iter()
            .all(|root| record.root_fingerprints.get(root) == root_fingerprints.get(root)),
        "an approved named root changed identity; explicitly reapprove it before connecting"
    );
    let root_registry_fingerprint = current_registry_fingerprint;
    let mut authority = FabricAuthority {
        schema_version: RECORD_VERSION,
        host_id: record.host_id.clone(),
        owner_key: record.owner_key.clone(),
        grant_id: record.grant_id.clone(),
        grant_generation: record.grant_generation,
        approved_roots: worker_roots,
        supervisor_boot_generation: inventory.boot_generation.clone(),
        root_registry_fingerprint,
        root_fingerprints,
        local_hmac_key: record.local_hmac_key.clone(),
        proof: String::new(),
    };
    authority.proof = sign_authority(&authority)?;
    Ok(authority)
}

pub fn validate_fabric_authority_proof(authority: &FabricAuthority) -> Result<()> {
    anyhow::ensure!(
        authority.schema_version == RECORD_VERSION,
        "browser Host scope protocol is unsupported"
    );
    anyhow::ensure!(
        !authority.host_id.is_empty()
            && authority.grant_generation > 0
            && !authority.owner_key.is_empty()
            && !authority.grant_id.is_empty()
            && !authority.supervisor_boot_generation.is_empty(),
        "browser Host scope is incomplete"
    );
    let mut roots = authority.approved_roots.clone();
    roots.sort();
    roots.dedup();
    anyhow::ensure!(
        !roots.is_empty() && roots == authority.approved_roots,
        "browser Host root scope is invalid"
    );
    anyhow::ensure!(
        roots
            .iter()
            .all(|name| authority.root_fingerprints.contains_key(name)),
        "browser Host root identity is missing"
    );
    let registry =
        root_registry_fingerprint(&authority.local_hmac_key, &authority.root_fingerprints)?;
    anyhow::ensure!(
        registry == authority.root_registry_fingerprint,
        "browser Host root registry proof is invalid"
    );
    anyhow::ensure!(
        sign_authority(authority)? == authority.proof,
        "browser Host scope proof is invalid"
    );
    Ok(())
}

fn sign_authority(authority: &FabricAuthority) -> Result<String> {
    let key = decode_hmac_key(&authority.local_hmac_key)?;
    let unsigned = json!({
        "schema_version": authority.schema_version,
        "host_id": authority.host_id,
        "owner_key": authority.owner_key,
        "grant_id": authority.grant_id,
        "grant_generation": authority.grant_generation,
        "approved_roots": authority.approved_roots,
        "supervisor_boot_generation": authority.supervisor_boot_generation,
        "root_registry_fingerprint": authority.root_registry_fingerprint,
        "root_fingerprints": authority.root_fingerprints,
    });
    Ok(hex_encode(
        hmac::sign(&key, &serde_json::to_vec(&unsigned)?).as_ref(),
    ))
}

fn decode_hmac_key(value: &str) -> Result<hmac::Key> {
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .context("local browser scope key is malformed")?;
    anyhow::ensure!(
        bytes.len() >= 32 && bytes.len() <= 64,
        "local browser scope key has an invalid size"
    );
    Ok(hmac::Key::new(hmac::HMAC_SHA256, &bytes))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn root_identity_message(name: &str, path: &Path, device: u64, inode: u64) -> Result<Vec<u8>> {
    let path = path
        .to_str()
        .context("Supervisor root path is not valid UTF-8")?;
    Ok(serde_json::to_vec(&json!({
        "name": name,
        "canonical_path": path,
        "device": device,
        "inode": inode,
    }))?)
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CredentialStatus {
    Pending,
    Active,
    Revoking,
}

/// The versioned record is one secure item. Implementations must make the
/// complete replacement atomic and read it back before reporting success.
pub trait CredentialStore: Send + Sync {
    fn load(&self) -> Result<Option<CredentialRecord>>;
    fn replace(&self, record: &CredentialRecord) -> Result<()>;
    fn delete(&self) -> Result<()>;
}

pub struct OsCredentialStore {
    profile: String,
    #[cfg(target_os = "linux")]
    pointer_path: PathBuf,
}

impl OsCredentialStore {
    pub fn open_profile(profile: &str) -> Result<Self> {
        anyhow::ensure!(
            profile.len() == 64 && profile.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "credential profile key is invalid"
        );
        let profile = profile.to_owned();
        #[cfg(target_os = "linux")]
        let pointer_path = state_directory()?.join(format!("{profile}.pointer"));
        #[cfg(target_os = "linux")]
        ensure_secret_tool()?;
        Ok(Self {
            profile,
            #[cfg(target_os = "linux")]
            pointer_path,
        })
    }
}

pub struct OsCredentialStoreFactory;

impl CredentialStoreFactory for OsCredentialStoreFactory {
    fn open(&self, profile: &str) -> Result<Box<dyn CredentialStore>> {
        Ok(Box::new(OsCredentialStore::open_profile(profile)?))
    }
}

impl CredentialStore for OsCredentialStore {
    fn load(&self) -> Result<Option<CredentialRecord>> {
        #[cfg(target_os = "macos")]
        let bytes =
            match security_framework::passwords::get_generic_password(STORE_SERVICE, &self.profile)
            {
                Ok(bytes) => bytes,
                Err(error) if error.code() == -25300 => return Ok(None),
                Err(_) => anyhow::bail!("macOS Keychain credential lookup failed"),
            };
        #[cfg(target_os = "linux")]
        let bytes = {
            let Some(version) = read_pointer(&self.pointer_path)? else {
                return Ok(None);
            };
            secret_tool_lookup(&self.profile, version)?
        };
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        anyhow::bail!("browser enrollment requires macOS Keychain or Linux Secret Service");
        anyhow::ensure!(
            bytes.len() <= MAX_RECORD_BYTES,
            "secure credential record is oversized"
        );
        let record: CredentialRecord =
            serde_json::from_slice(&bytes).context("secure credential record is malformed")?;
        anyhow::ensure!(
            record.schema_version == RECORD_VERSION,
            "secure credential schema is unsupported"
        );
        Ok(Some(record))
    }

    fn replace(&self, record: &CredentialRecord) -> Result<()> {
        let bytes = serde_json::to_vec(record)?;
        anyhow::ensure!(
            bytes.len() <= MAX_RECORD_BYTES,
            "secure credential record is oversized"
        );
        #[cfg(target_os = "macos")]
        {
            security_framework::passwords::set_generic_password(
                STORE_SERVICE,
                &self.profile,
                &bytes,
            )
            .map_err(|_| anyhow::anyhow!("macOS Keychain credential update failed"))?;
            let readback =
                security_framework::passwords::get_generic_password(STORE_SERVICE, &self.profile)
                    .map_err(|_| anyhow::anyhow!("macOS Keychain read-back failed"))?;
            anyhow::ensure!(
                readback == bytes,
                "macOS Keychain read-back did not match committed credential version"
            );
        }
        #[cfg(target_os = "linux")]
        {
            let old_version = read_pointer(&self.pointer_path)?.unwrap_or(0);
            let next_version = old_version
                .checked_add(1)
                .context("secure credential version exhausted")?;
            secret_tool_store(&self.profile, next_version, &bytes)?;
            let readback = secret_tool_lookup(&self.profile, next_version)?;
            anyhow::ensure!(
                readback == bytes,
                "Secret Service read-back did not match committed credential version"
            );
            write_pointer_atomic(&self.pointer_path, next_version)?;
            let committed = secret_tool_lookup(&self.profile, next_version)?;
            anyhow::ensure!(
                committed == bytes,
                "Secret Service committed record could not be read back"
            );
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        anyhow::bail!("browser enrollment requires macOS Keychain or Linux Secret Service");
        Ok(())
    }

    fn delete(&self) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            match security_framework::passwords::delete_generic_password(
                STORE_SERVICE,
                &self.profile,
            ) {
                Ok(()) => Ok(()),
                Err(error) if error.code() == -25300 => Ok(()),
                Err(_) => anyhow::bail!("macOS Keychain credential removal failed"),
            }
        }
        #[cfg(target_os = "linux")]
        {
            if let Some(version) = read_pointer(&self.pointer_path)? {
                secret_tool_clear(&self.profile, version)?;
            }
            match std::fs::remove_file(&self.pointer_path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(_) => anyhow::bail!("Secret Service pointer removal failed"),
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            anyhow::bail!("browser enrollment requires macOS Keychain or Linux Secret Service")
        }
    }
}

/// A finite state transition lock. It is intentionally separate from the
/// process-owned Link lease, so a running Link never blocks logout/revoke.
pub struct ProfileLock(std::fs::File);

impl ProfileLock {
    pub fn transition(profile: &str) -> Result<Self> {
        Self::acquire(profile, "transition")
    }

    pub fn link_lifetime(profile: &str) -> Result<Self> {
        Self::acquire(profile, "link")
    }

    fn acquire(profile: &str, kind: &str) -> Result<Self> {
        #[cfg(unix)]
        {
            let path = state_directory()?.join(format!("{profile}.{kind}.lock"));
            let file = std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)?;
            let operation = if kind == "link" {
                libc::LOCK_EX | libc::LOCK_NB
            } else {
                libc::LOCK_EX
            };
            let rc = unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&file), operation) };
            if rc != 0 {
                if kind == "link" {
                    anyhow::bail!("a Fabric Link already owns this credential profile");
                }
                anyhow::bail!("browser credential transition lock is unavailable");
            }
            Ok(Self(file))
        }
        #[cfg(not(unix))]
        {
            let _ = (profile, kind);
            anyhow::bail!("browser credential locking is unavailable on this operating system")
        }
    }
}

impl Drop for ProfileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::flock(std::os::fd::AsRawFd::as_raw_fd(&self.0), libc::LOCK_UN);
        }
    }
}

trait TransitionGuard {}

impl TransitionGuard for ProfileLock {}

#[cfg(test)]
struct NoopTransitionGuard;

#[cfg(test)]
impl TransitionGuard for NoopTransitionGuard {}

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

#[derive(Clone, Debug)]
pub struct VerifiedIdentity {
    pub owner_key: String,
    pub email: String,
}

#[derive(Clone)]
struct OAuthMetadata {
    authorization_endpoint: Url,
    token_endpoint: Url,
    issuer: String,
}

#[derive(Clone)]
pub struct BrowserOAuth {
    pub options: BrowserConnectOptions,
}

#[derive(Deserialize)]
struct ProtectedResourceMetadata {
    resource: String,
    authorization_servers: Vec<String>,
}

#[derive(Deserialize)]
struct AuthorizationServerMetadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    token_type: String,
    expires_in: u64,
    refresh_token: Option<String>,
}

impl BrowserOAuth {
    pub fn new(options: BrowserConnectOptions) -> Result<Self> {
        validate_oauth_options(&options)?;
        Ok(Self { options })
    }

    async fn fetch_json(&self, url: &Url) -> Result<Value> {
        validate_permitted_url(url, &self.options.permitted_origins)?;
        let host = url.host_str().context("OAuth endpoint has no host")?;
        let addrs = resolve_public_addresses(
            host,
            url.port_or_known_default()
                .context("OAuth endpoint has no port")?,
        )
        .await?;
        let client = pinned_http_client(host, &addrs)?;
        let response = client
            .get(url.clone())
            .send()
            .await
            .context("OAuth metadata request failed")?;
        anyhow::ensure!(
            !response.status().is_redirection(),
            "OAuth metadata redirects are disabled"
        );
        anyhow::ensure!(
            response.status().is_success(),
            "OAuth metadata endpoint rejected the request"
        );
        anyhow::ensure!(
            response
                .content_length()
                .is_none_or(|len| len as usize <= MAX_HTTP_BYTES),
            "OAuth metadata response is oversized"
        );
        let bytes = read_bounded_response(response).await?;
        let value: Value =
            serde_json::from_slice(&bytes).context("OAuth metadata is invalid JSON")?;
        Ok(value)
    }

    async fn post_form(&self, url: &Url, form: &[(&str, &str)]) -> Result<TokenResponse> {
        validate_permitted_url(url, &self.options.permitted_origins)?;
        let host = url.host_str().context("token endpoint has no host")?;
        let addrs = resolve_public_addresses(
            host,
            url.port_or_known_default()
                .context("token endpoint has no port")?,
        )
        .await?;
        let client = pinned_http_client(host, &addrs)?;
        let response = client
            .post(url.clone())
            .form(form)
            .send()
            .await
            .context("OAuth token request failed")?;
        anyhow::ensure!(
            !response.status().is_redirection(),
            "OAuth token endpoint redirects are disabled"
        );
        anyhow::ensure!(
            response.status().is_success(),
            "OAuth token endpoint rejected the request"
        );
        anyhow::ensure!(
            response
                .content_length()
                .is_none_or(|len| len as usize <= MAX_HTTP_BYTES),
            "OAuth token response is oversized"
        );
        let bytes = read_bounded_response(response).await?;
        let value: TokenResponse =
            serde_json::from_slice(&bytes).context("OAuth token response is malformed")?;
        anyhow::ensure!(
            value.token_type.eq_ignore_ascii_case("bearer"),
            "OAuth token type is unsupported"
        );
        anyhow::ensure!(
            value.expires_in > 0 && value.expires_in <= 86_400,
            "OAuth access token expiry is invalid"
        );
        anyhow::ensure!(
            value.access_token.starts_with("oauth:") && value.access_token.len() <= 8192,
            "Cloudflare returned an unsupported opaque access token"
        );
        Ok(value)
    }

    async fn metadata(&self) -> Result<OAuthMetadata> {
        let gateway = Url::parse(&self.options.gateway_url)?;
        let protected_url = gateway.join("/.well-known/oauth-protected-resource")?;
        let protected: ProtectedResourceMetadata =
            serde_json::from_value(self.fetch_json(&protected_url).await?)
                .context("protected-resource metadata is invalid")?;
        anyhow::ensure!(
            protected.resource == self.options.resource,
            "protected-resource metadata does not match the pinned resource"
        );
        anyhow::ensure!(
            protected
                .authorization_servers
                .iter()
                .any(|issuer| issuer == &self.options.issuer),
            "pinned OAuth issuer is not advertised for this resource"
        );
        let issuer_url = Url::parse(&self.options.issuer)?;
        let server_metadata_url = issuer_url.join("/.well-known/oauth-authorization-server")?;
        let metadata: AuthorizationServerMetadata =
            serde_json::from_value(self.fetch_json(&server_metadata_url).await?)
                .context("authorization-server metadata is invalid")?;
        anyhow::ensure!(
            metadata.issuer == self.options.issuer,
            "OAuth issuer does not match configured pin"
        );
        let authorization_endpoint = Url::parse(&metadata.authorization_endpoint)?;
        let token_endpoint = Url::parse(&metadata.token_endpoint)?;
        validate_permitted_url(&authorization_endpoint, &self.options.permitted_origins)?;
        validate_permitted_url(&token_endpoint, &self.options.permitted_origins)?;
        Ok(OAuthMetadata {
            authorization_endpoint,
            token_endpoint,
            issuer: metadata.issuer,
        })
    }

    pub async fn authorize(&self) -> Result<OAuthCredentials> {
        let metadata = self.metadata().await?;
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let redirect_uri = format!("http://127.0.0.1:{}/callback", address.port());
        let verifier = random_url_token(32)?;
        let challenge = URL_SAFE_NO_PAD.encode(digest(&SHA256, verifier.as_bytes()).as_ref());
        let state = random_url_token(32)?;
        let mut authorize = metadata.authorization_endpoint.clone();
        authorize
            .query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.options.client_id)
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("resource", &self.options.resource)
            .append_pair("state", &state);
        open_authorization_url(authorize.as_str())?;
        let code = tokio::time::timeout(LOOPBACK_TIMEOUT, wait_for_loopback_code(listener, &state))
            .await
            .context("OAuth browser login timed out")??;
        let token = self
            .post_form(
                &metadata.token_endpoint,
                &[
                    ("grant_type", "authorization_code"),
                    ("client_id", &self.options.client_id),
                    ("redirect_uri", &redirect_uri),
                    ("code", &code),
                    ("code_verifier", &verifier),
                    ("resource", &self.options.resource),
                ],
            )
            .await?;
        let refresh_token = token
            .refresh_token
            .context("Cloudflare did not issue a refresh token")?;
        Ok(OAuthCredentials {
            issuer: metadata.issuer,
            client_id: self.options.client_id.clone(),
            authorization_endpoint: metadata.authorization_endpoint.to_string(),
            token_endpoint: metadata.token_endpoint.to_string(),
            access_token: token.access_token,
            refresh_token,
            access_expires_at: unix_now().saturating_add(token.expires_in),
        })
    }

    pub async fn refresh(&self, credentials: &OAuthCredentials) -> Result<OAuthCredentials> {
        let metadata = self.metadata().await?;
        anyhow::ensure!(
            credentials.issuer == metadata.issuer
                && credentials.client_id == self.options.client_id
                && credentials.authorization_endpoint == metadata.authorization_endpoint.as_str()
                && credentials.token_endpoint == metadata.token_endpoint.as_str(),
            "OAuth issuer, client, or endpoint metadata changed; interactive authentication is required"
        );
        let token = self
            .post_form(
                &metadata.token_endpoint,
                &[
                    ("grant_type", "refresh_token"),
                    ("client_id", &self.options.client_id),
                    ("refresh_token", &credentials.refresh_token),
                    ("resource", &self.options.resource),
                ],
            )
            .await?;
        Ok(OAuthCredentials {
            issuer: metadata.issuer,
            client_id: self.options.client_id.clone(),
            authorization_endpoint: metadata.authorization_endpoint.to_string(),
            token_endpoint: metadata.token_endpoint.to_string(),
            access_token: token.access_token,
            refresh_token: token
                .refresh_token
                .unwrap_or_else(|| credentials.refresh_token.clone()),
            access_expires_at: unix_now().saturating_add(token.expires_in),
        })
    }

    pub async fn identity(&self, access_token: &str) -> Result<VerifiedIdentity> {
        let endpoint = Url::parse(&self.options.gateway_url)?.join("/v1/enrollment-identity")?;
        validate_permitted_url(&endpoint, &self.options.permitted_origins)?;
        let host = endpoint
            .host_str()
            .context("identity endpoint has no host")?;
        let addrs = resolve_public_addresses(
            host,
            endpoint
                .port_or_known_default()
                .context("identity endpoint has no port")?,
        )
        .await?;
        let client = pinned_http_client(host, &addrs)?;
        let response = client
            .get(endpoint)
            .bearer_auth(access_token)
            .send()
            .await
            .context("verified Fabric identity request failed")?;
        anyhow::ensure!(
            !response.status().is_redirection(),
            "identity endpoint redirects are disabled"
        );
        anyhow::ensure!(
            response.status().is_success(),
            "verified Fabric identity was rejected"
        );
        let bytes = read_bounded_response(response).await?;
        let value: Value = serde_json::from_slice(&bytes)?;
        let owner_key = value
            .get("owner_key")
            .and_then(Value::as_str)
            .context("Worker identity omitted owner key")?;
        let email = value
            .get("email")
            .and_then(Value::as_str)
            .context("Worker identity omitted email")?;
        anyhow::ensure!(
            owner_key.len() == 64 && owner_key.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "Worker identity owner key is invalid"
        );
        anyhow::ensure!(
            email.len() <= 320,
            "Worker identity display field is invalid"
        );
        Ok(VerifiedIdentity {
            owner_key: owner_key.to_ascii_lowercase(),
            email: email.to_owned(),
        })
    }
}

pub struct OAuthCredentials {
    pub issuer: String,
    pub client_id: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub access_token: String,
    pub refresh_token: String,
    /// Unix epoch seconds from OAuth `expires_in`.
    pub access_expires_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnrollmentStatus {
    pub host_id: String,
    pub status: String,
    pub generation: u64,
    pub root_names: Vec<String>,
    pub owner_key: Option<String>,
    pub grant_id: Option<String>,
    pub attempt_id: Option<String>,
    pub reservation_operation_id: Option<String>,
    pub cancel_operation_id: Option<String>,
    pub operation_id: Option<String>,
    pub operation_generation: Option<u64>,
    /// Worker epoch deadline, normalized from wire milliseconds to seconds.
    pub pending_expires_at: Option<u64>,
    /// Worker epoch deadline, normalized from wire milliseconds to seconds.
    pub expires_at: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerEnrollmentStatus {
    host_id: String,
    status: String,
    #[serde(alias = "grant_generation")]
    generation: u64,
    root_names: Vec<String>,
    #[serde(default)]
    owner_key: Option<String>,
    #[serde(default)]
    grant_id: Option<String>,
    #[serde(default)]
    attempt_id: Option<String>,
    #[serde(default)]
    reservation_operation_id: Option<String>,
    #[serde(default)]
    cancel_operation_id: Option<String>,
    #[serde(default)]
    operation_id: Option<String>,
    #[serde(default)]
    operation_generation: Option<u64>,
    #[serde(default)]
    pending_expires_at: Option<u64>,
    #[serde(default)]
    expires_at: Option<u64>,
}

impl TryFrom<WorkerEnrollmentStatus> for EnrollmentStatus {
    type Error = anyhow::Error;

    fn try_from(worker: WorkerEnrollmentStatus) -> Result<Self> {
        Ok(Self {
            host_id: worker.host_id,
            status: worker.status,
            generation: worker.generation,
            root_names: worker.root_names,
            owner_key: worker.owner_key,
            grant_id: worker.grant_id,
            attempt_id: worker.attempt_id,
            reservation_operation_id: worker.reservation_operation_id,
            cancel_operation_id: worker.cancel_operation_id,
            operation_id: worker.operation_id,
            operation_generation: worker.operation_generation,
            pending_expires_at: worker
                .pending_expires_at
                .map(worker_epoch_millis_to_seconds)
                .transpose()?,
            expires_at: worker
                .expires_at
                .map(worker_epoch_millis_to_seconds)
                .transpose()?,
        })
    }
}

fn worker_epoch_millis_to_seconds(timestamp_ms: u64) -> Result<u64> {
    anyhow::ensure!(
        timestamp_ms >= 1_000_000_000_000,
        "Worker enrollment deadline is not a Unix epoch millisecond timestamp"
    );
    Ok(timestamp_ms / 1_000)
}

/// Network boundary for the enrollment state machine. Production uses the
/// pinned OAuth/Worker client; tests inject a deterministic in-memory peer.
/// `EnrollmentStatus` deadlines crossing this trait are Unix seconds; the real
/// HTTP adapter converts the Worker’s millisecond wire values before returning.
#[allow(async_fn_in_trait)]
pub trait EnrollmentProtocol {
    async fn authorize(&self) -> Result<OAuthCredentials>;
    async fn oauth_configuration_matches(
        &self,
        options: &BrowserConnectOptions,
        record: &CredentialRecord,
    ) -> Result<bool>;
    async fn identity(&self, access_token: &str) -> Result<VerifiedIdentity>;
    async fn refresh(&self, credentials: &OAuthCredentials) -> Result<OAuthCredentials>;
    async fn status(&self, access_token: &str, host_id: &str) -> Result<Option<EnrollmentStatus>>;
    async fn reserve(
        &self,
        access_token: &str,
        record: &CredentialRecord,
    ) -> Result<EnrollmentStatus>;
    async fn activate(
        &self,
        access_token: &str,
        record: &CredentialRecord,
    ) -> Result<EnrollmentStatus>;
    async fn cancel(
        &self,
        access_token: &str,
        host_id: &str,
        status: &EnrollmentStatus,
        operation_id: &str,
    ) -> Result<EnrollmentStatus>;
    async fn revoke(
        &self,
        access_token: &str,
        host_id: &str,
        generation: u64,
        operation_id: &str,
    ) -> Result<EnrollmentStatus>;
}

#[allow(async_fn_in_trait)]
pub trait EnrollmentConfirmation {
    fn confirm_owner(&mut self, identity: &VerifiedIdentity, host_id: &str) -> Result<()>;
    fn approve_roots(&mut self, available: &[String]) -> Result<Vec<String>>;
}

pub trait CredentialStoreFactory {
    fn open(&self, profile: &str) -> Result<Box<dyn CredentialStore>>;
}

pub struct PreparedBrowserLink {
    pub record: CredentialRecord,
    pub authority: FabricAuthority,
    pub profile: String,
}

impl EnrollmentProtocol for BrowserOAuth {
    async fn authorize(&self) -> Result<OAuthCredentials> {
        BrowserOAuth::authorize(self).await
    }
    async fn oauth_configuration_matches(
        &self,
        options: &BrowserConnectOptions,
        record: &CredentialRecord,
    ) -> Result<bool> {
        let metadata = self.metadata().await?;
        Ok(options.issuer == record.issuer
            && options.client_id == record.oauth_client_id
            && metadata.issuer == record.issuer
            && metadata.authorization_endpoint.as_str() == record.oauth_authorization_endpoint
            && metadata.token_endpoint.as_str() == record.oauth_token_endpoint
            && options.resource == record.gateway_origin)
    }
    async fn identity(&self, access_token: &str) -> Result<VerifiedIdentity> {
        BrowserOAuth::identity(self, access_token).await
    }
    async fn refresh(&self, credentials: &OAuthCredentials) -> Result<OAuthCredentials> {
        BrowserOAuth::refresh(self, credentials).await
    }
    async fn status(&self, access_token: &str, host_id: &str) -> Result<Option<EnrollmentStatus>> {
        enrollment_status(self, access_token, host_id).await
    }
    async fn reserve(
        &self,
        access_token: &str,
        record: &CredentialRecord,
    ) -> Result<EnrollmentStatus> {
        reserve_record(self, access_token, record).await
    }
    async fn activate(
        &self,
        access_token: &str,
        record: &CredentialRecord,
    ) -> Result<EnrollmentStatus> {
        activate_record(self, access_token, record).await
    }
    async fn cancel(
        &self,
        access_token: &str,
        host_id: &str,
        status: &EnrollmentStatus,
        operation_id: &str,
    ) -> Result<EnrollmentStatus> {
        cancel_enrollment(self, access_token, host_id, status, operation_id).await
    }
    async fn revoke(
        &self,
        access_token: &str,
        host_id: &str,
        generation: u64,
        operation_id: &str,
    ) -> Result<EnrollmentStatus> {
        revoke_generation(self, access_token, host_id, generation, operation_id).await
    }
}

async fn authorize_saved_profile<P: EnrollmentProtocol>(
    record: &CredentialRecord,
    options: &BrowserConnectOptions,
    protocol: &P,
) -> Result<(OAuthCredentials, VerifiedIdentity)> {
    let credentials = protocol
        .authorize()
        .await
        .context("browser reauthentication did not complete")?;
    anyhow::ensure!(
        credentials.issuer == options.issuer
            && credentials.client_id == options.client_id
            && !credentials.authorization_endpoint.is_empty()
            && !credentials.token_endpoint.is_empty(),
        "OAuth response differs from the currently pinned issuer, client, or endpoints"
    );
    let identity = protocol
        .identity(&credentials.access_token)
        .await
        .context("browser reauthentication identity could not be verified")?;
    validate_verified_identity(&identity)?;
    anyhow::ensure!(
        identity.owner_key == record.owner_key,
        "browser reauthentication verified a different Fabric owner"
    );
    Ok((credentials, identity))
}

fn replace_saved_oauth(record: &mut CredentialRecord, credentials: OAuthCredentials) -> Result<()> {
    anyhow::ensure!(
        !credentials.issuer.is_empty()
            && !credentials.client_id.is_empty()
            && !credentials.authorization_endpoint.is_empty()
            && !credentials.token_endpoint.is_empty(),
        "OAuth response omitted a pinned configuration field"
    );
    record.issuer = credentials.issuer;
    record.oauth_client_id = credentials.client_id;
    record.oauth_authorization_endpoint = credentials.authorization_endpoint;
    record.oauth_token_endpoint = credentials.token_endpoint;
    record.oauth_access_token = credentials.access_token;
    record.oauth_refresh_token = credentials.refresh_token;
    record.access_expires_at = credentials.access_expires_at;
    record.refresh_in_flight = false;
    record.refresh_attempt_id = None;
    record.refresh_outcome_unknown = false;
    record.refresh_commit_pending = false;
    record.version = next_record_version(record.version)?;
    Ok(())
}

#[cfg(test)]
pub async fn prepare_browser_link<P, F, C>(
    options: &BrowserConnectOptions,
    inventory: LocalSupervisorInventory,
    protocol: &P,
    stores: &F,
    confirmation: &mut C,
) -> Result<PreparedBrowserLink>
where
    P: EnrollmentProtocol,
    F: CredentialStoreFactory,
    C: EnrollmentConfirmation,
{
    prepare_browser_link_with_transition(options, inventory, protocol, stores, confirmation, || {
        Ok(Box::new(NoopTransitionGuard))
    })
    .await
}

async fn prepare_browser_link_with_transition<P, F, C, A>(
    options: &BrowserConnectOptions,
    inventory: LocalSupervisorInventory,
    protocol: &P,
    stores: &F,
    confirmation: &mut C,
    mut acquire_transition: A,
) -> Result<PreparedBrowserLink>
where
    P: EnrollmentProtocol,
    F: CredentialStoreFactory,
    C: EnrollmentConfirmation,
    A: FnMut() -> Result<Box<dyn TransitionGuard>>,
{
    validate_oauth_options(options)?;
    anyhow::ensure!(
        options
            .host_id
            .as_deref()
            .is_none_or(|host| host == inventory.host_id),
        "requested Host id does not match the running local Supervisor"
    );
    anyhow::ensure!(
        !inventory.host_id.is_empty() && !inventory.boot_generation.is_empty(),
        "running Supervisor identity is incomplete"
    );
    let host_id = inventory.host_id.clone();
    let gateway_origin = Url::parse(&options.gateway_url)?
        .origin()
        .ascii_serialization();
    let profile = profile_key(&gateway_origin, "", &host_id);
    let store = stores.open(&profile)?;
    let mut record = store.load()?;
    let mut transition_guard: Option<Box<dyn TransitionGuard>>;

    if record.is_some() {
        transition_guard = Some(acquire_transition()?);
        let Some(mut existing) = store.load()? else {
            anyhow::bail!(
                "saved browser credential changed during reconnect; retry `fabric connect`"
            );
        };
        anyhow::ensure!(
            existing.gateway_origin == gateway_origin && existing.host_id == host_id,
            "stored browser credential belongs to a different Fabric Host"
        );
        let config_matches = protocol
            .oauth_configuration_matches(options, &existing)
            .await?;
        let recovery_needed = existing.refresh_in_flight
            || existing.refresh_outcome_unknown
            || existing.refresh_commit_pending
            || !config_matches
            || (existing.status != CredentialStatus::Active
                && existing.access_expires_at <= unix_now());
        if recovery_needed {
            let expected_version = existing.version;
            let replacement = {
                drop(transition_guard.take());
                authorize_saved_profile(&existing, options, protocol).await?
            };
            transition_guard = Some(acquire_transition()?);
            existing = store.load()?.context(
                "saved browser credential was removed during OAuth recovery; retry enrollment",
            )?;
            anyhow::ensure!(
                existing.version == expected_version
                    && existing.owner_key == replacement.1.owner_key
                    && existing.host_id == host_id
                    && existing.gateway_origin == gateway_origin,
                "saved browser credential changed during OAuth recovery; retry `fabric connect`"
            );
            replace_saved_oauth(&mut existing, replacement.0)?;
            store.replace(&existing)?;
        } else if let Err(refresh_error) =
            verify_or_finish_refresh(&mut existing, store.as_ref(), protocol).await
        {
            let durable = store.load()?.context(
                "saved browser credential disappeared during OAuth refresh; retry enrollment",
            )?;
            if !durable.refresh_outcome_unknown {
                return Err(refresh_error);
            }
            let expected_version = durable.version;
            let replacement = {
                drop(transition_guard.take());
                authorize_saved_profile(&durable, options, protocol).await?
            };
            transition_guard = Some(acquire_transition()?);
            existing = store.load()?.context(
                "saved browser credential was removed during OAuth recovery; retry enrollment",
            )?;
            anyhow::ensure!(
                existing.version == expected_version
                    && existing.refresh_outcome_unknown
                    && existing.owner_key == replacement.1.owner_key
                    && existing.host_id == host_id
                    && existing.gateway_origin == gateway_origin,
                "saved browser credential changed during OAuth recovery; retry `fabric connect`"
            );
            replace_saved_oauth(&mut existing, replacement.0)?;
            store.replace(&existing)?;
        }
        let identity = protocol
            .identity(&existing.oauth_access_token)
            .await
            .context("verified Fabric owner identity could not be refreshed")?;
        anyhow::ensure!(
            identity.owner_key == existing.owner_key,
            "stored Fabric credential belongs to a different verified owner; revoke it before switching accounts"
        );
        let current = protocol
            .status(&existing.oauth_access_token, &host_id)
            .await?;
        match existing.status {
            CredentialStatus::Revoking => {
                let current = current
                    .as_ref()
                    .context("the saved Host grant is not owned by the signed-in Fabric account")?;
                finish_saved_revocation(&existing, protocol, current).await?;
                store.delete().context(
                    "the previous enrollment was revoked but its secure record could not be removed",
                )?;
                anyhow::bail!(
                    "the previous Host enrollment was revoked; run `fabric connect` again to confirm a fresh enrollment"
                );
            }
            CredentialStatus::Pending => match current.as_ref() {
                None => resume_pending(&mut existing, store.as_ref(), protocol).await?,
                Some(current) if current.status == "active" => {
                    validate_remote_scope(current, &existing, "active")?;
                    existing.status = CredentialStatus::Active;
                    existing.grant_expires_at =
                        current.expires_at.unwrap_or(existing.grant_expires_at);
                    existing.version = next_record_version(existing.version)?;
                    store.replace(&existing)?;
                }
                Some(current) if current.status == "pending" => {
                    validate_pending_scope(current, &existing)?;
                    resume_pending(&mut existing, store.as_ref(), protocol).await?;
                }
                Some(current) if current.status == "revoked" => {
                    validate_cancelled_pending(current, &existing)?;
                    anyhow::bail!(
                        "this saved enrollment was explicitly cancelled; run `fabric logout` to remove its local recovery record"
                    );
                }
                _ => anyhow::bail!(
                    "saved browser enrollment is no longer pending; retry `fabric logout` before reconnecting"
                ),
            },
            CredentialStatus::Active => {
                let current = current
                    .as_ref()
                    .context("the saved Host grant is not owned by the signed-in Fabric account")?;
                validate_remote_scope(current, &existing, "active")?;
                if current
                    .expires_at
                    .is_some_and(|expiry| expiry <= unix_now())
                {
                    reenroll_expired_grant(
                        &mut existing,
                        &inventory,
                        &identity,
                        store.as_ref(),
                        protocol,
                        confirmation,
                        &mut transition_guard,
                        &mut acquire_transition,
                    )
                    .await?;
                }
            }
        }
        record = Some(existing);
    } else {
        let credentials = protocol.authorize().await?;
        anyhow::ensure!(
            credentials.issuer == options.issuer,
            "OAuth response issuer differs from the configured issuer pin"
        );
        let identity = protocol.identity(&credentials.access_token).await?;
        validate_verified_identity(&identity)?;
        confirmation.confirm_owner(&identity, &host_id)?;
        let available = inventory
            .roots
            .iter()
            .map(|root| root.name.clone())
            .collect::<Vec<_>>();
        let approved_roots = confirmation.approve_roots(&available)?;
        validate_approved_roots(&approved_roots, &available)?;

        // Browser interaction and terminal prompts run before the finite
        // transition lock. Recheck the atomic profile after acquiring it so a
        // concurrent logout/re-enrollment cannot be overwritten.
        transition_guard = Some(acquire_transition()?);
        anyhow::ensure!(
            store.load()?.is_none(),
            "browser credential profile changed during authorization; retry `fabric connect`"
        );

        let current = protocol.status(&credentials.access_token, &host_id).await?;
        let expected_generation = current.as_ref().map_or(0, |status| status.generation);
        if let Some(status) = current.as_ref() {
            anyhow::ensure!(
                matches!(status.status.as_str(), "active" | "pending" | "revoked"),
                "Worker returned an unsupported Host enrollment state"
            );
            if status.status == "pending"
                && status
                    .pending_expires_at
                    .is_some_and(|expiry| expiry > unix_now())
            {
                anyhow::bail!(
                    "an unexpired enrollment attempt already exists; resume it with its secure credential record"
                );
            }
            if status.status == "active"
                && status.expires_at.is_some_and(|expiry| expiry > unix_now())
            {
                anyhow::bail!(
                    "an unexpired Host enrollment already exists; resume its saved credential or revoke it before enrolling again"
                );
            }
        }

        let local_hmac_key = random_secret(32)?;
        let root_fingerprints = inventory_root_fingerprints(&inventory, &local_hmac_key)?;
        let root_registry_fingerprint =
            root_registry_fingerprint(&local_hmac_key, &root_fingerprints)?;
        let grant_generation = expected_generation
            .checked_add(1)
            .context("Host grant generation exhausted")?;
        let grant_secret = random_secret(48)?;
        let mut roots_sorted = approved_roots;
        roots_sorted.sort();
        let mut pending = CredentialRecord {
            schema_version: RECORD_VERSION,
            version: 1,
            gateway_origin: gateway_origin.clone(),
            issuer: credentials.issuer.clone(),
            oauth_client_id: credentials.client_id.clone(),
            oauth_authorization_endpoint: credentials.authorization_endpoint.clone(),
            oauth_token_endpoint: credentials.token_endpoint.clone(),
            owner_key: identity.owner_key,
            email: identity.email,
            host_id: host_id.clone(),
            grant_id: Uuid::new_v4().to_string(),
            grant_secret,
            grant_generation,
            grant_expires_at: 0,
            attempt_id: Uuid::new_v4().to_string(),
            reserve_operation_id: Uuid::new_v4().to_string(),
            expected_generation,
            activation_operation_id: Uuid::new_v4().to_string(),
            cancel_operation_id: None,
            revoke_operation_id: None,
            oauth_access_token: credentials.access_token,
            oauth_refresh_token: credentials.refresh_token,
            access_expires_at: credentials.access_expires_at,
            refresh_in_flight: false,
            refresh_attempt_id: None,
            refresh_outcome_unknown: false,
            refresh_commit_pending: false,
            status: CredentialStatus::Pending,
            supervisor_boot_generation: inventory.boot_generation.clone(),
            root_registry_fingerprint,
            root_fingerprints,
            approved_roots: roots_sorted,
            local_hmac_key,
        };
        // The complete idempotency and recovery state reaches secure storage
        // before the first Worker reservation can commit.
        store.replace(&pending)?;
        let reservation = protocol
            .reserve(&pending.oauth_access_token, &pending)
            .await?;
        validate_pending_scope(&reservation, &pending)?;
        pending.grant_generation = reservation.generation;
        pending.version = next_record_version(pending.version)?;
        store.replace(&pending)?;
        let activated = protocol
            .activate(&pending.oauth_access_token, &pending)
            .await?;
        validate_remote_scope(&activated, &pending, "active")?;
        pending.status = CredentialStatus::Active;
        pending.grant_expires_at = activated
            .expires_at
            .context("Worker activation omitted grant expiry")?;
        pending.version = next_record_version(pending.version)?;
        store.replace(&pending)?;
        record = Some(pending);
    }

    let record = record.context("browser enrollment record was not created")?;
    record.validate_for_link(&gateway_origin)?;
    let authority = make_fabric_authority(
        &record,
        &WorkerAuthSnapshot {
            mode: "browser".to_owned(),
            owner_key: record.owner_key.clone(),
            grant_id: record.grant_id.clone(),
            grant_generation: record.grant_generation,
            approved_roots: record.approved_roots.clone(),
        },
        &inventory,
    )?;
    drop(transition_guard.take());
    Ok(PreparedBrowserLink {
        record,
        authority,
        profile,
    })
}

async fn resume_pending<P: EnrollmentProtocol>(
    record: &mut CredentialRecord,
    store: &dyn CredentialStore,
    protocol: &P,
) -> Result<()> {
    anyhow::ensure!(
        record.cancel_operation_id.is_none(),
        "an explicitly cancelled enrollment attempt cannot be resumed"
    );
    let reservation = protocol.reserve(&record.oauth_access_token, record).await?;
    validate_pending_scope(&reservation, record)?;
    record.grant_generation = reservation.generation;
    record.version = next_record_version(record.version)?;
    store.replace(record)?;
    let activated = protocol
        .activate(&record.oauth_access_token, record)
        .await?;
    validate_remote_scope(&activated, record, "active")?;
    record.status = CredentialStatus::Active;
    record.grant_expires_at = activated
        .expires_at
        .context("Worker activation omitted grant expiry")?;
    record.version = next_record_version(record.version)?;
    store.replace(record)
}

// Keep the verified identity, local inventory, storage, protocol, and confirmation inputs
// explicit at this security-sensitive transition boundary.
#[allow(clippy::too_many_arguments)]
async fn reenroll_expired_grant<P, C, A>(
    record: &mut CredentialRecord,
    inventory: &LocalSupervisorInventory,
    identity: &VerifiedIdentity,
    store: &dyn CredentialStore,
    protocol: &P,
    confirmation: &mut C,
    transition_guard: &mut Option<Box<dyn TransitionGuard>>,
    acquire_transition: &mut A,
) -> Result<()>
where
    P: EnrollmentProtocol,
    C: EnrollmentConfirmation,
    A: FnMut() -> Result<Box<dyn TransitionGuard>>,
{
    let expected_version = record.version;
    drop(transition_guard.take());
    confirmation.confirm_owner(identity, &record.host_id)?;
    let available = inventory
        .roots
        .iter()
        .map(|root| root.name.clone())
        .collect::<Vec<_>>();
    let mut approved_roots = confirmation.approve_roots(&available)?;
    validate_approved_roots(&approved_roots, &available)?;
    approved_roots.sort();

    *transition_guard = Some(acquire_transition()?);
    let latest = store
        .load()?
        .context("saved browser credential was removed during expired-grant confirmation")?;
    anyhow::ensure!(
        latest.version == expected_version
            && latest.status == CredentialStatus::Active
            && latest.owner_key == record.owner_key
            && latest.host_id == record.host_id
            && latest.gateway_origin == record.gateway_origin
            && latest.grant_id == record.grant_id
            && latest.grant_generation == record.grant_generation,
        "saved browser credential changed during expired-grant confirmation; retry `fabric connect`"
    );
    *record = latest;
    let current = protocol
        .status(&record.oauth_access_token, &record.host_id)
        .await?
        .context("expired Host grant is no longer owned by the verified account")?;
    validate_remote_scope(&current, record, "active")?;
    anyhow::ensure!(
        current
            .expires_at
            .is_some_and(|expiry| expiry <= unix_now()),
        "Host grant expiry changed during re-enrollment confirmation"
    );

    let local_hmac_key = random_secret(32)?;
    let root_fingerprints = inventory_root_fingerprints(inventory, &local_hmac_key)?;
    let root_registry_fingerprint = root_registry_fingerprint(&local_hmac_key, &root_fingerprints)?;
    record.version = next_record_version(record.version)?;
    record.grant_id = Uuid::new_v4().to_string();
    record.grant_secret = random_secret(48)?;
    record.expected_generation = current.generation;
    record.grant_generation = current
        .generation
        .checked_add(1)
        .context("Host grant generation exhausted")?;
    record.grant_expires_at = 0;
    record.attempt_id = Uuid::new_v4().to_string();
    record.reserve_operation_id = Uuid::new_v4().to_string();
    record.activation_operation_id = Uuid::new_v4().to_string();
    record.cancel_operation_id = None;
    record.revoke_operation_id = None;
    record.refresh_in_flight = false;
    record.refresh_attempt_id = None;
    record.refresh_outcome_unknown = false;
    record.refresh_commit_pending = false;
    record.status = CredentialStatus::Pending;
    record.supervisor_boot_generation = inventory.boot_generation.clone();
    record.root_registry_fingerprint = root_registry_fingerprint;
    record.root_fingerprints = root_fingerprints;
    record.approved_roots = approved_roots;
    record.local_hmac_key = local_hmac_key;
    store.replace(record)?;

    let reservation = protocol.reserve(&record.oauth_access_token, record).await?;
    validate_pending_scope(&reservation, record)?;
    record.grant_generation = reservation.generation;
    record.version = next_record_version(record.version)?;
    store.replace(record)?;
    let activated = protocol
        .activate(&record.oauth_access_token, record)
        .await?;
    validate_remote_scope(&activated, record, "active")?;
    record.status = CredentialStatus::Active;
    record.grant_expires_at = activated
        .expires_at
        .context("Worker activation omitted grant expiry")?;
    record.version = next_record_version(record.version)?;
    store.replace(record)
}

async fn finish_saved_revocation<P: EnrollmentProtocol>(
    record: &CredentialRecord,
    protocol: &P,
    current: &EnrollmentStatus,
) -> Result<()> {
    anyhow::ensure!(
        record.status == CredentialStatus::Revoking,
        "credential record is not awaiting revocation"
    );
    let operation_id = record
        .revoke_operation_id
        .as_deref()
        .context("saved revocation omitted its idempotency key")?;
    let revoked = if current.status == "revoked"
        && current.host_id == record.host_id
        && current.owner_key.as_deref() == Some(record.owner_key.as_str())
        && current.grant_id.as_deref() == Some(record.grant_id.as_str())
        && current.generation == record.expected_generation.saturating_add(1)
        && current.operation_id.as_deref() == Some(operation_id)
        && current.operation_generation == Some(record.expected_generation.saturating_add(1))
        && current.root_names == record.approved_roots
    {
        current.clone()
    } else {
        anyhow::ensure!(
            current.status == "active" && current.generation == record.expected_generation,
            "Worker Host state changed before the saved revocation could commit"
        );
        validate_remote_scope(current, record, "active")?;
        protocol
            .revoke(
                &record.oauth_access_token,
                &record.host_id,
                record.expected_generation,
                operation_id,
            )
            .await?
    };
    anyhow::ensure!(
        revoked.status == "revoked"
            && revoked.host_id == record.host_id
            && revoked.owner_key.as_deref() == Some(record.owner_key.as_str())
            && revoked.grant_id.as_deref() == Some(record.grant_id.as_str())
            && revoked.generation == record.expected_generation.saturating_add(1)
            && revoked.operation_id.as_deref() == Some(operation_id)
            && revoked.operation_generation == Some(record.expected_generation.saturating_add(1))
            && revoked.root_names == record.approved_roots,
        "Host revocation did not reach the Worker authorization commit point"
    );
    Ok(())
}

async fn verify_or_finish_refresh<P: EnrollmentProtocol>(
    record: &mut CredentialRecord,
    store: &dyn CredentialStore,
    protocol: &P,
) -> Result<()> {
    if record.refresh_commit_pending {
        verify_refresh_identity(record, protocol).await?;
        refresh_complete(record)?;
        return store.replace(record);
    }
    if record.access_expires_at > unix_now().saturating_add(90) {
        return Ok(());
    }
    refresh_begin(record, Uuid::new_v4().to_string())?;
    store.replace(record)?;
    let old_credentials = OAuthCredentials {
        issuer: record.issuer.clone(),
        client_id: record.oauth_client_id.clone(),
        authorization_endpoint: record.oauth_authorization_endpoint.clone(),
        token_endpoint: record.oauth_token_endpoint.clone(),
        access_token: record.oauth_access_token.clone(),
        refresh_token: record.oauth_refresh_token.clone(),
        access_expires_at: record.access_expires_at,
    };
    let rotated = match protocol.refresh(&old_credentials).await {
        Ok(value) => value,
        Err(error) => {
            refresh_unknown(record);
            store
                .replace(record)
                .context("refresh outcome is unknown and secure state could not be committed")?;
            return Err(error)
                .context("OAuth refresh outcome is unknown; interactive recovery is required");
        }
    };
    refresh_commit(record, rotated)?;
    store
        .replace(record)
        .context("rotated OAuth credentials could not be committed; Link is halted")?;
    verify_refresh_identity(record, protocol).await?;
    refresh_complete(record)?;
    store.replace(record)
}

pub(crate) async fn refresh_record_for_link<P: EnrollmentProtocol>(
    record: &mut CredentialRecord,
    store: &dyn CredentialStore,
    protocol: &P,
) -> Result<()> {
    anyhow::ensure!(
        !record.refresh_in_flight
            && !record.refresh_outcome_unknown
            && !record.refresh_commit_pending,
        "interactive browser reauthentication is required to recover the saved OAuth rotation"
    );
    verify_or_finish_refresh(record, store, protocol).await?;
    anyhow::ensure!(
        record.status == CredentialStatus::Active,
        "browser Link requires an active Host grant"
    );
    let identity = protocol.identity(&record.oauth_access_token).await?;
    anyhow::ensure!(
        identity.owner_key == record.owner_key,
        "refreshed OAuth token changed the verified owner identity"
    );
    let status = protocol
        .status(&record.oauth_access_token, &record.host_id)
        .await?
        .context("refreshed OAuth token cannot verify the owner Host grant")?;
    validate_remote_scope(&status, record, "active")
}

async fn verify_refresh_identity<P: EnrollmentProtocol>(
    record: &CredentialRecord,
    protocol: &P,
) -> Result<()> {
    let identity = protocol.identity(&record.oauth_access_token).await?;
    anyhow::ensure!(
        identity.owner_key == record.owner_key,
        "refreshed OAuth token changed the verified owner identity"
    );
    let status = protocol
        .status(&record.oauth_access_token, &record.host_id)
        .await?
        .context("refreshed OAuth token cannot verify the owner Host grant")?;
    match record.status {
        CredentialStatus::Revoking => anyhow::ensure!(
            status.status == "active" && status.generation == record.expected_generation,
            "refreshed OAuth token changed the Host generation during revocation"
        ),
        CredentialStatus::Pending if status.status == "pending" => {
            validate_pending_scope(&status, record)?
        }
        CredentialStatus::Pending if status.status == "active" => {
            validate_remote_scope(&status, record, "active")?
        }
        CredentialStatus::Active => validate_remote_scope(&status, record, "active")?,
        _ => anyhow::bail!("refreshed OAuth token cannot verify the saved Host grant state"),
    }
    Ok(())
}

fn validate_pending_scope(status: &EnrollmentStatus, record: &CredentialRecord) -> Result<()> {
    anyhow::ensure!(
        status.status == "pending"
            && status.host_id == record.host_id
            && status.owner_key.as_deref() == Some(record.owner_key.as_str())
            && status.grant_id.as_deref() == Some(record.grant_id.as_str())
            && status.generation == record.expected_generation.saturating_add(1)
            && status.root_names == record.approved_roots,
        "Worker pending enrollment differs from the saved secure attempt"
    );
    anyhow::ensure!(
        status
            .attempt_id
            .as_deref()
            .is_none_or(|value| value == record.attempt_id)
            && status
                .reservation_operation_id
                .as_deref()
                .is_none_or(|value| value == record.reserve_operation_id),
        "Worker pending attempt identity changed"
    );
    Ok(())
}

fn validate_cancelled_pending(status: &EnrollmentStatus, record: &CredentialRecord) -> Result<()> {
    let operation_id = record
        .cancel_operation_id
        .as_deref()
        .context("saved pending cancellation omitted its operation id")?;
    anyhow::ensure!(
        status.status == "revoked"
            && status.host_id == record.host_id
            && status.owner_key.as_deref() == Some(record.owner_key.as_str())
            && status.grant_id.as_deref() == Some(record.grant_id.as_str())
            && status.generation == record.grant_generation.saturating_add(1)
            && status.root_names == record.approved_roots
            && status.attempt_id.as_deref() == Some(record.attempt_id.as_str())
            && status.reservation_operation_id.as_deref()
                == Some(record.reserve_operation_id.as_str())
            && status.cancel_operation_id.as_deref() == Some(operation_id),
        "Worker cancellation differs from the saved pending operation"
    );
    Ok(())
}

fn validate_remote_scope(
    status: &EnrollmentStatus,
    record: &CredentialRecord,
    expected_status: &str,
) -> Result<()> {
    anyhow::ensure!(
        status.status == expected_status
            && status.host_id == record.host_id
            && status.owner_key.as_deref() == Some(record.owner_key.as_str())
            && status.grant_id.as_deref() == Some(record.grant_id.as_str())
            && status.generation == record.grant_generation
            && status.root_names == record.approved_roots,
        "Worker Host grant, generation, or root scope changed; reconnect and explicitly reapprove"
    );
    Ok(())
}

fn validate_verified_identity(identity: &VerifiedIdentity) -> Result<()> {
    anyhow::ensure!(
        identity.owner_key.len() == 64
            && identity
                .owner_key
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit()),
        "Worker verified an invalid owner identity"
    );
    anyhow::ensure!(
        !identity.email.is_empty()
            && identity.email.len() <= 320
            && !identity.email.chars().any(char::is_control),
        "Worker verified an invalid owner display value"
    );
    Ok(())
}

fn validate_approved_roots(selected: &[String], available: &[String]) -> Result<()> {
    anyhow::ensure!(
        !selected.is_empty() && selected.len() <= 32,
        "root approval must select between one and 32 names"
    );
    let selected_set = selected.iter().collect::<BTreeSet<_>>();
    anyhow::ensure!(
        selected_set.len() == selected.len()
            && selected.iter().all(|root| available.contains(root)),
        "root approval must contain only unique Supervisor-configured names"
    );
    Ok(())
}

fn next_record_version(version: u64) -> Result<u64> {
    version
        .checked_add(1)
        .context("credential record version exhausted")
}

struct TerminalEnrollmentConfirmation;

impl EnrollmentConfirmation for TerminalEnrollmentConfirmation {
    fn confirm_owner(&mut self, identity: &VerifiedIdentity, host_id: &str) -> Result<()> {
        require_confirmed_identity(identity, host_id)
    }

    fn approve_roots(&mut self, available: &[String]) -> Result<Vec<String>> {
        select_roots(available)
    }
}

pub async fn run_connect(options: BrowserConnectOptions) -> Result<()> {
    let backend = crate::session_control::SessionBackend::local_control().await?;
    let inventory = backend.fabric_inventory().await?;
    let gateway_origin = Url::parse(&options.gateway_url)?
        .origin()
        .ascii_serialization();
    let profile = profile_key(&gateway_origin, "", &inventory.host_id);
    let link_lease = ProfileLock::link_lifetime(&profile)?;
    let protocol = BrowserOAuth::new(options.clone())?;
    let mut confirmation = TerminalEnrollmentConfirmation;
    let prepared = prepare_browser_link_with_transition(
        &options,
        inventory,
        &protocol,
        &OsCredentialStoreFactory,
        &mut confirmation,
        || Ok(Box::new(ProfileLock::transition(&profile)?)),
    )
    .await?;
    crate::gateway::run_browser_link_with_lease(protocol, prepared, link_lease).await
}

pub async fn run_logout(options: BrowserConnectOptions) -> Result<()> {
    let host_id = options
        .host_id
        .clone()
        .unwrap_or(crate::host_identity::resolve()?);
    let gateway_origin = Url::parse(&options.gateway_url)?
        .origin()
        .ascii_serialization();
    let profile = profile_key(&gateway_origin, "", &host_id);
    let stores = OsCredentialStoreFactory;
    let store = stores.open(&profile)?;
    let protocol = BrowserOAuth::new(options.clone())?;
    let did_logout = logout_saved_profile_with_transition(
        &host_id,
        &gateway_origin,
        &options,
        store.as_ref(),
        &protocol,
        || Ok(Box::new(ProfileLock::transition(&profile)?)),
    )
    .await?;
    if did_logout {
        println!("Fabric Host grant revoked and local secure credentials removed.");
    } else {
        println!("No saved browser credential exists for this local Host.");
    }
    Ok(())
}

#[cfg(test)]
pub async fn logout_saved_profile<P: EnrollmentProtocol>(
    host_id: &str,
    gateway_origin: &str,
    options: &BrowserConnectOptions,
    store: &dyn CredentialStore,
    protocol: &P,
) -> Result<bool> {
    logout_saved_profile_with_transition(host_id, gateway_origin, options, store, protocol, || {
        Ok(Box::new(NoopTransitionGuard))
    })
    .await
}

async fn logout_saved_profile_with_transition<P, A>(
    host_id: &str,
    gateway_origin: &str,
    options: &BrowserConnectOptions,
    store: &dyn CredentialStore,
    protocol: &P,
    mut acquire_transition: A,
) -> Result<bool>
where
    P: EnrollmentProtocol,
    A: FnMut() -> Result<Box<dyn TransitionGuard>>,
{
    let Some(mut record) = store.load()? else {
        return Ok(false);
    };
    anyhow::ensure!(
        record.host_id == host_id && record.gateway_origin == gateway_origin,
        "saved browser credential belongs to another Fabric Host"
    );
    let mut transition_guard: Option<Box<dyn TransitionGuard>>;
    let config_matches = protocol
        .oauth_configuration_matches(options, &record)
        .await?;
    let reauthenticate = record.refresh_in_flight
        || record.refresh_outcome_unknown
        || record.refresh_commit_pending
        || !config_matches
        || (record.status != CredentialStatus::Active && record.access_expires_at <= unix_now());
    if reauthenticate {
        let expected_version = record.version;
        let expected_status = record.status;
        let expected_grant_id = record.grant_id.clone();
        let replacement = authorize_saved_profile(&record, options, protocol).await?;
        transition_guard = Some(acquire_transition()?);
        record = store.load()?.context(
            "saved browser credential was removed during OAuth recovery; logout was not completed",
        )?;
        anyhow::ensure!(
            record.version == expected_version
                && record.status == expected_status
                && record.grant_id == expected_grant_id
                && record.owner_key == replacement.1.owner_key
                && record.host_id == host_id
                && record.gateway_origin == gateway_origin,
            "saved browser credential changed during OAuth recovery; retry `fabric logout`"
        );
        replace_saved_oauth(&mut record, replacement.0)?;
        store.replace(&record)?;
    } else {
        let expected_version = record.version;
        transition_guard = Some(acquire_transition()?);
        record = store
            .load()?
            .context("saved browser credential was removed before logout; retry `fabric logout`")?;
        anyhow::ensure!(
            record.version == expected_version
                && record.host_id == host_id
                && record.gateway_origin == gateway_origin,
            "saved browser credential changed before logout; retry `fabric logout`"
        );
    }
    if let Err(refresh_error) = verify_or_finish_refresh(&mut record, store, protocol).await {
        let durable = store
            .load()?
            .context("saved browser credential disappeared during OAuth refresh; retry logout")?;
        if !durable.refresh_outcome_unknown {
            return Err(refresh_error);
        }
        let expected_version = durable.version;
        let replacement = {
            drop(transition_guard.take());
            authorize_saved_profile(&durable, options, protocol).await?
        };
        transition_guard = Some(acquire_transition()?);
        record = store.load()?.context(
            "saved browser credential was removed during OAuth recovery; retry `fabric logout`",
        )?;
        anyhow::ensure!(
            record.version == expected_version
                && record.refresh_outcome_unknown
                && record.owner_key == replacement.1.owner_key
                && record.host_id == host_id
                && record.gateway_origin == gateway_origin,
            "saved browser credential changed during OAuth recovery; retry `fabric logout`"
        );
        replace_saved_oauth(&mut record, replacement.0)?;
        store.replace(&record)?;
    }
    let identity = protocol.identity(&record.oauth_access_token).await?;
    anyhow::ensure!(
        identity.owner_key == record.owner_key,
        "verified Fabric owner differs from the saved credential"
    );
    let current = protocol.status(&record.oauth_access_token, host_id).await?;
    match record.status {
        CredentialStatus::Pending => {
            cancel_saved_pending(&mut record, store, protocol, current.as_ref()).await?;
        }
        CredentialStatus::Active => {
            let current = current
                .as_ref()
                .context("saved Host enrollment is not owned by the verified account")?;
            validate_remote_scope(current, &record, "active")?;
            record.status = CredentialStatus::Revoking;
            record.expected_generation = record.grant_generation;
            record.revoke_operation_id = Some(Uuid::new_v4().to_string());
            record.version = next_record_version(record.version)?;
            store.replace(&record)?;
            let operation_id = record
                .revoke_operation_id
                .as_deref()
                .context("revocation id was not saved")?;
            let revoked = protocol
                .revoke(
                    &record.oauth_access_token,
                    host_id,
                    record.expected_generation,
                    operation_id,
                )
                .await?;
            anyhow::ensure!(
                revoked.status == "revoked"
                    && revoked.host_id == record.host_id
                    && revoked.owner_key.as_deref() == Some(record.owner_key.as_str())
                    && revoked.grant_id.as_deref() == Some(record.grant_id.as_str())
                    && revoked.generation == record.expected_generation.saturating_add(1)
                    && revoked.operation_id.as_deref() == Some(operation_id)
                    && revoked.operation_generation
                        == Some(record.expected_generation.saturating_add(1))
                    && revoked.root_names == record.approved_roots,
                "Host revocation did not commit; secure credentials remain available for retry"
            );
        }
        CredentialStatus::Revoking => {
            let current = current
                .as_ref()
                .context("saved Host enrollment is not owned by the verified account")?;
            anyhow::ensure!(
                record.revoke_operation_id.is_some(),
                "saved revocation omitted its idempotency key"
            );
            let committed = current.status == "revoked"
                && current.host_id == record.host_id
                && current.owner_key.as_deref() == Some(record.owner_key.as_str())
                && current.grant_id.as_deref() == Some(record.grant_id.as_str())
                && current.generation == record.expected_generation.saturating_add(1)
                && current.operation_id.as_deref() == record.revoke_operation_id.as_deref()
                && current.operation_generation
                    == Some(record.expected_generation.saturating_add(1))
                && current.root_names == record.approved_roots;
            let revoked = if committed {
                current.clone()
            } else {
                anyhow::ensure!(
                    current.status == "active" && current.generation == record.expected_generation,
                    "Worker Host state changed during saved revocation"
                );
                validate_remote_scope(current, &record, "active")?;
                protocol
                    .revoke(
                        &record.oauth_access_token,
                        host_id,
                        record.expected_generation,
                        record
                            .revoke_operation_id
                            .as_deref()
                            .context("saved revocation omitted its idempotency key")?,
                    )
                    .await?
            };
            anyhow::ensure!(
                revoked.status == "revoked"
                    && revoked.host_id == record.host_id
                    && revoked.owner_key.as_deref() == Some(record.owner_key.as_str())
                    && revoked.grant_id.as_deref() == Some(record.grant_id.as_str())
                    && revoked.generation == record.expected_generation.saturating_add(1)
                    && revoked.operation_id.as_deref() == record.revoke_operation_id.as_deref()
                    && revoked.operation_generation
                        == Some(record.expected_generation.saturating_add(1))
                    && revoked.root_names == record.approved_roots,
                "Host revocation did not commit; secure credentials remain available for retry"
            );
        }
    }
    store
        .delete()
        .context("Worker grant was revoked but secure credential removal failed")?;
    drop(transition_guard.take());
    Ok(true)
}

async fn cancel_saved_pending<P: EnrollmentProtocol>(
    record: &mut CredentialRecord,
    store: &dyn CredentialStore,
    protocol: &P,
    current: Option<&EnrollmentStatus>,
) -> Result<()> {
    let pending = match current {
        Some(status) if status.status == "revoked" => {
            validate_cancelled_pending(status, record)?;
            return Ok(());
        }
        Some(status) if status.status == "pending" => {
            validate_pending_scope(status, record)?;
            status.clone()
        }
        Some(_) => anyhow::bail!("Worker Host state changed during pending cancellation"),
        None => {
            anyhow::ensure!(
                record.cancel_operation_id.is_none(),
                "saved cancellation cannot be reconciled without its owner-scoped Worker status"
            );
            let reservation = protocol
                .reserve(&record.oauth_access_token, record)
                .await
                .context("saved pending reservation has no remote status; exact retry failed")?;
            validate_pending_scope(&reservation, record)?;
            reservation
        }
    };

    let mut record_changed = false;
    if record.grant_generation != pending.generation {
        record.grant_generation = pending.generation;
        record_changed = true;
    }
    let operation_id = match record.cancel_operation_id.clone() {
        Some(operation_id) => operation_id,
        None => {
            let operation_id = Uuid::new_v4().to_string();
            record.cancel_operation_id = Some(operation_id.clone());
            record_changed = true;
            operation_id
        }
    };
    if record_changed {
        record.version = next_record_version(record.version)?;
        store.replace(record)?;
    }

    let cancelled = protocol
        .cancel(
            &record.oauth_access_token,
            &record.host_id,
            &pending,
            &operation_id,
        )
        .await?;
    validate_cancelled_pending(&cancelled, record)
}

async fn enrollment_status(
    oauth: &BrowserOAuth,
    access_token: &str,
    host_id: &str,
) -> Result<Option<EnrollmentStatus>> {
    let request = BrowserApi::new(oauth, access_token).await?;
    let url =
        Url::parse(&oauth.options.gateway_url)?.join(&format!("/v1/enrollments/{host_id}"))?;
    request.get_status(url, host_id).await
}

async fn reserve_record(
    oauth: &BrowserOAuth,
    access_token: &str,
    record: &CredentialRecord,
) -> Result<EnrollmentStatus> {
    let mut request = BrowserApi::new(oauth, access_token).await?;
    let url =
        Url::parse(&record.gateway_origin)?.join(&format!("/v1/enrollments/{}", record.host_id))?;
    let digest_hex = hex_encode(&Sha256::digest(record.grant_secret.as_bytes()));
    let value = request
        .post_json(
            url,
            &json!({
                "attempt_id": record.attempt_id,
                "grant_id": record.grant_id,
                "grant_digest": digest_hex,
                "root_names": record.approved_roots,
                "expected_generation": record.expected_generation,
                "operation_id": record.reserve_operation_id,
            }),
        )
        .await?;
    parse_enrollment_status(value, &record.host_id)
}

async fn activate_record(
    oauth: &BrowserOAuth,
    access_token: &str,
    record: &CredentialRecord,
) -> Result<EnrollmentStatus> {
    let mut request = BrowserApi::new(oauth, access_token).await?;
    let url = Url::parse(&record.gateway_origin)?
        .join(&format!("/v1/enrollments/{}/activate", record.host_id))?;
    request.grant = Some(&record.grant_secret);
    let value = request
        .post_json(
            url,
            &json!({
                "attempt_id": record.attempt_id,
                "reservation_operation_id": record.reserve_operation_id,
                "expected_generation": record.grant_generation,
                "activation_operation_id": record.activation_operation_id,
            }),
        )
        .await?;
    parse_enrollment_status(value, &record.host_id)
}

async fn cancel_enrollment(
    oauth: &BrowserOAuth,
    access_token: &str,
    host_id: &str,
    status: &EnrollmentStatus,
    operation_id: &str,
) -> Result<EnrollmentStatus> {
    let mut request = BrowserApi::new(oauth, access_token).await?;
    let attempt_id = status
        .attempt_id
        .as_deref()
        .context("owner pending status omitted attempt id")?;
    let reservation_operation_id = status
        .reservation_operation_id
        .as_deref()
        .context("owner pending status omitted reservation operation id")?;
    let url = Url::parse(&oauth.options.gateway_url)?
        .join(&format!("/v1/enrollments/{host_id}/pending"))?;
    let value = request
        .post_json_with_method(
            url,
            reqwest::Method::DELETE,
            &json!({
                "attempt_id": attempt_id,
                "reservation_operation_id": reservation_operation_id,
                "expected_generation": status.generation,
                "operation_id": operation_id,
            }),
        )
        .await?;
    parse_enrollment_status(value, host_id)
}

async fn revoke_generation(
    oauth: &BrowserOAuth,
    access_token: &str,
    host_id: &str,
    generation: u64,
    operation_id: &str,
) -> Result<EnrollmentStatus> {
    let mut request = BrowserApi::new(oauth, access_token).await?;
    let url =
        Url::parse(&oauth.options.gateway_url)?.join(&format!("/v1/enrollments/{host_id}"))?;
    let value = request
        .post_json_with_method(
            url,
            reqwest::Method::DELETE,
            &json!({
                "operation_id": operation_id,
                "expected_generation": generation,
            }),
        )
        .await?;
    parse_enrollment_status(value, host_id)
}

fn parse_enrollment_status(value: Value, expected_host_id: &str) -> Result<EnrollmentStatus> {
    let worker: WorkerEnrollmentStatus =
        serde_json::from_value(value).context("Worker enrollment response is malformed")?;
    let status =
        EnrollmentStatus::try_from(worker).context("Worker enrollment deadline is malformed")?;
    anyhow::ensure!(
        status.host_id == expected_host_id,
        "Worker enrollment response changed Host identity"
    );
    anyhow::ensure!(
        status.generation > 0 && matches!(status.status.as_str(), "pending" | "active" | "revoked"),
        "Worker enrollment status is invalid"
    );
    match status.status.as_str() {
        "pending" => anyhow::ensure!(
            status.pending_expires_at.is_some(),
            "Worker pending enrollment omitted its expiry"
        ),
        "active" => anyhow::ensure!(
            status.expires_at.is_some(),
            "Worker active grant omitted its expiry"
        ),
        _ => {}
    }
    anyhow::ensure!(
        !status.root_names.is_empty() && status.root_names.len() <= 32,
        "Worker enrollment root scope is invalid"
    );
    anyhow::ensure!(
        status
            .root_names
            .iter()
            .all(|name| crate::named_roots::validate_root_name(name).is_ok()),
        "Worker enrollment returned an invalid root name"
    );
    anyhow::ensure!(
        status.root_names.windows(2).all(|pair| pair[0] < pair[1]),
        "Worker enrollment root scope is not canonical"
    );
    Ok(status)
}

struct BrowserApi<'a> {
    oauth: &'a BrowserOAuth,
    access_token: &'a str,
    grant: Option<&'a str>,
}

impl<'a> BrowserApi<'a> {
    async fn new(oauth: &'a BrowserOAuth, access_token: &'a str) -> Result<Self> {
        anyhow::ensure!(
            access_token.starts_with("oauth:"),
            "unsupported OAuth token"
        );
        Ok(Self {
            oauth,
            access_token,
            grant: None,
        })
    }

    async fn post_json(&mut self, url: Url, body: &Value) -> Result<Value> {
        self.post_json_with_method(url, reqwest::Method::POST, body)
            .await
    }

    async fn get_status(&self, url: Url, host_id: &str) -> Result<Option<EnrollmentStatus>> {
        validate_permitted_url(&url, &self.oauth.options.permitted_origins)?;
        let host = url.host_str().context("enrollment endpoint has no host")?;
        let port = url
            .port_or_known_default()
            .context("enrollment endpoint has no port")?;
        let addrs = resolve_public_addresses(host, port).await?;
        let client = pinned_http_client(host, &addrs)?;
        let response = client
            .get(url)
            .bearer_auth(self.access_token)
            .send()
            .await
            .context("Fabric enrollment status request failed")?;
        anyhow::ensure!(
            !response.status().is_redirection(),
            "Fabric enrollment redirects are disabled"
        );
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        anyhow::ensure!(
            response.status().is_success(),
            "Fabric enrollment status was rejected"
        );
        let bytes = read_bounded_response(response).await?;
        let value: Value =
            serde_json::from_slice(&bytes).context("Fabric enrollment status is malformed")?;
        parse_enrollment_status(value, host_id).map(Some)
    }

    async fn post_json_with_method(
        &mut self,
        url: Url,
        method: reqwest::Method,
        body: &Value,
    ) -> Result<Value> {
        validate_permitted_url(&url, &self.oauth.options.permitted_origins)?;
        let host = url.host_str().context("enrollment endpoint has no host")?;
        let port = url
            .port_or_known_default()
            .context("enrollment endpoint has no port")?;
        let addrs = resolve_public_addresses(host, port).await?;
        let client = pinned_http_client(host, &addrs)?;
        let mut request = client
            .request(method, url)
            .bearer_auth(self.access_token)
            .json(body);
        if let Some(grant) = self.grant {
            request = request.header("x-temote-fabric-host-grant", grant);
        }
        let response = request
            .send()
            .await
            .context("Fabric enrollment request failed")?;
        anyhow::ensure!(
            !response.status().is_redirection(),
            "Fabric enrollment redirects are disabled"
        );
        anyhow::ensure!(
            response.status().is_success(),
            "Fabric enrollment was rejected"
        );
        let bytes = read_bounded_response(response).await?;
        serde_json::from_slice(&bytes).context("Fabric enrollment response is malformed")
    }
}

pub fn require_confirmed_identity(identity: &VerifiedIdentity, host_id: &str) -> Result<()> {
    use std::io::{IsTerminal, Write as _};
    anyhow::ensure!(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "browser enrollment requires an interactive terminal to confirm the verified owner"
    );
    eprintln!("Cloudflare verified this account as: {}", identity.email);
    eprint!("To claim Fabric Host {host_id}, type its id: ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    anyhow::ensure!(
        answer.trim() == host_id,
        "browser enrollment was not confirmed"
    );
    Ok(())
}

pub fn select_roots(available: &[String]) -> Result<Vec<String>> {
    use std::io::{IsTerminal, Write as _};
    anyhow::ensure!(
        !available.is_empty(),
        "the running Supervisor has no configured named roots; configure roots and restart Supervisor before reconnecting"
    );
    anyhow::ensure!(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "browser enrollment requires an interactive terminal to approve named roots"
    );
    eprintln!("Configured named roots:");
    for name in available {
        eprintln!("  {name}");
    }
    eprint!("Enter root names to authorize, separated by commas: ");
    std::io::stderr().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let selected = input
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    anyhow::ensure!(
        !selected.is_empty() && selected.iter().all(|name| available.contains(name)),
        "root selection must contain only configured root names"
    );
    let selected = selected.into_iter().collect::<Vec<_>>();
    eprintln!("Fabric access will be limited to: {}", selected.join(", "));
    eprint!("Approve this root scope? Type yes: ");
    std::io::stderr().flush()?;
    input.clear();
    std::io::stdin().read_line(&mut input)?;
    anyhow::ensure!(input.trim() == "yes", "root scope was not approved");
    Ok(selected)
}

pub fn validate_oauth_options(options: &BrowserConnectOptions) -> Result<()> {
    let gateway = Url::parse(&options.gateway_url).context("Fabric URL is invalid")?;
    anyhow::ensure!(
        gateway.scheme() == "https"
            && gateway.host_str().is_some()
            && gateway.username().is_empty()
            && gateway.password().is_none(),
        "Fabric URL must be an HTTPS origin without credentials"
    );
    anyhow::ensure!(
        gateway.path() == "/" && gateway.query().is_none() && gateway.fragment().is_none(),
        "Fabric URL must be an origin without a path"
    );
    let issuer = Url::parse(&options.issuer).context("OAuth issuer pin is invalid")?;
    anyhow::ensure!(
        issuer.scheme() == "https"
            && issuer.path() == "/"
            && issuer.query().is_none()
            && issuer.fragment().is_none(),
        "OAuth issuer pin must be an HTTPS origin"
    );
    anyhow::ensure!(
        !options.client_id.is_empty() && options.client_id.len() <= 256,
        "OAuth public client ID is invalid"
    );
    anyhow::ensure!(
        !options.resource.is_empty() && options.resource.len() <= 2048,
        "OAuth resource indicator is invalid"
    );
    anyhow::ensure!(
        options.resource == gateway.origin().ascii_serialization(),
        "OAuth resource indicator must equal the configured Fabric origin"
    );
    anyhow::ensure!(
        !options.permitted_origins.is_empty(),
        "explicit permitted OAuth origins are required"
    );
    for origin in &options.permitted_origins {
        let parsed = Url::parse(origin).context("permitted OAuth origin is invalid")?;
        anyhow::ensure!(
            parsed.scheme() == "https"
                && parsed.path() == "/"
                && parsed.query().is_none()
                && parsed.fragment().is_none(),
            "permitted origins must be HTTPS origins"
        );
    }
    anyhow::ensure!(
        options
            .permitted_origins
            .contains(&gateway.origin().ascii_serialization()),
        "Fabric origin is not in the explicit permitted-origin set"
    );
    anyhow::ensure!(
        options
            .permitted_origins
            .contains(&issuer.origin().ascii_serialization()),
        "OAuth issuer origin is not in the explicit permitted-origin set"
    );
    Ok(())
}

fn validate_permitted_url(url: &Url, permitted_origins: &[String]) -> Result<()> {
    anyhow::ensure!(
        url.scheme() == "https" && url.username().is_empty() && url.password().is_none(),
        "OAuth endpoints must use HTTPS without embedded credentials"
    );
    anyhow::ensure!(
        url.query().is_none() && url.fragment().is_none(),
        "OAuth endpoint metadata URL contains query or fragment"
    );
    anyhow::ensure!(
        permitted_origins.contains(&url.origin().ascii_serialization()),
        "OAuth endpoint origin is not explicitly permitted"
    );
    Ok(())
}

async fn resolve_public_addresses(host: &str, port: u16) -> Result<Vec<SocketAddr>> {
    let addrs = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::lookup_host((host, port)),
    )
    .await
    .context("OAuth endpoint DNS lookup timed out")??
    .collect::<Vec<_>>();
    anyhow::ensure!(
        !addrs.is_empty() && addrs.len() <= 32,
        "OAuth endpoint DNS result is unavailable or oversized"
    );
    anyhow::ensure!(
        addrs.iter().all(|addr| is_public_ip(addr.ip())),
        "OAuth endpoint DNS resolved to a non-public address"
    );
    let mut unique = addrs;
    unique.sort();
    unique.dedup();
    Ok(unique)
}

fn pinned_http_client(host: &str, addresses: &[SocketAddr]) -> Result<reqwest::Client> {
    anyhow::ensure!(
        !addresses.is_empty() && addresses.iter().all(|address| is_public_ip(address.ip())),
        "OAuth endpoint addresses are not public"
    );
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(HTTP_TIMEOUT)
        .pool_max_idle_per_host(0)
        .resolve_to_addrs(host, addresses)
        .user_agent(concat!("temote-fabric/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("could not create pinned OAuth HTTP client")
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(value) => {
            let [a, b, c, _] = value.octets();
            !value.is_private()
                && !value.is_loopback()
                && !value.is_link_local()
                && !value.is_broadcast()
                && !value.is_unspecified()
                && !value.is_multicast()
                && !(a == 100 && (64..=127).contains(&b))
                && !(a == 192 && b == 0 && c == 0)
                && !(a == 192 && b == 0 && c == 2)
                && !(a == 192 && b == 88 && c == 99)
                && !(a == 198 && (b == 18 || b == 19))
                && !(a == 198 && b == 51 && c == 100)
                && !(a == 203 && b == 0 && c == 113)
                && a < 224
        }
        IpAddr::V6(value) => {
            let segments = value.segments();
            (segments[0] & 0xe000) == 0x2000
                && !value.is_loopback()
                && !value.is_unspecified()
                && !value.is_multicast()
                && !value.is_unicast_link_local()
                && !(segments[0] == 0x2001 && segments[1] == 0x0db8)
        }
    }
}

async fn read_bounded_response(mut response: reqwest::Response) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("HTTP response read failed")?
    {
        anyhow::ensure!(
            bytes.len().saturating_add(chunk.len()) <= MAX_HTTP_BYTES,
            "HTTP response is oversized"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn wait_for_loopback_code(
    listener: tokio::net::TcpListener,
    expected_state: &str,
) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    loop {
        let (mut stream, peer) = listener.accept().await?;
        anyhow::ensure!(
            peer.ip().is_loopback(),
            "OAuth callback peer is not loopback"
        );
        let mut request = vec![0_u8; 8192];
        let count = stream.read(&mut request).await?;
        anyhow::ensure!(count < request.len(), "OAuth callback request is oversized");
        let text =
            std::str::from_utf8(&request[..count]).context("OAuth callback request is invalid")?;
        let line = text
            .lines()
            .next()
            .context("OAuth callback request is empty")?;
        let path = line
            .strip_prefix("GET ")
            .and_then(|rest| rest.split_once(' '))
            .map(|(path, _)| path)
            .context("OAuth callback request method is invalid")?;
        let callback = Url::parse(&format!("http://127.0.0.1{path}"))?;
        if callback.path() != "/callback" {
            continue;
        }
        let mut state_values = callback
            .query_pairs()
            .filter(|(key, _)| key == "state")
            .map(|(_, value)| value.into_owned());
        let mut code_values = callback
            .query_pairs()
            .filter(|(key, _)| key == "code")
            .map(|(_, value)| value.into_owned());
        let state = state_values
            .next()
            .context("OAuth callback omitted state")?;
        let code = code_values.next().context("OAuth callback omitted code")?;
        anyhow::ensure!(
            state_values.next().is_none() && code_values.next().is_none(),
            "OAuth callback is ambiguous"
        );
        anyhow::ensure!(
            state == expected_state && !code.is_empty() && code.len() <= 4096,
            "OAuth callback state did not match"
        );
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 31\r\nConnection: close\r\n\r\nLogin complete. You may close it.").await?;
        return Ok(code);
    }
}

fn open_authorization_url(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(target_os = "linux")]
    let program = "xdg-open";
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    anyhow::bail!("browser enrollment is unavailable on this operating system");
    let status = Command::new(program)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("could not launch the system browser")?;
    anyhow::ensure!(status.success(), "system browser launcher failed");
    Ok(())
}

fn random_url_token(bytes: usize) -> Result<String> {
    let mut random = vec![0; bytes];
    SystemRandom::new()
        .fill(&mut random)
        .map_err(|_| anyhow::anyhow!("secure random generator failed"))?;
    Ok(URL_SAFE_NO_PAD.encode(random))
}

pub fn random_secret(bytes: usize) -> Result<String> {
    random_url_token(bytes)
}

pub fn profile_key(origin: &str, owner_key: &str, host_id: &str) -> String {
    // Owner identity is stored inside the atomic credential record and checked
    // against Access before use. The locator omits it so reconnect can find the
    // record before refreshing the user's OAuth token; a different owner can
    // never overwrite an occupied Host profile without first revoking it.
    let _ = owner_key;
    hex_encode(&Sha256::digest(format!("{origin}\0{host_id}").as_bytes()))
}

pub fn refresh_begin(record: &mut CredentialRecord, attempt_id: String) -> Result<()> {
    anyhow::ensure!(
        matches!(
            record.status,
            CredentialStatus::Active | CredentialStatus::Pending | CredentialStatus::Revoking
        ),
        "cannot refresh a terminal enrollment"
    );
    anyhow::ensure!(
        !record.refresh_in_flight && !record.refresh_outcome_unknown,
        "prior refresh outcome is unresolved"
    );
    record.refresh_in_flight = true;
    record.refresh_attempt_id = Some(attempt_id);
    record.version = record
        .version
        .checked_add(1)
        .context("credential version exhausted")?;
    Ok(())
}

pub fn refresh_unknown(record: &mut CredentialRecord) {
    record.refresh_in_flight = false;
    record.refresh_outcome_unknown = true;
}

pub fn refresh_commit(record: &mut CredentialRecord, rotated: OAuthCredentials) -> Result<()> {
    anyhow::ensure!(
        record.refresh_in_flight && record.refresh_attempt_id.is_some(),
        "no refresh attempt is pending"
    );
    anyhow::ensure!(
        rotated.issuer == record.issuer
            && rotated.client_id == record.oauth_client_id
            && rotated.authorization_endpoint == record.oauth_authorization_endpoint
            && rotated.token_endpoint == record.oauth_token_endpoint,
        "refreshed OAuth issuer, client, or endpoint metadata changed"
    );
    record.oauth_access_token = rotated.access_token;
    record.oauth_refresh_token = rotated.refresh_token;
    record.access_expires_at = rotated.access_expires_at;
    record.refresh_in_flight = false;
    record.refresh_outcome_unknown = false;
    record.refresh_commit_pending = true;
    record.version = record
        .version
        .checked_add(1)
        .context("credential version exhausted")?;
    Ok(())
}

pub fn refresh_complete(record: &mut CredentialRecord) -> Result<()> {
    anyhow::ensure!(
        record.refresh_commit_pending && record.refresh_attempt_id.is_some(),
        "no committed refresh awaits identity verification"
    );
    record.refresh_commit_pending = false;
    record.refresh_attempt_id = None;
    record.version = record
        .version
        .checked_add(1)
        .context("credential version exhausted")?;
    Ok(())
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn state_directory() -> Result<PathBuf> {
    let base = crate::platform_paths::state_dir()
        .or_else(crate::platform_paths::data_local_dir)
        .context("no private state directory is available")?;
    let path = base.join("temote-mcp").join("fabric-browser");
    std::fs::create_dir_all(&path).context("cannot create private browser-auth state directory")?;
    #[cfg(unix)]
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    let metadata = std::fs::symlink_metadata(&path)?;
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "browser-auth state directory must be a real directory"
    );
    Ok(path)
}

#[cfg(target_os = "linux")]
fn ensure_secret_tool() -> Result<()> {
    let output = Command::new("secret-tool")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    anyhow::ensure!(
        output.is_ok(),
        "Linux Secret Service is unavailable; install and unlock a Secret Service provider"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn read_pointer(path: &Path) -> Result<Option<u64>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => anyhow::bail!("Secret Service record pointer could not be read"),
    };
    let value: u64 =
        serde_json::from_slice(&bytes).context("Secret Service record pointer is malformed")?;
    anyhow::ensure!(value > 0, "Secret Service record pointer is invalid");
    Ok(Some(value))
}

#[cfg(target_os = "linux")]
fn write_pointer_atomic(path: &Path, version: u64) -> Result<()> {
    let parent = path.parent().context("credential pointer has no parent")?;
    let temporary = parent.join(format!(".pointer-{}.tmp", Uuid::new_v4()));
    let bytes = serde_json::to_vec(&version)?;
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path).context("Secret Service record pointer commit failed")?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn secret_tool_store(profile: &str, version: u64, bytes: &[u8]) -> Result<()> {
    let mut child = Command::new("secret-tool")
        .args(secret_tool_store_args(profile, version))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("Linux Secret Service store could not start")?;
    child
        .stdin
        .take()
        .context("Secret Service stdin unavailable")?
        .write_all(bytes)?;
    let status = child.wait()?;
    anyhow::ensure!(
        status.success(),
        "Linux Secret Service refused the credential record"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn secret_tool_lookup(profile: &str, version: u64) -> Result<Vec<u8>> {
    let output = Command::new("secret-tool")
        .args(secret_tool_lookup_args(profile, version))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .context("Linux Secret Service lookup failed")?;
    anyhow::ensure!(
        output.status.success(),
        "Linux Secret Service credential is unavailable"
    );
    anyhow::ensure!(
        output.stdout.len() <= MAX_RECORD_BYTES,
        "secure credential record is oversized"
    );
    Ok(output.stdout)
}

#[cfg(target_os = "linux")]
fn secret_tool_clear(profile: &str, version: u64) -> Result<()> {
    let status = Command::new("secret-tool")
        .args(secret_tool_clear_args(profile, version))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("Linux Secret Service removal failed")?;
    anyhow::ensure!(status.success(), "Linux Secret Service removal failed");
    Ok(())
}

#[cfg(target_os = "linux")]
fn secret_tool_profile_key(profile: &str, version: u64) -> String {
    format!("{profile}-{version}")
}

#[cfg(target_os = "linux")]
fn secret_tool_store_args(profile: &str, version: u64) -> [String; 6] {
    [
        "store".to_owned(),
        "--label=Temote Fabric browser credential".to_owned(),
        "service".to_owned(),
        STORE_SERVICE.to_owned(),
        "profile".to_owned(),
        secret_tool_profile_key(profile, version),
    ]
}

#[cfg(target_os = "linux")]
fn secret_tool_lookup_args(profile: &str, version: u64) -> [String; 5] {
    [
        "lookup".to_owned(),
        "service".to_owned(),
        STORE_SERVICE.to_owned(),
        "profile".to_owned(),
        secret_tool_profile_key(profile, version),
    ]
}

#[cfg(target_os = "linux")]
fn secret_tool_clear_args(profile: &str, version: u64) -> [String; 5] {
    [
        "clear".to_owned(),
        "service".to_owned(),
        STORE_SERVICE.to_owned(),
        "profile".to_owned(),
        secret_tool_profile_key(profile, version),
    ]
}

#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;
    use std::sync::Arc;

    #[derive(Default)]
    pub struct MemoryStore(Mutex<Option<CredentialRecord>>);

    impl CredentialStore for MemoryStore {
        fn load(&self) -> Result<Option<CredentialRecord>> {
            Ok(self.0.lock().unwrap().clone())
        }
        fn replace(&self, record: &CredentialRecord) -> Result<()> {
            *self.0.lock().unwrap() = Some(record.clone());
            Ok(())
        }
        fn delete(&self) -> Result<()> {
            *self.0.lock().unwrap() = None;
            Ok(())
        }
    }

    #[test]
    fn browser_options_require_explicit_https_issuer_and_origin_pins() {
        let valid = BrowserConnectOptions {
            gateway_url: "https://fabric.example".into(),
            host_id: Some("host-a".into()),
            issuer: "https://login.example".into(),
            client_id: "cli".into(),
            permitted_origins: vec![
                "https://fabric.example".into(),
                "https://login.example".into(),
            ],
            resource: "https://fabric.example".into(),
        };
        assert!(validate_oauth_options(&valid).is_ok());
        let mut bad = valid.clone();
        bad.permitted_origins.pop();
        assert!(validate_oauth_options(&bad).is_err());
        let mut bad = valid;
        bad.gateway_url = "http://127.0.0.1:8787".into();
        assert!(validate_oauth_options(&bad).is_err());
    }

    #[test]
    fn worker_grant_reply_shape_normalizes_epoch_milliseconds() {
        let worker_reply = json!({
            "host_id": "host-a",
            "owner_key": "a".repeat(64),
            "grant_id": "00000000-0000-4000-8000-000000000001",
            "generation": 7,
            "status": "active",
            "root_names": ["src"],
            "expires_at": 1_900_000_000_000u64,
        });
        let status = parse_enrollment_status(worker_reply, "host-a").unwrap();
        assert_eq!(status.generation, 7);
        assert!(
            status
                .owner_key
                .as_deref()
                .is_some_and(|owner| owner.len() == 64 && owner.bytes().all(|byte| byte == b'a'))
        );
        assert_eq!(status.root_names, ["src"]);
        assert_eq!(status.expires_at, Some(1_900_000_000));
    }

    #[test]
    fn worker_pending_deadline_is_normalized_from_epoch_milliseconds() {
        let worker_reply = json!({
            "host_id": "host-a",
            "owner_key": "a".repeat(64),
            "grant_id": "00000000-0000-4000-8000-000000000001",
            "generation": 7,
            "status": "pending",
            "root_names": ["src"],
            "attempt_id": "00000000-0000-4000-8000-000000000002",
            "reservation_operation_id": "00000000-0000-4000-8000-000000000003",
            "operation_id": "00000000-0000-4000-8000-000000000003",
            "operation_generation": 7,
            "pending_expires_at": 1_900_000_300_000u64,
        });
        let status = parse_enrollment_status(worker_reply, "host-a").unwrap();
        assert_eq!(status.pending_expires_at, Some(1_900_000_300));
        assert!(
            worker_epoch_millis_to_seconds(unix_now().saturating_sub(1) * 1_000).unwrap()
                <= unix_now()
        );
        assert!(worker_epoch_millis_to_seconds(unix_now()).is_err());
    }

    #[test]
    fn worker_active_expiry_from_an_expired_wire_timestamp_is_normalized() {
        let expired_ms = unix_now().saturating_sub(1).saturating_mul(1_000);
        let worker_reply = json!({
            "host_id": "host-a",
            "owner_key": "a".repeat(64),
            "grant_id": "00000000-0000-4000-8000-000000000001",
            "generation": 7,
            "status": "active",
            "root_names": ["src"],
            "expires_at": expired_ms,
        });
        let status = parse_enrollment_status(worker_reply, "host-a").unwrap();
        assert!(status.expires_at.is_some_and(|expiry| expiry <= unix_now()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn secret_service_store_and_lookup_use_one_identical_versioned_profile_key() {
        let profile = "a".repeat(64);
        let store_args = secret_tool_store_args(&profile, 3);
        let lookup_args = secret_tool_lookup_args(&profile, 3);
        let clear_args = secret_tool_clear_args(&profile, 3);
        let key = format!("{profile}-3");
        assert_eq!(store_args[5], key);
        assert_eq!(lookup_args[4], key);
        assert_eq!(clear_args[4], key);
        assert_eq!(lookup_args[4].matches("-3").count(), 1);
    }

    #[test]
    fn public_ip_filter_rejects_private_reserved_and_mixed_address_classes() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "100.64.0.1",
            "169.254.1.2",
            "192.0.2.1",
            "198.18.0.1",
            "224.0.0.1",
            "::1",
            "fe80::1",
            "2001:db8::1",
        ] {
            assert!(!is_public_ip(ip.parse().unwrap()), "accepted {ip}");
        }
        for ip in ["1.1.1.1", "2606:4700:4700::1111"] {
            assert!(is_public_ip(ip.parse().unwrap()), "rejected {ip}");
        }
    }

    #[test]
    fn ambiguous_refresh_halts_and_success_preserves_host_scope() {
        let mut record = CredentialRecord {
            schema_version: RECORD_VERSION,
            version: 1,
            gateway_origin: "https://fabric.example".into(),
            issuer: "https://login.example".into(),
            oauth_client_id: "fake-public-cli".into(),
            oauth_authorization_endpoint: "https://login.example/authorize".into(),
            oauth_token_endpoint: "https://login.example/token".into(),
            owner_key: "a".repeat(64),
            email: "owner@example.test".into(),
            host_id: "host-a".into(),
            grant_id: Uuid::new_v4().to_string(),
            grant_secret: "secret sentinel".into(),
            grant_generation: 4,
            grant_expires_at: 9_999_999_999,
            attempt_id: Uuid::new_v4().to_string(),
            reserve_operation_id: Uuid::new_v4().to_string(),
            expected_generation: 3,
            activation_operation_id: Uuid::new_v4().to_string(),
            cancel_operation_id: None,
            revoke_operation_id: None,
            oauth_access_token: "oauth:old".into(),
            oauth_refresh_token: "refresh-old".into(),
            access_expires_at: 1,
            refresh_in_flight: false,
            refresh_attempt_id: None,
            refresh_outcome_unknown: false,
            refresh_commit_pending: false,
            status: CredentialStatus::Active,
            supervisor_boot_generation: Uuid::new_v4().to_string(),
            root_registry_fingerprint: "registry".into(),
            root_fingerprints: BTreeMap::from([("src".into(), "fp".into())]),
            approved_roots: vec!["src".into()],
            local_hmac_key: "key".into(),
        };
        refresh_begin(&mut record, Uuid::new_v4().to_string()).unwrap();
        let before = record.grant_generation;
        refresh_unknown(&mut record);
        assert!(record.validate_for_link("https://fabric.example").is_err());
        let mut known = record.clone();
        known.refresh_outcome_unknown = false;
        known.refresh_in_flight = true;
        let issuer = known.issuer.clone();
        refresh_commit(
            &mut known,
            OAuthCredentials {
                issuer,
                client_id: "fake-public-cli".into(),
                authorization_endpoint: "https://login.example/authorize".into(),
                token_endpoint: "https://login.example/token".into(),
                access_token: "oauth:new".into(),
                refresh_token: "refresh-new".into(),
                access_expires_at: 500,
            },
        )
        .unwrap();
        assert_eq!(known.grant_generation, before);
        assert_eq!(known.approved_roots, ["src"]);
        assert_eq!(known.root_fingerprints["src"], "fp");

        let mut linkable = record;
        linkable.refresh_in_flight = false;
        linkable.refresh_attempt_id = None;
        linkable.refresh_outcome_unknown = false;
        linkable.refresh_commit_pending = false;
        assert!(linkable.validate_for_link("https://fabric.example").is_ok());
        linkable.grant_expires_at = unix_now().saturating_sub(1);
        assert!(
            linkable
                .validate_for_link("https://fabric.example")
                .is_err()
        );
    }

    #[test]
    fn fake_store_keeps_the_credential_record_whole() {
        let store = MemoryStore::default();
        assert!(store.load().unwrap().is_none());
        let serialized = r#"{"schema_version":1,"version":1,"gateway_origin":"https://fabric.example","issuer":"https://login.example","oauth_client_id":"fake-public-cli","oauth_authorization_endpoint":"https://login.example/authorize","oauth_token_endpoint":"https://login.example/token","owner_key":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","email":"owner@example.test","host_id":"host-a","grant_id":"00000000-0000-4000-8000-000000000001","grant_secret":"grant","grant_generation":1,"grant_expires_at":9999999999,"attempt_id":"00000000-0000-4000-8000-000000000002","reserve_operation_id":"00000000-0000-4000-8000-000000000003","expected_generation":0,"activation_operation_id":"00000000-0000-4000-8000-000000000004","cancel_operation_id":null,"revoke_operation_id":null,"oauth_access_token":"oauth:access","oauth_refresh_token":"refresh","access_expires_at":9999999999,"refresh_in_flight":false,"refresh_attempt_id":null,"refresh_outcome_unknown":false,"refresh_commit_pending":false,"status":"active","supervisor_boot_generation":"boot","root_registry_fingerprint":"reg","root_fingerprints":{"src":"fp"},"approved_roots":["src"],"local_hmac_key":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}"#;
        let record: CredentialRecord = serde_json::from_str(serialized).unwrap();
        store.replace(&record).unwrap();
        assert_eq!(store.load().unwrap().unwrap().grant_generation, 1);
        store.delete().unwrap();
        assert!(store.load().unwrap().is_none());
    }

    #[derive(Default)]
    struct FakeStoreState {
        record: Option<CredentialRecord>,
        fail_next_replace: bool,
    }

    #[derive(Clone, Default)]
    struct FakeStoreFactory(Arc<Mutex<FakeStoreState>>);

    struct SharedFakeStore(Arc<Mutex<FakeStoreState>>);

    impl CredentialStore for SharedFakeStore {
        fn load(&self) -> Result<Option<CredentialRecord>> {
            Ok(self.0.lock().unwrap().record.clone())
        }
        fn replace(&self, record: &CredentialRecord) -> Result<()> {
            let mut state = self.0.lock().unwrap();
            if state.fail_next_replace {
                state.fail_next_replace = false;
                anyhow::bail!("injected atomic credential commit failure");
            }
            state.record = Some(record.clone());
            Ok(())
        }
        fn delete(&self) -> Result<()> {
            self.0.lock().unwrap().record = None;
            Ok(())
        }
    }

    impl CredentialStoreFactory for FakeStoreFactory {
        fn open(&self, profile: &str) -> Result<Box<dyn CredentialStore>> {
            anyhow::ensure!(
                profile.len() == 64,
                "test profile locator must be a non-secret hash"
            );
            Ok(Box::new(SharedFakeStore(Arc::clone(&self.0))))
        }
    }

    #[derive(Default)]
    struct FakeWorkerState {
        status: Option<EnrollmentStatus>,
        revoked_operation: Option<String>,
        oauth_issuer: String,
        oauth_client_id: String,
        oauth_authorization_endpoint: String,
        oauth_token_endpoint: String,
        refresh_error: Option<String>,
        refresh_tokens: Vec<String>,
        authorize_calls: usize,
        identity_calls: usize,
        refresh_calls: usize,
        reserve_calls: usize,
        fail_next_reserve: bool,
        activate_calls: usize,
        revoke_calls: usize,
        lose_next_revoke_response: bool,
        cancel_calls: usize,
        lose_next_cancel_response: bool,
    }

    #[derive(Clone)]
    struct FakeProtocol {
        inner: Arc<Mutex<FakeWorkerState>>,
        store: FakeStoreFactory,
        owner_key: String,
        log: Arc<Mutex<Vec<&'static str>>>,
    }

    impl FakeProtocol {
        fn new(store: FakeStoreFactory, log: Arc<Mutex<Vec<&'static str>>>) -> Self {
            let state = FakeWorkerState {
                oauth_issuer: "https://login.example".to_owned(),
                oauth_client_id: "fake-public-cli".to_owned(),
                oauth_authorization_endpoint: "https://login.example/authorize".to_owned(),
                oauth_token_endpoint: "https://login.example/token".to_owned(),
                ..FakeWorkerState::default()
            };
            Self {
                inner: Arc::new(Mutex::new(state)),
                store,
                owner_key: "a".repeat(64),
                log,
            }
        }

        fn status(&self) -> Option<EnrollmentStatus> {
            self.inner.lock().unwrap().status.clone()
        }
    }

    impl EnrollmentProtocol for FakeProtocol {
        async fn authorize(&self) -> Result<OAuthCredentials> {
            self.log.lock().unwrap().push("authorize");
            let mut state = self.inner.lock().unwrap();
            state.authorize_calls += 1;
            let suffix = if state.authorize_calls == 1 {
                "initial"
            } else {
                "reauth"
            };
            Ok(OAuthCredentials {
                issuer: state.oauth_issuer.clone(),
                client_id: state.oauth_client_id.clone(),
                authorization_endpoint: state.oauth_authorization_endpoint.clone(),
                token_endpoint: state.oauth_token_endpoint.clone(),
                access_token: format!("oauth:{suffix}"),
                refresh_token: format!("refresh:{suffix}"),
                access_expires_at: unix_now().saturating_add(3600),
            })
        }

        async fn oauth_configuration_matches(
            &self,
            options: &BrowserConnectOptions,
            record: &CredentialRecord,
        ) -> Result<bool> {
            let state = self.inner.lock().unwrap();
            Ok(options.issuer == record.issuer
                && options.issuer == state.oauth_issuer
                && options.client_id == record.oauth_client_id
                && options.client_id == state.oauth_client_id
                && record.oauth_authorization_endpoint == state.oauth_authorization_endpoint
                && record.oauth_token_endpoint == state.oauth_token_endpoint
                && options.resource == record.gateway_origin)
        }

        async fn identity(&self, access_token: &str) -> Result<VerifiedIdentity> {
            self.log.lock().unwrap().push("identity");
            self.inner.lock().unwrap().identity_calls += 1;
            anyhow::ensure!(
                access_token.starts_with("oauth:"),
                "identity endpoint requires OAuth token"
            );
            Ok(VerifiedIdentity {
                owner_key: self.owner_key.clone(),
                email: "owner@example.test".to_owned(),
            })
        }

        async fn refresh(&self, credentials: &OAuthCredentials) -> Result<OAuthCredentials> {
            self.log.lock().unwrap().push("refresh");
            let mut state = self.inner.lock().unwrap();
            state.refresh_calls += 1;
            state.refresh_tokens.push(credentials.refresh_token.clone());
            anyhow::ensure!(
                credentials.issuer == state.oauth_issuer
                    && credentials.client_id == state.oauth_client_id
                    && credentials.authorization_endpoint == state.oauth_authorization_endpoint
                    && credentials.token_endpoint == state.oauth_token_endpoint,
                "fake refusing an old refresh token after a pin change"
            );
            if let Some(error) = state.refresh_error.take() {
                anyhow::bail!("{error}");
            }
            anyhow::ensure!(
                credentials.refresh_token.starts_with("refresh:"),
                "refresh token missing"
            );
            Ok(OAuthCredentials {
                issuer: credentials.issuer.clone(),
                client_id: credentials.client_id.clone(),
                authorization_endpoint: credentials.authorization_endpoint.clone(),
                token_endpoint: credentials.token_endpoint.clone(),
                access_token: "oauth:rotated".to_owned(),
                refresh_token: "refresh:rotated".to_owned(),
                access_expires_at: unix_now().saturating_add(3600),
            })
        }

        async fn status(
            &self,
            access_token: &str,
            host_id: &str,
        ) -> Result<Option<EnrollmentStatus>> {
            self.log.lock().unwrap().push("status");
            anyhow::ensure!(
                access_token.starts_with("oauth:"),
                "status endpoint requires OAuth token"
            );
            let status = self.inner.lock().unwrap().status.clone();
            anyhow::ensure!(
                status
                    .as_ref()
                    .is_none_or(|status| status.host_id == host_id),
                "wrong Host status"
            );
            Ok(status)
        }

        async fn reserve(
            &self,
            access_token: &str,
            record: &CredentialRecord,
        ) -> Result<EnrollmentStatus> {
            self.log.lock().unwrap().push("reserve");
            anyhow::ensure!(
                access_token.starts_with("oauth:"),
                "reserve endpoint requires OAuth token"
            );
            let saved = self.store.0.lock().unwrap().record.clone();
            anyhow::ensure!(
                saved
                    .as_ref()
                    .is_some_and(|saved| saved.status == CredentialStatus::Pending
                        && saved.attempt_id == record.attempt_id
                        && saved.reserve_operation_id == record.reserve_operation_id),
                "complete pending record must be committed before reservation"
            );
            let mut state = self.inner.lock().unwrap();
            state.reserve_calls += 1;
            if state.fail_next_reserve {
                state.fail_next_reserve = false;
                anyhow::bail!("fake reserve response lost before Worker commit");
            }
            if let Some(current) = state.status.as_ref().filter(|current| {
                current.status == "pending"
                    && current.owner_key.as_deref() == Some(record.owner_key.as_str())
                    && current.grant_id.as_deref() == Some(record.grant_id.as_str())
                    && current.attempt_id.as_deref() == Some(record.attempt_id.as_str())
                    && current.reservation_operation_id.as_deref()
                        == Some(record.reserve_operation_id.as_str())
                    && current.root_names == record.approved_roots
            }) {
                return Ok(current.clone());
            }
            if let Some(current) = state.status.as_ref() {
                anyhow::ensure!(
                    current.attempt_id.as_deref() != Some(record.attempt_id.as_str()),
                    "fake Worker rejects reusing a terminal reservation attempt"
                );
                match current.status.as_str() {
                    "active" => anyhow::ensure!(
                        current
                            .expires_at
                            .is_some_and(|expiry| expiry <= unix_now()),
                        "fake Worker refuses to replace an unexpired active Host grant"
                    ),
                    "pending" => anyhow::ensure!(
                        current
                            .pending_expires_at
                            .is_some_and(|expiry| expiry <= unix_now()),
                        "fake Worker refuses to replace an unexpired pending attempt"
                    ),
                    "revoked" => {}
                    _ => anyhow::bail!("fake Worker returned an unsupported Host status"),
                }
                anyhow::ensure!(
                    current.generation == record.expected_generation,
                    "reservation did not compare-and-swap the observed generation"
                );
            } else {
                anyhow::ensure!(
                    record.expected_generation == 0,
                    "first reservation must start at generation zero"
                );
            }
            let status = EnrollmentStatus {
                host_id: record.host_id.clone(),
                status: "pending".to_owned(),
                generation: record.expected_generation + 1,
                root_names: record.approved_roots.clone(),
                owner_key: Some(record.owner_key.clone()),
                grant_id: Some(record.grant_id.clone()),
                attempt_id: Some(record.attempt_id.clone()),
                reservation_operation_id: Some(record.reserve_operation_id.clone()),
                cancel_operation_id: None,
                operation_id: Some(record.reserve_operation_id.clone()),
                operation_generation: Some(record.expected_generation + 1),
                pending_expires_at: Some(unix_now().saturating_add(300)),
                expires_at: None,
            };
            state.status = Some(status.clone());
            Ok(status)
        }

        async fn activate(
            &self,
            access_token: &str,
            record: &CredentialRecord,
        ) -> Result<EnrollmentStatus> {
            self.log.lock().unwrap().push("activate");
            anyhow::ensure!(
                access_token.starts_with("oauth:"),
                "activate endpoint requires OAuth token"
            );
            let saved = self.store.0.lock().unwrap().record.clone();
            anyhow::ensure!(
                saved
                    .as_ref()
                    .is_some_and(|saved| saved.status == CredentialStatus::Pending
                        && saved.grant_generation == record.grant_generation),
                "reservation generation must be committed before activation"
            );
            let mut state = self.inner.lock().unwrap();
            state.activate_calls += 1;
            let pending = state.status.as_ref().context("no pending reservation")?;
            anyhow::ensure!(
                pending.status == "pending"
                    && pending.attempt_id.as_deref() == Some(record.attempt_id.as_str())
                    && pending.reservation_operation_id.as_deref()
                        == Some(record.reserve_operation_id.as_str()),
                "activation changed the reservation attempt"
            );
            let status = EnrollmentStatus {
                host_id: record.host_id.clone(),
                status: "active".to_owned(),
                generation: record.grant_generation,
                root_names: record.approved_roots.clone(),
                owner_key: Some(record.owner_key.clone()),
                grant_id: Some(record.grant_id.clone()),
                attempt_id: Some(record.attempt_id.clone()),
                reservation_operation_id: Some(record.reserve_operation_id.clone()),
                cancel_operation_id: None,
                operation_id: Some(record.reserve_operation_id.clone()),
                operation_generation: Some(record.grant_generation),
                pending_expires_at: None,
                expires_at: Some(unix_now().saturating_add(90 * 24 * 60 * 60)),
            };
            state.status = Some(status.clone());
            Ok(status)
        }

        async fn cancel(
            &self,
            access_token: &str,
            host_id: &str,
            current: &EnrollmentStatus,
            operation_id: &str,
        ) -> Result<EnrollmentStatus> {
            self.log.lock().unwrap().push("cancel");
            anyhow::ensure!(
                access_token.starts_with("oauth:"),
                "cancel endpoint requires OAuth token"
            );
            let mut state = self.inner.lock().unwrap();
            state.cancel_calls += 1;
            if let Some(current) = state.status.as_ref()
                && current.status == "revoked"
                && current.cancel_operation_id.as_deref() == Some(operation_id)
            {
                return Ok(current.clone());
            }
            anyhow::ensure!(
                current.status == "pending" && current.host_id == host_id,
                "wrong pending enrollment"
            );
            let revoked = EnrollmentStatus {
                status: "revoked".to_owned(),
                generation: current.generation + 1,
                cancel_operation_id: Some(operation_id.to_owned()),
                operation_id: Some(operation_id.to_owned()),
                operation_generation: Some(current.generation + 1),
                pending_expires_at: None,
                expires_at: None,
                ..current.clone()
            };
            state.revoked_operation = Some(operation_id.to_owned());
            state.status = Some(revoked.clone());
            if state.lose_next_cancel_response {
                state.lose_next_cancel_response = false;
                anyhow::bail!("fake cancellation response lost after Worker commit");
            }
            Ok(revoked)
        }

        async fn revoke(
            &self,
            access_token: &str,
            host_id: &str,
            generation: u64,
            operation_id: &str,
        ) -> Result<EnrollmentStatus> {
            self.log.lock().unwrap().push("revoke");
            anyhow::ensure!(
                access_token.starts_with("oauth:"),
                "revoke endpoint requires OAuth token"
            );
            let mut state = self.inner.lock().unwrap();
            state.revoke_calls += 1;
            let current = state.status.as_ref().context("Host grant does not exist")?;
            if current.status == "revoked"
                && current.generation == generation + 1
                && state.revoked_operation.as_deref() == Some(operation_id)
            {
                return Ok(current.clone());
            }
            anyhow::ensure!(
                current.status == "active"
                    && current.host_id == host_id
                    && current.generation == generation,
                "revoke generation compare-and-swap failed"
            );
            let revoked = EnrollmentStatus {
                status: "revoked".to_owned(),
                generation: generation + 1,
                operation_id: Some(operation_id.to_owned()),
                operation_generation: Some(generation + 1),
                expires_at: None,
                pending_expires_at: None,
                ..current.clone()
            };
            state.revoked_operation = Some(operation_id.to_owned());
            state.status = Some(revoked.clone());
            if state.lose_next_revoke_response {
                state.lose_next_revoke_response = false;
                anyhow::bail!("fake revocation response lost after Worker commit");
            }
            Ok(revoked)
        }
    }

    struct FakeConfirmation {
        log: Arc<Mutex<Vec<&'static str>>>,
        confirmed: usize,
        approved: usize,
    }

    impl EnrollmentConfirmation for FakeConfirmation {
        fn confirm_owner(&mut self, identity: &VerifiedIdentity, host_id: &str) -> Result<()> {
            self.log.lock().unwrap().push("confirm_owner");
            anyhow::ensure!(
                identity.email == "owner@example.test",
                "expected fake verified owner"
            );
            anyhow::ensure!(host_id == "host-a", "expected exact fake Host id");
            self.confirmed += 1;
            Ok(())
        }
        fn approve_roots(&mut self, available: &[String]) -> Result<Vec<String>> {
            self.log.lock().unwrap().push("approve_roots");
            anyhow::ensure!(
                available == ["other", "src"],
                "root inventory differs from configured fake Supervisor"
            );
            self.approved += 1;
            Ok(vec!["src".to_owned()])
        }
    }

    struct RecordingTransitionGuard(Arc<Mutex<Vec<&'static str>>>);

    impl TransitionGuard for RecordingTransitionGuard {}

    impl Drop for RecordingTransitionGuard {
        fn drop(&mut self) {
            self.0.lock().unwrap().push("unlock");
        }
    }

    struct TemporaryInventory {
        inventory: LocalSupervisorInventory,
        path: PathBuf,
    }

    impl Drop for TemporaryInventory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn fake_inventory() -> TemporaryInventory {
        let path = std::env::temp_dir().join(format!("temote-browser-test-{}", Uuid::new_v4()));
        let other = path.join("other");
        let src = path.join("src");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::create_dir_all(&src).unwrap();
        let root = |name: &str, path: PathBuf| {
            let metadata = std::fs::metadata(&path).unwrap();
            #[cfg(unix)]
            use std::os::unix::fs::MetadataExt;
            #[cfg(unix)]
            let (device, inode) = (metadata.dev(), metadata.ino());
            #[cfg(not(unix))]
            let (device, inode) = (0, 0);
            LocalRootIdentity {
                name: name.to_owned(),
                canonical_path: std::fs::canonicalize(path).unwrap(),
                device,
                inode,
            }
        };
        TemporaryInventory {
            inventory: LocalSupervisorInventory {
                host_id: "host-a".to_owned(),
                boot_generation: Uuid::new_v4().to_string(),
                control_protocol: crate::session_control::CONTROL_PROTOCOL_VERSION,
                roots: vec![root("other", other), root("src", src)],
            },
            path,
        }
    }

    fn options() -> BrowserConnectOptions {
        BrowserConnectOptions {
            gateway_url: "https://fabric.example".to_owned(),
            host_id: Some("host-a".to_owned()),
            issuer: "https://login.example".to_owned(),
            client_id: "fake-public-cli".to_owned(),
            permitted_origins: vec![
                "https://fabric.example".to_owned(),
                "https://login.example".to_owned(),
            ],
            resource: "https://fabric.example".to_owned(),
        }
    }

    async fn active_fake_profile() -> (
        TemporaryInventory,
        FakeStoreFactory,
        Arc<Mutex<Vec<&'static str>>>,
        FakeProtocol,
        FakeConfirmation,
    ) {
        let temporary = fake_inventory();
        let store = FakeStoreFactory::default();
        let log = Arc::new(Mutex::new(Vec::new()));
        let protocol = FakeProtocol::new(store.clone(), Arc::clone(&log));
        let mut confirmation = FakeConfirmation {
            log: Arc::clone(&log),
            confirmed: 0,
            approved: 0,
        };
        let prepared = prepare_browser_link(
            &options(),
            temporary.inventory.clone(),
            &protocol,
            &store,
            &mut confirmation,
        )
        .await
        .unwrap();
        assert_eq!(prepared.record.status, CredentialStatus::Active);
        (temporary, store, log, protocol, confirmation)
    }

    #[tokio::test]
    async fn connect_reauthenticates_same_owner_when_issuer_pin_changes_without_refreshing_old_token()
     {
        let (temporary, store, _log, protocol, mut confirmation) = active_fake_profile().await;
        let mut saved = store.0.lock().unwrap().record.clone().unwrap();
        saved.access_expires_at = unix_now();
        store.0.lock().unwrap().record = Some(saved);
        let counts_before = {
            let state = protocol.inner.lock().unwrap();
            (
                state.refresh_calls,
                state.identity_calls,
                state.authorize_calls,
            )
        };
        let mut changed = options();
        changed.issuer = "https://other-login.example".to_owned();
        changed
            .permitted_origins
            .push("https://other-login.example".to_owned());
        {
            let mut state = protocol.inner.lock().unwrap();
            state.oauth_issuer = changed.issuer.clone();
            state.oauth_authorization_endpoint = "https://other-login.example/authorize".to_owned();
            state.oauth_token_endpoint = "https://other-login.example/token".to_owned();
        }
        let result = prepare_browser_link(
            &changed,
            temporary.inventory.clone(),
            &protocol,
            &store,
            &mut confirmation,
        )
        .await;
        let resumed = result.unwrap();
        assert_eq!(resumed.record.issuer, changed.issuer);
        let state = protocol.inner.lock().unwrap();
        assert_eq!(state.refresh_calls, counts_before.0);
        assert_eq!(state.identity_calls, counts_before.1 + 2);
        assert_eq!(state.authorize_calls, counts_before.2 + 1);
    }

    #[tokio::test]
    async fn changed_oauth_client_or_endpoint_requires_fresh_login_before_token_use() {
        for change_client in [false, true] {
            let (temporary, store, _log, protocol, mut confirmation) = active_fake_profile().await;
            let mut changed_options = options();
            let mut saved = store.0.lock().unwrap().record.clone().unwrap();
            saved.access_expires_at = unix_now();
            store.0.lock().unwrap().record = Some(saved.clone());
            {
                let mut state = protocol.inner.lock().unwrap();
                if change_client {
                    state.oauth_client_id = "new-public-client".to_owned();
                    changed_options.client_id = state.oauth_client_id.clone();
                } else {
                    state.oauth_token_endpoint =
                        "https://login.example/rotated-token-endpoint".to_owned();
                }
            }
            let resumed = prepare_browser_link(
                &changed_options,
                temporary.inventory.clone(),
                &protocol,
                &store,
                &mut confirmation,
            )
            .await
            .unwrap();
            let state = protocol.inner.lock().unwrap();
            assert_eq!(state.refresh_calls, 0, "old refresh token must not be sent");
            assert_eq!(
                state.authorize_calls, 2,
                "changed configuration requires browser login"
            );
            assert_eq!(resumed.record.grant_id, saved.grant_id);
            assert_eq!(resumed.record.grant_generation, saved.grant_generation);
            if change_client {
                assert_eq!(resumed.record.oauth_client_id, "new-public-client");
            } else {
                assert_eq!(
                    resumed.record.oauth_token_endpoint,
                    "https://login.example/rotated-token-endpoint"
                );
            }
        }
    }

    #[tokio::test]
    async fn expired_active_grant_reenrolls_same_owner_after_fresh_confirmation_outside_lock() {
        let (temporary, store, log, protocol, mut confirmation) = active_fake_profile().await;
        let mut saved = store.0.lock().unwrap().record.clone().unwrap();
        let old_grant_id = saved.grant_id.clone();
        saved.grant_expires_at = unix_now().saturating_sub(1);
        saved.version = next_record_version(saved.version).unwrap();
        store.0.lock().unwrap().record = Some(saved.clone());
        let remote = protocol.status().unwrap();
        let expired_seconds = unix_now().saturating_sub(1);
        let wire_reply = json!({
            "host_id": remote.host_id,
            "status": "active",
            "generation": remote.generation,
            "owner_key": remote.owner_key,
            "grant_id": remote.grant_id,
            "root_names": remote.root_names,
            "attempt_id": remote.attempt_id,
            "reservation_operation_id": remote.reservation_operation_id,
            "operation_id": remote.operation_id,
            "operation_generation": remote.operation_generation,
            "expires_at": expired_seconds.saturating_mul(1_000),
        });
        let remote = parse_enrollment_status(wire_reply, "host-a").unwrap();
        assert_eq!(remote.expires_at, Some(expired_seconds));
        protocol.inner.lock().unwrap().status = Some(remote);
        log.lock().unwrap().clear();

        let prepared = prepare_browser_link_with_transition(
            &options(),
            temporary.inventory.clone(),
            &protocol,
            &store,
            &mut confirmation,
            || {
                log.lock().unwrap().push("lock");
                Ok(Box::new(RecordingTransitionGuard(Arc::clone(&log))))
            },
        )
        .await
        .unwrap();
        let events = log.lock().unwrap().clone();
        let unlocked = events.iter().position(|event| *event == "unlock").unwrap();
        let confirmed = events
            .iter()
            .position(|event| *event == "confirm_owner")
            .unwrap();
        let approved = events
            .iter()
            .position(|event| *event == "approve_roots")
            .unwrap();
        let relocked = events.iter().rposition(|event| *event == "lock").unwrap();
        assert!(unlocked < confirmed && confirmed < approved && approved < relocked);
        assert_eq!(prepared.record.status, CredentialStatus::Active);
        assert_ne!(prepared.record.grant_id, old_grant_id);
        assert_eq!(prepared.record.expected_generation, 1);
        assert_eq!(prepared.record.grant_generation, 2);
        assert_eq!(prepared.record.approved_roots, ["src"]);
        assert_eq!(confirmation.confirmed, 2);
        assert_eq!(protocol.inner.lock().unwrap().reserve_calls, 2);
        assert_eq!(protocol.inner.lock().unwrap().authorize_calls, 1);
    }

    #[tokio::test]
    async fn fresh_login_does_not_replace_an_unexpired_remote_active_grant() {
        let (temporary, store, _log, protocol, mut confirmation) = active_fake_profile().await;
        store.0.lock().unwrap().record = None;
        let before = {
            let state = protocol.inner.lock().unwrap();
            (
                state.reserve_calls,
                state.status.as_ref().unwrap().generation,
            )
        };

        assert!(
            prepare_browser_link(
                &options(),
                temporary.inventory.clone(),
                &protocol,
                &store,
                &mut confirmation,
            )
            .await
            .is_err()
        );

        assert!(store.0.lock().unwrap().record.is_none());
        let after = protocol.inner.lock().unwrap();
        assert_eq!(after.reserve_calls, before.0);
        assert_eq!(after.revoke_calls, 0);
        assert_eq!(after.status.as_ref().unwrap().status, "active");
        assert_eq!(after.status.as_ref().unwrap().generation, before.1);
    }

    #[tokio::test]
    async fn fresh_login_reenrolls_an_expired_worker_active_grant_with_new_attempt_ids() {
        let (temporary, store, _log, protocol, mut confirmation) = active_fake_profile().await;
        let old = store.0.lock().unwrap().record.clone().unwrap();
        store.0.lock().unwrap().record = None;
        let remote = protocol.status().unwrap();
        let expired_seconds = unix_now().saturating_sub(1);
        let wire_reply = json!({
            "host_id": remote.host_id,
            "status": "active",
            "generation": remote.generation,
            "owner_key": remote.owner_key,
            "grant_id": remote.grant_id,
            "root_names": remote.root_names,
            "attempt_id": remote.attempt_id,
            "reservation_operation_id": remote.reservation_operation_id,
            "operation_id": remote.operation_id,
            "operation_generation": remote.operation_generation,
            "expires_at": expired_seconds.saturating_mul(1_000),
        });
        let normalized = parse_enrollment_status(wire_reply, "host-a").unwrap();
        assert_eq!(normalized.expires_at, Some(expired_seconds));
        protocol.inner.lock().unwrap().status = Some(normalized);

        let prepared = prepare_browser_link(
            &options(),
            temporary.inventory.clone(),
            &protocol,
            &store,
            &mut confirmation,
        )
        .await
        .unwrap();

        assert_eq!(prepared.record.status, CredentialStatus::Active);
        assert_eq!(prepared.record.expected_generation, old.grant_generation);
        assert_ne!(prepared.record.grant_id, old.grant_id);
        assert_ne!(prepared.record.attempt_id, old.attempt_id);
        let state = protocol.inner.lock().unwrap();
        assert_eq!(state.status.as_ref().unwrap().status, "active");
        assert_eq!(
            state.status.as_ref().unwrap().generation,
            old.grant_generation + 1
        );
        assert_eq!(
            state.status.as_ref().unwrap().grant_id.as_deref(),
            Some(prepared.record.grant_id.as_str())
        );
        assert_eq!(state.revoke_calls, 0);
        assert_eq!(state.reserve_calls, 2);
    }

    #[tokio::test]
    async fn expired_access_pending_profile_reauthenticates_before_exact_resume() {
        let (temporary, store, _log, protocol, mut confirmation) = active_fake_profile().await;
        let mut saved = store.0.lock().unwrap().record.clone().unwrap();
        saved.status = CredentialStatus::Pending;
        saved.access_expires_at = unix_now();
        saved.version = next_record_version(saved.version).unwrap();
        store.0.lock().unwrap().record = Some(saved);
        let mut pending = protocol.status().unwrap();
        pending.status = "pending".to_owned();
        pending.pending_expires_at = Some(unix_now().saturating_add(300));
        pending.expires_at = None;
        protocol.inner.lock().unwrap().status = Some(pending);

        let resumed = prepare_browser_link(
            &options(),
            temporary.inventory.clone(),
            &protocol,
            &store,
            &mut confirmation,
        )
        .await
        .unwrap();
        assert_eq!(resumed.record.status, CredentialStatus::Active);
        assert_eq!(protocol.inner.lock().unwrap().authorize_calls, 2);
        assert_eq!(protocol.inner.lock().unwrap().refresh_calls, 0);
        assert_eq!(confirmation.confirmed, 1);
    }

    #[tokio::test]
    async fn expired_access_revoking_profile_reauthenticates_before_finishing_revoke() {
        let (temporary, store, _log, protocol, mut confirmation) = active_fake_profile().await;
        let mut saved = store.0.lock().unwrap().record.clone().unwrap();
        saved.status = CredentialStatus::Revoking;
        saved.expected_generation = saved.grant_generation;
        saved.revoke_operation_id = Some(Uuid::new_v4().to_string());
        saved.access_expires_at = unix_now();
        saved.version = next_record_version(saved.version).unwrap();
        store.0.lock().unwrap().record = Some(saved);

        assert!(
            prepare_browser_link(
                &options(),
                temporary.inventory.clone(),
                &protocol,
                &store,
                &mut confirmation,
            )
            .await
            .is_err()
        );
        assert!(store.0.lock().unwrap().record.is_none());
        let state = protocol.inner.lock().unwrap();
        assert_eq!(state.authorize_calls, 2);
        assert_eq!(state.refresh_calls, 0);
        assert_eq!(state.revoke_calls, 1);
    }

    #[tokio::test]
    async fn saved_revocation_retry_recognizes_commit_without_replaying_the_old_attempt() {
        let (temporary, store, _log, protocol, mut confirmation) = active_fake_profile().await;
        let mut saved = store.0.lock().unwrap().record.clone().unwrap();
        saved.status = CredentialStatus::Revoking;
        saved.expected_generation = saved.grant_generation;
        saved.revoke_operation_id = Some(Uuid::new_v4().to_string());
        saved.access_expires_at = unix_now();
        saved.version = next_record_version(saved.version).unwrap();
        store.0.lock().unwrap().record = Some(saved);
        protocol.inner.lock().unwrap().lose_next_revoke_response = true;

        assert!(
            prepare_browser_link(
                &options(),
                temporary.inventory.clone(),
                &protocol,
                &store,
                &mut confirmation,
            )
            .await
            .is_err()
        );
        let pending_local = store.0.lock().unwrap().record.clone().unwrap();
        let committed_remote = protocol.status().unwrap();
        assert_eq!(committed_remote.status, "revoked");
        assert_eq!(
            committed_remote.operation_id.as_deref(),
            pending_local.revoke_operation_id.as_deref()
        );

        assert!(
            prepare_browser_link(
                &options(),
                temporary.inventory.clone(),
                &protocol,
                &store,
                &mut confirmation,
            )
            .await
            .is_err()
        );
        assert!(store.0.lock().unwrap().record.is_none());
        let state = protocol.inner.lock().unwrap();
        assert_eq!(
            state.revoke_calls, 1,
            "the committed revoke is not replayed"
        );
        assert_eq!(
            state.reserve_calls, 1,
            "the terminal attempt is never resumed"
        );
        assert_eq!(state.status.as_ref().unwrap().status, "revoked");
    }

    #[tokio::test]
    async fn logout_pending_profile_with_expired_access_reauthenticates_before_cancel() {
        let (_temporary, store, _log, protocol, _confirmation) = active_fake_profile().await;
        let mut saved = store.0.lock().unwrap().record.clone().unwrap();
        saved.status = CredentialStatus::Pending;
        saved.access_expires_at = unix_now();
        saved.version = next_record_version(saved.version).unwrap();
        store.0.lock().unwrap().record = Some(saved);
        let mut pending = protocol.status().unwrap();
        pending.status = "pending".to_owned();
        pending.pending_expires_at = Some(unix_now().saturating_add(300));
        pending.expires_at = None;
        protocol.inner.lock().unwrap().status = Some(pending);

        assert!(
            logout_saved_profile(
                "host-a",
                "https://fabric.example",
                &options(),
                &SharedFakeStore(Arc::clone(&store.0)),
                &protocol,
            )
            .await
            .unwrap()
        );
        let state = protocol.inner.lock().unwrap();
        assert_eq!(state.authorize_calls, 2);
        assert_eq!(state.refresh_calls, 0);
        assert_eq!(state.cancel_calls, 1);
        drop(state);
        assert!(store.0.lock().unwrap().record.is_none());
    }

    #[tokio::test]
    async fn invalid_grant_reauthenticates_same_owner_without_locking_browser_wait() {
        let (temporary, store, log, protocol, mut confirmation) = active_fake_profile().await;
        let mut saved = store.0.lock().unwrap().record.clone().unwrap();
        saved.access_expires_at = unix_now();
        store.0.lock().unwrap().record = Some(saved);
        protocol.inner.lock().unwrap().refresh_error = Some("invalid_grant".to_owned());
        log.lock().unwrap().clear();

        let resumed = prepare_browser_link_with_transition(
            &options(),
            temporary.inventory.clone(),
            &protocol,
            &store,
            &mut confirmation,
            || {
                log.lock().unwrap().push("lock");
                Ok(Box::new(RecordingTransitionGuard(Arc::clone(&log))))
            },
        )
        .await
        .unwrap();

        let events = log.lock().unwrap().clone();
        let unlock = events.iter().position(|event| *event == "unlock").unwrap();
        let authorize = events
            .iter()
            .position(|event| *event == "authorize")
            .unwrap();
        let reacquire = events.iter().rposition(|event| *event == "lock").unwrap();
        assert!(unlock < authorize && authorize < reacquire, "{events:?}");
        let state = protocol.inner.lock().unwrap();
        assert_eq!(state.refresh_calls, 1);
        assert_eq!(state.refresh_tokens, ["refresh:initial"]);
        assert_eq!(state.authorize_calls, 2);
        assert_eq!(state.reserve_calls, 1);
        assert_eq!(state.activate_calls, 1);
        drop(state);
        assert_eq!(resumed.record.oauth_access_token, "oauth:reauth");
        assert!(!resumed.record.refresh_in_flight);
        assert!(!resumed.record.refresh_outcome_unknown);
        assert_eq!(resumed.record.grant_generation, 1);
        assert_eq!(resumed.record.approved_roots, ["src"]);
    }

    #[tokio::test]
    async fn uncertain_refresh_logout_reauthenticates_same_owner_then_revokes() {
        let (_temporary, store, _log, protocol, _confirmation) = active_fake_profile().await;
        let mut saved = store.0.lock().unwrap().record.clone().unwrap();
        saved.refresh_in_flight = true;
        saved.refresh_attempt_id = Some(Uuid::new_v4().to_string());
        saved.version += 1;
        store.0.lock().unwrap().record = Some(saved);

        let did_logout = logout_saved_profile(
            "host-a",
            "https://fabric.example",
            &options(),
            &SharedFakeStore(Arc::clone(&store.0)),
            &protocol,
        )
        .await
        .unwrap();
        assert!(did_logout);
        let state = protocol.inner.lock().unwrap();
        assert_eq!(
            state.refresh_calls, 0,
            "uncertain old refresh token is never replayed"
        );
        assert_eq!(state.authorize_calls, 2);
        assert_eq!(state.revoke_calls, 1);
        drop(state);
        assert!(store.0.lock().unwrap().record.is_none());
        assert_eq!(protocol.status().unwrap().status, "revoked");
    }

    #[tokio::test]
    async fn uncertain_refresh_reauth_rejects_a_different_owner_without_revoking() {
        let (_temporary, store, _log, mut protocol, _confirmation) = active_fake_profile().await;
        let mut saved = store.0.lock().unwrap().record.clone().unwrap();
        saved.refresh_outcome_unknown = true;
        saved.refresh_attempt_id = Some(Uuid::new_v4().to_string());
        saved.version += 1;
        store.0.lock().unwrap().record = Some(saved.clone());
        protocol.owner_key = "b".repeat(64);

        let result = logout_saved_profile(
            "host-a",
            "https://fabric.example",
            &options(),
            &SharedFakeStore(Arc::clone(&store.0)),
            &protocol,
        )
        .await;
        assert!(result.is_err());
        assert_eq!(
            store.0.lock().unwrap().record.as_ref().unwrap().version,
            saved.version
        );
        assert_eq!(protocol.inner.lock().unwrap().refresh_calls, 0);
        assert_eq!(protocol.inner.lock().unwrap().revoke_calls, 0);
    }

    #[tokio::test]
    async fn restart_with_refresh_commit_pending_reauthenticates_without_replaying_refresh() {
        let (temporary, store, _log, protocol, mut confirmation) = active_fake_profile().await;
        let mut saved = store.0.lock().unwrap().record.clone().unwrap();
        saved.refresh_in_flight = true;
        saved.refresh_commit_pending = true;
        saved.refresh_attempt_id = Some(Uuid::new_v4().to_string());
        saved.version = next_record_version(saved.version).unwrap();
        store.0.lock().unwrap().record = Some(saved);

        let prepared = prepare_browser_link(
            &options(),
            temporary.inventory.clone(),
            &protocol,
            &store,
            &mut confirmation,
        )
        .await
        .unwrap();
        assert_eq!(prepared.record.oauth_access_token, "oauth:reauth");
        assert!(!prepared.record.refresh_in_flight);
        assert!(!prepared.record.refresh_commit_pending);
        assert_eq!(protocol.inner.lock().unwrap().refresh_calls, 0);
        assert_eq!(protocol.inner.lock().unwrap().authorize_calls, 2);
    }

    #[tokio::test]
    async fn background_refresh_refuses_to_adopt_a_commit_pending_record() {
        let (_temporary, store, _log, protocol, _confirmation) = active_fake_profile().await;
        let mut saved = store.0.lock().unwrap().record.clone().unwrap();
        saved.refresh_commit_pending = true;
        saved.refresh_attempt_id = Some(Uuid::new_v4().to_string());
        store.0.lock().unwrap().record = Some(saved.clone());
        let result = refresh_record_for_link(
            &mut saved,
            &SharedFakeStore(Arc::clone(&store.0)),
            &protocol,
        )
        .await;
        assert!(result.is_err());
        assert_eq!(protocol.inner.lock().unwrap().refresh_calls, 0);
        assert!(
            store
                .0
                .lock()
                .unwrap()
                .record
                .as_ref()
                .unwrap()
                .refresh_commit_pending
        );
    }

    #[tokio::test]
    async fn fresh_connect_waits_for_confirmation_before_taking_transition_lock() {
        let temporary = fake_inventory();
        let store = FakeStoreFactory::default();
        let log = Arc::new(Mutex::new(Vec::new()));
        let protocol = FakeProtocol::new(store.clone(), Arc::clone(&log));
        let mut confirmation = FakeConfirmation {
            log: Arc::clone(&log),
            confirmed: 0,
            approved: 0,
        };
        prepare_browser_link_with_transition(
            &options(),
            temporary.inventory.clone(),
            &protocol,
            &store,
            &mut confirmation,
            || {
                log.lock().unwrap().push("lock");
                Ok(Box::new(RecordingTransitionGuard(Arc::clone(&log))))
            },
        )
        .await
        .unwrap();
        let events = log.lock().unwrap().clone();
        let roots_approved = events
            .iter()
            .position(|event| *event == "approve_roots")
            .unwrap();
        let lock_acquired = events.iter().position(|event| *event == "lock").unwrap();
        assert!(roots_approved < lock_acquired, "{events:?}");
    }

    #[tokio::test]
    async fn fake_end_to_end_connect_resume_refresh_revoke_and_reenroll() {
        let temporary = fake_inventory();
        let inventory = temporary.inventory.clone();
        let store = FakeStoreFactory::default();
        let log = Arc::new(Mutex::new(Vec::new()));
        let protocol = FakeProtocol::new(store.clone(), Arc::clone(&log));
        let mut confirmation = FakeConfirmation {
            log: Arc::clone(&log),
            confirmed: 0,
            approved: 0,
        };
        let connect_options = options();

        let first = prepare_browser_link(
            &connect_options,
            inventory.clone(),
            &protocol,
            &store,
            &mut confirmation,
        )
        .await
        .unwrap();
        assert_eq!(first.record.status, CredentialStatus::Active);
        assert_eq!(first.record.approved_roots, ["src"]);
        assert_eq!(protocol.status().unwrap().generation, 1);
        assert_eq!(
            &*log.lock().unwrap(),
            &[
                "authorize",
                "identity",
                "confirm_owner",
                "approve_roots",
                "status",
                "reserve",
                "activate"
            ]
        );
        assert_eq!(confirmation.confirmed, 1);
        assert_eq!(confirmation.approved, 1);

        let mut expiring = store.0.lock().unwrap().record.clone().unwrap();
        expiring.access_expires_at = unix_now();
        store.0.lock().unwrap().record = Some(expiring.clone());
        let grant_secret = expiring.grant_secret.clone();
        refresh_record_for_link(
            &mut expiring,
            &SharedFakeStore(Arc::clone(&store.0)),
            &protocol,
        )
        .await
        .unwrap();
        assert_eq!(expiring.oauth_access_token, "oauth:rotated");
        assert_eq!(expiring.grant_generation, 1);
        assert_eq!(expiring.grant_secret, grant_secret);
        assert_eq!(expiring.approved_roots, ["src"]);
        assert_eq!(protocol.inner.lock().unwrap().refresh_calls, 1);

        let before_resume_authorizations = protocol.inner.lock().unwrap().authorize_calls;
        let resumed = prepare_browser_link(
            &connect_options,
            inventory.clone(),
            &protocol,
            &store,
            &mut confirmation,
        )
        .await
        .unwrap();
        assert_eq!(resumed.authority.grant_generation, 1);
        assert_eq!(
            protocol.inner.lock().unwrap().authorize_calls,
            before_resume_authorizations,
            "a valid stored profile verifies identity and reconnects without another browser login"
        );
        assert_eq!(
            confirmation.confirmed, 1,
            "stored-profile resume does not repeat owner confirmation"
        );

        let did_logout = logout_saved_profile(
            "host-a",
            "https://fabric.example",
            &connect_options,
            &SharedFakeStore(Arc::clone(&store.0)),
            &protocol,
        )
        .await
        .unwrap();
        assert!(did_logout);
        assert!(store.0.lock().unwrap().record.is_none());
        assert_eq!(protocol.status().unwrap().status, "revoked");
        assert_eq!(protocol.status().unwrap().generation, 2);

        let reenrolled = prepare_browser_link(
            &connect_options,
            inventory,
            &protocol,
            &store,
            &mut confirmation,
        )
        .await
        .unwrap();
        assert_eq!(
            reenrolled.authority.grant_generation, 3,
            "fresh enrollment after logout uses the owner's current tombstone generation"
        );
        assert_eq!(
            protocol.inner.lock().unwrap().authorize_calls,
            before_resume_authorizations + 1
        );
        assert_eq!(confirmation.confirmed, 2);
        assert_eq!(protocol.inner.lock().unwrap().reserve_calls, 2);
        assert_eq!(protocol.inner.lock().unwrap().activate_calls, 2);
    }

    #[tokio::test]
    async fn fake_flow_stops_before_reservation_when_atomic_pending_commit_fails() {
        let temporary = fake_inventory();
        let store = FakeStoreFactory::default();
        store.0.lock().unwrap().fail_next_replace = true;
        let log = Arc::new(Mutex::new(Vec::new()));
        let protocol = FakeProtocol::new(store.clone(), Arc::clone(&log));
        let mut confirmation = FakeConfirmation {
            log: Arc::clone(&log),
            confirmed: 0,
            approved: 0,
        };
        let result = prepare_browser_link(
            &options(),
            temporary.inventory.clone(),
            &protocol,
            &store,
            &mut confirmation,
        )
        .await;
        assert!(result.is_err());
        assert_eq!(
            protocol.inner.lock().unwrap().reserve_calls,
            0,
            "Worker reservation cannot start until the complete recovery record commits"
        );
        assert!(protocol.status().is_none());
    }

    #[tokio::test]
    async fn pending_connect_retries_the_exact_saved_reservation_after_status_none() {
        let temporary = fake_inventory();
        let store = FakeStoreFactory::default();
        let log = Arc::new(Mutex::new(Vec::new()));
        let protocol = FakeProtocol::new(store.clone(), Arc::clone(&log));
        protocol.inner.lock().unwrap().fail_next_reserve = true;
        let mut confirmation = FakeConfirmation {
            log: Arc::clone(&log),
            confirmed: 0,
            approved: 0,
        };

        assert!(
            prepare_browser_link(
                &options(),
                temporary.inventory.clone(),
                &protocol,
                &store,
                &mut confirmation,
            )
            .await
            .is_err()
        );
        let saved = store.0.lock().unwrap().record.clone().unwrap();
        assert_eq!(saved.status, CredentialStatus::Pending);
        assert!(protocol.status().is_none());

        let resumed = prepare_browser_link(
            &options(),
            temporary.inventory.clone(),
            &protocol,
            &store,
            &mut confirmation,
        )
        .await
        .unwrap();
        assert_eq!(resumed.record.status, CredentialStatus::Active);
        assert_eq!(resumed.record.attempt_id, saved.attempt_id);
        assert_eq!(
            resumed.record.reserve_operation_id,
            saved.reserve_operation_id
        );
        assert_eq!(resumed.record.grant_id, saved.grant_id);
        assert_eq!(protocol.inner.lock().unwrap().reserve_calls, 2);
        assert_eq!(protocol.inner.lock().unwrap().authorize_calls, 1);
        assert_eq!(confirmation.confirmed, 1);
    }

    #[tokio::test]
    async fn logout_recovers_an_unreserved_pending_attempt_then_cancels_it() {
        let _temporary = fake_inventory();
        let store = FakeStoreFactory::default();
        let log = Arc::new(Mutex::new(Vec::new()));
        let protocol = FakeProtocol::new(store.clone(), Arc::clone(&log));
        protocol.inner.lock().unwrap().fail_next_reserve = true;
        let mut confirmation = FakeConfirmation {
            log: Arc::clone(&log),
            confirmed: 0,
            approved: 0,
        };
        assert!(
            prepare_browser_link(
                &options(),
                _temporary.inventory.clone(),
                &protocol,
                &store,
                &mut confirmation,
            )
            .await
            .is_err()
        );
        let saved = store.0.lock().unwrap().record.clone().unwrap();

        assert!(
            logout_saved_profile(
                "host-a",
                "https://fabric.example",
                &options(),
                &SharedFakeStore(Arc::clone(&store.0)),
                &protocol,
            )
            .await
            .unwrap()
        );
        assert!(store.0.lock().unwrap().record.is_none());
        let status = protocol.status().unwrap();
        assert_eq!(status.status, "revoked");
        assert_eq!(
            status.attempt_id.as_deref(),
            Some(saved.attempt_id.as_str())
        );
        assert_eq!(
            status.reservation_operation_id.as_deref(),
            Some(saved.reserve_operation_id.as_str())
        );
        assert!(status.cancel_operation_id.is_some());
        let state = protocol.inner.lock().unwrap();
        assert_eq!(state.reserve_calls, 2);
        assert_eq!(state.cancel_calls, 1);
    }

    #[tokio::test]
    async fn logout_reconciles_lost_pending_cancel_response_by_exact_operation_generation() {
        let (_temporary, store, _log, protocol, _confirmation) = active_fake_profile().await;
        let mut saved = store.0.lock().unwrap().record.clone().unwrap();
        saved.status = CredentialStatus::Pending;
        saved.version = next_record_version(saved.version).unwrap();
        store.0.lock().unwrap().record = Some(saved.clone());
        let mut pending = protocol.status().unwrap();
        pending.status = "pending".to_owned();
        pending.pending_expires_at = Some(unix_now().saturating_add(300));
        pending.expires_at = None;
        protocol.inner.lock().unwrap().status = Some(pending);
        protocol.inner.lock().unwrap().lose_next_cancel_response = true;

        assert!(
            logout_saved_profile(
                "host-a",
                "https://fabric.example",
                &options(),
                &SharedFakeStore(Arc::clone(&store.0)),
                &protocol,
            )
            .await
            .is_err()
        );
        let cancelled_record = store.0.lock().unwrap().record.clone().unwrap();
        let operation_id = cancelled_record.cancel_operation_id.clone().unwrap();
        let remote = protocol.status().unwrap();
        assert_eq!(remote.status, "revoked");
        assert_eq!(remote.generation, saved.grant_generation + 1);
        assert_eq!(
            remote.cancel_operation_id.as_deref(),
            Some(operation_id.as_str())
        );
        assert!(
            protocol
                .reserve(&cancelled_record.oauth_access_token, &cancelled_record)
                .await
                .is_err(),
            "a terminal cancellation attempt cannot be reserved again"
        );

        assert!(
            logout_saved_profile(
                "host-a",
                "https://fabric.example",
                &options(),
                &SharedFakeStore(Arc::clone(&store.0)),
                &protocol,
            )
            .await
            .unwrap()
        );
        assert!(store.0.lock().unwrap().record.is_none());
        assert_eq!(protocol.inner.lock().unwrap().cancel_calls, 1);
    }
}
