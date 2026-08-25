use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::approvals::{self, ApprovalReceiver, ApprovalSender, RuntimeHandle};
use crate::config;
use crate::named_roots::NamedRoots;

const MAX_MANAGED_SESSIONS: usize = 64;

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
    public_sessions: Mutex<HashSet<String>>,
    transitions: Mutex<()>,
    closed: AtomicBool,
    max_sessions: usize,
}

impl SessionSupervisor {
    pub fn new(roots: NamedRoots) -> (Arc<Self>, ApprovalReceiver) {
        Self::with_limit(roots, MAX_MANAGED_SESSIONS)
    }

    fn with_limit(roots: NamedRoots, max_sessions: usize) -> (Arc<Self>, ApprovalReceiver) {
        let (approval_sender, approval_receiver) = approvals::approval_channel();
        (
            Arc::new(Self {
                roots,
                approval_sender,
                sessions: Mutex::new(HashMap::new()),
                public_sessions: Mutex::new(HashSet::new()),
                transitions: Mutex::new(()),
                closed: AtomicBool::new(false),
                max_sessions,
            }),
            approval_receiver,
        )
    }

    pub fn roots_configured(&self) -> bool {
        !self.roots.is_empty()
    }

    pub fn approval_sender(&self) -> ApprovalSender {
        self.approval_sender.clone()
    }

    #[cfg(test)]
    pub async fn start(
        &self,
        logical_path: &str,
        session_id: Option<&str>,
    ) -> Result<ManagedSessionInfo> {
        self.start_with_environment(
            logical_path,
            session_id,
            approvals::CapturedStartEnvironment::capture(),
        )
        .await
    }

    pub async fn start_with_environment(
        &self,
        logical_path: &str,
        session_id: Option<&str>,
        environment: approvals::CapturedStartEnvironment,
    ) -> Result<ManagedSessionInfo> {
        self.start_named_with_environment(logical_path, session_id, environment, false)
            .await
    }

    pub async fn start_public_with_environment(
        &self,
        logical_path: &str,
        session_id: Option<&str>,
        environment: approvals::CapturedStartEnvironment,
    ) -> Result<ManagedSessionInfo> {
        self.start_named_with_environment(logical_path, session_id, environment, true)
            .await
    }

    async fn start_named_with_environment(
        &self,
        logical_path: &str,
        session_id: Option<&str>,
        environment: approvals::CapturedStartEnvironment,
        public: bool,
    ) -> Result<ManagedSessionInfo> {
        let _transition = self.transitions.lock().await;
        self.reap_finished().await;
        anyhow::ensure!(
            self.roots_configured(),
            "TEMOTE_MCP_ROOTS is not configured; session_start is disabled"
        );
        let cwd = self.roots.resolve(logical_path)?;
        let id = config::session_id(session_id)?;
        let info = self
            .start_resolved(
                cwd,
                id.clone(),
                false,
                Some(logical_path.to_owned()),
                environment,
            )
            .await?;
        if public {
            self.public_sessions.lock().await.insert(id);
        }
        Ok(info)
    }

    pub async fn start_local_with_environment(
        &self,
        cwd: &std::path::Path,
        session_id: Option<&str>,
        yolo: bool,
        environment: approvals::CapturedStartEnvironment,
    ) -> Result<ManagedSessionInfo> {
        let _transition = self.transitions.lock().await;
        self.reap_finished().await;
        let cwd = config::canonical_directory(cwd)?;
        let id = config::session_id(session_id)?;
        self.start_resolved(cwd, id, yolo, None, environment).await
    }

