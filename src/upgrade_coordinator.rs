use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::session_control::{self, InstalledUpgradeExecutable, RemoteUpgradePreflight};
use crate::upgrade_transaction::{
    self, UpgradeApplyDisposition, UpgradeCommitBarrier, UpgradeCoordinatorExecutor,
    UpgradeCoordinatorStep, UpgradeCoordinatorStepFuture, UpgradeTransaction,
    UpgradeTransactionState, UpgradeTransactionStatus, UpgradeVerifiedIdentity,
};

// READY follows a second bounded digest and capability check of an executable
// that may be as large as MAX_UPGRADE_EXECUTABLE_BYTES. Debug builds and slower
// filesystems can legitimately take longer than the ordinary control timeout.
const COORDINATOR_READY_TIMEOUT: Duration = Duration::from_secs(30);
const COORDINATOR_COMMIT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct CoordinatorCommit {
    stream: Arc<Mutex<Option<StdUnixStream>>>,
}

impl CoordinatorCommit {
    fn decide(&self, message: &[u8]) -> Result<()> {
        let mut stream = self
            .stream
            .lock()
            .expect("coordinator commit mutex poisoned")
            .take()
            .context("upgrade coordinator commit was already decided")?;
        stream.write_all(message)?;
        stream.flush()?;
        stream.shutdown(std::net::Shutdown::Both)?;
        Ok(())
    }

    pub fn commit(&self) -> Result<()> {
        self.decide(b"COMMIT\n")
    }
    pub fn abort(&self) -> Result<()> {
        self.decide(b"ABORT\n")
    }

    #[cfg(test)]
    pub fn test_pair() -> Result<(Self, StdUnixStream)> {
        let (writer, reader) = StdUnixStream::pair()?;
        Ok((
            Self {
                stream: Arc::new(Mutex::new(Some(writer))),
            },
            reader,
        ))
    }
}

impl Drop for CoordinatorCommit {
    fn drop(&mut self) {
        if Arc::strong_count(&self.stream) == 1 {
            let _ = self.abort();
        }
    }
}

pub struct PreparedRemoteUpgrade {
    pub status: UpgradeTransactionStatus,
    pub commit: Option<CoordinatorCommit>,
    pub accepted_new: bool,
}

pub async fn preflight() -> Result<(InstalledUpgradeExecutable, RemoteUpgradePreflight)> {
    let executable = session_control::capture_installed_upgrade_executable()?;
    let preflight = session_control::remote_upgrade_preflight(&executable).await?;
    Ok((executable, preflight))
}

