#![allow(dead_code)]

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config;

pub const UPGRADE_TRANSACTION_SCHEMA_VERSION: u64 = 1;
pub const MAX_UPGRADE_TRANSACTION_BYTES: usize = 64 * 1024;
pub const MAX_UPGRADE_TRANSACTIONS: usize = 64;
const MAX_UPGRADE_TRANSACTION_STRING_BYTES: usize = 256;
const MAX_FAILURE_SUMMARY_BYTES: usize = 1024;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpgradeTransactionState {
    Prepared,
    Committed,
    SupervisorHandoff,
    SessionsVerifying,
    IngressRestarting,
    EndpointVerifying,
    PluginReconciling,
    Completed,
    Failed,
    RolledBack,
}

impl UpgradeTransactionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Committed => "committed",
            Self::SupervisorHandoff => "supervisor_handoff",
            Self::SessionsVerifying => "sessions_verifying",
            Self::IngressRestarting => "ingress_restarting",
            Self::EndpointVerifying => "endpoint_verifying",
            Self::PluginReconciling => "plugin_reconciling",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::RolledBack => "rolled_back",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::RolledBack)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpgradeTransaction {
    pub schema: u64,
    pub transaction_id: String,
    pub source_version: String,
    pub target_version: String,
    pub host_id: String,
    pub source_boot_generation: String,
    pub state: UpgradeTransactionState,
    pub created_at: u64,
    pub updated_at: u64,
    pub reconnect_expected: bool,
    pub supervisor_handoff_required: bool,
    pub ingress_restart_required: bool,
    pub failure_summary: Option<String>,
}

impl UpgradeTransaction {
    pub fn new(
        source_version: impl Into<String>,
        target_version: impl Into<String>,
        host_id: impl Into<String>,
        source_boot_generation: impl Into<String>,
        reconnect_expected: bool,
        supervisor_handoff_required: bool,
        ingress_restart_required: bool,
    ) -> Self {
        let now = config::unix_time();
        Self {
            schema: UPGRADE_TRANSACTION_SCHEMA_VERSION,
            transaction_id: uuid::Uuid::new_v4().to_string(),
            source_version: source_version.into(),
            target_version: target_version.into(),
            host_id: host_id.into(),
            source_boot_generation: source_boot_generation.into(),
            state: UpgradeTransactionState::Prepared,
            created_at: now,
            updated_at: now,
            reconnect_expected,
            supervisor_handoff_required,
            ingress_restart_required,
            failure_summary: None,
        }
    }

    pub fn set_state(&mut self, state: UpgradeTransactionState) -> Result<()> {
        anyhow::ensure!(
            !self.state.is_terminal(),
            "upgrade transaction {} is already terminal ({})",
            self.transaction_id,
            self.state.as_str()
        );
        self.state = state;
        self.updated_at = config::unix_time();
        Ok(())
    }
}

fn upgrade_transaction_directory() -> Result<PathBuf> {
    Ok(config::state_dir()?.join("upgrade-transactions"))
}

fn ensure_directory() -> Result<PathBuf> {
    let directory = upgrade_transaction_directory()?;
    std::fs::create_dir_all(&directory).with_context(|| {
        format!(
            "failed to create upgrade transaction directory {}",
            directory.display()
        )
    })?;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    Ok(directory)
}

fn validate_transaction_id(transaction_id: &str) -> Result<()> {
    let parsed = uuid::Uuid::parse_str(transaction_id)
        .with_context(|| format!("invalid upgrade transaction ID {transaction_id:?}"))?;
    anyhow::ensure!(
        parsed.to_string() == transaction_id,
        "upgrade transaction ID must be a canonical UUID"
    );
    Ok(())
}

fn transaction_path(transaction_id: &str) -> Result<PathBuf> {
    validate_transaction_id(transaction_id)?;
    Ok(ensure_directory()?.join(format!("{transaction_id}.json")))
}

fn lock_path(transaction_id: &str) -> Result<PathBuf> {
    validate_transaction_id(transaction_id)?;
    Ok(ensure_directory()?.join(format!("{transaction_id}.lock")))
}