    async fn start_resolved(
        &self,
        cwd: std::path::PathBuf,
        id: String,
        yolo: bool,
        logical_path: Option<String>,
        environment: approvals::CapturedStartEnvironment,
    ) -> Result<ManagedSessionInfo> {
        anyhow::ensure!(
            !self.closed.load(Ordering::Acquire),
            "session supervisor is shutting down; new sessions are disabled"
        );
        {
            let sessions = self.sessions.lock().await;
            anyhow::ensure!(
                !sessions.contains_key(&id),
                "session {id} is already managed by this supervisor process"
            );
            anyhow::ensure!(
                sessions.len() < self.max_sessions,
                "managed session limit reached ({})",
                self.max_sessions
            );
        }
        anyhow::ensure!(
            !config::session_is_active(&id).await?,
            "session {id} is already running"
        );

        let handle = approvals::spawn_runtime_with_logical_path_and_environment(
            &cwd,
            Some(&id),
            yolo,
            self.approval_sender.clone(),
            logical_path,
            environment,
        )
        .await
        .with_context(|| format!("failed to start managed session {id}"))?;
        let info = ManagedSessionInfo {
            session_id: id.clone(),
            cwd: handle.cwd().to_owned(),
            status: "active",
            yolo,
        };
        self.sessions.lock().await.insert(id, handle);
        Ok(info)
    }

    pub async fn stop(&self, session_id: &str) -> Result<()> {
        self.stop_owned(session_id, false).await
    }

    pub async fn stop_public(&self, session_id: &str) -> Result<()> {
        self.stop_owned(session_id, true).await
    }

    async fn stop_owned(&self, session_id: &str, public_only: bool) -> Result<()> {
        let _transition = self.transitions.lock().await;
        self.reap_finished().await;
        config::validate_session_id(session_id)?;
        if public_only && !self.public_sessions.lock().await.contains(session_id) {
            anyhow::bail!(
                "session {session_id} was not created through the public HTTP supervisor"
            );
        }
        let handle = {
            let mut sessions = self.sessions.lock().await;
            sessions.remove(session_id)
        };
        let Some(handle) = handle else {
            if config::session_is_active(session_id).await? {
                anyhow::bail!(
                    "session {session_id} is active but is not managed by this supervisor process"
                );
            }
            anyhow::bail!("session {session_id} is not managed by this supervisor process");
        };
        self.public_sessions.lock().await.remove(session_id);
        handle.shutdown().await
    }

    #[cfg(test)]
    pub(crate) async fn crash_for_test(&self, session_id: &str) -> Result<()> {
        let sessions = self.sessions.lock().await;
        let handle = sessions.get(session_id).with_context(|| {
            format!("session {session_id} is not managed by this supervisor process")
        })?;
        handle.crash_for_test().await
    }

    pub async fn reap_finished(&self) {
        let finished = {
            let sessions = self.sessions.lock().await;
            sessions
                .iter()
                .filter_map(|(id, handle)| handle.is_finished().then_some(id.clone()))
                .collect::<Vec<_>>()
        };
        for id in finished {
            let handle = self.sessions.lock().await.remove(&id);
            if let Some(handle) = handle {
                self.public_sessions.lock().await.remove(&id);
                if let Err(error) = handle.wait().await {
                    eprintln!("managed session {id} exited: {error:#}");
                }
            }
        }
    }

