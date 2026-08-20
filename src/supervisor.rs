use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::approvals::{self, ApprovalReceiver, ApprovalSender, RuntimeHandle};
use crate::config;
use crate::named_roots::NamedRoots;

#[derive(Debug, Serialize)]
pub struct ManagedSessionInfo {
    pub session_id: String,
    pub cwd: std::path::PathBuf,
    pub status: &'static str,
    pub yolo: bool,
}

pub struct SessionSupervisor {
    roots: NamedRoots,
    approval_sender: ApprovalSender,
    sessions: Mutex<HashMap<String, RuntimeHandle>>,
}

impl SessionSupervisor {
    pub fn new(roots: NamedRoots) -> (Arc<Self>, ApprovalReceiver) {
        let (approval_sender, approval_receiver) = approvals::approval_channel();
        (
            Arc::new(Self {
                roots,
                approval_sender,
                sessions: Mutex::new(HashMap::new()),
            }),
            approval_receiver,
        )
    }

    pub fn roots_configured(&self) -> bool {
        !self.roots.is_empty()
    }

    pub async fn start(
        &self,
        logical_path: &str,
        session_id: Option<&str>,
    ) -> Result<ManagedSessionInfo> {
        anyhow::ensure!(
            self.roots_configured(),
            "TEMOTE_MCP_ROOTS is not configured; session_start is disabled"
        );
        let cwd = self.roots.resolve(logical_path)?;
        let id = config::session_id(session_id)?;

        {
            let mut sessions = self.sessions.lock().await;
            sessions.retain(|_, handle| !handle.is_finished());
            anyhow::ensure!(
                !sessions.contains_key(&id),
                "session {id} is already managed by this serve process"
            );
        }
        anyhow::ensure!(
            !config::session_is_active(&id).await?,
            "session {id} is already running"
        );

        let handle = approvals::spawn_runtime(&cwd, Some(&id), false, self.approval_sender.clone())
            .await
            .with_context(|| format!("failed to start managed session {id}"))?;
        let info = ManagedSessionInfo {
            session_id: id.clone(),
            cwd: handle.cwd().to_owned(),
            status: "active",
            yolo: false,
        };
        self.sessions.lock().await.insert(id, handle);
        Ok(info)
    }

    pub async fn stop(&self, session_id: &str) -> Result<()> {
        config::validate_session_id(session_id)?;
        let handle = {
            let mut sessions = self.sessions.lock().await;
            sessions.retain(|_, handle| !handle.is_finished());
            sessions.remove(session_id)
        };
        let Some(handle) = handle else {
            if config::session_is_active(session_id).await? {
                anyhow::bail!(
                    "session {session_id} is active but is not managed by this serve process"
                );
            }
            anyhow::bail!("session {session_id} is not managed by this serve process");
        };
        handle.shutdown().await
    }

    pub async fn shutdown(&self) -> Result<()> {
        let handles = {
            let mut sessions = self.sessions.lock().await;
            sessions
                .drain()
                .map(|(_, handle)| handle)
                .collect::<Vec<_>>()
        };
        let mut first_error = None;
        for handle in handles {
            if let Err(error) = handle.shutdown().await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        if let Some(error) = first_error {
            return Err(error).context("failed to stop all managed sessions");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn fixture() -> (tempfile::TempDir, NamedRoots) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("volume");
        std::fs::create_dir_all(root.join("repo-a")).unwrap();
        std::fs::create_dir_all(root.join("repo-b")).unwrap();
        let canonical = std::fs::canonicalize(root).unwrap();
        let roots =
            NamedRoots::from_canonical_roots(BTreeMap::from([("src".to_owned(), canonical)]))
                .unwrap();
        (temp, roots)
    }

    #[tokio::test]
    async fn starts_multiple_sessions_rejects_duplicate_and_cleans_up() {
        let (_temp, roots) = fixture();
        let (supervisor, _approvals) = SessionSupervisor::new(roots);
        let first_id = format!("managed-{}", uuid::Uuid::new_v4());
        let second_id = format!("managed-{}", uuid::Uuid::new_v4());

        let first = supervisor
            .start("src/repo-a", Some(&first_id))
            .await
            .unwrap();
        let second = supervisor
            .start("src/repo-b", Some(&second_id))
            .await
            .unwrap();
        assert_eq!(first.status, "active");
        assert_eq!(second.status, "active");
        assert!(!first.yolo && !second.yolo);
        assert!(config::session_is_active(&first_id).await.unwrap());
        assert!(config::session_is_active(&second_id).await.unwrap());

        let duplicate = supervisor.start("src/repo-a", Some(&first_id)).await;
        assert!(duplicate.is_err());

        supervisor.stop(&first_id).await.unwrap();
        assert!(!config::session_is_active(&first_id).await.unwrap());
        supervisor.shutdown().await.unwrap();
        assert!(!config::session_is_active(&second_id).await.unwrap());
        assert!(!config::socket_path(&second_id).unwrap().exists());
        let metadata = std::fs::read(config::session_path(&second_id).unwrap()).unwrap();
        let session: config::Session = serde_json::from_slice(&metadata).unwrap();
        assert_eq!(session.process_id, 0);
    }

    #[tokio::test]
    async fn cannot_stop_unmanaged_active_session() {
        let (_temp, roots) = fixture();
        let (supervisor, _approvals) = SessionSupervisor::new(roots);
        let unmanaged_id = format!("unmanaged-{}", uuid::Uuid::new_v4());
        let cwd = tempfile::tempdir().unwrap();
        let (sender, _receiver) = approvals::approval_channel();
        let handle = approvals::spawn_runtime(cwd.path(), Some(&unmanaged_id), false, sender)
            .await
            .unwrap();

        let error = supervisor.stop(&unmanaged_id).await.unwrap_err();
        assert!(error.to_string().contains("not managed"));
        assert!(config::session_is_active(&unmanaged_id).await.unwrap());
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn managed_approval_waits_for_local_console_and_identifies_session() {
        let (_temp, roots) = fixture();
        let (supervisor, mut approval_receiver) = SessionSupervisor::new(roots);
        let id = format!("approval-{}", uuid::Uuid::new_v4());
        let info = supervisor.start("src/repo-a", Some(&id)).await.unwrap();
        let request_id = id.clone();
        let request_cwd = info.cwd.clone();
        let request = tokio::spawn(async move {
            approvals::request(
                &request_id,
                "git_pull",
                "git pull --ff-only".to_owned(),
                request_cwd,
            )
            .await
        });

        let prompt =
            tokio::time::timeout(std::time::Duration::from_secs(2), approval_receiver.recv())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(prompt.session_id, id);
        assert_eq!(prompt.request.operation, "git_pull");
        assert_eq!(prompt.request.cwd, info.cwd);
        assert!(!request.is_finished());
        prompt.respond(false);
        assert!(!request.await.unwrap().unwrap());
        supervisor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn roots_unset_fails_closed() {
        let (supervisor, _approvals) = SessionSupervisor::new(NamedRoots::default());
        let error = supervisor
            .start("src/repo-a", Some("no-roots-test"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not configured"));
    }
}