fn validate_transaction(transaction: &UpgradeTransaction) -> Result<()> {
    anyhow::ensure!(
        transaction.schema == UPGRADE_TRANSACTION_SCHEMA_VERSION,
        "unsupported upgrade transaction schema"
    );
    validate_transaction_id(&transaction.transaction_id)?;
    for (name, value) in [
        ("source_version", &transaction.source_version),
        ("target_version", &transaction.target_version),
        (
            "source_boot_generation",
            &transaction.source_boot_generation,
        ),
    ] {
        anyhow::ensure!(
            !value.trim().is_empty() && value.len() <= MAX_UPGRADE_TRANSACTION_STRING_BYTES,
            "upgrade transaction {name} is empty or oversized"
        );
        anyhow::ensure!(
            !value.contains('\0'),
            "upgrade transaction {name} must not contain NUL"
        );
    }
    crate::host_identity::validate(&transaction.host_id)
        .context("invalid upgrade transaction host_id")?;
    if let Some(summary) = &transaction.failure_summary {
        anyhow::ensure!(
            summary.len() <= MAX_FAILURE_SUMMARY_BYTES,
            "upgrade transaction failure summary exceeds {MAX_FAILURE_SUMMARY_BYTES} bytes"
        );
        anyhow::ensure!(
            !summary.contains('\0'),
            "upgrade transaction failure summary must not contain NUL"
        );
    }
    Ok(())
}

pub fn write_transaction(transaction: &UpgradeTransaction) -> Result<PathBuf> {
    validate_transaction(transaction)?;
    let path = transaction_path(&transaction.transaction_id)?;
    let bytes = serde_json::to_vec_pretty(transaction)?;
    anyhow::ensure!(
        bytes.len() <= MAX_UPGRADE_TRANSACTION_BYTES,
        "upgrade transaction exceeds {MAX_UPGRADE_TRANSACTION_BYTES} bytes"
    );
    let temporary = path.with_extension(format!(
        "json.{}.{}.tmp",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)
            .with_context(|| {
                format!(
                    "failed to create upgrade transaction {}",
                    temporary.display()
                )
            })?;
        file.write_all(&bytes)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, &path)
            .with_context(|| format!("failed to replace upgrade transaction {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map(|()| path)
}

pub fn read_transaction(transaction_id: &str) -> Result<UpgradeTransaction> {
    let path = transaction_path(transaction_id)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("cannot open upgrade transaction {}", path.display()))?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file(),
        "upgrade transaction must be a regular file"
    );
    anyhow::ensure!(
        metadata.len() <= MAX_UPGRADE_TRANSACTION_BYTES as u64,
        "upgrade transaction exceeds {MAX_UPGRADE_TRANSACTION_BYTES} bytes"
    );
    let mode = metadata.permissions().mode() & 0o777;
    anyhow::ensure!(
        mode & 0o077 == 0,
        "upgrade transaction must be owner-only (mode {mode:04o})"
    );
    let file = file;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_UPGRADE_TRANSACTION_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= MAX_UPGRADE_TRANSACTION_BYTES,
        "upgrade transaction exceeds {MAX_UPGRADE_TRANSACTION_BYTES} bytes"
    );
    let transaction: UpgradeTransaction =
        serde_json::from_slice(&bytes).context("invalid upgrade transaction")?;
    validate_transaction(&transaction)?;
    anyhow::ensure!(
        transaction.transaction_id == transaction_id,
        "upgrade transaction ID does not match its file name"
    );
    Ok(transaction)
}

pub fn remove_transaction(transaction_id: &str) -> Result<()> {
    let path = transaction_path(transaction_id)?;
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.file_type().is_file(),
                "refusing to remove upgrade transaction through a non-regular file: {}",
                path.display()
            );
            std::fs::remove_file(&path)
                .with_context(|| format!("cannot remove upgrade transaction {}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("cannot inspect upgrade transaction {}", path.display())),
    }
}

