//! Versioned, typed local task packets. The supervisor is only a transport:
//! authorization, receipts, runtime leases and evidence remain in orchestration.

use std::path::PathBuf;

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{config, observation, orchestration};

pub(crate) const PROTOCOL_VERSION: u64 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BackendName {
    Codex,
    #[cfg(feature = "network")]
    Opencode,
    #[serde(alias = "devin")]
    DevinAcp,
    #[cfg(feature = "network")]
    DevinCloud,
}

impl From<BackendName> for orchestration::Backend {
    fn from(value: BackendName) -> Self {
        match value {
            BackendName::Codex => Self::Codex,
            #[cfg(feature = "network")]
            BackendName::Opencode => Self::OpenCode,
            BackendName::DevinAcp => Self::DevinAcp,
            #[cfg(feature = "network")]
            BackendName::DevinCloud => Self::DevinCloud,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum TaskAction {
    Change {
        owner_provisioning_operation_id: Uuid,
        command: crate::change_cli::ChangeCommand,
    },
    Start {
        backend: BackendName,
        operation_id: Uuid,
        task: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        continuation: Option<crate::codex_app_server::CodexContinuation>,
        #[serde(skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        variant: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        workspace_requirement: Option<String>,
        #[serde(default)]
        cloud: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        devin_mode: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        swe_tier: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        repos: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_acu_limit: Option<u64>,
    },
    List {
        limit: Option<usize>,
    },
    Get {
        backend: BackendName,
        task_id: Uuid,
        #[serde(skip_serializing_if = "Option::is_none")]
        after_revision: Option<u64>,
        #[serde(default)]
        wait_ms: u64,
    },
    Control {
        backend: BackendName,
        task_id: Uuid,
        operation_id: Uuid,
        action: ControlAction,
        #[serde(skip_serializing_if = "Option::is_none")]
        input: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ControlAction {
    Steer,
    Resume,
    Interrupt,
}

impl ControlAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Steer => "steer",
            Self::Resume => "resume",
            Self::Interrupt => "interrupt",
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskPacket {
    pub schema_version: u64,
    pub session_id: String,
    pub instance: SessionInstance,
    pub action: TaskAction,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionInstance {
    pub cwd: PathBuf,
    pub started_at: u64,
    pub process_id: u32,
    pub permission_mode: config::PermissionMode,
    pub permitted_directories: Vec<PathBuf>,
    pub grants: config::SessionGrants,
}

impl SessionInstance {
    fn matches(&self, session: &config::Session) -> bool {
        self.cwd == session.cwd
            && self.started_at == session.started_at
            && self.process_id == session.process_id
            && self.permission_mode == session.permission_mode
            && self.permitted_directories == session.permitted_directories
            && self.grants == session.grants
    }
}

impl TaskPacket {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == PROTOCOL_VERSION,
            "unsupported local task protocol version"
        );
        ensure!(
            !self.session_id.is_empty() && self.session_id.len() <= 128,
            "invalid session_id"
        );
        ensure!(
            self.instance.started_at != 0 && self.instance.process_id != 0,
            "invalid session instance"
        );
        ensure!(
            !self.instance.permission_mode.is_yolo()
                && !self.instance.permitted_directories.is_empty()
                && serde_json::to_vec(&self.instance)?.len() <= 64 * 1024,
            "invalid local task scope"
        );
        Ok(())
    }
}

// Keep each typed authority and request component explicit at this boundary.
#[allow(clippy::too_many_arguments)]
fn validate_start_options(
    backend: BackendName,
    _model: &Option<String>,
    effort: &Option<String>,
    continuation: &Option<crate::codex_app_server::CodexContinuation>,
    agent: &Option<String>,
    variant: &Option<String>,
    workspace_requirement: &Option<String>,
    cloud: bool,
    title: &Option<String>,
    devin_mode: &Option<String>,
    swe_tier: &Option<String>,
    repos: &[String],
    max_acu_limit: Option<u64>,
) -> Result<()> {
    let allowed = match backend {
        BackendName::Codex => {
            agent.is_none()
                && variant.is_none()
                && workspace_requirement.is_none()
                && !cloud
                && title.is_none()
                && devin_mode.is_none()
                && swe_tier.is_none()
                && repos.is_empty()
                && max_acu_limit.is_none()
        }
        #[cfg(feature = "network")]
        BackendName::Opencode => {
            continuation.is_none()
                && effort.is_none()
                && !cloud
                && title.is_none()
                && devin_mode.is_none()
                && swe_tier.is_none()
                && repos.is_empty()
                && max_acu_limit.is_none()
                && workspace_requirement
                    .as_deref()
                    .is_none_or(|value| value == "managed_commands")
        }
        BackendName::DevinAcp => {
            continuation.is_none()
                && effort.is_none()
                && variant.is_none()
                && workspace_requirement.is_none()
                && title.is_none()
                && devin_mode.is_none()
                && swe_tier.is_none()
                && repos.is_empty()
                && max_acu_limit.is_none()
        }
        #[cfg(feature = "network")]
        BackendName::DevinCloud => {
            continuation.is_none()
                && _model.is_none()
                && effort.is_none()
                && agent.is_none()
                && variant.is_none()
                && workspace_requirement.is_none()
                && !cloud
        }
    };
    ensure!(allowed, "unsupported options for selected task backend");
    Ok(())
}

pub(crate) async fn dispatch(packet: TaskPacket) -> Result<Value> {
    packet.validate()?;
    let session = config::load_session(&packet.session_id).await?;
    ensure!(
        session.id == packet.session_id && packet.instance.matches(&session),
        "session instance changed; reconcile retained tasks before another operation"
    );
    ensure!(
        session.permission_mode != config::PermissionMode::Yolo,
        "local task protocol requires an ask or agent session"
    );
    let actor = observation::ActorRef {
        transport: "local-task".to_owned(),
        principal: None,
    };
    match packet.action {
        TaskAction::Change {
            owner_provisioning_operation_id,
            command,
        } => {
            let receipt = crate::repository_store::RepositoryStore::new()?
                .read(owner_provisioning_operation_id)?
                .ok_or_else(|| anyhow::anyhow!("Change owner provisioning receipt missing"))?;
            ensure!(
                receipt.session_id == session.id
                    && receipt
                        .activated_owner
                        .as_ref()
                        .is_some_and(|owner| owner.matches(&session)),
                "Change owner full session instance mismatch"
            );
            crate::change_cli::execute_managed(crate::change_cli::ChangeInvocation {
                provisioning_operation_id: owner_provisioning_operation_id,
                request: command,
            })
            .await
        }
        TaskAction::Start {
            backend,
            operation_id,
            task,
            model,
            effort,
            continuation,
            agent,
            variant,
            workspace_requirement,
            cloud,
            title,
            devin_mode,
            swe_tier,
            repos,
            max_acu_limit,
        } => {
            validate_start_options(
                backend,
                &model,
                &effort,
                &continuation,
                &agent,
                &variant,
                &workspace_requirement,
                cloud,
                &title,
                &devin_mode,
                &swe_tier,
                &repos,
                max_acu_limit,
            )?;
            let mut args = json!({"operation_id": operation_id, "task": task});
            let object = args.as_object_mut().expect("object literal");
            for (key, value) in [
                ("model", model.map(Value::String)),
                ("effort", effort.map(Value::String)),
                ("continuation", continuation.map(|value| json!(value))),
                ("agent", agent.map(Value::String)),
                ("variant", variant.map(Value::String)),
                (
                    "workspace_requirement",
                    workspace_requirement.map(Value::String),
                ),
                ("title", title.map(Value::String)),
                ("devin_mode", devin_mode.map(Value::String)),
                ("swe_tier", swe_tier.map(Value::String)),
                ("max_acu_limit", max_acu_limit.map(|n| json!(n))),
            ] {
                if let Some(value) = value {
                    object.insert(key.to_owned(), value);
                }
            }
            if cloud {
                object.insert("cloud".to_owned(), json!(true));
            }
            if !repos.is_empty() {
                object.insert("repos".to_owned(), json!(repos));
            }
            orchestration::invoke(
                backend.into(),
                orchestration::Operation::TaskStart,
                &args,
                &session,
                &actor,
                None,
            )
            .await
        }
        TaskAction::List { limit } => {
            let args = match limit {
                Some(limit) => json!({"limit": limit}),
                None => json!({}),
            };
            orchestration::task_list(&args, &session).await
        }
        TaskAction::Get {
            backend,
            task_id,
            after_revision,
            wait_ms,
        } => {
            let mut args = json!({"task_id": task_id, "wait_ms": wait_ms});
            if let Some(revision) = after_revision {
                args["after_revision"] = json!(revision);
            }
            orchestration::invoke(
                backend.into(),
                orchestration::Operation::TaskGet,
                &args,
                &session,
                &actor,
                None,
            )
            .await
        }
        TaskAction::Control {
            backend,
            task_id,
            operation_id,
            action,
            input,
        } => {
            let mut args = json!({"task_id": task_id, "operation_id": operation_id, "action": action.as_str()});
            match (action, input) {
                (ControlAction::Steer, Some(input)) => args["input"] = json!(input),
                (ControlAction::Steer, None) => bail!("steer requires input"),
                (_, Some(_)) => bail!("{} does not accept input", action.as_str()),
                (_, None) => {}
            }
            orchestration::invoke(
                backend.into(),
                orchestration::Operation::TaskControl,
                &args,
                &session,
                &actor,
                None,
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instance() -> SessionInstance {
        SessionInstance {
            cwd: PathBuf::from("/tmp/session"),
            started_at: 1,
            process_id: 2,
            permission_mode: config::PermissionMode::Agent,
            permitted_directories: vec![PathBuf::from("/tmp/session")],
            grants: config::SessionGrants::default(),
        }
    }

    #[test]
    fn a_packet_cannot_inherit_a_changed_session_scope() -> noprop::TestResult {
        crate::test_support::run(0x4c4f_4341_4c46_454e, 256, |ctx| {
            let changes = noprop::sample_usize_in(ctx, 0..=255);
            let original = instance();
            let mut session = config::Session {
                id: "s".to_owned(),
                cwd: original.cwd.clone(),
                started_at: original.started_at,
                process_id: original.process_id,
                permission_mode: original.permission_mode,
                permitted_directories: original.permitted_directories.clone(),
                grants: original.grants.clone(),
            };
            if changes & 1 != 0 {
                session.cwd = PathBuf::from("/tmp/replacement");
            }
            if changes & 2 != 0 {
                session.started_at += 1;
            }
            if changes & 4 != 0 {
                session.process_id += 1;
            }
            if changes & 8 != 0 {
                session.permission_mode = config::PermissionMode::Ask;
            }
            if changes & 16 != 0 {
                session
                    .permitted_directories
                    .push(PathBuf::from("/tmp/extra"));
            }
            if changes & 32 != 0 {
                session.grants.listen_ports.push(4211);
            }
            if changes & 64 != 0 {
                session
                    .grants
                    .dev_tool_env_prefixes
                    .push("TEST_".to_owned());
            }
            if changes & 128 != 0 {
                session.grants.ambient_git_credentials = true;
            }
            assert_eq!(original.matches(&session), changes == 0);
            Ok(())
        })
    }

    #[test]
    fn typed_packet_rejects_unknown_fields_and_versions() {
        let packet = json!({"schema_version": 1, "session_id": "s", "instance": instance(), "action": {"operation": "control", "backend": "codex", "task_id": Uuid::nil(), "operation_id": Uuid::nil(), "action": "interrupt", "argv": ["sh"]}});
        assert!(serde_json::from_value::<TaskPacket>(packet).is_err());
        let packet = TaskPacket {
            schema_version: PROTOCOL_VERSION,
            session_id: "s".to_owned(),
            instance: instance(),
            action: TaskAction::List { limit: None },
        };
        assert!(packet.validate().is_ok());
        assert!(
            TaskPacket {
                schema_version: 2,
                ..packet
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn local_packet_roundtrips_task_identity_and_control() {
        let operation_id = Uuid::parse_str("0199aaaa-aaaa-7aaa-8aaa-aaaaaaaaaaaa").unwrap();
        let task_id = Uuid::parse_str("0199bbbb-bbbb-7bbb-8bbb-bbbbbbbbbbbb").unwrap();
        let start = TaskPacket {
            schema_version: PROTOCOL_VERSION,
            session_id: "dogfood-20261005".to_owned(),
            instance: instance(),
            action: TaskAction::Start {
                backend: BackendName::Codex,
                operation_id,
                task: "inspect the repository".to_owned(),
                model: Some("gpt-6".to_owned()),
                effort: Some("medium".to_owned()),
                continuation: None,
                agent: None,
                variant: None,
                workspace_requirement: None,
                cloud: false,
                title: None,
                devin_mode: None,
                swe_tier: None,
                repos: Vec::new(),
                max_acu_limit: None,
            },
        };
        let decoded: TaskPacket =
            serde_json::from_slice(&serde_json::to_vec(&start).unwrap()).unwrap();
        let TaskAction::Start {
            operation_id: decoded_id,
            ..
        } = decoded.action
        else {
            panic!("start packet")
        };
        assert_eq!(decoded_id, operation_id);
        let mut continued = serde_json::to_value(&start).unwrap();
        continued["action"]["continuation"] = json!({"type":"previous_task","task_id":task_id});
        let decoded: TaskPacket = serde_json::from_value(continued).unwrap();
        let TaskAction::Start { continuation, .. } = decoded.action else {
            panic!("start packet")
        };
        assert_eq!(
            continuation,
            Some(crate::codex_app_server::CodexContinuation::PreviousTask { task_id })
        );
        let control = TaskPacket {
            schema_version: PROTOCOL_VERSION,
            session_id: "dogfood-20261005".to_owned(),
            instance: instance(),
            action: TaskAction::Control {
                backend: BackendName::DevinAcp,
                task_id,
                operation_id,
                action: ControlAction::Interrupt,
                input: None,
            },
        };
        let encoded = serde_json::to_value(&control).unwrap();
        assert_eq!(encoded["action"]["backend"], "devin_acp");
        assert_eq!(encoded["action"]["task_id"], task_id.to_string());
        assert!(serde_json::from_value::<TaskPacket>(encoded).is_ok());
    }

    #[test]
    fn local_change_ready_packet_carries_operation_ids_not_caller_task_id() {
        let packet = json!({
            "schema_version": PROTOCOL_VERSION,
            "session_id": "owner",
            "instance": instance(),
            "action": {
                "operation": "change",
                "owner_provisioning_operation_id": Uuid::new_v4(),
                "command": {
                    "action": "create_ready",
                    "operation_id": Uuid::new_v4(),
                    "allocation_operation_id": Uuid::new_v4(),
                    "task_operation_id": Uuid::new_v4(),
                    "parent_task_id": null,
                    "parent_change_id": null,
                    "base": {"kind":"origin_main"}
                }
            }
        });
        assert!(serde_json::from_value::<TaskPacket>(packet.clone()).is_ok());
        let mut forged = packet;
        forged["action"]["command"]["task_id"] = json!(Uuid::new_v4());
        assert!(serde_json::from_value::<TaskPacket>(forged).is_err());
    }

    #[cfg(feature = "network")]
    #[test]
    fn managed_commands_selector_is_only_for_opencode() {
        let selected = Some("managed_commands".to_owned());
        let empty = None;
        assert!(
            validate_start_options(
                BackendName::Opencode,
                &empty,
                &empty,
                &None,
                &empty,
                &empty,
                &selected,
                false,
                &empty,
                &None,
                &empty,
                &[],
                None,
            )
            .is_ok()
        );
        assert!(
            validate_start_options(
                BackendName::Codex,
                &empty,
                &empty,
                &None,
                &empty,
                &empty,
                &selected,
                false,
                &empty,
                &None,
                &empty,
                &[],
                None,
            )
            .is_err()
        );
    }
}
