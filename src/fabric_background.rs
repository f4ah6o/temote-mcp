//! Owner-managed browser Link. Public lifecycle data never carries credentials.
use crate::fabric_browser::{self, BrowserConnectOptions};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    Starting,
    Online,
    Reconnecting,
    Stopping,
    Stopped,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StopReason {
    Requested,
    StartupCancelled,
    RecoveryRequired,
    AuthenticationRejected,
    ScopeChanged,
    LinkFailed,
}

pub(crate) struct ManagedControl {
    stopped: watch::Sender<Option<StopReason>>,
    ready: watch::Sender<Option<u64>>,
    committed: watch::Sender<bool>,
    verified: watch::Sender<bool>,
    state: Mutex<PublicState>,
}
impl ManagedControl {
    fn new(state: PublicState) -> Arc<Self> {
        Arc::new(Self {
            stopped: watch::channel(None).0,
            ready: watch::channel(None).0,
            committed: watch::channel(false).0,
            verified: watch::channel(true).0,
            state: Mutex::new(state),
        })
    }
    pub(crate) fn stop(&self, reason: StopReason) {
        self.stopped.send_if_modified(|value| {
            if value.is_some() {
                false
            } else {
                *value = Some(reason);
                true
            }
        });
        self.set_phase(Phase::Stopping, None);
    }
    pub(crate) async fn shutdown(&self) {
        let mut rx = self.stopped.subscribe();
        while rx.borrow_and_update().is_none() {
            if rx.changed().await.is_err() {
                break;
            }
        }
    }
    pub(crate) async fn admission_verified(&self) -> Result<()> {
        let mut verified = self.verified.subscribe();
        while !*verified.borrow_and_update() {
            tokio::select! { _ = self.shutdown() => anyhow::bail!("managed admission halted"), result = verified.changed() => { result?; } }
        }
        anyhow::ensure!(self.stopped.borrow().is_none(), "managed admission halted");
        Ok(())
    }
    pub(crate) fn pause_verification(&self) {
        self.verified.send_replace(false);
        let generation = self.snapshot().ok().and_then(|state| state.generation);
        self.set_phase(Phase::Reconnecting, generation);
    }
    pub(crate) fn resume_verification(&self) {
        self.verified.send_replace(true);
        if let Some(generation) = self.snapshot().ok().and_then(|state| state.generation) {
            self.set_phase(Phase::Online, Some(generation));
        }
    }
    pub(crate) async fn connected(&self, generation: u64) -> Result<()> {
        self.admission_verified().await?;
        self.set_phase(Phase::Online, Some(generation));
        anyhow::ensure!(
            self.stopped.borrow().is_none(),
            "managed Link halted before readiness"
        );
        self.ready.send_replace(Some(generation));
        let mut commit = self.committed.subscribe();
        while !*commit.borrow_and_update() {
            tokio::select! { _ = self.shutdown() => anyhow::bail!("managed startup cancelled"),
            result = commit.changed() => { result.context("managed startup unavailable")?; } }
        }
        anyhow::ensure!(
            self.stopped.borrow().is_none(),
            "managed Link halted before commit"
        );
        Ok(())
    }
    pub(crate) fn reconnecting(&self) {
        self.set_phase(Phase::Reconnecting, None);
    }
    fn set_phase(&self, phase: Phase, generation: Option<u64>) {
        if let Ok(mut state) = self.state.lock() {
            if matches!(state.phase, Phase::Stopping | Phase::Stopped) && phase != Phase::Stopped {
                return;
            }
            state.phase = phase;
            state.generation = generation;
            state.reason = *self.stopped.borrow();
            #[cfg(unix)]
            if process::write_state(&state).is_err() && phase != Phase::Stopped {
                self.stopped.send_replace(Some(StopReason::LinkFailed));
            }
        }
    }
    fn snapshot(&self) -> Result<PublicState> {
        self.state
            .lock()
            .map(|state| state.clone())
            .map_err(|_| anyhow::anyhow!("managed state unavailable"))
    }
}