pub async fn prepare_apply(
    executable: InstalledUpgradeExecutable,
    preflight: RemoteUpgradePreflight,
    expected_version: Option<&str>,
) -> Result<PreparedRemoteUpgrade> {
    if let Some(expected) = expected_version {
        anyhow::ensure!(
            expected == executable.target_version,
            "installed target version changed: expected {expected}, found {}",
            executable.target_version
        );
    }
    anyhow::ensure!(
        preflight.blocked_session_count == 0,
        "upgrade is blocked by {} session(s)",
        preflight.blocked_session_count
    );
    anyhow::ensure!(
        !preflight.direct_ingress_blocked,
        "direct ingress upgrade is blocked"
    );
    session_control::revalidate_installed_upgrade_executable(&executable)?;
    let _admission = upgrade_transaction::acquire_admission_lock()?;
    session_control::verify_planned_upgrade_sessions(&preflight.planned_sessions, true).await?;

    // Ownerless PREPARED means COMMIT could never have been observed. It is the
    // only non-terminal state that is safe to terminalize automatically.
    for mut transaction in upgrade_transaction::load_transactions()? {
        if transaction.state == UpgradeTransactionState::Prepared
            && !upgrade_transaction::transaction_lock_is_held(&transaction.transaction_id)?
        {
            transaction.mark_failed("prepared coordinator owner disappeared before commit")?;
            upgrade_transaction::write_transaction(&transaction)?;
        }
    }
    let persisted = upgrade_transaction::load_transactions()?;
    let sequence = upgrade_transaction::next_transaction_sequence(&persisted)?;
    match upgrade_transaction::admit_apply(&persisted, &executable.target_version)? {
        UpgradeApplyDisposition::ExistingActive(id) => {
            return Ok(PreparedRemoteUpgrade {
                status: upgrade_transaction::read_transaction(&id)?.status(),
                commit: None,
                accepted_new: false,
            });
        }
        UpgradeApplyDisposition::AlreadyCompleted(id) => {
            let completed = upgrade_transaction::read_transaction(&id)?;
            if completed_upgrade_is_current(&completed, &preflight, &executable.digest_hex())? {
                return Ok(PreparedRemoteUpgrade {
                    status: completed.status(),
                    commit: None,
                    accepted_new: false,
                });
            }
        }
        UpgradeApplyDisposition::ConflictActive(id) => anyhow::bail!(
            "upgrade transaction {id} already owns the runtime for a different target"
        ),
        UpgradeApplyDisposition::StartNew => {}
    }

    let mut transaction = prepared_transaction_from_approved(
        preflight,
        executable.target_version.clone(),
        executable.digest_hex(),
        crate::host_identity::resolve()?,
        crate::boot_identity::generation().to_owned(),
        sequence,
    );
    upgrade_transaction::write_transaction(&transaction)?;
    let commit = match spawn_coordinator(&executable, &transaction.transaction_id).await {
        Ok(commit) => commit,
        Err(error) => {
            transaction.mark_failed("upgrade coordinator failed before commit")?;
            upgrade_transaction::write_transaction(&transaction)?;
            return Err(error);
        }
    };
    // `_admission` intentionally remains held until READY proves child ownership.
    Ok(PreparedRemoteUpgrade {
        status: transaction.status(),
        commit: Some(commit),
        accepted_new: true,
    })
}

fn completed_upgrade_is_current(
    completed: &UpgradeTransaction,
    preflight: &RemoteUpgradePreflight,
    approved_digest: &str,
) -> Result<bool> {
    if preflight.supervisor_handoff_required || preflight.direct_ingress_action == "restart" {
        return Ok(false);
    }
    anyhow::ensure!(
        completed.approved_executable_sha256.as_deref() == Some(approved_digest),
        "completed upgrade identity does not match the installed candidate"
    );
    Ok(true)
}

fn prepared_transaction_from_approved(
    preflight: RemoteUpgradePreflight,
    target_version: String,
    approved_executable_sha256: String,
    host_id: String,
    source_boot_generation: String,
    sequence: u64,
) -> UpgradeTransaction {
    let mut transaction = UpgradeTransaction::new(
        preflight.source_version,
        target_version,
        host_id,
        source_boot_generation,
        preflight.reconnect_expected,
        preflight.supervisor_handoff_required,
        preflight.direct_ingress_action == "restart",
    );
    transaction.planned_session_count = preflight.planned_sessions.len();
    transaction.sequence = sequence;
    transaction.planned_sessions = preflight.planned_sessions;
    transaction.approved_executable_sha256 = Some(approved_executable_sha256);
    transaction
}

