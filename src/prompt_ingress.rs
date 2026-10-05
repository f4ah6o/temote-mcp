//! Owner-local, authenticated prompt ingress. This module does not dispatch work.
//! The transport must obtain the peer UID from a local Unix socket; a caller
//! supplied UID, public HTTP request, or MCP tool argument is not authority.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use uuid::Uuid;

use crate::config;

const SCHEMA: u32 = 1;
const MAX_BODY: usize = 64 * 1024;
const MAX_RECORD: usize = 4096;
const MAX_FILES: usize = 8192;
const MAX_REQUEST: usize = MAX_BODY + 4096;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRequest {
    schema_version: u32,
    envelope: Envelope,
    body: Option<String>,
}

#[derive(Serialize)]
struct WireResponse {
    schema_version: u32,
    status: &'static str,
    observation_id: Option<Uuid>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Agent {
    Codex,
    OpenCode,
    DevinAcp,
    DevinCloud,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    UserPromptAccepted,
    UserSteerAccepted,
    ConversationBound,
    ConversationUnbound,
    PromptObservationGap,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GapReason {
    HookUnavailable,
    DeliveryFailed,
    UnsupportedContent,
    SourceSequenceGap,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionFence {
    pub session_id: String,
    pub started_at: u64,
    pub process_id: u32,
    pub scope_cwd: PathBuf,
}

impl SessionFence {
    fn matches(&self, session: &config::Session) -> bool {
        self.session_id == session.id
            && self.started_at == session.started_at
            && self.process_id == session.process_id
            && self.scope_cwd == session.cwd
    }
}

/// Exact source identity and optional *explicit* linkage. All strings are
/// validated before storage; no prompt text enters this envelope.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Envelope {
    pub schema_version: u32,
    pub host_id: String,
    pub session: Option<SessionFence>,
    pub agent: Agent,
    pub conversation_id: String,
    pub source_event_id: String,
    pub kind: Kind,
    #[serde(default)]
    pub gap_reason: Option<GapReason>,
    pub repository_key: Option<String>,
    pub workspace_id: Option<String>,
    pub task_id: Option<String>,
    pub execution_id: Option<String>,
    #[serde(default)]
    pub agent_turn_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Record {
    pub id: Uuid,
    pub envelope: Envelope,
    pub observed_at: u64,
    pub body_sha256: Option<String>,
    pub body_bytes: usize,
    /// Opaque local reference. Ordinary projections never include its body.
    pub content_ref: Option<String>,
    /// Read-time additive links, never part of the durable envelope.
    #[serde(skip)]
    pub correlations: Vec<CanonicalLink>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CanonicalLink {
    pub schema_version: u32,
    pub prompt_id: Uuid,
    pub host_id: String,
    pub session: SessionFence,
    pub agent: Agent,
    pub conversation_id: String,
    pub task_id: String,
    pub execution_id: String,
    #[serde(default)]
    pub agent_turn_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Append {
    Accepted(Uuid),
    Duplicate(Uuid),
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Coverage {
    Supported,
    Partial,
    Unavailable,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct WorkerPolicy {
    pub max_age_seconds: u64,
    pub max_items: usize,
}

/// Deterministic provider-neutral worker eligibility. Only structural refs
/// cross this boundary; local prompt bodies remain in the owner store.
pub(crate) fn worker_eligible(
    records: &[Record],
    now: u64,
    policy: WorkerPolicy,
) -> Result<Vec<serde_json::Value>> {
    ensure!(
        policy.max_age_seconds <= 86_400 && policy.max_items <= 16,
        "prompt worker policy exceeds local budget"
    );
    Ok(records
        .iter()
        .rev()
        .filter(|record| {
            matches!(
                record.envelope.kind,
                Kind::UserPromptAccepted | Kind::UserSteerAccepted
            ) && now >= record.observed_at
                && now - record.observed_at <= policy.max_age_seconds
        })
        .take(policy.max_items)
        .map(|record| {
            serde_json::json!({
                "observation_id": record.id,
                "agent": record.envelope.agent,
                "kind": record.envelope.kind,
                "observed_at": record.observed_at,
                "body_sha256": record.body_sha256,
                "provenance": "owner_local_source_asserted",
                "content_policy": "structural_only",
            })
        })
        .collect())
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Capability {
    pub agent: Agent,
    pub surface: &'static str,
    pub native_capability: Coverage,
    /// Actual Temote-observed coverage for this surface in this build.
    pub coverage: Coverage,
    pub reason: &'static str,
}

/// Versioned assessment of the currently probed *direct* surfaces. A native
/// pre-submit hook is not proof that a user turn was accepted.
pub(crate) fn installed_capabilities() -> [Capability; 5] {
    [
        Capability {
            agent: Agent::Codex,
            surface: "direct_ui",
            native_capability: Coverage::Partial,
            coverage: Coverage::Unavailable,
            reason: "Codex 0.157.1 UserPromptSubmit is pre-acceptance; no direct accepted-turn adapter is installed.",
        },
        Capability {
            agent: Agent::Codex,
            surface: "temote_owned_app_server",
            native_capability: Coverage::Supported,
            coverage: Coverage::Supported,
            reason: "The Codex 0.157.1 v2 item/started userMessage adapter observes this client's owned conversations; external direct UI coverage is separate.",
        },
        Capability {
            agent: Agent::OpenCode,
            surface: "direct_ui",
            native_capability: Coverage::Unavailable,
            coverage: Coverage::Unavailable,
            reason: "No installed authenticated accepted-user-turn adapter for unrelated OpenCode direct conversations has been verified.",
        },
        Capability {
            agent: Agent::DevinAcp,
            surface: "direct_ui",
            native_capability: Coverage::Unavailable,
            coverage: Coverage::Unavailable,
            reason: "Devin ACP session/prompt is client initiated; no verified hook for unrelated direct user turns is installed.",
        },
        Capability {
            agent: Agent::DevinCloud,
            surface: "direct_ui",
            native_capability: Coverage::Unavailable,
            coverage: Coverage::Unavailable,
            reason: "No verified Devin Cloud direct user-turn webhook or stable message event is installed.",
        },
    ]
}

/// Construct only from OS-reported peer credentials at the local ingress.
pub(crate) struct AuthenticatedLocalPeer {
    uid: u32,
}

impl AuthenticatedLocalPeer {
    pub(crate) fn from_unix_stream(stream: &tokio::net::UnixStream) -> Result<Self> {
        let uid = stream.peer_cred()?.uid();
        ensure!(
            uid == unsafe { libc::geteuid() },
            "local peer is not the Temote owner"
        );
        Ok(Self { uid })
    }
}

pub(crate) struct Store {
    directory: PathBuf,
}

impl Store {
    pub(crate) fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    pub(crate) fn default_store() -> Result<Self> {
        Ok(Self::new(config::state_dir()?.join("agent-prompts")))
    }

    /// This is the transport integration seam. Authority comes from the
    /// Unix stream's OS peer credentials, never a deserialized request field.
    pub(crate) async fn observe(
        &self,
        peer: &AuthenticatedLocalPeer,
        envelope: Envelope,
        body: Option<&str>,
    ) -> Result<Append> {
        ensure!(peer.uid == unsafe { libc::geteuid() }, "local peer changed");
        ensure!(
            envelope.host_id == crate::host_identity::resolve()?,
            "prompt host identity mismatch"
        );
        if let Some(fence) = &envelope.session {
            let current = config::load_session(&fence.session_id).await?;
            ensure!(fence.matches(&current), "session instance changed");
        }
        self.append(envelope, body)
    }

    /// Internal app-server producer. It has no caller-controlled socket peer;
    /// authority is the live, fully fenced session supplied by its owner.
    pub(crate) async fn observe_owned(
        &self,
        envelope: Envelope,
        body: Option<&str>,
    ) -> Result<Append> {
        ensure!(
            envelope.agent == Agent::Codex,
            "owned prompt agent mismatch"
        );
        ensure!(
            envelope.session.is_some(),
            "owned prompt has no session fence"
        );
        ensure!(
            envelope.task_id.is_some()
                && envelope.execution_id.is_some()
                && envelope.agent_turn_id.is_some(),
            "owned prompt lacks canonical task binding"
        );
        ensure!(
            envelope.host_id == crate::host_identity::resolve()?,
            "prompt host identity mismatch"
        );
        let fence = envelope.session.as_ref().unwrap();
        let current = config::load_session(&fence.session_id).await?;
        ensure!(fence.matches(&current), "session instance changed");
        self.append(envelope, body)
    }

    fn append(&self, envelope: Envelope, body: Option<&str>) -> Result<Append> {
        validate(&envelope, body)?;
        private_dir(&self.directory)?;
        private_dir(&self.directory.join("records"))?;
        private_dir(&self.directory.join("content"))?;
        let _lock = Lock::new(&self.directory.join(".lock"))?;
        let identity = serde_json::to_vec(&(
            &envelope.host_id,
            envelope.agent,
            &envelope.conversation_id,
            &envelope.source_event_id,
        ))?;
        let key = hex_digest(&identity);
        let record_path = self.directory.join("records").join(format!("{key}.json"));
        let digest = body.map(|body| hex_digest(body.as_bytes()));
        if record_path.exists() {
            let existing: Record = read_private(&record_path, MAX_RECORD)?;
            ensure!(
                existing.envelope == envelope && existing.body_sha256 == digest,
                "conflicting prompt source identity"
            );
            if let (Some(name), Some(text)) = (&existing.content_ref, body) {
                ensure!(
                    name == &format!("{key}.txt"),
                    "invalid prompt content reference"
                );
                let path = self.directory.join("content").join(name);
                if path.exists() {
                    ensure!(
                        hex_digest(&read_private_bytes(&path, MAX_BODY)?)
                            == hex_digest(text.as_bytes()),
                        "prompt content integrity mismatch"
                    );
                } else {
                    create_private(&path, text.as_bytes())?;
                }
            }
            return Ok(Append::Duplicate(existing.id));
        }
        ensure!(
            fs::read_dir(self.directory.join("records"))?
                .take(MAX_FILES + 1)
                .count()
                < MAX_FILES,
            "prompt store retention limit reached"
        );
        let id = Uuid::new_v4();
        let content_ref = body.map(|_| format!("{key}.txt"));
        if let (Some(text), Some(name)) = (body, &content_ref) {
            let path = self.directory.join("content").join(name);
            if path.exists() {
                let existing = read_private_bytes(&path, MAX_BODY)?;
                ensure!(
                    hex_digest(&existing) == hex_digest(text.as_bytes()),
                    "conflicting orphan prompt content"
                );
            } else {
                create_private(&path, text.as_bytes())?;
            }
        }
        let record = Record {
            id,
            envelope,
            observed_at: config::unix_time(),
            body_sha256: digest,
            body_bytes: body.map_or(0, str::len),
            content_ref,
            correlations: Vec::new(),
        };
        create_private(&record_path, &serde_json::to_vec(&record)?)?;
        Ok(Append::Accepted(id))
    }

    /// Additive exact correlation. The orchestration owner supplies only
    /// canonical backend conversation/task/execution identity from a retained
    /// accepted task. Timing or repository proximity cannot create a link.
    pub(crate) async fn link_exact(&self, link: CanonicalLink) -> Result<()> {
        ensure!(
            link.schema_version == SCHEMA,
            "unsupported prompt link schema"
        );
        ensure!(
            link.host_id == crate::host_identity::resolve()?,
            "prompt link host identity mismatch"
        );
        ident(&link.conversation_id, 256)?;
        ident(&link.task_id, 256)?;
        ident(&link.execution_id, 256)?;
        if let Some(turn) = &link.agent_turn_id {
            ident(turn, 256)?;
        }
        let current = config::load_session(&link.session.session_id).await?;
        ensure!(
            link.session.matches(&current),
            "prompt link session instance changed"
        );
        private_dir(&self.directory)?;
        private_dir(&self.directory.join("links"))?;
        let _lock = Lock::new(&self.directory.join(".lock"))?;
        let records_dir = self.directory.join("records");
        check_dir(&records_dir)?;
        let mut found = false;
        for entry in fs::read_dir(records_dir)?.take(MAX_FILES + 1) {
            let record: Record = read_private(&entry?.path(), MAX_RECORD)?;
            if record.id == link.prompt_id {
                ensure!(
                    record.envelope.host_id == link.host_id
                        && record.envelope.agent == link.agent
                        && record.envelope.conversation_id == link.conversation_id,
                    "prompt link source identity mismatch"
                );
                ensure!(
                    record
                        .envelope
                        .task_id
                        .as_ref()
                        .is_none_or(|id| id == &link.task_id)
                        && record
                            .envelope
                            .execution_id
                            .as_ref()
                            .is_none_or(|id| id == &link.execution_id)
                        && record
                            .envelope
                            .agent_turn_id
                            .as_ref()
                            .is_none_or(|id| link.agent_turn_id.as_ref() == Some(id)),
                    "prompt link conflicts with explicit source identity"
                );
                found = true;
                break;
            }
        }
        ensure!(found, "prompt observation not found");
        let key = hex_digest(&serde_json::to_vec(&(
            &link.prompt_id,
            &link.session,
            &link.task_id,
        ))?);
        let path = self.directory.join("links").join(format!("{key}.json"));
        if path.exists() {
            let existing: CanonicalLink = read_private(&path, MAX_RECORD)?;
            ensure!(existing == link, "conflicting prompt correlation");
            return Ok(());
        }
        ensure!(
            fs::read_dir(self.directory.join("links"))?
                .take(MAX_FILES + 1)
                .count()
                < MAX_FILES,
            "prompt link retention limit reached"
        );
        create_private(&path, &serde_json::to_vec(&link)?)
    }

    /// Bounded structural projection for an exact live session instance.
    /// Unbound events remain local until an explicit canonical link is added.
    pub(crate) fn for_session(&self, session: &config::Session) -> Result<Vec<Record>> {
        let host_id = crate::host_identity::resolve()?;
        let dir = self.directory.join("records");
        if !dir.exists() {
            return Ok(Vec::new());
        }
        check_dir(&dir)?;
        let mut linked: HashMap<Uuid, Vec<CanonicalLink>> = HashMap::new();
        let links_dir = self.directory.join("links");
        if links_dir.exists() {
            check_dir(&links_dir)?;
            for entry in fs::read_dir(links_dir)?.take(MAX_FILES + 1) {
                let link: CanonicalLink = read_private(&entry?.path(), MAX_RECORD)?;
                if link.host_id == host_id && link.session.matches(session) {
                    let links = linked.entry(link.prompt_id).or_default();
                    if links.len() < 8 {
                        links.push(link);
                    }
                }
            }
        }
        let mut records = Vec::new();
        for entry in fs::read_dir(dir)?.take(MAX_FILES + 1) {
            let entry = entry?;
            ensure!(records.len() < MAX_FILES, "prompt store exceeds scan limit");
            if entry.path().extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let mut record: Record = read_private(&entry.path(), MAX_RECORD)?;
            let mut correlations = linked.remove(&record.id).unwrap_or_default();
            correlations.retain(|link| {
                link.host_id == record.envelope.host_id
                    && link.agent == record.envelope.agent
                    && link.conversation_id == record.envelope.conversation_id
            });
            if record.envelope.host_id == host_id
                && (record
                    .envelope
                    .session
                    .as_ref()
                    .is_some_and(|fence| fence.matches(session))
                    || !correlations.is_empty())
            {
                record.correlations = correlations;
                record
                    .correlations
                    .sort_by(|a, b| a.task_id.cmp(&b.task_id));
                records.push(record);
            }
        }
        records.sort_by_key(|r| (r.observed_at, r.id));
        if records.len() > 64 {
            records.drain(..records.len() - 64);
        }
        Ok(records)
    }
}

/// Explicit owner-side daemon entrypoint. No public HTTP or MCP prompt write
/// is exposed. The socket resides under the private prompt state directory.
pub(crate) async fn serve_local() -> Result<()> {
    let store = Store::default_store()?;
    private_dir(&store.directory)?;
    let path = store.directory.join("ingress.sock");
    if let Ok(meta) = fs::symlink_metadata(&path) {
        ensure!(
            meta.file_type().is_socket()
                && meta.permissions().mode() & 0o077 == 0
                && meta.uid() == unsafe { libc::geteuid() },
            "prompt ingress path is not a private socket"
        );
        match tokio::time::timeout(
            std::time::Duration::from_secs(1),
            UnixStream::connect(&path),
        )
        .await
        {
            Ok(Ok(_)) => anyhow::bail!("prompt ingress listener is already running"),
            Ok(Err(error)) if error.kind() == std::io::ErrorKind::ConnectionRefused => {}
            Ok(Err(error)) => return Err(error).context("cannot probe prompt ingress socket"),
            Err(_) => anyhow::bail!("prompt ingress socket liveness is uncertain"),
        }
        fs::remove_file(&path).context("cannot remove stale prompt ingress socket")?;
    }
    let listener = UnixListener::bind(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    let inode = fs::symlink_metadata(&path)?.ino();
    struct SocketGuard(PathBuf, u64);
    impl Drop for SocketGuard {
        fn drop(&mut self) {
            if fs::symlink_metadata(&self.0).is_ok_and(|meta| meta.ino() == self.1) {
                let _ = fs::remove_file(&self.0);
            }
        }
    }
    let _guard = SocketGuard(path, inode);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let _ = tokio::time::timeout(std::time::Duration::from_secs(5),
                    handle_connection(stream, &store)).await;
            }
            signal = tokio::signal::ctrl_c() => { signal?; return Ok(()); }
        }
    }
}

async fn handle_connection(mut stream: UnixStream, store: &Store) -> Result<()> {
    let peer = AuthenticatedLocalPeer::from_unix_stream(&stream)?;
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length).await?;
    let length = u32::from_be_bytes(length) as usize;
    ensure!(
        length > 0 && length <= MAX_REQUEST,
        "prompt request exceeds limit"
    );
    let mut bytes = vec![0_u8; length];
    stream.read_exact(&mut bytes).await?;
    let result = async {
        let request: WireRequest = serde_json::from_slice(&bytes)?;
        ensure!(
            request.schema_version == SCHEMA,
            "unsupported prompt wire version"
        );
        store
            .observe(&peer, request.envelope, request.body.as_deref())
            .await
    }
    .await;
    let response = match result {
        Ok(Append::Accepted(id)) => WireResponse {
            schema_version: SCHEMA,
            status: "accepted",
            observation_id: Some(id),
        },
        Ok(Append::Duplicate(id)) => WireResponse {
            schema_version: SCHEMA,
            status: "duplicate",
            observation_id: Some(id),
        },
        Err(_) => WireResponse {
            schema_version: SCHEMA,
            status: "rejected",
            observation_id: None,
        },
    };
    let bytes = serde_json::to_vec(&response)?;
    stream
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .await?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}

fn validate(e: &Envelope, body: Option<&str>) -> Result<()> {
    ensure!(e.schema_version == SCHEMA, "unsupported prompt schema");
    crate::host_identity::validate(&e.host_id)?;
    ident(&e.conversation_id, 256)?;
    ident(&e.source_event_id, 256)?;
    for value in [
        &e.repository_key,
        &e.workspace_id,
        &e.task_id,
        &e.execution_id,
        &e.agent_turn_id,
    ]
    .into_iter()
    .flatten()
    {
        ident(value, 256)?;
    }
    if let Some(fence) = &e.session {
        config::validate_session_id(&fence.session_id)?;
        ensure!(
            fence.started_at > 0 && fence.process_id > 0,
            "incomplete session fence"
        );
    }
    ensure!(
        body.is_none_or(|b| b.len() <= MAX_BODY),
        "prompt body exceeds limit"
    );
    ensure!(body.is_none_or(|b| !b.is_empty()), "empty user instruction");
    ensure!(
        matches!(e.kind, Kind::UserPromptAccepted | Kind::UserSteerAccepted) == body.is_some(),
        "body is required only for accepted user instructions"
    );
    ensure!(
        (e.kind == Kind::PromptObservationGap) == e.gap_reason.is_some(),
        "gap reason is required only for observation gaps"
    );
    Ok(())
}

fn ident(value: &str, max: usize) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= max
            && value.bytes().all(|b| b.is_ascii_alphanumeric()
                || matches!(b, b'_' | b'-' | b'.' | b':' | b'/' | b'@')),
        "invalid prompt identifier"
    );
    Ok(())
}

fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn private_dir(path: &Path) -> Result<()> {
    if !path.exists() {
        fs::create_dir_all(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    check_dir(path)
}

fn check_dir(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.file_type().is_dir()
            && meta.permissions().mode() & 0o077 == 0
            && meta.uid() == unsafe { libc::geteuid() },
        "prompt directory is not owner-only"
    );
    Ok(())
}

fn create_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    File::open(path.parent().context("missing parent")?)?.sync_all()?;
    Ok(())
}

fn read_private<T: serde::de::DeserializeOwned>(path: &Path, limit: usize) -> Result<T> {
    let bytes = read_private_bytes(path, limit)?;
    serde_json::from_slice(&bytes).context("invalid prompt record")
}

fn read_private_bytes(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file()
            && meta.len() <= limit as u64
            && meta.permissions().mode() & 0o077 == 0
            && meta.uid() == unsafe { libc::geteuid() },
        "prompt record is not a bounded owner-only file"
    );
    let mut bytes = Vec::new();
    file.take((limit + 1) as u64).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "prompt record exceeds limit");
    Ok(bytes)
}

struct Lock {
    file: File,
}
impl Lock {
    fn new(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        let meta = file.metadata()?;
        ensure!(
            meta.permissions().mode() & 0o077 == 0 && meta.uid() == unsafe { libc::geteuid() },
            "prompt lock is not owner-only"
        );
        ensure!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0,
            "prompt lock failed"
        );
        Ok(Self { file })
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope() -> Envelope {
        Envelope {
            schema_version: SCHEMA,
            host_id: crate::host_identity::resolve().unwrap(),
            session: None,
            agent: Agent::Codex,
            conversation_id: "thread_1".into(),
            source_event_id: "turn_1".into(),
            kind: Kind::UserPromptAccepted,
            gap_reason: None,
            repository_key: None,
            workspace_id: None,
            task_id: None,
            execution_id: None,
            agent_turn_id: None,
        }
    }