/// Bounded retry under the same approved scope; refresh ambiguity is fatal.
pub(crate) fn retry_guard_read(
    error: &anyhow::Error,
    rotation_attempted: bool,
    now: u64,
    outage_since: &mut Option<u64>,
) -> bool {
    if rotation_attempted
        || error.downcast_ref::<fabric_browser::BrowserReadFailure>()
            != Some(&fabric_browser::BrowserReadFailure::Transient)
    {
        return false;
    }
    let started = *outage_since.get_or_insert(now);
    now.saturating_sub(started) < 60
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicState {
    schema_version: u32,
    profile: String,
    instance: String,
    pid: u32,
    gateway_origin: String,
    issuer: String,
    client_id: String,
    host_id: String,
    approved_roots: Vec<String>,
    phase: Phase,
    reason: Option<StopReason>,
    generation: Option<u64>,
}

#[cfg(unix)]
mod process {
    use super::*;
    use crate::fabric_browser::{NoninteractiveStore, OsCredentialStore, ProfileLock};
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::{
        fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        net::UnixStream,
        process::CommandExt,
    };
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    const MAX_MESSAGE: usize = 8192;
    const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
    const CONTROL_TIMEOUT: Duration = Duration::from_secs(3);

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Startup {
        options: BrowserConnectOptions,
        profile: String,
        instance: String,
        boot_generation: String,
    }
    #[derive(Serialize, Deserialize, PartialEq, Eq)]
    #[serde(tag = "message", rename_all = "snake_case", deny_unknown_fields)]
    enum Handoff {
        Ready { profile: String, instance: String },
        Commit,
        Ack,
        Failed,
    }
    #[derive(Serialize, Deserialize)]
    #[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
    enum Request {
        Status { profile: String, instance: String },
        Stop { profile: String, instance: String },
    }
    impl Request {
        fn binding(&self) -> (&str, &str) {
            match self {
                Self::Status { profile, instance } | Self::Stop { profile, instance } => {
                    (profile, instance)
                }
            }
        }
    }

    fn uid() -> u32 {
        unsafe { libc::geteuid() }
    }
    fn validate_profile(profile: &str) -> Result<()> {
        anyhow::ensure!(
            profile.len() == 64 && profile.bytes().all(|b| b.is_ascii_hexdigit()),
            "managed profile invalid"
        );
        Ok(())
    }
    fn state_dir() -> Result<PathBuf> {
        Ok(fabric_browser::browser_state_path()?.join("background"))
    }
    fn state_path(profile: &str) -> Result<PathBuf> {
        validate_profile(profile)?;
        Ok(state_dir()?.join(format!("{profile}.json")))
    }
    fn socket_dir() -> PathBuf {
        use sha2::{Digest, Sha256};
        let namespace =
            temote_mcp::environment::var("TEMOTE_MCP_SOCKET_NAMESPACE").unwrap_or_default();
        let digest = format!("{:x}", Sha256::digest(namespace.as_bytes()));
        PathBuf::from(format!("/tmp/temote-fbg-{}-{}", uid(), &digest[..12]))
    }
    fn socket_path(profile: &str) -> Result<PathBuf> {
        validate_profile(profile)?;
        Ok(socket_dir().join(format!("{}.sock", &profile[..32])))
    }
    fn check_dir(path: &Path) -> Result<()> {
        let m = std::fs::symlink_metadata(path)?;
        anyhow::ensure!(
            m.is_dir()
                && !m.file_type().is_symlink()
                && m.uid() == uid()
                && m.mode() & 0o777 == 0o700,
            "managed directory ownership or permissions invalid"
        );
        Ok(())
    }
    fn create_dir(path: &Path) -> Result<()> {
        if !path.exists() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)?;
        }
        check_dir(path)
    }
    pub(super) fn write_state(state: &PublicState) -> Result<()> {
        let directory = state_dir()?;
        check_dir(&directory)?;
        let path = state_path(&state.profile)?;
        let temp = directory.join(format!(".{}.{}.tmp", state.profile, uuid::Uuid::new_v4()));
        let result = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&temp)?;
            let bytes = serde_json::to_vec(state)?;
            anyhow::ensure!(bytes.len() <= MAX_MESSAGE, "managed state oversized");
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(&temp, &path)?;
            std::fs::File::open(directory)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }
    fn read_state(profile: &str) -> Result<Option<PublicState>> {
        let path = state_path(profile)?;
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => anyhow::bail!("managed state unavailable"),
        };
        check_dir(&state_dir()?)?;
        let m = file.metadata()?;
        anyhow::ensure!(
            m.is_file()
                && m.uid() == uid()
                && m.mode() & 0o777 == 0o600
                && m.len() <= MAX_MESSAGE as u64,
            "managed state invalid"
        );
        let mut bytes = Vec::new();
        file.take((MAX_MESSAGE + 1) as u64)
            .read_to_end(&mut bytes)?;
        let state: PublicState =
            serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("managed state invalid"))?;
        anyhow::ensure!(
            state.schema_version == 1
                && state.profile == profile
                && uuid::Uuid::parse_str(&state.instance).is_ok(),
            "managed state binding invalid"
        );
        Ok(Some(state))
    }
    fn peer_owner(stream: &tokio::net::UnixStream) -> Result<()> {
        let cred = stream.peer_cred()?;
        anyhow::ensure!(cred.uid() == uid(), "managed socket peer is not the owner");
        Ok(())
    }
    async fn send<T: Serialize>(stream: &mut tokio::net::UnixStream, value: &T) -> Result<()> {
        let bytes = serde_json::to_vec(value)?;
        anyhow::ensure!(bytes.len() <= MAX_MESSAGE, "managed message oversized");
        stream.write_u32(bytes.len() as u32).await?;
        stream.write_all(&bytes).await?;
        Ok(())
    }
    async fn receive<T: serde::de::DeserializeOwned>(
        stream: &mut tokio::net::UnixStream,
    ) -> Result<T> {
        let len = stream.read_u32().await? as usize;
        anyhow::ensure!(len <= MAX_MESSAGE, "managed message oversized");
        let mut bytes = vec![0; len];
        stream.read_exact(&mut bytes).await?;
        serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("managed message invalid"))
    }
    async fn query(state: &PublicState, stop: bool) -> Result<PublicState> {
        tokio::time::timeout(CONTROL_TIMEOUT, async {
            check_dir(&socket_dir())?;
            let path = socket_path(&state.profile)?;
            let m = std::fs::symlink_metadata(&path)?;
            anyhow::ensure!(
                m.file_type().is_socket() && m.uid() == uid() && m.mode() & 0o777 == 0o600,
                "managed socket invalid"
            );
            let mut stream = tokio::net::UnixStream::connect(path).await?;
            peer_owner(&stream)?;
            let request = if stop {
                Request::Stop {
                    profile: state.profile.clone(),
                    instance: state.instance.clone(),
                }
            } else {
                Request::Status {
                    profile: state.profile.clone(),
                    instance: state.instance.clone(),
                }
            };
            send(&mut stream, &request).await?;
            let reply: PublicState = receive(&mut stream).await?;
            anyhow::ensure!(
                reply.schema_version == 1
                    && reply.profile == state.profile
                    && reply.instance == state.instance
                    && reply.host_id == state.host_id
                    && reply.gateway_origin == state.gateway_origin,
                "managed response binding changed"
            );
            Ok(reply)
        })
        .await
        .context("managed control timeout")?
    }
    struct OwnerSocket {
        path: PathBuf,
        inode: u64,
        device: u64,
    }
    impl Drop for OwnerSocket {
        fn drop(&mut self) {
            if let Ok(m) = std::fs::symlink_metadata(&self.path)
                && m.ino() == self.inode
                && m.dev() == self.device
                && m.file_type().is_socket()
            {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
    async fn bind_control(
        control: &Arc<ManagedControl>,
    ) -> Result<(OwnerSocket, tokio::task::JoinHandle<()>)> {
        create_dir(&socket_dir())?;
        create_dir(&state_dir()?)?;
        let state = control.snapshot()?;
        let path = socket_path(&state.profile)?;
        if let Ok(m) = std::fs::symlink_metadata(&path) {
            anyhow::ensure!(
                m.file_type().is_socket() && m.uid() == uid(),
                "existing managed socket invalid"
            );
            // Lifetime lease is held here. A reachable socket is never unlinked,
            // including a collision in the short-path digest.
            anyhow::ensure!(
                tokio::net::UnixStream::connect(&path).await.is_err(),
                "managed socket already has an owner"
            );
            std::fs::remove_file(&path)?;
        }
        let listener = tokio::net::UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let m = std::fs::symlink_metadata(&path)?;
        let owner = OwnerSocket {
            path,
            inode: m.ino(),
            device: m.dev(),
        };
        write_state(&state)?;
        let control = control.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let control = control.clone();
                tokio::spawn(async move {
                    let _ = tokio::time::timeout(CONTROL_TIMEOUT, async {
                        peer_owner(&stream)?;
                        let request: Request = receive(&mut stream).await?;
                        let state = control.snapshot()?;
                        anyhow::ensure!(
                            request.binding() == (state.profile.as_str(), state.instance.as_str()),
                            "managed request binding invalid"
                        );
                        if matches!(request, Request::Stop { .. }) {
                            control.stop(StopReason::Requested);
                        }
                        send(&mut stream, &control.snapshot()?).await
                    })
                    .await;
                });
            }
        });
        Ok((owner, task))
    }
    async fn select_profile(options: &BrowserConnectOptions) -> Result<Option<PublicState>> {
        fabric_browser::validate_oauth_options(options)?;
        let origin = url::Url::parse(&options.gateway_url)?
            .origin()
            .ascii_serialization();
        let states = if let Some(host) = &options.host_id {
            read_state(&fabric_browser::profile_key(&origin, "", host))?
                .into_iter()
                .collect::<Vec<_>>()
        } else {
            let entries = match std::fs::read_dir(state_dir()?) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(_) => anyhow::bail!("managed state unavailable"),
            };
            let mut states = Vec::new();
            for (index, entry) in entries.enumerate() {
                anyhow::ensure!(
                    index < 256,
                    "managed profile inventory exceeds the local bound"
                );
                let entry = entry?;
                let name = entry.file_name();
                let Some(profile) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
                    continue;
                };
                if validate_profile(profile).is_ok()
                    && let Some(state) = read_state(profile)?
                {
                    states.push(state);
                }
            }
            states
        };
        let mut matches = states
            .into_iter()
            .filter(|state| state.gateway_origin == origin)
            .collect::<Vec<_>>();
        if matches.len() <= 1 {
            return Ok(matches.pop());
        }
        // The current Supervisor can disambiguate old stopped Host profiles.
        // No OAuth configuration or credential access is needed for control.
        if let Ok(ping) = crate::session_control::read_only_local_status().await
            && let Some(host) = ping["host_id"].as_str()
            && let Some(state) = matches.iter().find(|state| state.host_id == host)
        {
            return Ok(Some(state.clone()));
        }
        let mut live = Vec::new();
        for state in matches {
            match query(&state, false).await {
                Ok(_) => live.push(state),
                Err(error) if definitely_absent(&error) => {}
                Err(_) => anyhow::bail!("managed Host state is uncertain; supply --host-id"),
            }
        }
        anyhow::ensure!(
            live.len() <= 1,
            "multiple managed Hosts match; supply --host-id"
        );
        Ok(live.pop())
    }
    fn definitely_absent(error: &anyhow::Error) -> bool {
        error.downcast_ref::<std::io::Error>().is_some_and(|e| {
            matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            )
        })
    }
    async fn wait_stopped(state: &PublicState) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(45), async {
            loop {
                if let Some(current) = read_state(&state.profile)?
                    && current.instance != state.instance
                    && query(&current, false).await.is_ok()
                {
                    return Ok::<_, anyhow::Error>(());
                }
                if ProfileLock::link_lifetime(&state.profile).is_ok() {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .context("background Link is still stopping")?
    }

    pub(crate) async fn projection(options: &BrowserConnectOptions) -> serde_json::Value {
        match select_profile(options).await {
            Ok(Some(state)) => match query(&state, false).await {
                Ok(current) => {
                    serde_json::json!({"state":current.phase,"reason":current.reason,"host_id":current.host_id,"pid":current.pid,"generation":current.generation,"configuration_matches":current.issuer == options.issuer && current.client_id == options.client_id})
                }
                Err(error) if definitely_absent(&error) => {
                    serde_json::json!({"state":"not_running","last_state":state.phase,"reason":state.reason})
                }
                Err(_) => serde_json::json!({"state":"unavailable","last_state":state.phase}),
            },
            Ok(None) => serde_json::json!({"state":"not_running"}),
            Err(_) => serde_json::json!({"state":"unavailable"}),
        }
    }
    pub(crate) async fn disconnect(options: BrowserConnectOptions) -> Result<()> {
        let Some(state) = select_profile(&options).await? else {
            println!("No managed background Link is recorded.");
            return Ok(());
        };
        let reply = query(&state, true)
            .await
            .context("managed Link is unavailable; no PID signal was sent")?;
        println!(
            "{}",
            serde_json::json!({"background":reply.phase,"host_id":reply.host_id})
        );
        // An acknowledgement means stopping, not stopped. Confirm that the
        // exact instance has released its lifetime lease before success.
        wait_stopped(&state).await?;
        println!("Background Link stopped.");
        Ok(())
    }
    fn spawn_child(
        lease: &ProfileLock,
        configure: impl FnOnce(&mut Command, i32, i32),
    ) -> Result<(std::process::Child, tokio::net::UnixStream)> {
        let (parent, child_stream) = UnixStream::pair()?;
        parent.set_nonblocking(true)?;
        let channel_fd = child_stream.as_raw_fd();
        let lease_fd = lease.raw_fd();
        let mut command = Command::new(std::env::current_exe()?);
        command
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for name in [
            "HOME",
            "PATH",
            "XDG_STATE_HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_CACHE_HOME",
            "XDG_RUNTIME_DIR",
            "TMPDIR",
            "TEMOTE_RUNTIME_DIR",
            "TEMOTE_MCP_RUNTIME_DIR",
            "TEMOTE_SOCKET_NAMESPACE",
            "TEMOTE_MCP_SOCKET_NAMESPACE",
        ] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        configure(&mut command, channel_fd, lease_fd);
        unsafe {
            command.pre_exec(move || {
                // Only async-signal-safe syscalls between fork and exec.
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                for fd in [channel_fd, lease_fd] {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let child = command.spawn()?;
        drop(child_stream);
        let channel = tokio::net::UnixStream::from_std(parent)?;
        Ok((child, channel))
    }

    pub(crate) async fn start(options: BrowserConnectOptions) -> Result<()> {
        crate::fabric_cli::check_options(&options).await?;
        let inventory = crate::session_control::read_only_fabric_inventory().await.context("Background Link needs read-only root identity; review `temote upgrade --dry-run` and restart the upgraded Supervisor before connecting")?;
        let origin = url::Url::parse(&options.gateway_url)?
            .origin()
            .ascii_serialization();
        let display_host_id = crate::host_identity::validate(&inventory.host_id)?;
        let profile = fabric_browser::profile_key(&origin, "", &inventory.host_id);
        let mut lease = ProfileLock::link_lifetime(&profile)?;
        let protocol = fabric_browser::BrowserOAuth::new(options.clone())?;
        let prepared =
            fabric_browser::prepare_interactive_link(&options, inventory, &protocol, &profile)
                .await?;
        let startup = Startup {
            options,
            profile: profile.clone(),
            instance: uuid::Uuid::new_v4().to_string(),
            boot_generation: prepared.authority.supervisor_boot_generation,
        };
        // Credentials are deliberately dropped before spawning. Re-exec reads
        // an already approved grant through the noninteractive store boundary.
        drop(prepared.record);
        lease.disarm_unlock();
        let (mut child, mut channel) = spawn_child(&lease, |command, channel_fd, lease_fd| {
            command.args([
                "__fabric-background-child",
                &channel_fd.to_string(),
                &lease_fd.to_string(),
            ]);
        })?;
        let result = tokio::time::timeout(STARTUP_TIMEOUT, async {
            send(&mut channel, &startup).await?;
            let reply: Handoff = receive(&mut channel).await?;
            anyhow::ensure!(
                reply
                    == Handoff::Ready {
                        profile: startup.profile.clone(),
                        instance: startup.instance.clone()
                    },
                "background Link did not become ready; run foreground connect to review recovery"
            );
            // Parent close must not explicitly unlock the inherited open-file
            // description after committing ownership to the child.
            lease.disarm_unlock();
            send(&mut channel, &Handoff::Commit).await?;
            anyhow::ensure!(
                receive::<Handoff>(&mut channel).await? == Handoff::Ack,
                "background handoff acknowledgement unavailable"
            );
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("background Link startup timed out")
        .and_then(|r| r);
        drop(channel);
        if let Err(error) = result {
            // This is the child handle we just spawned, never a PID from a
            // lifecycle file. Stop its exact instance first if commit succeeded.
            if let Ok(Some(state)) = read_state(&profile)
                && state.instance == startup.instance
            {
                let _ = query(&state, true).await;
            }
            let stopped = tokio::time::timeout(Duration::from_secs(40), async {
                loop {
                    if child.try_wait()?.is_some() {
                        return Ok::<_, std::io::Error>(());
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
            .await;
            if !matches!(stopped, Ok(Ok(()))) {
                let _ = child.kill();
                let _ = child.wait();
            }
            return Err(error);
        }
        println!(
            "Background Link ready for Host {}. Use `temote fabric status` or `temote fabric disconnect`.",
            display_host_id
        );
        Ok(())
    }
    pub(crate) async fn child(channel_fd: i32, lease_fd: i32) -> Result<()> {
        anyhow::ensure!(
            channel_fd > 2 && lease_fd > 2 && channel_fd != lease_fd,
            "invalid managed descriptors"
        );
        for fd in [channel_fd, lease_fd] {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            anyhow::ensure!(
                flags >= 0
                    && unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == 0,
                "managed descriptor unavailable"
            );
        }
        let channel = unsafe { UnixStream::from_raw_fd(channel_fd) };
        channel.set_nonblocking(true)?;
        let mut channel = tokio::net::UnixStream::from_std(channel)?;
        peer_owner(&channel)?;
        let startup: Startup =
            tokio::time::timeout(STARTUP_TIMEOUT, receive(&mut channel)).await??;
        validate_profile(&startup.profile)?;
        anyhow::ensure!(
            uuid::Uuid::parse_str(&startup.instance).is_ok(),
            "managed instance invalid"
        );
        let lease = unsafe { ProfileLock::from_inherited(&startup.profile, lease_fd)? };
        // Disable interaction for the entire detached process as a second
        // boundary; no short-lived guard can accidentally re-enable it.
        #[cfg(target_os = "macos")]
        let _no_ui =
            security_framework::os::macos::keychain::SecKeychain::disable_user_interaction()
                .map_err(|_| {
                    anyhow::anyhow!("background Keychain interaction guard unavailable")
                })?;
        let protocol = fabric_browser::BrowserOAuth::new(startup.options.clone())?;
        let store = NoninteractiveStore(OsCredentialStore::open_profile_for_status(
            &startup.profile,
        )?);
        let inventory = crate::session_control::read_only_fabric_inventory().await?;
        // Display identity comes only from public inventory/settings. Keep it
        // independent of the secret-bearing saved credential record.
        let display_host_id = crate::host_identity::validate(&inventory.host_id)?;
        let display_gateway_origin = url::Url::parse(&startup.options.gateway_url)?
            .origin()
            .ascii_serialization();
        let prepared =
            fabric_browser::resume_browser_link(&startup.options, &inventory, &store, &protocol)
                .await?;
        anyhow::ensure!(
            prepared.profile == startup.profile
                && prepared.authority.supervisor_boot_generation == startup.boot_generation,
            "managed startup scope changed"
        );
        let control = ManagedControl::new(PublicState {
            schema_version: 1,
            profile: startup.profile.clone(),
            instance: startup.instance.clone(),
            pid: std::process::id(),
            gateway_origin: display_gateway_origin.clone(),
            issuer: startup.options.issuer.clone(),
            client_id: startup.options.client_id.clone(),
            host_id: display_host_id.clone(),
            approved_roots: prepared.record.approved_roots.clone(),
            phase: Phase::Starting,
            reason: None,
            generation: None,
        });
        let link = crate::gateway::run_managed_browser_link(
            protocol,
            prepared,
            control.clone(),
            display_host_id,
            display_gateway_origin,
        );
        run_instance(startup, lease, channel, control, link).await
    }

    async fn run_instance<F: std::future::Future<Output = Result<()>>>(
        startup: Startup,
        mut lease: ProfileLock,
        mut channel: tokio::net::UnixStream,
        control: Arc<ManagedControl>,
        link: F,
    ) -> Result<()> {
        let (socket, server) = bind_control(&control).await?;
        tokio::pin!(link);
        let handshake = async {
            let mut ready = control.ready.subscribe();
            while ready.borrow_and_update().is_none() {
                tokio::select! { result = ready.changed() => { result?; },
                _ = channel.readable() => { let mut byte = [0]; match channel.try_read(&mut byte) { Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}, _ => anyhow::bail!("parent left before commit") } },
                _ = control.shutdown() => anyhow::bail!("managed startup halted") }
            }
            send(
                &mut channel,
                &Handoff::Ready {
                    profile: startup.profile,
                    instance: startup.instance,
                },
            )
            .await?;
            anyhow::ensure!(
                receive::<Handoff>(&mut channel).await? == Handoff::Commit,
                "managed startup not committed"
            );
            lease.commit_handoff();
            control.committed.send_replace(true);
            send(&mut channel, &Handoff::Ack).await?;
            Ok::<_, anyhow::Error>(())
        };
        let (startup_result, ended) = tokio::select! { result = &mut link => (result.and_then(|_| Err(anyhow::anyhow!("Link ended before startup"))), true),
        result = tokio::time::timeout(STARTUP_TIMEOUT,handshake) => (result.context("managed startup timeout").and_then(|r| r), false) };
        let result = if ended {
            startup_result
        } else if startup_result.is_ok() {
            drop(channel);
            (&mut link).await
        } else {
            control.stop(StopReason::StartupCancelled);
            let _ = tokio::time::timeout(Duration::from_secs(40), &mut link).await;
            startup_result
        };
        if result.is_err() {
            control.stop(StopReason::LinkFailed);
        }
        control.set_phase(Phase::Stopped, None);
        server.abort();
        let _ = server.await;
        drop(socket);
        // Socket and final public state are cleaned up/committed while the
        // profile lease is still held, before a successor can acquire it.
        drop(lease);
        result
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        const FIXTURE: &str = "fabric_background::process::tests::managed_process_fixture";
        struct TestChild(std::process::Child);
        impl std::ops::Deref for TestChild {
            type Target = std::process::Child;
            fn deref(&self) -> &Self::Target {
                &self.0
            }
        }
        impl std::ops::DerefMut for TestChild {
            fn deref_mut(&mut self) -> &mut Self::Target {
                &mut self.0
            }
        }
        impl Drop for TestChild {
            fn drop(&mut self) {
                if !matches!(self.0.try_wait(), Ok(Some(_))) {
                    let _ = self.0.kill();
                    let _ = self.0.wait();
                }
            }
        }

        fn public_startup() -> Startup {
            let host = format!("fixture-{}", uuid::Uuid::new_v4());
            Startup {
                profile: fabric_browser::profile_key("https://fabric.example", "", &host),
                instance: uuid::Uuid::new_v4().to_string(),
                boot_generation: "fixture-boot".into(),
                options: BrowserConnectOptions {
                    gateway_url: "https://fabric.example".into(),
                    issuer: "https://login.example".into(),
                    client_id: "public-client".into(),
                    host_id: Some(host),
                    resource: "https://fabric.example".into(),
                    oauth_callback_port: None,
                    permitted_origins: vec![
                        "https://fabric.example".into(),
                        "https://login.example".into(),
                    ],
                },
            }
        }
        fn public_state(startup: &Startup) -> PublicState {
            PublicState {
                schema_version: 1,
                profile: startup.profile.clone(),
                instance: startup.instance.clone(),
                pid: std::process::id(),
                gateway_origin: startup.options.gateway_url.clone(),
                issuer: startup.options.issuer.clone(),
                client_id: startup.options.client_id.clone(),
                host_id: startup.options.host_id.clone().unwrap(),
                approved_roots: vec!["src".into()],
                phase: Phase::Starting,
                reason: None,
                generation: None,
            }
        }
        async fn wait_child(child: &mut std::process::Child) {
            tokio::time::timeout(Duration::from_secs(8), async {
                loop {
                    if let Some(status) = child.try_wait().unwrap() {
                        assert!(status.success());
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
        }
        // Run this executable only as a private env_clear fixture. It uses the
        // production spawn/lease/socket/handoff functions with an in-memory
        // Link future, never a provider or OS credential store.
        #[tokio::test]
        #[ignore = "invoked by the private process driver"]
        async fn managed_process_fixture() {
            let role = std::env::var("TEMOTE_TEST_BG_ROLE").unwrap();
            let case = std::env::var("TEMOTE_TEST_BG_CASE").unwrap();
            if role == "worker" {
                let fds = std::env::var("TEMOTE_TEST_BG_FDS")
                    .unwrap()
                    .split(',')
                    .map(|s| s.parse::<i32>().unwrap())
                    .collect::<Vec<_>>();
                let stream = unsafe { UnixStream::from_raw_fd(fds[0]) };
                stream.set_nonblocking(true).unwrap();
                let mut channel = tokio::net::UnixStream::from_std(stream).unwrap();
                let startup: Startup = receive(&mut channel).await.unwrap();
                let lease =
                    unsafe { ProfileLock::from_inherited(&startup.profile, fds[1]).unwrap() };
                let control = ManagedControl::new(public_state(&startup));
                let link_control = control.clone();
                let profile = startup.profile.clone();
                let link = async {
                    link_control.connected(23).await?;
                    if case == "retry" {
                        // Pause after the first gate while asynchronous local
                        // verification completes. The final gate must hold RPC.
                        let verification = async {
                            link_control.pause_verification();
                            Ok(None)
                        };
                        let gate = crate::gateway::verify_managed_admission(
                            Some(link_control.as_ref()),
                            verification,
                        );
                        assert!(
                            tokio::time::timeout(Duration::from_millis(30), gate)
                                .await
                                .is_err()
                        );
                        link_control.resume_verification();
                        crate::gateway::verify_managed_admission(
                            Some(link_control.as_ref()),
                            async { Ok(None) },
                        )
                        .await
                        .unwrap();
                        link_control.stop(StopReason::Requested);
                    }
                    if case == "auth" {
                        link_control.stop(StopReason::AuthenticationRejected);
                    }
                    if case == "transition" {
                        assert!(ProfileLock::try_transition(&profile).is_err());
                        // A failed try-lock precedes all store access and POST.
                        link_control.stop(StopReason::RecoveryRequired);
                    }
                    link_control.shutdown().await;
                    Ok(())
                };
                let result = run_instance(startup, lease, channel, control, link).await;
                if case == "cancel" {
                    assert!(result.is_err());
                } else {
                    result.unwrap();
                }
                return;
            }
            assert_eq!(role, "driver");
            let startup = public_startup();
            let mut lease = ProfileLock::link_lifetime(&startup.profile).unwrap();
            let transition = if case == "transition" {
                Some(ProfileLock::transition(&startup.profile).unwrap())
            } else {
                None
            };
            lease.disarm_unlock();
            let (child, mut channel) = spawn_child(&lease, |command, channel_fd, lease_fd| {
                command
                    .args(["--ignored", "--exact", FIXTURE, "--nocapture"])
                    .env("TEMOTE_TEST_BG_ROLE", "worker")
                    .env("TEMOTE_TEST_BG_CASE", &case)
                    .env("TEMOTE_TEST_BG_FDS", format!("{channel_fd},{lease_fd}"))
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::inherit());
            })
            .unwrap();
            let mut child = TestChild(child);
            send(&mut channel, &startup).await.unwrap();
            let ready: Handoff =
                tokio::time::timeout(Duration::from_secs(5), receive(&mut channel))
                    .await
                    .unwrap()
                    .unwrap();
            assert!(
                ready
                    == Handoff::Ready {
                        profile: startup.profile.clone(),
                        instance: startup.instance.clone()
                    }
            );
            assert!(ProfileLock::link_lifetime(&startup.profile).is_err());
            if case == "cancel" {
                drop(channel);
                wait_child(&mut child).await;
                // A child's pre-commit close cannot explicitly unlock the
                // parent's shared open-file description.
                assert!(ProfileLock::link_lifetime(&startup.profile).is_err());
                drop(lease);
                assert!(ProfileLock::link_lifetime(&startup.profile).is_ok());
            } else {
                lease.disarm_unlock();
                send(&mut channel, &Handoff::Commit).await.unwrap();
                assert!(receive::<Handoff>(&mut channel).await.unwrap() == Handoff::Ack);
                drop(channel);
                drop(lease);
                if case == "normal" {
                    assert!(ProfileLock::link_lifetime(&startup.profile).is_err());
                    assert_eq!(projection(&startup.options).await["state"], "online");
                    let state = read_state(&startup.profile).unwrap().unwrap();
                    let mut wrong_instance = state.clone();
                    wrong_instance.instance = uuid::Uuid::new_v4().to_string();
                    assert!(query(&wrong_instance, true).await.is_err());
                    assert_eq!(query(&state, false).await.unwrap().phase, Phase::Online);
                    // A wrong saved PID has no role in status or stop targeting.
                    let mut wrong_pid = state.clone();
                    wrong_pid.pid = std::process::id();
                    write_state(&wrong_pid).unwrap();
                    let mut changed_options = startup.options.clone();
                    changed_options.client_id = "replacement-public-client".into();
                    changed_options.issuer = "https://new-login.example".into();
                    changed_options
                        .permitted_origins
                        .push("https://new-login.example".into());
                    assert_eq!(projection(&changed_options).await["state"], "online");
                    assert_eq!(
                        projection(&changed_options).await["configuration_matches"],
                        false
                    );
                    disconnect(changed_options).await.unwrap();
                }
                wait_child(&mut child).await;
                assert!(ProfileLock::link_lifetime(&startup.profile).is_ok());
                let state = read_state(&startup.profile).unwrap().unwrap();
                assert_eq!(state.phase, Phase::Stopped);
                assert_eq!(
                    state.reason,
                    Some(match case.as_str() {
                        "auth" => StopReason::AuthenticationRejected,
                        "transition" => StopReason::RecoveryRequired,
                        _ => StopReason::Requested,
                    })
                );
                assert_eq!(projection(&startup.options).await["state"], "not_running");
                assert!(disconnect(startup.options.clone()).await.is_err());
                if case == "normal" {
                    let successor_lease = ProfileLock::link_lifetime(&startup.profile).unwrap();
                    let mut successor = state.clone();
                    successor.instance = uuid::Uuid::new_v4().to_string();
                    successor.phase = Phase::Starting;
                    write_state(&successor).unwrap();
                    // Metadata alone cannot prove old-instance completion.
                    assert!(
                        tokio::time::timeout(Duration::from_millis(150), wait_stopped(&state))
                            .await
                            .is_err()
                    );
                    let successor_control = ManagedControl::new(successor);
                    let (successor_socket, successor_server) =
                        bind_control(&successor_control).await.unwrap();
                    tokio::time::timeout(Duration::from_secs(1), wait_stopped(&state))
                        .await
                        .unwrap()
                        .unwrap();
                    successor_server.abort();
                    let _ = successor_server.await;
                    drop(successor_socket);
                    assert!(ProfileLock::link_lifetime(&startup.profile).is_err());
                    drop(successor_lease);
                    write_state(&state).unwrap();
                }

                // Stale records cannot signal the driver's PID or fake live
                // status; the driver reaches this assertion after the request.
                let bytes = std::fs::read(state_path(&startup.profile).unwrap()).unwrap();
                let public = String::from_utf8(bytes).unwrap();
                for secret in [
                    "ACCESS_SECRET_SENTINEL",
                    "REFRESH_SECRET_SENTINEL",
                    "GRANT_SECRET_SENTINEL",
                    "local_hmac_key",
                    "canonical_path",
                ] {
                    assert!(!public.contains(secret));
                }
            }
            drop(transition);
            assert!(!socket_path(&startup.profile).unwrap().exists());
            // Refuse to unlink a successor socket with another inode.
            let path = socket_path(&startup.profile).unwrap();
            let first = std::os::unix::net::UnixListener::bind(&path).unwrap();
            let m = path.symlink_metadata().unwrap();
            let owner = OwnerSocket {
                path: path.clone(),
                inode: m.ino(),
                device: m.dev(),
            };
            std::fs::remove_file(&path).unwrap();
            let successor = std::os::unix::net::UnixListener::bind(&path).unwrap();
            drop(owner);
            assert!(path.exists());
            drop(first);
            drop(successor);
            std::fs::remove_file(path).unwrap();
            std::fs::remove_dir(socket_dir()).unwrap();
            println!("PRIVATE_BACKGROUND_FIXTURE_PASS");
        }
        #[test]
        fn private_process_handoff_stop_and_recovery() {
            for case in ["normal", "cancel", "auth", "transition", "retry"] {
                let private = tempfile::tempdir().unwrap();
                let executable = private.path().join("test-cli");
                std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
                let mut child = Command::new(executable)
                    .args(["--ignored", "--exact", FIXTURE, "--nocapture"])
                    .env_clear()
                    .env("HOME", private.path())
                    .env("PATH", "/usr/bin:/bin")
                    .env("XDG_STATE_HOME", private.path().join("state"))
                    .env(
                        "TEMOTE_SOCKET_NAMESPACE",
                        format!("bg{}", &uuid::Uuid::new_v4().simple().to_string()[..10]),
                    )
                    .env("TEMOTE_TEST_BG_ROLE", "driver")
                    .env("TEMOTE_TEST_BG_CASE", case)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap();
                let started = std::time::Instant::now();
                loop {
                    if child.try_wait().unwrap().is_some() {
                        break;
                    }
                    if started.elapsed() > Duration::from_secs(20) {
                        let _ = child.kill();
                        panic!("private fixture timeout: {case}");
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                let output = child.wait_with_output().unwrap();
                assert!(
                    output.status.success(),
                    "{case}: {} {}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                assert!(
                    String::from_utf8_lossy(&output.stdout)
                        .contains("PRIVATE_BACKGROUND_FIXTURE_PASS")
                );
            }
        }
    }
}

#[cfg(unix)]
pub(crate) use process::{
    child as run_child, disconnect as run_disconnect, projection, start as run_start,
};
#[cfg(not(unix))]
pub(crate) async fn projection(_: &BrowserConnectOptions) -> serde_json::Value {
    serde_json::json!({"state":"unsupported"})
}
#[cfg(not(unix))]
pub(crate) async fn run_start(_: BrowserConnectOptions) -> Result<()> {
    anyhow::bail!("background Link requires a Unix host")
}
#[cfg(not(unix))]
pub(crate) async fn run_disconnect(_: BrowserConnectOptions) -> Result<()> {
    anyhow::bail!("background Link requires a Unix host")
}
#[cfg(not(unix))]
pub(crate) async fn run_child(_: i32, _: i32) -> Result<()> {
    anyhow::bail!("background Link requires a Unix host")
}

#[cfg(test)]
mod guard_tests {
    use super::*;
    #[test]
    fn read_outage_can_recover_but_auth_rotation_and_timeout_halt() {
        let transient: anyhow::Error = fabric_browser::BrowserReadFailure::Transient.into();
        let mut since = None;
        assert!(retry_guard_read(&transient, false, 100, &mut since));
        assert!(retry_guard_read(&transient, false, 159, &mut since));
        assert!(!retry_guard_read(&transient, false, 160, &mut since));
        assert!(!retry_guard_read(&transient, true, 101, &mut None));
        for fatal in [
            fabric_browser::BrowserReadFailure::Authentication,
            fabric_browser::BrowserReadFailure::Rejected,
        ] {
            assert!(!retry_guard_read(&fatal.into(), false, 100, &mut None));
        }
        assert!(!retry_guard_read(
            &anyhow::anyhow!("changed root identity"),
            false,
            100,
            &mut None
        ));
        // Successful validation clears the outage window in the manager, so a
        // later independent brief outage can retry under the same grant.
        since = None;
        assert!(retry_guard_read(&transient, false, 900, &mut since));
    }
}