async fn spawn_coordinator(
    executable: &InstalledUpgradeExecutable,
    transaction_id: &str,
) -> Result<CoordinatorCommit> {
    let (parent, child) = StdUnixStream::pair()?;
    let child_fd = child.as_raw_fd();
    let executable_fd = executable.execution_fd();
    let mut command = Command::new(executable.execution_path());
    command
        .args([
            "upgrade-coordinator",
            "--transaction",
            transaction_id,
            "--commit-fd",
            &child_fd.to_string(),
            "--executable-fd",
            &executable_fd.to_string(),
        ])
        .arg("--installed-locator")
        .arg(executable.installed_locator())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(child_fd, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::fcntl(executable_fd, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let forked = libc::fork();
            if forked < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if forked > 0 {
                libc::_exit(0);
            }
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut launcher = command
        .spawn()
        .context("failed to spawn detached upgrade coordinator")?;
    drop(child);
    let launcher_status = tokio::time::timeout(
        COORDINATOR_READY_TIMEOUT,
        tokio::task::spawn_blocking(move || launcher.wait()),
    )
    .await
    .context("upgrade coordinator launcher did not exit")?
    .context("upgrade coordinator launcher wait task failed")??;
    anyhow::ensure!(
        launcher_status.success(),
        "upgrade coordinator launcher failed"
    );
    parent.set_nonblocking(true)?;
    let mut parent = tokio::net::UnixStream::from_std(parent)?;
    let mut line = String::new();
    let read = tokio::time::timeout(
        COORDINATOR_READY_TIMEOUT,
        BufReader::new(&mut parent).read_line(&mut line),
    )
    .await
    .context("upgrade coordinator did not become ready")??;
    anyhow::ensure!(
        read > 0 && line == "READY\n",
        "upgrade coordinator failed before READY"
    );
    Ok(CoordinatorCommit {
        stream: Arc::new(Mutex::new(Some(parent.into_std()?))),
    })
}

pub async fn run_child(
    transaction_id: String,
    commit_fd: RawFd,
    executable_fd: RawFd,
) -> Result<()> {
    anyhow::ensure!(commit_fd >= 3, "invalid coordinator commit FD");
    anyhow::ensure!(
        executable_fd >= 3 && executable_fd != commit_fd,
        "invalid coordinator executable FD"
    );
    let mut running_executable = unsafe { std::fs::File::from_raw_fd(executable_fd) };
    let approved_running_digest = bounded_file_digest_hex(&mut running_executable)?;
    let stream = unsafe { StdUnixStream::from_raw_fd(commit_fd) };
    stream.set_nonblocking(true)?;
    let mut stream = tokio::net::UnixStream::from_std(stream)?;
    let lock = upgrade_transaction::acquire_transaction_lock(&transaction_id)?;
    let transaction = upgrade_transaction::read_transaction(&transaction_id)?;
    let executable = session_control::capture_installed_upgrade_executable()?;
    validate_approved_candidate_identity(
        &transaction,
        env!("CARGO_PKG_VERSION"),
        &approved_running_digest,
    )?;
    validate_approved_candidate(&transaction, &executable)?;
    session_control::verify_planned_upgrade_sessions(&transaction.planned_sessions, true).await?;
    stream.write_all(b"READY\n").await?;
    let mut line = String::new();
    let decision = tokio::time::timeout(
        COORDINATOR_COMMIT_TIMEOUT,
        BufReader::new(&mut stream).read_line(&mut line),
    )
    .await;
    let barrier = UpgradeCommitBarrier::new();
    match decision {
        Ok(Ok(read)) if read > 0 && line == "COMMIT\n" => barrier.commit()?,
        _ => barrier.abort()?,
    }
    session_control::revalidate_installed_upgrade_executable(&executable)?;
    let mut executor = ConcreteExecutor::new(transaction.clone(), executable);
    upgrade_transaction::run_upgrade_coordinator_with_lock(
        &transaction_id,
        lock,
        barrier.waiter(),
        &mut executor,
    )
    .await?;
    Ok(())
}

fn bounded_file_digest_hex(file: &mut std::fs::File) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::{Read, Seek};

    file.seek(std::io::SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut copied = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        copied += read as u64;
        anyhow::ensure!(
            copied <= 256 * 1024 * 1024,
            "coordinator executable exceeds bounded identity size"
        );
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn validate_approved_candidate(
    transaction: &UpgradeTransaction,
    executable: &InstalledUpgradeExecutable,
) -> Result<()> {
    validate_approved_candidate_identity(
        transaction,
        &executable.target_version,
        &executable.digest_hex(),
    )
}

fn validate_approved_candidate_identity(
    transaction: &UpgradeTransaction,
    target_version: &str,
    digest_hex: &str,
) -> Result<()> {
    anyhow::ensure!(
        target_version == transaction.target_version,
        "coordinator executable target version mismatch"
    );
    let approved_digest = transaction
        .approved_executable_sha256
        .as_deref()
        .context("upgrade transaction has no approved executable identity")?;
    anyhow::ensure!(
        digest_hex == approved_digest,
        "installed Temote executable changed after approval"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preflight(supervisor_handoff_required: bool) -> RemoteUpgradePreflight {
        RemoteUpgradePreflight {
            source_version: "2026.8.0".to_owned(),
            target_version: "2026.9.0".to_owned(),
            compatible: true,
            supervisor_handoff_required,
            planned_session_count: 1,
            blocked_session_count: 0,
            blocker_reasons: Vec::new(),
            direct_ingress_action: if supervisor_handoff_required {
                "restart".to_owned()
            } else {
                "untouched".to_owned()
            },
            direct_ingress_blocked: false,
            reconnect_expected: supervisor_handoff_required,
            plugin_reconciliation_required: true,
            client_restart_required_if_plugin_replaced: true,
            planned_sessions: vec![upgrade_transaction::UpgradePlannedSession {
                session_id: "session-a".to_owned(),
                source_process_id: 100,
                source_started_at: 10,
            }],
        }
    }

    #[test]
    fn same_version_candidate_swap_is_rejected_by_approved_digest() {
        let mut transaction =
            UpgradeTransaction::new("2026.8.0", "2026.9.0", "host-a", "boot-a", true, true, true);
        transaction.approved_executable_sha256 = Some("a".repeat(64));

        let error = validate_approved_candidate_identity(&transaction, "2026.9.0", &"b".repeat(64))
            .unwrap_err();
        assert!(error.to_string().contains("changed after approval"));
    }

    #[test]
    fn opened_executable_identity_survives_path_replacement() {
        use sha2::{Digest, Sha256};

        let directory = tempfile::tempdir().unwrap();
        let installed = directory.path().join("temote-mcp");
        let replacement = directory.path().join("replacement");
        std::fs::write(&installed, b"approved image").unwrap();
        std::fs::write(&replacement, b"different image").unwrap();
        let mut approved = std::fs::File::open(&installed).unwrap();
        std::fs::rename(&replacement, &installed).unwrap();

        let open_digest = bounded_file_digest_hex(&mut approved).unwrap();
        let installed_digest = format!("{:x}", Sha256::digest(std::fs::read(&installed).unwrap()));
        assert_ne!(open_digest, installed_digest);
        assert_eq!(
            open_digest,
            format!("{:x}", Sha256::digest(b"approved image"))
        );
    }

    #[test]
    fn old_transaction_without_approved_digest_fails_closed_for_execution() {
        let transaction =
            UpgradeTransaction::new("2026.8.0", "2026.9.0", "host-a", "boot-a", true, true, true);
        let error = validate_approved_candidate_identity(&transaction, "2026.9.0", &"a".repeat(64))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("no approved executable identity")
        );
    }

    #[test]
    fn historical_completion_does_not_override_fresh_required_work() {
        let mut completed =
            UpgradeTransaction::new("2026.8.0", "2026.9.0", "host-a", "boot-a", true, true, true);
        completed.approved_executable_sha256 = Some("a".repeat(64));
        assert!(
            !completed_upgrade_is_current(&completed, &preflight(true), &"a".repeat(64)).unwrap()
        );
        assert!(
            completed_upgrade_is_current(&completed, &preflight(false), &"a".repeat(64)).unwrap()
        );
    }

    #[test]
    fn no_handoff_preflight_persists_exact_session_count_and_identity() {
        let transaction = prepared_transaction_from_approved(
            preflight(false),
            "2026.9.0".to_owned(),
            "a".repeat(64),
            "host-a".to_owned(),
            "boot-a".to_owned(),
            1,
        );
        upgrade_transaction::write_transaction(&transaction).unwrap();
        let restored = upgrade_transaction::read_transaction(&transaction.transaction_id).unwrap();
        assert_eq!(restored.planned_session_count, 1);
        assert_eq!(restored.planned_sessions.len(), 1);
        assert_eq!(restored.planned_sessions[0].session_id, "session-a");
        upgrade_transaction::remove_transaction(&transaction.transaction_id).unwrap();
    }
}

struct ConcreteExecutor {
    transaction: UpgradeTransaction,
    executable: InstalledUpgradeExecutable,
    restored: Option<usize>,
    verified: Option<UpgradeVerifiedIdentity>,
}

impl ConcreteExecutor {
    fn new(transaction: UpgradeTransaction, executable: InstalledUpgradeExecutable) -> Self {
        Self {
            transaction,
            executable,
            restored: None,
            verified: None,
        }
    }
}

impl UpgradeCoordinatorExecutor for ConcreteExecutor {
    fn execute_phase(
        &mut self,
        phase: UpgradeTransactionState,
    ) -> UpgradeCoordinatorStepFuture<'_> {
        Box::pin(async move {
            match phase {
                UpgradeTransactionState::SupervisorHandoff => {
                    let executable =
                        session_control::revalidate_installed_upgrade_executable(&self.executable)?;
                    session_control::verify_planned_upgrade_sessions(
                        &self.transaction.planned_sessions,
                        true,
                    )
                    .await?;
                    self.restored = Some(
                        session_control::apply_supervisor_upgrade(
                            &executable,
                            self.executable.installed_locator(),
                            &self.transaction.target_version,
                            false,
                            Some(&self.transaction.planned_sessions),
                        )
                        .await?,
                    );
                }
                UpgradeTransactionState::SessionsVerifying => {
                    let active = session_control::verify_planned_upgrade_sessions(
                        &self.transaction.planned_sessions,
                        !self.transaction.supervisor_handoff_required,
                    )
                    .await?;
                    self.restored = Some(active);
                }
                UpgradeTransactionState::IngressRestarting => {
                    let executable =
                        session_control::revalidate_installed_upgrade_executable(&self.executable)?;
                    session_control::verify_planned_upgrade_sessions(
                        &self.transaction.planned_sessions,
                        !self.transaction.supervisor_handoff_required,
                    )
                    .await?;
                    let prepared = crate::lifecycle::prepare_direct_ingress_upgrade(
                        &self.transaction.target_version,
                    )
                    .await?;
                    crate::lifecycle::apply_direct_ingress_upgrade(
                        prepared,
                        &executable,
                        self.executable.installed_locator(),
                    )
                    .await?;
                }
                UpgradeTransactionState::EndpointVerifying => {
                    let identity = crate::lifecycle::verify_direct_ingress_identity(
                        &self.transaction.target_version,
                        &self.transaction.host_id,
                        &self.transaction.source_boot_generation,
                        self.transaction.ingress_restart_required,
                        &self.transaction.transaction_id,
                    )
                    .await?;
                    self.verified = Some(UpgradeVerifiedIdentity {
                        host_id: identity.host_id,
                        version: identity.version,
                        boot_generation: identity.boot_generation,
                    });
                }
                UpgradeTransactionState::PluginReconciling => {
                    let executable =
                        session_control::revalidate_installed_upgrade_executable(&self.executable)?;
                    session_control::reconcile_codex_plugin(
                        &executable,
                        self.executable.installed_locator(),
                    )?
                }
                UpgradeTransactionState::Completed => {}
                _ => anyhow::bail!("unsupported coordinator phase {}", phase.as_str()),
            }
            Ok(UpgradeCoordinatorStep::Continue)
        })
    }

    fn verified_identity(&self) -> Option<UpgradeVerifiedIdentity> {
        self.verified.clone()
    }
    fn restored_session_count(&self) -> Option<usize> {
        self.restored
    }
}

pub fn status(transaction_id: &str) -> Result<UpgradeTransactionStatus> {
    let transaction = upgrade_transaction::read_transaction(transaction_id)?;
    let alive = (!transaction.state.is_terminal())
        .then(|| upgrade_transaction::transaction_lock_is_held(transaction_id))
        .transpose()?;
    let mut status = transaction.status();
    status.coordinator_alive = alive;
    status.incomplete = alive == Some(false);
    Ok(status)
}