    pub async fn shutdown(&self) -> Result<()> {
        let _transition = self.transitions.lock().await;
        self.closed.store(true, Ordering::Release);
        let handles = {
            let mut sessions = self.sessions.lock().await;
            sessions.drain().collect::<Vec<_>>()
        };
        self.public_sessions.lock().await.clear();
        let mut first_error = None;
        for (session_id, handle) in handles {
            if let Err(error) = handle.shutdown().await {
                if first_error.is_none() {
                    first_error = Some(error);
                }
                continue;
            }
            let _ = session_id;
        }
        if let Some(error) = first_error {
            return Err(error).context("failed to stop all managed sessions");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashSet};

    use super::*;
    use crate::test_support;

    async fn cleanup_session(id: &str) {
        let _ = tokio::fs::remove_file(config::socket_path(id).unwrap()).await;
        let _ = tokio::fs::remove_file(config::session_path(id).unwrap()).await;
        let _ = tokio::fs::remove_file(config::session_lifecycle_path(id).unwrap()).await;
    }

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
        assert_eq!(
            config::read_session_lifecycle(&first_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            config::LifecycleStatus::Stopped
        );
        supervisor.shutdown().await.unwrap();
        assert!(!config::session_is_active(&second_id).await.unwrap());
        assert!(!config::socket_path(&second_id).unwrap().exists());
        assert!(config::session_path(&first_id).unwrap().exists());
        assert!(config::session_path(&second_id).unwrap().exists());
        assert_eq!(
            config::read_session_lifecycle(&second_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            config::LifecycleStatus::Stopped
        );
        cleanup_session(&first_id).await;
        cleanup_session(&second_id).await;
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
    async fn stopping_session_denies_pending_approval() {
        let (_temp, roots) = fixture();
        let (supervisor, mut approval_receiver) = SessionSupervisor::new(roots);
        let id = format!("approval-stop-{}", uuid::Uuid::new_v4());
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
        assert!(!request.is_finished());

        supervisor.stop(&id).await.unwrap();
        let allowed = tokio::time::timeout(std::time::Duration::from_secs(2), request)
            .await
            .expect("pending approval must resolve when its session stops")
            .unwrap()
            .unwrap();
        assert!(!allowed);
        prompt.respond(true);
    }

    #[tokio::test]
    async fn managed_session_capacity_recovers_after_stop() {
        let (_temp, roots) = fixture();
        let (supervisor, _approvals) = SessionSupervisor::with_limit(roots, 2);
        let first = format!("capacity-a-{}", uuid::Uuid::new_v4());
        let second = format!("capacity-b-{}", uuid::Uuid::new_v4());
        let third = format!("capacity-c-{}", uuid::Uuid::new_v4());

        supervisor.start("src/repo-a", Some(&first)).await.unwrap();
        supervisor.start("src/repo-b", Some(&second)).await.unwrap();
        let error = supervisor
            .start("src/repo-a", Some(&third))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("session limit"));

        supervisor.stop(&first).await.unwrap();
        supervisor.start("src/repo-a", Some(&third)).await.unwrap();
        supervisor.shutdown().await.unwrap();
        assert!(!config::session_is_active(&second).await.unwrap());
        assert!(!config::session_is_active(&third).await.unwrap());
    }

    #[tokio::test]
    async fn start_after_shutdown_is_rejected() {
        let (_temp, roots) = fixture();
        let (supervisor, _approvals) = SessionSupervisor::new(roots);
        supervisor.shutdown().await.unwrap();
        let id = format!("closed-{}", uuid::Uuid::new_v4());

        let error = supervisor.start("src/repo-a", Some(&id)).await.unwrap_err();
        assert!(error.to_string().contains("shutting down"));
        assert!(!config::session_is_active(&id).await.unwrap());
    }

    #[test]
    fn generated_start_shutdown_races_leave_no_active_session() -> noprop::TestResult {
        let (_temp, roots) = fixture();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();

        test_support::run(0x5355_5052_4143_4501, 64, |ctx| {
            let id = format!("shutdown-race-{:x}", noprop::sample_u64(ctx));
            let (supervisor, _approvals) = SessionSupervisor::new(roots.clone());
            runtime.block_on(async {
                let barrier = Arc::new(tokio::sync::Barrier::new(3));
                let start_supervisor = Arc::clone(&supervisor);
                let start_barrier = Arc::clone(&barrier);
                let start_id = id.clone();
                let start = tokio::spawn(async move {
                    start_barrier.wait().await;
                    start_supervisor.start("src/repo-a", Some(&start_id)).await
                });

                let shutdown_supervisor = Arc::clone(&supervisor);
                let shutdown_barrier = Arc::clone(&barrier);
                let shutdown = tokio::spawn(async move {
                    shutdown_barrier.wait().await;
                    shutdown_supervisor.shutdown().await
                });

                barrier.wait().await;
                let start_result = start.await.unwrap();
                shutdown.await.unwrap().unwrap();
                if let Err(error) = start_result {
                    assert!(
                        error.to_string().contains("shutting down"),
                        "unexpected start race error: {error:#}"
                    );
                }
                assert!(
                    !config::session_is_active(&id).await.unwrap(),
                    "session survived supervisor shutdown: {id}"
                );
            });
            Ok(())
        })
    }

    #[test]
    fn generated_start_stop_sequences_match_active_session_model() -> noprop::TestResult {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        test_support::run(0x5355_5056_5354_4154, 64, |ctx| {
            let (_temp, roots) = fixture();
            let nonce = noprop::sample_u64(ctx);
            let max_sessions = noprop::sample_usize_in(ctx, 1..=3);
            let ids = [
                format!("pbt-{nonce:x}-a"),
                format!("pbt-{nonce:x}-b"),
                format!("pbt-{nonce:x}-c"),
            ];
            let steps = (0..12)
                .map(|_| {
                    (
                        noprop::sample_usize_in(ctx, 0..=3),
                        noprop::sample_usize_in(ctx, 0..ids.len()),
                    )
                })
                .collect::<Vec<_>>();

            runtime.block_on(async {
                for id in &ids {
                    cleanup_session(id).await;
                }
                let (supervisor, _approvals) =
                    SessionSupervisor::with_limit(roots, max_sessions);
                let mut active = HashSet::new();
                let mut failure = None::<String>;

                for (operation, index) in steps {
                    let id = &ids[index];
                    match operation {
                        0 | 1 => {
                            let logical = if operation == 0 { "src/repo-a" } else { "src/repo-b" };
                            let expected =
                                !active.contains(id) && active.len() < max_sessions;
                            let result = supervisor.start(logical, Some(id)).await;
                            if result.is_ok() != expected {
                                failure = Some(format!(
                                    "start mismatch: id={id:?} logical={logical:?} expected={expected} result={result:?}"
                                ));
                                break;
                            }
                            if result.is_ok() {
                                active.insert(id.clone());
                            }
                        }
                        2 => {
                            let expected = active.contains(id);
                            let result = supervisor.stop(id).await;
                            if result.is_ok() != expected {
                                failure = Some(format!(
                                    "stop mismatch: id={id:?} expected={expected} result={result:?}"
                                ));
                                break;
                            }
                            if result.is_ok() {
                                active.remove(id);
                            }
                        }
                        _ => {
                            let result = supervisor.start("src/missing", Some(id)).await;
                            if result.is_ok() {
                                failure = Some(format!("missing root path unexpectedly started: id={id:?}"));
                                break;
                            }
                        }
                    }

                    for candidate in &ids {
                        let actual = config::session_is_active(candidate).await.unwrap();
                        let expected = active.contains(candidate);
                        if actual != expected {
                            failure = Some(format!(
                                "active-state mismatch: id={candidate:?} actual={actual} expected={expected}"
                            ));
                            break;
                        }
                    }
                    if failure.is_some() {
                        break;
                    }
                }

                supervisor.shutdown().await.unwrap();
                for id in &ids {
                    assert!(!config::session_is_active(id).await.unwrap());
                    if let Some(lifecycle) = config::read_session_lifecycle(id).await.unwrap() {
                        assert_eq!(lifecycle.status, config::LifecycleStatus::Stopped);
                    }
                    cleanup_session(id).await;
                }
                if let Some(failure) = failure {
                    panic!("{failure}");
                }
            });
            Ok(())
        })
    }

    #[tokio::test]
    async fn one_runtime_failure_does_not_stop_other_session() {
        let (_temp, roots) = fixture();
        let (supervisor, _approvals) = SessionSupervisor::new(roots);
        let failed_id = format!("isolated-failure-{}", uuid::Uuid::new_v4());
        let healthy_id = format!("isolated-healthy-{}", uuid::Uuid::new_v4());
        supervisor
            .start("src/repo-a", Some(&failed_id))
            .await
            .unwrap();
        supervisor
            .start("src/repo-b", Some(&healthy_id))
            .await
            .unwrap();

        {
            let sessions = supervisor.sessions.lock().await;
            sessions
                .get(&failed_id)
                .unwrap()
                .crash_for_test()
                .await
                .unwrap();
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if config::read_session_lifecycle(&failed_id)
                    .await
                    .unwrap()
                    .is_some_and(|state| state.status == config::LifecycleStatus::Crashed)
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("failed runtime did not persist crashed state");
        supervisor.reap_finished().await;

        let failed = config::read_session_lifecycle(&failed_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(failed.status, config::LifecycleStatus::Crashed);
        assert!(config::session_is_active(&healthy_id).await.unwrap());

        supervisor.shutdown().await.unwrap();
        let healthy = config::read_session_lifecycle(&healthy_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(healthy.status, config::LifecycleStatus::Stopped);
        cleanup_session(&failed_id).await;
        cleanup_session(&healthy_id).await;
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