pub fn list_transaction_ids() -> Result<Vec<String>> {
    let directory = ensure_directory()?;
    let mut ids = Vec::new();
    for entry in std::fs::read_dir(&directory).with_context(|| {
        format!(
            "cannot scan upgrade transactions in {}",
            directory.display()
        )
    })? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(id) = name.strip_suffix(".json") else {
            continue;
        };
        if validate_transaction_id(id).is_err() {
            continue;
        }
        ids.push(id.to_owned());
        anyhow::ensure!(
            ids.len() <= MAX_UPGRADE_TRANSACTIONS,
            "too many upgrade transactions under {}",
            directory.display()
        );
    }
    ids.sort();
    Ok(ids)
}

pub fn acquire_transaction_lock(transaction_id: &str) -> Result<UpgradeTransactionLock> {
    let path = lock_path(transaction_id)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("cannot open upgrade transaction lock {}", path.display()))?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file(),
        "upgrade transaction lock must be a regular file"
    );
    let mode = metadata.permissions().mode() & 0o777;
    anyhow::ensure!(
        mode & 0o077 == 0,
        "upgrade transaction lock must be owner-only (mode {mode:04o})"
    );
    let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    anyhow::ensure!(
        locked == 0,
        "upgrade transaction {} is owned by another live process",
        transaction_id
    );
    Ok(UpgradeTransactionLock { _file: file, path })
}

pub struct UpgradeTransactionLock {
    _file: File,
    path: PathBuf,
}

