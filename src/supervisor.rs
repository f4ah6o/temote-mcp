use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::approvals::{self, ApprovalReceiver, ApprovalSender, RuntimeHandle};
use crate::config;
use crate::named_roots::NamedRoots;

const MAX_MANAGED_SESSIONS: usize = 64;
const MAX_AUTOMATIC_RESTARTS: u32 = 5;
const MAX_RESTART_BACKOFF_SECONDS: u64 = 30;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UpgradeSessionPlan {
    pub session_id: String,
    pub cwd: PathBuf,
    pub permitted_directories: Vec<PathBuf>,
    pub yolo: bool,
    pub logical_path: Option<String>,
    pub restart_policy: String,
    pub public: bool,
    pub restart_context_keys: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SupervisorUpgradePlan {
    pub plan_schema: u64,
    pub source_version: String,
    pub target_version: String,
    pub control_protocol: u64,
    pub lifecycle_schema: u64,
    pub supervisor_pid: u32,
    pub created_at: u64,
    pub handoff_required: bool,
    pub sessions: Vec<UpgradeSessionPlan>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UpgradeSessionBlocker {
    pub session_id: String,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SupervisorUpgradePreview {
    pub plan: SupervisorUpgradePlan,
    pub blocked_sessions: Vec<UpgradeSessionBlocker>,
}

#[derive(Clone, Copy)]
struct UpgradePlanOptions {
    fence: bool,
    force: bool,
    collect_blockers: bool,
}

#[derive(Clone)]
struct RestartSpec {
    cwd: PathBuf,
    yolo: bool,
    logical_path: Option<String>,
    environment: approvals::CapturedStartEnvironment,
    public: bool,
}

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
    restart_specs: Mutex<HashMap<String, RestartSpec>>,
    public_sessions: Mutex<HashSet<String>>,
    transitions: Mutex<()>,
    closed: AtomicBool,
    upgrade_fenced: AtomicBool,
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
                restart_specs: Mutex::new(HashMap::new()),
                public_sessions: Mutex::new(HashSet::new()),
                transitions: Mutex::new(()),
                closed: AtomicBool::new(false),
                upgrade_fenced: AtomicBool::new(false),
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

    fn ensure_mutations_allowed(&self) -> Result<()> {
        anyhow::ensure!(
            !self.upgrade_fenced.load(Ordering::Acquire),
            "session lifecycle is temporarily fenced for supervisor upgrade"
        );
        Ok(())
    }

    pub fn clear_upgrade_fence(&self) {
        self.upgrade_fenced.store(false, Ordering::Release);
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
        self.ensure_mutations_allowed()?;
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
                public,
            )
            .await?;
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
        self.ensure_mutations_allowed()?;
        self.reap_finished().await;
        let cwd = config::canonical_directory(cwd)?;
        let id = config::session_id(session_id)?;
        self.start_resolved(cwd, id, yolo, None, environment, false)
            .await
    }

    async fn start_resolved(
        &self,
        cwd: std::path::PathBuf,
        id: String,
        yolo: bool,
        logical_path: Option<String>,
        environment: approvals::CapturedStartEnvironment,
        public: bool,
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

        let spec = RestartSpec {
            cwd: cwd.clone(),
            yolo,
            logical_path: logical_path.clone(),
            environment: environment.clone(),
            public,
        };
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
        self.sessions.lock().await.insert(id.clone(), handle);
        self.restart_specs.lock().await.insert(id.clone(), spec);
        if public {
            self.public_sessions.lock().await.insert(id);
        }
        Ok(info)
    }

    pub async fn stop(&self, session_id: &str) -> Result<()> {
        self.stop_owned(session_id, false).await
    }

    pub async fn stop_public(&self, session_id: &str) -> Result<()> {
        self.stop_owned(session_id, true).await
    }

    pub async fn set_permission_yolo(&self, session_id: &str, value: bool) -> Result<()> {
        let _transition = self.transitions.lock().await;
        self.ensure_mutations_allowed()?;
        self.reap_finished().await;
        config::validate_session_id(session_id)?;
        let sessions = self.sessions.lock().await;
        let handle = sessions.get(session_id).with_context(|| {
            format!("session {session_id} is not managed by this supervisor process")
        })?;
        handle.set_yolo(value).await?;
        drop(sessions);
        if let Some(spec) = self.restart_specs.lock().await.get_mut(session_id) {
            spec.yolo = value;
        }
        Ok(())
    }

    pub async fn allow_directory(&self, session_id: &str, path: std::path::PathBuf) -> Result<()> {
        let _transition = self.transitions.lock().await;
        self.ensure_mutations_allowed()?;
        self.reap_finished().await;
        config::validate_session_id(session_id)?;
        let sessions = self.sessions.lock().await;
        let handle = sessions.get(session_id).with_context(|| {
            format!("session {session_id} is not managed by this supervisor process")
        })?;
        handle.allow_directory(path).await
    }

    pub async fn revoke_directory(&self, session_id: &str, path: std::path::PathBuf) -> Result<()> {
        let _transition = self.transitions.lock().await;
        self.ensure_mutations_allowed()?;
        self.reap_finished().await;
        config::validate_session_id(session_id)?;
        let sessions = self.sessions.lock().await;
        let handle = sessions.get(session_id).with_context(|| {
            format!("session {session_id} is not managed by this supervisor process")
        })?;
        handle.revoke_directory(path).await
    }

    pub async fn set_restart_policy(&self, session_id: &str, policy: &str) -> Result<()> {
        let _transition = self.transitions.lock().await;
        self.ensure_mutations_allowed()?;
        self.reap_finished().await;
        config::validate_session_id(session_id)?;
        anyhow::ensure!(
            matches!(policy, "never" | "on-failure"),
            "restart policy must be never or on-failure"
        );
        let mut lifecycle = config::read_session_lifecycle(session_id)
            .await?
            .with_context(|| format!("session {session_id} has no lifecycle metadata"))?;
        lifecycle.restart_policy = policy.to_owned();
        if policy == "never" {
            lifecycle.next_restart_at = None;
            lifecycle.restart_limit_reason = None;
        } else if lifecycle.status == config::LifecycleStatus::Crashed
            && self.restart_specs.lock().await.contains_key(session_id)
            && lifecycle.restart_count < MAX_AUTOMATIC_RESTARTS
        {
            lifecycle.next_restart_at = Some(config::unix_time() + 1);
            lifecycle.restart_limit_reason = None;
        }
        config::save_session_lifecycle(session_id, &lifecycle).await
    }

    async fn schedule_restart(&self, session_id: &str, reason: &str) -> Result<()> {
        let mut lifecycle = config::read_session_lifecycle(session_id)
            .await?
            .with_context(|| format!("session {session_id} has no lifecycle metadata"))?;
        if lifecycle.restart_policy != "on-failure" {
            return Ok(());
        }
        if lifecycle.restart_count >= MAX_AUTOMATIC_RESTARTS {
            lifecycle.next_restart_at = None;
            lifecycle.restart_limit_reason = Some(format!(
                "automatic restart limit reached after {} attempts",
                lifecycle.restart_count
            ));
            lifecycle.exit_reason = lifecycle.restart_limit_reason.clone();
            config::save_session_lifecycle(session_id, &lifecycle).await?;
            return Ok(());
        }
        let shift = lifecycle.restart_count.min(5);
        let backoff = (1_u64 << shift).min(MAX_RESTART_BACKOFF_SECONDS);
        lifecycle.restart_count += 1;
        lifecycle.next_restart_at = Some(config::unix_time() + backoff);
        lifecycle.restart_limit_reason = None;
        lifecycle.exit_reason = Some(format!(
            "{reason}; automatic restart {} scheduled in {backoff}s",
            lifecycle.restart_count
        ));
        config::save_session_lifecycle(session_id, &lifecycle).await
    }

    async fn restart_due_sessions(&self) {
        if self.upgrade_fenced.load(Ordering::Acquire) {
            return;
        }
        let now = config::unix_time();
        let specs = self.restart_specs.lock().await.clone();
        for (id, spec) in specs {
            if self.sessions.lock().await.contains_key(&id) {
                continue;
            }
            let Ok(Some(mut lifecycle)) = config::read_session_lifecycle(&id).await else {
                continue;
            };
            if lifecycle.restart_policy != "on-failure"
                || lifecycle.status != config::LifecycleStatus::Crashed
                || lifecycle.next_restart_at.is_none_or(|at| at > now)
            {
                continue;
            }
            lifecycle.last_restart_at = Some(now);
            lifecycle.next_restart_at = None;
            if let Err(error) = config::save_session_lifecycle(&id, &lifecycle).await {
                eprintln!("failed to persist restart attempt for {id}: {error:#}");
                continue;
            }
            if let Err(error) = self
                .start_resolved(
                    spec.cwd.clone(),
                    id.clone(),
                    spec.yolo,
                    spec.logical_path.clone(),
                    spec.environment.clone(),
                    spec.public,
                )
                .await
            {
                eprintln!("automatic restart for session {id} failed: {error:#}");
                if let Err(save_error) = self
                    .schedule_restart(&id, &format!("automatic restart attempt failed: {error:#}"))
                    .await
                {
                    eprintln!("failed to schedule another restart for {id}: {save_error:#}");
                }
            }
        }
    }

    async fn stop_owned(&self, session_id: &str, public_only: bool) -> Result<()> {
        let _transition = self.transitions.lock().await;
        self.ensure_mutations_allowed()?;
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
            if self.restart_specs.lock().await.remove(session_id).is_some() {
                self.public_sessions.lock().await.remove(session_id);
                if let Some(mut lifecycle) = config::read_session_lifecycle(session_id).await? {
                    lifecycle.status = config::LifecycleStatus::Stopped;
                    lifecycle.stopped_at = Some(config::unix_time());
                    lifecycle.next_restart_at = None;
                    lifecycle.exit_reason =
                        Some("graceful stop cancelled pending automatic restart".to_owned());
                    lifecycle.last_error = None;
                    config::save_session_lifecycle(session_id, &lifecycle).await?;
                }
                return Ok(());
            }
            anyhow::bail!("session {session_id} is not managed by this supervisor process");
        };
        self.restart_specs.lock().await.remove(session_id);
        self.public_sessions.lock().await.remove(session_id);
        handle.shutdown().await
    }

    pub async fn build_upgrade_plan(
        &self,
        target_version: &str,
        control_protocol: u64,
        lifecycle_schema: u64,
        available_environment: &approvals::CapturedStartEnvironment,
        fence: bool,
        force: bool,
    ) -> Result<SupervisorUpgradePlan> {
        let preview = self
            .prepare_upgrade_plan(
                target_version,
                control_protocol,
                lifecycle_schema,
                available_environment,
                UpgradePlanOptions {
                    fence,
                    force,
                    collect_blockers: false,
                },
            )
            .await?;
        Ok(preview.plan)
    }

    pub async fn preview_upgrade_plan(
        &self,
        target_version: &str,
        control_protocol: u64,
        lifecycle_schema: u64,
        available_environment: &approvals::CapturedStartEnvironment,
        force: bool,
    ) -> Result<SupervisorUpgradePreview> {
        self.prepare_upgrade_plan(
            target_version,
            control_protocol,
            lifecycle_schema,
            available_environment,
            UpgradePlanOptions {
                fence: false,
                force,
                collect_blockers: true,
            },
        )
        .await
    }

    async fn prepare_upgrade_plan(
        &self,
        target_version: &str,
        control_protocol: u64,
        lifecycle_schema: u64,
        available_environment: &approvals::CapturedStartEnvironment,
        options: UpgradePlanOptions,
    ) -> Result<SupervisorUpgradePreview> {
        let _transition = self.transitions.lock().await;
        anyhow::ensure!(
            !self.upgrade_fenced.load(Ordering::Acquire),
            "another supervisor upgrade is already in progress"
        );
        self.reap_finished().await;

        let source_version = env!("CARGO_PKG_VERSION").to_owned();
        let handoff_required = options.force || source_version != target_version;
        let mut plans = Vec::new();
        let mut blocked_sessions = Vec::new();
        if handoff_required {
            let ids = {
                let sessions = self.sessions.lock().await;
                let mut ids = sessions.keys().cloned().collect::<Vec<_>>();
                ids.sort();
                ids
            };
            for id in ids {
                let attempt: Result<UpgradeSessionPlan> = async {
                    let snapshot = {
                        let sessions = self.sessions.lock().await;
                        let handle = sessions.get(&id).with_context(|| {
                            format!("session {id} disappeared during upgrade preflight")
                        })?;
                        handle.snapshot().await?
                    };
                    let spec = self
                        .restart_specs
                        .lock()
                        .await
                        .get(&id)
                        .cloned()
                        .with_context(|| {
                            format!("session {id} has no in-memory restart context")
                        })?;
                    if let Some(logical_path) = spec.logical_path.as_deref() {
                        let resolved = self.roots.resolve(logical_path)?;
                        anyhow::ensure!(
                            resolved == snapshot.cwd,
                            "session {id} named root no longer resolves to its current cwd"
                        );
                    } else {
                        anyhow::ensure!(
                            config::canonical_directory(&spec.cwd)? == snapshot.cwd,
                            "session {id} local cwd no longer resolves to its current cwd"
                        );
                    }
                    let mismatches = spec
                        .environment
                        .restart_context_mismatches(available_environment);
                    anyhow::ensure!(
                        mismatches.is_empty(),
                        "session {id} restart context is unavailable or changed for keys: {}",
                        mismatches.join(", ")
                    );
                    let lifecycle = config::read_session_lifecycle(&id)
                        .await?
                        .with_context(|| format!("session {id} has no lifecycle metadata"))?;
                    anyhow::ensure!(
                        lifecycle.status == config::LifecycleStatus::Active,
                        "session {id} is not in active lifecycle state during upgrade preflight"
                    );
                    Ok(UpgradeSessionPlan {
                        session_id: id.clone(),
                        cwd: snapshot.cwd,
                        permitted_directories: snapshot.permitted_directories,
                        yolo: snapshot.yolo,
                        logical_path: spec.logical_path,
                        restart_policy: lifecycle.restart_policy,
                        public: self.public_sessions.lock().await.contains(&id),
                        restart_context_keys: spec.environment.restart_context_keys(),
                    })
                }
                .await;
                match attempt {
                    Ok(plan) => plans.push(plan),
                    Err(error) if options.collect_blockers => {
                        blocked_sessions.push(UpgradeSessionBlocker {
                            session_id: id,
                            reason: format!("{error:#}"),
                        })
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        if options.fence && handoff_required && blocked_sessions.is_empty() {
            self.upgrade_fenced.store(true, Ordering::Release);
        }
        Ok(SupervisorUpgradePreview {
            plan: SupervisorUpgradePlan {
                plan_schema: 1,
                source_version,
                target_version: target_version.to_owned(),
                control_protocol,
                lifecycle_schema,
                supervisor_pid: std::process::id(),
                created_at: config::unix_time(),
                handoff_required,
                sessions: plans,
            },
            blocked_sessions,
        })
    }

    pub async fn quiesce_for_upgrade(&self, plan: &SupervisorUpgradePlan) -> Result<()> {
        let _transition = self.transitions.lock().await;
        anyhow::ensure!(
            self.upgrade_fenced.load(Ordering::Acquire),
            "supervisor upgrade is not fenced"
        );
        let mut quiesced = Vec::new();
        for planned in &plan.sessions {
            let result = {
                let sessions = self.sessions.lock().await;
                let handle = sessions.get(&planned.session_id).with_context(|| {
                    format!(
                        "session {} disappeared before upgrade quiesce",
                        planned.session_id
                    )
                })?;
                handle.set_upgrade_quiesced(true).await
            };
            if let Err(error) = result {
                for id in quiesced.into_iter().rev() {
                    if let Some(handle) = self.sessions.lock().await.get(&id) {
                        let _ = handle.set_upgrade_quiesced(false).await;
                    }
                }
                self.upgrade_fenced.store(false, Ordering::Release);
                return Err(error).with_context(|| {
                    format!(
                        "session {} is busy; supervisor upgrade aborted",
                        planned.session_id
                    )
                });
            }
            quiesced.push(planned.session_id.clone());
        }
        Ok(())
    }

    pub async fn drain_for_upgrade(&self, plan: &SupervisorUpgradePlan) -> Result<()> {
        let _transition = self.transitions.lock().await;
        anyhow::ensure!(
            self.upgrade_fenced.load(Ordering::Acquire),
            "supervisor upgrade is not fenced"
        );
        for planned in &plan.sessions {
            let handle = self.sessions.lock().await.remove(&planned.session_id);
            let Some(handle) = handle else {
                anyhow::bail!(
                    "session {} disappeared before upgrade drain",
                    planned.session_id
                );
            };
            handle
                .shutdown()
                .await
                .with_context(|| format!("failed to drain session {}", planned.session_id))?;
        }
        Ok(())
    }

    pub async fn rollback_upgrade(&self, plan: &SupervisorUpgradePlan) -> Result<()> {
        let _transition = self.transitions.lock().await;
        let mut first_error = None;
        for planned in &plan.sessions {
            if config::session_is_active(&planned.session_id)
                .await
                .unwrap_or(false)
            {
                if let Some(handle) = self.sessions.lock().await.get(&planned.session_id) {
                    let _ = handle.set_upgrade_quiesced(false).await;
                }
                continue;
            }
            let spec = self
                .restart_specs
                .lock()
                .await
                .get(&planned.session_id)
                .cloned();
            let result = match spec {
                Some(spec) => self
                    .start_resolved(
                        spec.cwd,
                        planned.session_id.clone(),
                        spec.yolo,
                        spec.logical_path,
                        spec.environment,
                        spec.public,
                    )
                    .await
                    .map(|_| ()),
                None => Err(anyhow::anyhow!(
                    "session {} restart context disappeared during rollback",
                    planned.session_id
                )),
            };
            if let Err(error) = result
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        self.upgrade_fenced.store(false, Ordering::Release);
        if let Some(error) = first_error {
            Err(error).context("supervisor upgrade rollback was incomplete")
        } else {
            Ok(())
        }
    }

    pub async fn restore_upgrade_plan(
        &self,
        plan: &SupervisorUpgradePlan,
        available_environment: &approvals::CapturedStartEnvironment,
    ) -> Result<()> {
        anyhow::ensure!(
            plan.plan_schema == 1,
            "unsupported supervisor restore plan schema"
        );
        anyhow::ensure!(
            plan.target_version == env!("CARGO_PKG_VERSION"),
            "restore plan targets Temote {}, but this binary is {}",
            plan.target_version,
            env!("CARGO_PKG_VERSION")
        );
        let mut restored = Vec::new();
        for planned in &plan.sessions {
            let environment = available_environment
                .select_restart_context(&planned.restart_context_keys)
                .with_context(|| {
                    format!(
                        "restart context unavailable for session {}",
                        planned.session_id
                    )
                })?;
            let cwd = if let Some(logical_path) = planned.logical_path.as_deref() {
                let resolved = self.roots.resolve(logical_path)?;
                anyhow::ensure!(
                    resolved == planned.cwd,
                    "session {} named root changed during supervisor handoff",
                    planned.session_id
                );
                resolved
            } else {
                let resolved = config::canonical_directory(&planned.cwd)?;
                anyhow::ensure!(
                    resolved == planned.cwd,
                    "session {} cwd changed during supervisor handoff",
                    planned.session_id
                );
                resolved
            };
            let result = self
                .start_resolved(
                    cwd,
                    planned.session_id.clone(),
                    planned.yolo,
                    planned.logical_path.clone(),
                    environment,
                    planned.public,
                )
                .await;
            match result {
                Ok(_) => restored.push(planned.session_id.clone()),
                Err(error) => {
                    for id in restored.into_iter().rev() {
                        if let Some(handle) = self.sessions.lock().await.remove(&id) {
                            let _ = handle.shutdown().await;
                        }
                    }
                    return Err(error).with_context(|| {
                        format!("failed to restore session {}", planned.session_id)
                    });
                }
            }
            if planned.restart_policy != "never" {
                let mut lifecycle = config::read_session_lifecycle(&planned.session_id)
                    .await?
                    .with_context(|| {
                        format!(
                            "restored session {} has no lifecycle metadata",
                            planned.session_id
                        )
                    })?;
                lifecycle.restart_policy = planned.restart_policy.clone();
                config::save_session_lifecycle(&planned.session_id, &lifecycle).await?;
            }
        }

        for planned in &plan.sessions {
            anyhow::ensure!(
                config::session_is_active(&planned.session_id).await?,
                "restored session {} did not pass its socket probe",
                planned.session_id
            );
            let metadata = config::read_session_metadata(&planned.session_id).await?;
            anyhow::ensure!(
                metadata.cwd == planned.cwd
                    && metadata.yolo == planned.yolo
                    && metadata.permitted_directories == planned.permitted_directories,
                "restored session {} metadata does not match the upgrade plan",
                planned.session_id
            );
            let lifecycle = config::read_session_lifecycle(&planned.session_id)
                .await?
                .with_context(|| {
                    format!("restored session {} has no lifecycle", planned.session_id)
                })?;
            anyhow::ensure!(
                lifecycle.restart_policy == planned.restart_policy,
                "restored session {} restart policy changed during handoff",
                planned.session_id
            );
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn crash_for_test(&self, session_id: &str) -> Result<()> {
        let sessions = self.sessions.lock().await;
        let handle = sessions.get(session_id).with_context(|| {
            format!("session {session_id} is not managed by this supervisor process")
        })?;
        handle.crash_for_test().await
    }

    #[cfg(test)]
    pub(crate) async fn is_managed_for_test(&self, session_id: &str) -> bool {
        self.sessions.lock().await.contains_key(session_id)
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
                match handle.wait().await {
                    Ok(()) => {
                        self.restart_specs.lock().await.remove(&id);
                        self.public_sessions.lock().await.remove(&id);
                    }
                    Err(error) => {
                        eprintln!("managed session {id} exited: {error:#}");
                        let should_restart = config::read_session_lifecycle(&id)
                            .await
                            .ok()
                            .flatten()
                            .is_some_and(|state| state.restart_policy == "on-failure");
                        if should_restart && self.restart_specs.lock().await.contains_key(&id) {
                            if let Err(schedule_error) = self
                                .schedule_restart(&id, "unexpected runtime failure")
                                .await
                            {
                                eprintln!(
                                    "failed to schedule automatic restart for {id}: {schedule_error:#}"
                                );
                            }
                        } else {
                            self.restart_specs.lock().await.remove(&id);
                            self.public_sessions.lock().await.remove(&id);
                        }
                    }
                }
            }
        }
        self.restart_due_sessions().await;
    }

    pub async fn shutdown(&self) -> Result<()> {
        let _transition = self.transitions.lock().await;
        self.closed.store(true, Ordering::Release);
        let handles = {
            let mut sessions = self.sessions.lock().await;
            sessions.drain().collect::<Vec<_>>()
        };
        self.restart_specs.lock().await.clear();
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
    async fn on_failure_policy_restarts_crash_and_graceful_stop_does_not_restart() {
        let (_temp, roots) = fixture();
        let (supervisor, _approvals) = SessionSupervisor::new(roots);
        let id = format!("auto-restart-{}", uuid::Uuid::new_v4());
        supervisor.start("src/repo-a", Some(&id)).await.unwrap();
        let initial = config::read_session_lifecycle(&id).await.unwrap().unwrap();
        assert_eq!(initial.restart_policy, "never");
        assert_eq!(initial.restart_count, 0);

        supervisor
            .set_restart_policy(&id, "on-failure")
            .await
            .unwrap();
        supervisor.crash_for_test(&id).await.unwrap();
        let mut scheduled = tokio::time::timeout(std::time::Duration::from_secs(4), async {
            loop {
                supervisor.reap_finished().await;
                let lifecycle = config::read_session_lifecycle(&id).await.unwrap().unwrap();
                if lifecycle.status == config::LifecycleStatus::Crashed
                    && lifecycle.restart_count == 1
                    && lifecycle.next_restart_at.is_some()
                {
                    break lifecycle;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("crashed session did not schedule an automatic restart");

        // The production policy intentionally waits at least one second before restarting.
        // Make the scheduled attempt due now so this test verifies the restart transition
        // without depending on wall-clock backoff while the full suite is under load.
        scheduled.next_restart_at = Some(config::unix_time());
        config::save_session_lifecycle(&id, &scheduled)
            .await
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(4), async {
            loop {
                supervisor.reap_finished().await;
                let lifecycle = config::read_session_lifecycle(&id).await.unwrap().unwrap();
                if lifecycle.status == config::LifecycleStatus::Active
                    && lifecycle.restart_count == 1
                    && lifecycle.last_restart_at.is_some()
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("scheduled automatic restart did not become active");

        supervisor.stop(&id).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        supervisor.reap_finished().await;
        let stopped = config::read_session_lifecycle(&id).await.unwrap().unwrap();
        assert_eq!(stopped.status, config::LifecycleStatus::Stopped);
        assert_eq!(stopped.restart_count, 1);
        assert!(stopped.next_restart_at.is_none());
        assert!(!config::session_is_active(&id).await.unwrap());
        supervisor.shutdown().await.unwrap();
        cleanup_session(&id).await;
    }

    #[tokio::test]
    async fn automatic_restart_rate_limit_is_bounded() {
        let (_temp, roots) = fixture();
        let (supervisor, _approvals) = SessionSupervisor::new(roots);
        let id = format!("restart-limit-{}", uuid::Uuid::new_v4());
        supervisor.start("src/repo-a", Some(&id)).await.unwrap();
        supervisor
            .set_restart_policy(&id, "on-failure")
            .await
            .unwrap();

        for _ in 0..=MAX_AUTOMATIC_RESTARTS {
            supervisor
                .schedule_restart(&id, "test crash")
                .await
                .unwrap();
        }
        let lifecycle = config::read_session_lifecycle(&id).await.unwrap().unwrap();
        assert_eq!(lifecycle.restart_count, MAX_AUTOMATIC_RESTARTS);
        assert!(lifecycle.next_restart_at.is_none());
        assert!(
            lifecycle
                .restart_limit_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("restart limit reached"))
        );

        supervisor.stop(&id).await.unwrap();
        supervisor.shutdown().await.unwrap();
        cleanup_session(&id).await;
    }

    #[tokio::test]
    async fn upgrade_plan_restore_preserves_session_state_without_persisting_context_values() {
        let (_temp, roots) = fixture();
        let (supervisor, _approvals) = SessionSupervisor::new(roots.clone());
        let id = format!("upgrade-restore-{}", uuid::Uuid::new_v4());
        let environment = approvals::CapturedStartEnvironment::from_values(BTreeMap::from([
            ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
            ("KINTONE_PASSWORD".to_owned(), "must-not-persist".to_owned()),
        ]))
        .unwrap();
        supervisor
            .start_with_environment("src/repo-a", Some(&id), environment.clone())
            .await
            .unwrap();
        let extra_root = std::fs::canonicalize(_temp.path().join("volume/repo-b")).unwrap();
        supervisor.allow_directory(&id, extra_root).await.unwrap();
        supervisor.set_permission_yolo(&id, true).await.unwrap();
        supervisor
            .set_restart_policy(&id, "on-failure")
            .await
            .unwrap();

        let plan = supervisor
            .build_upgrade_plan(env!("CARGO_PKG_VERSION"), 1, 1, &environment, true, true)
            .await
            .unwrap();
        assert!(plan.handoff_required);
        assert_eq!(plan.sessions.len(), 1);
        assert!(plan.sessions[0].yolo);
        assert_eq!(plan.sessions[0].restart_policy, "on-failure");
        assert_eq!(
            plan.sessions[0].restart_context_keys,
            ["KINTONE_PASSWORD", "PATH"]
        );
        let serialized = serde_json::to_string(&plan).unwrap();
        assert!(!serialized.contains("must-not-persist"));

        supervisor.quiesce_for_upgrade(&plan).await.unwrap();
        supervisor.drain_for_upgrade(&plan).await.unwrap();
        assert!(!config::session_is_active(&id).await.unwrap());

        let (replacement, _replacement_approvals) = SessionSupervisor::new(roots);
        replacement
            .restore_upgrade_plan(&plan, &environment)
            .await
            .unwrap();
        assert!(config::session_is_active(&id).await.unwrap());
        let metadata = config::read_session_metadata(&id).await.unwrap();
        assert!(metadata.yolo);
        let lifecycle = config::read_session_lifecycle(&id).await.unwrap().unwrap();
        assert_eq!(lifecycle.restart_policy, "on-failure");

        replacement.shutdown().await.unwrap();
        supervisor.clear_upgrade_fence();
        supervisor.shutdown().await.unwrap();
        cleanup_session(&id).await;
    }

    #[tokio::test]
    async fn same_version_upgrade_plan_is_idempotent_and_does_not_fence() {
        let (_temp, roots) = fixture();
        let (supervisor, _approvals) = SessionSupervisor::new(roots);
        let id = format!("upgrade-noop-{}", uuid::Uuid::new_v4());
        let environment = approvals::CapturedStartEnvironment::default();
        supervisor
            .start_with_environment("src/repo-a", Some(&id), environment.clone())
            .await
            .unwrap();
        let plan = supervisor
            .build_upgrade_plan(env!("CARGO_PKG_VERSION"), 1, 1, &environment, true, false)
            .await
            .unwrap();
        assert!(!plan.handoff_required);
        assert!(plan.sessions.is_empty());
        // A no-op must not leave lifecycle mutation fenced.
        supervisor.stop(&id).await.unwrap();
        supervisor.shutdown().await.unwrap();
        cleanup_session(&id).await;
    }

    #[tokio::test]
    async fn upgrade_preview_collects_blockers_without_mutation_or_secret_values() {
        let (_temp, roots) = fixture();
        let (supervisor, _approvals) = SessionSupervisor::new(roots);
        let ready_id = format!("upgrade-preview-ready-{}", uuid::Uuid::new_v4());
        let blocked_id = format!("upgrade-preview-blocked-{}", uuid::Uuid::new_v4());
        let ready_environment =
            approvals::CapturedStartEnvironment::from_values(BTreeMap::from([(
                "PATH".to_owned(),
                "/usr/bin:/bin".to_owned(),
            )]))
            .unwrap();
        let blocked_environment = approvals::CapturedStartEnvironment::from_values(BTreeMap::from(
            [("KINTONE_PASSWORD".to_owned(), "original-secret".to_owned())],
        ))
        .unwrap();
        let available_environment =
            approvals::CapturedStartEnvironment::from_values(BTreeMap::from([
                ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
                ("KINTONE_PASSWORD".to_owned(), "different-secret".to_owned()),
            ]))
            .unwrap();
        supervisor
            .start_with_environment("src/repo-a", Some(&ready_id), ready_environment)
            .await
            .unwrap();
        supervisor
            .start_with_environment("src/repo-a", Some(&blocked_id), blocked_environment)
            .await
            .unwrap();

        let preview = supervisor
            .preview_upgrade_plan(
                env!("CARGO_PKG_VERSION"),
                1,
                1,
                &available_environment,
                true,
            )
            .await
            .unwrap();
        assert!(preview.plan.handoff_required);
        assert_eq!(preview.plan.sessions.len(), 1);
        assert_eq!(preview.plan.sessions[0].session_id, ready_id);
        assert_eq!(preview.blocked_sessions.len(), 1);
        assert_eq!(preview.blocked_sessions[0].session_id, blocked_id);
        assert!(
            preview.blocked_sessions[0]
                .reason
                .contains("KINTONE_PASSWORD")
        );
        let serialized = serde_json::to_string(&preview).unwrap();
        assert!(!serialized.contains("original-secret"));
        assert!(!serialized.contains("different-secret"));
        assert!(config::session_is_active(&ready_id).await.unwrap());
        assert!(config::session_is_active(&blocked_id).await.unwrap());

        let error = supervisor
            .build_upgrade_plan(
                env!("CARGO_PKG_VERSION"),
                1,
                1,
                &available_environment,
                true,
                true,
            )
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("KINTONE_PASSWORD"));
        assert!(config::session_is_active(&ready_id).await.unwrap());
        assert!(config::session_is_active(&blocked_id).await.unwrap());

        // Neither preview nor failed destructive preflight may leave mutation fenced.
        supervisor.stop(&ready_id).await.unwrap();
        supervisor.stop(&blocked_id).await.unwrap();
        supervisor.shutdown().await.unwrap();
        cleanup_session(&ready_id).await;
        cleanup_session(&blocked_id).await;
    }

    #[tokio::test]
    async fn upgrade_preflight_rejects_changed_restart_context_without_exposing_values() {
        let (_temp, roots) = fixture();
        let (supervisor, _approvals) = SessionSupervisor::new(roots);
        let id = format!("upgrade-context-{}", uuid::Uuid::new_v4());
        let original = approvals::CapturedStartEnvironment::from_values(BTreeMap::from([(
            "KINTONE_PASSWORD".to_owned(),
            "original-secret".to_owned(),
        )]))
        .unwrap();
        let changed = approvals::CapturedStartEnvironment::from_values(BTreeMap::from([(
            "KINTONE_PASSWORD".to_owned(),
            "different-secret".to_owned(),
        )]))
        .unwrap();
        supervisor
            .start_with_environment("src/repo-a", Some(&id), original)
            .await
            .unwrap();

        let preview = supervisor
            .preview_upgrade_plan(env!("CARGO_PKG_VERSION"), 1, 1, &changed, true)
            .await
            .unwrap();
        assert!(preview.plan.sessions.is_empty());
        assert_eq!(preview.blocked_sessions.len(), 1);
        let blocker = &preview.blocked_sessions[0];
        assert_eq!(blocker.session_id, id);
        assert!(blocker.reason.contains("KINTONE_PASSWORD"));
        assert!(!blocker.reason.contains("original-secret"));
        assert!(!blocker.reason.contains("different-secret"));

        let error = supervisor
            .build_upgrade_plan(env!("CARGO_PKG_VERSION"), 1, 1, &changed, true, true)
            .await
            .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("KINTONE_PASSWORD"));
        assert!(!text.contains("original-secret"));
        assert!(!text.contains("different-secret"));

        // Failed preflight must not leave the lifecycle fenced.
        supervisor.stop(&id).await.unwrap();
        supervisor.shutdown().await.unwrap();
        cleanup_session(&id).await;
    }

    #[tokio::test]
    async fn upgrade_quiesce_fails_closed_while_approval_is_in_flight() {
        let (_temp, roots) = fixture();
        let (supervisor, mut approvals) = SessionSupervisor::new(roots);
        let id = format!("upgrade-busy-{}", uuid::Uuid::new_v4());
        let environment = approvals::CapturedStartEnvironment::default();
        let info = supervisor
            .start_with_environment("src/repo-a", Some(&id), environment.clone())
            .await
            .unwrap();
        let request_id = id.clone();
        let request_cwd = info.cwd.clone();
        let pending = tokio::spawn(async move {
            approvals::request(
                &request_id,
                "upgrade-busy-test",
                "pending approval".to_owned(),
                request_cwd,
            )
            .await
        });
        let prompt = tokio::time::timeout(std::time::Duration::from_secs(1), approvals.recv())
            .await
            .unwrap()
            .unwrap();

        let plan = supervisor
            .build_upgrade_plan(env!("CARGO_PKG_VERSION"), 1, 1, &environment, true, true)
            .await
            .unwrap();
        let error = supervisor.quiesce_for_upgrade(&plan).await.unwrap_err();
        assert!(format!("{error:#}").contains("in-flight operation"));
        prompt.respond(false);
        assert!(!pending.await.unwrap().unwrap());

        // Busy abort clears the fence and leaves the session usable/stoppable.
        supervisor.stop(&id).await.unwrap();
        supervisor.shutdown().await.unwrap();
        cleanup_session(&id).await;
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
