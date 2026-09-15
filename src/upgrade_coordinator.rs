use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
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

const COORDINATOR_READY_TIMEOUT: Duration = Duration::from_secs(5);
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
    let executable_path = session_control::revalidate_installed_upgrade_executable(&executable)?;
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
    match upgrade_transaction::classify_persisted_apply(&executable.target_version)? {
        UpgradeApplyDisposition::ExistingActive(id) => {
            return Ok(PreparedRemoteUpgrade {
                status: upgrade_transaction::read_transaction(&id)?.status(),
                commit: None,
                accepted_new: false,
            });
        }
        UpgradeApplyDisposition::AlreadyCompleted(id) => {
            return Ok(PreparedRemoteUpgrade {
                status: upgrade_transaction::read_transaction(&id)?.status(),
                commit: None,
                accepted_new: false,
            });
        }
        UpgradeApplyDisposition::ConflictActive(id) => anyhow::bail!(
            "upgrade transaction {id} already owns the runtime for a different target"
        ),
        UpgradeApplyDisposition::StartNew => {}
    }

    let host_id = crate::host_identity::resolve()?;
    let mut transaction = UpgradeTransaction::new(
        preflight.source_version,
        executable.target_version.clone(),
        host_id,
        crate::boot_identity::generation(),
        preflight.reconnect_expected,
        preflight.supervisor_handoff_required,
        preflight.direct_ingress_action == "restart",
    );
    transaction.planned_session_count = preflight.planned_session_count;
    transaction.planned_sessions = preflight.planned_sessions;
    transaction.approved_executable_sha256 = Some(executable.digest_hex());
    upgrade_transaction::write_transaction(&transaction)?;
    let commit = match spawn_coordinator(&executable_path, &transaction.transaction_id).await {
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

async fn spawn_coordinator(
    executable: &PathBuf,
    transaction_id: &str,
) -> Result<CoordinatorCommit> {
    let (parent, child) = StdUnixStream::pair()?;
    let child_fd = child.as_raw_fd();
    let mut command = Command::new(executable);
    command
        .args([
            "upgrade-coordinator",
            "--transaction",
            transaction_id,
            "--commit-fd",
            &child_fd.to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(child_fd, libc::F_SETFD, 0) < 0 {
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

pub async fn run_child(transaction_id: String, commit_fd: RawFd) -> Result<()> {
    anyhow::ensure!(commit_fd >= 3, "invalid coordinator commit FD");
    let stream = unsafe { StdUnixStream::from_raw_fd(commit_fd) };
    stream.set_nonblocking(true)?;
    let mut stream = tokio::net::UnixStream::from_std(stream)?;
    let lock = upgrade_transaction::acquire_transaction_lock(&transaction_id)?;
    let transaction = upgrade_transaction::read_transaction(&transaction_id)?;
    let executable = session_control::capture_installed_upgrade_executable()?;
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
                            &self.transaction.target_version,
                            false,
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
                    crate::lifecycle::apply_direct_ingress_upgrade(prepared, &executable).await?;
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
                    session_control::reconcile_codex_plugin(&executable)?
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