    #[test]
    fn replay_and_conflict_survive_reopen_without_body_leak() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("prompts"));
        let id = match store
            .append(envelope(), Some("private instruction"))
            .unwrap()
        {
            Append::Accepted(id) => id,
            _ => panic!("first append"),
        };
        let reopened = Store::new(dir.path().join("prompts"));
        assert_eq!(
            reopened
                .append(envelope(), Some("private instruction"))
                .unwrap(),
            Append::Duplicate(id)
        );
        assert!(reopened.append(envelope(), Some("changed")).is_err());
        let name = fs::read_dir(dir.path().join("prompts/records"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let serialized = fs::read_to_string(name).unwrap();
        assert!(!serialized.contains("private instruction"));
        assert_eq!(
            fs::metadata(dir.path().join("prompts/content"))
                .unwrap()
                .permissions()
                .mode()
                & 0o077,
            0
        );
        let content_path = fs::read_dir(dir.path().join("prompts/content"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(
            fs::metadata(content_path).unwrap().permissions().mode() & 0o077,
            0
        );
    }

    #[test]
    fn codex_v2_source_item_replay_uses_stable_item_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("prompts"));
        let event = serde_json::json!({
            "threadId": "thread-1", "turnId": "turn-1", "startedAtMs": 42,
            "item": {"type": "userMessage", "id": "item-1",
                "content": [{"type": "text", "text": "private instruction"}]}
        });
        let candidate =
            crate::codex_prompt_observer::parse_item_started("item/started", Some(&event)).unwrap();
        let mut e = envelope();
        e.conversation_id = candidate.thread_id;
        e.source_event_id = candidate.item_id;
        e.agent_turn_id = Some(candidate.turn_id);
        let body = candidate.body.unwrap();
        let first = store.append(e.clone(), Some(&body)).unwrap();
        let Append::Accepted(id) = first else {
            panic!("first observation must be new");
        };
        assert_eq!(
            store.append(e.clone(), Some(&body)).unwrap(),
            Append::Duplicate(id)
        );
        assert!(store.append(e, Some("different instruction")).is_err());
    }

    #[tokio::test]
    async fn owned_observer_rejects_stale_session_before_storage() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("prompts"));
        let mut e = envelope();
        e.session = Some(SessionFence {
            session_id: Uuid::new_v4().to_string(),
            started_at: 10,
            process_id: 42,
            scope_cwd: dir.path().to_path_buf(),
        });
        e.task_id = Some(Uuid::new_v4().to_string());
        e.execution_id = Some(Uuid::new_v4().to_string());
        e.agent_turn_id = Some("turn-1".into());
        assert!(
            store
                .observe_owned(e, Some("private intent"))
                .await
                .is_err()
        );
        assert!(!dir.path().join("prompts/records").exists());
    }

    #[test]
    fn rejects_unbounded_or_non_user_content() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("prompts"));
        let mut e = envelope();
        e.conversation_id = "x".repeat(257);
        assert!(store.append(e, Some("hi")).is_err());
        let mut e = envelope();
        e.kind = Kind::PromptObservationGap;
        e.gap_reason = Some(GapReason::HookUnavailable);
        assert!(store.append(e, Some("hidden")).is_err());
        assert!(
            store
                .append(envelope(), Some(&"x".repeat(MAX_BODY + 1)))
                .is_err()
        );
    }

    #[test]
    fn native_hook_capability_reports_only_owned_runtime_coverage() {
        let capabilities = installed_capabilities();
        assert!(
            capabilities
                .iter()
                .filter(|c| c.surface == "direct_ui")
                .all(|c| c.coverage == Coverage::Unavailable)
        );
        assert!(
            capabilities
                .iter()
                .any(|c| c.surface == "temote_owned_app_server"
                    && c.native_capability == Coverage::Supported
                    && c.coverage == Coverage::Supported)
        );
    }

    #[test]
    fn retry_after_content_write_before_record_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("prompts"));
        let e = envelope();
        let identity =
            serde_json::to_vec(&(&e.host_id, e.agent, &e.conversation_id, &e.source_event_id))
                .unwrap();
        private_dir(&dir.path().join("prompts")).unwrap();
        let content_dir = dir.path().join("prompts/content");
        private_dir(&content_dir).unwrap();
        create_private(
            &content_dir.join(format!("{}.txt", hex_digest(&identity))),
            b"body",
        )
        .unwrap();
        assert!(matches!(
            store.append(e, Some("body")).unwrap(),
            Append::Accepted(_)
        ));
    }

    #[test]
    fn context_projection_requires_full_instance_fence() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("prompts"));
        let mut e = envelope();
        let session_id = Uuid::new_v4().to_string();
        e.session = Some(SessionFence {
            session_id: session_id.clone(),
            started_at: 10,
            process_id: 42,
            scope_cwd: dir.path().to_path_buf(),
        });
        store.append(e, Some("private intent")).unwrap();
        let mut session = config::Session {
            id: session_id,
            cwd: dir.path().to_path_buf(),
            permitted_directories: vec![dir.path().to_path_buf()],
            started_at: 10,
            process_id: 42,
            permission_mode: config::PermissionMode::Agent,
            grants: config::SessionGrants::default(),
        };
        assert_eq!(store.for_session(&session).unwrap().len(), 1);
        session.started_at = 11;
        assert!(store.for_session(&session).unwrap().is_empty());
    }

    #[test]
    fn unbound_prompt_appears_only_after_exact_additive_link() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("prompts"));
        let e = envelope();
        let id = match store.append(e.clone(), Some("private intent")).unwrap() {
            Append::Accepted(id) => id,
            _ => panic!("first append"),
        };
        let session = config::Session {
            id: Uuid::new_v4().to_string(),
            cwd: dir.path().to_path_buf(),
            permitted_directories: vec![dir.path().to_path_buf()],
            started_at: 10,
            process_id: 42,
            permission_mode: config::PermissionMode::Agent,
            grants: config::SessionGrants::default(),
        };
        assert!(store.for_session(&session).unwrap().is_empty());
        let link = CanonicalLink {
            schema_version: SCHEMA,
            prompt_id: id,
            host_id: e.host_id,
            session: SessionFence {
                session_id: session.id.clone(),
                started_at: 10,
                process_id: 42,
                scope_cwd: dir.path().to_path_buf(),
            },
            agent: e.agent,
            conversation_id: e.conversation_id,
            task_id: "task_1".into(),
            execution_id: "thread_1".into(),
            agent_turn_id: None,
        };
        let links_dir = dir.path().join("prompts/links");
        private_dir(&links_dir).unwrap();
        create_private(
            &links_dir.join("link.json"),
            &serde_json::to_vec(&link).unwrap(),
        )
        .unwrap();
        let view = store.for_session(&session).unwrap();
        assert_eq!(view.len(), 1);
        assert_eq!(view[0].correlations[0], link);
        let mut replaced = session;
        replaced.started_at = 11;
        assert!(store.for_session(&replaced).unwrap().is_empty());
    }

    #[test]
    fn worker_projection_is_fresh_bounded_and_structural_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("prompts"));
        store
            .append(envelope(), Some("private worker input"))
            .unwrap();
        let path = fs::read_dir(dir.path().join("prompts/records"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let mut record: Record = read_private(&path, MAX_RECORD).unwrap();
        record.observed_at = 100;
        let policy = WorkerPolicy {
            max_age_seconds: 60,
            max_items: 1,
        };
        let eligible = worker_eligible(&[record.clone()], 120, policy).unwrap();
        assert_eq!(eligible.len(), 1);
        assert!(!eligible[0].to_string().contains("private worker input"));
        assert!(
            worker_eligible(&[record.clone()], 200, policy)
                .unwrap()
                .is_empty()
        );
        assert!(
            worker_eligible(
                &[record],
                120,
                WorkerPolicy {
                    max_age_seconds: 60,
                    max_items: 17
                }
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn local_wire_accepts_and_replays_with_os_peer_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("prompts"));
        let request = serde_json::to_vec(&serde_json::json!({
            "schema_version": SCHEMA, "envelope": envelope(), "body": "private intent"
        }))
        .unwrap();
        let mut accepted_id: Option<serde_json::Value> = None;
        for expected in ["accepted", "duplicate"] {
            let (mut client, server) = UnixStream::pair().unwrap();
            let server_side = handle_connection(server, &store);
            let client_side = async {
                client
                    .write_all(&(request.len() as u32).to_be_bytes())
                    .await
                    .unwrap();
                client.write_all(&request).await.unwrap();
                let mut length = [0_u8; 4];
                client.read_exact(&mut length).await.unwrap();
                let mut response = vec![0_u8; u32::from_be_bytes(length) as usize];
                client.read_exact(&mut response).await.unwrap();
                response
            };
            let (server_result, response) = tokio::join!(server_side, client_side);
            server_result.unwrap();
            let response: serde_json::Value = serde_json::from_slice(&response).unwrap();
            assert_eq!(response["status"], expected);
            assert!(!response.to_string().contains("private intent"));
            if let Some(ref id) = accepted_id {
                assert_eq!(&response["observation_id"], id);
            } else {
                accepted_id = Some(response["observation_id"].clone());
            }
        }
    }
}