impl UpgradeTransactionLock {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        transaction: UpgradeTransaction,
    }

    impl Fixture {
        fn new() -> Self {
            let mut transaction = UpgradeTransaction::new(
                "2026.8.0",
                "2026.9.0",
                "mac-main",
                "boot-generation-a",
                true,
                true,
                true,
            );
            transaction.failure_summary = None;
            Self { transaction }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = remove_transaction(&self.transaction.transaction_id);
            let _ = std::fs::remove_file(lock_path(&self.transaction.transaction_id).unwrap());
        }
    }

    #[test]
    fn transaction_round_trips_every_state() {
        let mut fixture = Fixture::new();
        for state in [
            UpgradeTransactionState::Prepared,
            UpgradeTransactionState::Committed,
            UpgradeTransactionState::SupervisorHandoff,
            UpgradeTransactionState::SessionsVerifying,
            UpgradeTransactionState::IngressRestarting,
            UpgradeTransactionState::EndpointVerifying,
            UpgradeTransactionState::PluginReconciling,
        ] {
            fixture.transaction.state = state;
            write_transaction(&fixture.transaction).unwrap();
            let read = read_transaction(&fixture.transaction.transaction_id).unwrap();
            assert_eq!(read.state, state);
            assert_eq!(read.transaction_id, fixture.transaction.transaction_id);
        }
    }

    #[test]
    fn transaction_serialization_contains_only_safe_schema_fields() {
        let fixture = Fixture::new();
        let value = serde_json::to_value(&fixture.transaction).unwrap();
        let keys = value
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for expected in [
            "schema",
            "transaction_id",
            "source_version",
            "target_version",
            "host_id",
            "source_boot_generation",
            "state",
            "created_at",
            "updated_at",
            "reconnect_expected",
            "supervisor_handoff_required",
            "ingress_restart_required",
            "failure_summary",
        ] {
            assert!(keys.iter().any(|key| key == expected), "missing {expected}");
        }
        assert_eq!(keys.len(), 13);
        let encoded = serde_json::to_string(&fixture.transaction).unwrap();
        for forbidden in ["token", "secret", "authorization", "cookie", "password"] {
            assert!(!encoded.to_ascii_lowercase().contains(forbidden));
        }
    }

    #[test]
    fn unknown_state_and_schema_are_rejected() {
        let fixture = Fixture::new();
        write_transaction(&fixture.transaction).unwrap();
        let path = transaction_path(&fixture.transaction.transaction_id).unwrap();

        let mut value = serde_json::to_value(&fixture.transaction).unwrap();
        value["state"] = serde_json::json!("teleporting");
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(read_transaction(&fixture.transaction.transaction_id).is_err());

        value["state"] = serde_json::json!("prepared");
        value["schema"] = serde_json::json!(999);
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(read_transaction(&fixture.transaction.transaction_id).is_err());
    }

    #[test]
    fn invalid_transaction_ids_are_rejected_before_path_use() {
        let repeated = "a".repeat(36);
        for id in ["", "../escape", "not-a-uuid", repeated.as_str()] {
            assert!(validate_transaction_id(id).is_err(), "accepted {id:?}");
            assert!(transaction_path(id).is_err());
            assert!(lock_path(id).is_err());
        }
    }

    #[test]
    fn terminal_states_cannot_transition_again() {
        let mut transaction = Fixture::new().transaction.clone();
        for terminal in [
            UpgradeTransactionState::Completed,
            UpgradeTransactionState::Failed,
            UpgradeTransactionState::RolledBack,
        ] {
            let mut copy = transaction.clone();
            copy.state = terminal;
            assert!(copy.set_state(UpgradeTransactionState::Committed).is_err());
        }
        assert!(!transaction.state.is_terminal());
        transaction
            .set_state(UpgradeTransactionState::Committed)
            .unwrap();
        assert_eq!(transaction.state, UpgradeTransactionState::Committed);
        assert!(transaction.updated_at >= transaction.created_at);
    }

    #[test]
    fn write_is_atomic_and_leaves_no_temporary_files() {
        let fixture = Fixture::new();
        write_transaction(&fixture.transaction).unwrap();
        write_transaction(&fixture.transaction).unwrap();
        let directory = upgrade_transaction_directory().unwrap();
        for entry in std::fs::read_dir(directory).unwrap() {
            let name = entry.unwrap().file_name();
            let name = name.to_string_lossy().into_owned();
            assert!(
                !name.contains(&fixture.transaction.transaction_id) || !name.ends_with(".tmp"),
                "orphan temporary transaction file: {name}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn read_rejects_symlinks_public_modes_and_oversized_files() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let path = transaction_path(&fixture.transaction.transaction_id).unwrap();
        let target = path.with_extension("target");
        write_transaction(&fixture.transaction).unwrap();
        std::fs::rename(&path, &target).unwrap();
        symlink(&target, &path).unwrap();
        assert!(read_transaction(&fixture.transaction.transaction_id).is_err());
        std::fs::remove_file(&path).unwrap();

        std::fs::copy(&target, &path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_transaction(&fixture.transaction.transaction_id).is_err());
        std::fs::remove_file(&path).unwrap();

        std::fs::write(&path, vec![b'x'; MAX_UPGRADE_TRANSACTION_BYTES + 1]).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read_transaction(&fixture.transaction.transaction_id).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(&target).unwrap();
    }

    #[test]
    fn transaction_lock_is_exclusive_and_released_on_drop() {
        let fixture = Fixture::new();
        let lock = acquire_transaction_lock(&fixture.transaction.transaction_id).unwrap();
        assert_eq!(
            lock.path(),
            lock_path(&fixture.transaction.transaction_id).unwrap()
        );
        assert!(acquire_transaction_lock(&fixture.transaction.transaction_id).is_err());
        drop(lock);
        assert!(acquire_transaction_lock(&fixture.transaction.transaction_id).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn transaction_lock_rejects_symlinks() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let path = lock_path(&fixture.transaction.transaction_id).unwrap();
        let target = path.with_extension("target");
        std::fs::write(&target, b"").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&target, &path).unwrap();
        assert!(acquire_transaction_lock(&fixture.transaction.transaction_id).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(&target).unwrap();
    }

    #[test]
    fn list_transaction_ids_ignores_other_files() {
        let fixture = Fixture::new();
        write_transaction(&fixture.transaction).unwrap();
        let directory = upgrade_transaction_directory().unwrap();
        std::fs::write(directory.join("not-a-transaction.txt"), b"x").unwrap();
        std::fs::write(directory.join("notes.json"), b"x").unwrap();
        let ids = list_transaction_ids().unwrap();
        assert!(ids.contains(&fixture.transaction.transaction_id));
        assert!(!ids.iter().any(|id| id == "notes"));
        let _ = std::fs::remove_file(directory.join("not-a-transaction.txt"));
        let _ = std::fs::remove_file(directory.join("notes.json"));
    }
}
