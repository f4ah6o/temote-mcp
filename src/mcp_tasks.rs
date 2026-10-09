//! Private MCP task relay. Only shared task contracts cross this boundary;
//! the managed supervisor owns all backend runtimes, regardless of frontend.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::local_tasks::SessionInstance;
use crate::orchestration::{self, Backend, Operation, TaskRequest};
use crate::{config, evidence, observation};
use uuid::Uuid;

pub(crate) const PROTOCOL_VERSION: u64 = 1;
// A 1 MiB task/input can expand sixfold under JSON escaping. Scope and
// selectors have separate bounded overhead; ordinary control stays at 64 KiB.
pub(crate) const MAX_REQUEST_BYTES: usize = 6 * 1024 * 1024 + 128 * 1024;
pub(crate) const MAX_RESPONSE_BYTES: usize = 52 * 1024 * 1024;
pub(crate) const FRAME_PREFIX: &[u8] = b"mcp_task_relay_v1\n";
pub(crate) const MAX_EVIDENCE_REQUEST_BYTES: usize = 128 * 1024;
pub(crate) const MAX_EVIDENCE_RESPONSE_BYTES: usize = 6 * evidence::MAX_READ_BYTES + 4096;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskPacket {
    pub schema_version: u64,
    pub session_id: String,
    pub instance: SessionInstance,
    pub backend: Backend,
    pub operation: Operation,
    pub actor: observation::ActorRef,
    pub args: Value,
}

impl TaskPacket {
    pub(crate) fn new(
        backend: Backend,
        operation: Operation,
        args: &Value,
        session: &config::Session,
        public: bool,
    ) -> Result<Self> {
        let packet = Self {
            schema_version: PROTOCOL_VERSION,
            session_id: session.id.clone(),
            instance: session.into(),
            backend,
            operation,
            actor: observation::ActorRef::mcp(public),
            args: args.clone(),
        };
        packet.validate()?;
        Ok(packet)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        validate_authority(
            self.schema_version,
            &self.session_id,
            &self.instance,
            &self.actor,
        )?;
        let args = self
            .args
            .as_object()
            .context("MCP task arguments must be an object")?;
        if let Some(id) = args.get("session_id") {
            ensure!(
                id.as_str() == Some(self.session_id.as_str()),
                "MCP task session_id mismatch"
            );
        }
        // The shared parser validates values/capabilities. This allowlist also
        // rejects unused host primitives rather than carrying them to a backend.
        ensure!(
            args.keys().all(|key| self.accepts_key(key)),
            "unsupported MCP task argument"
        );
        TaskRequest::parse(self.backend, self.operation, &self.args)?;
        ensure!(
            serde_json::to_vec(self)?.len() < MAX_REQUEST_BYTES,
            "MCP task relay request too large"
        );
        Ok(())
    }

    fn accepts_key(&self, key: &str) -> bool {
        if key == "session_id" {
            return true;
        }
        match self.operation {
            Operation::Status => false,
            Operation::TaskList => key == "limit",
            Operation::TaskGet => matches!(key, "task_id" | "after_revision" | "wait_ms"),
            Operation::TaskControl => matches!(
                key,
                "task_id" | "operation_id" | "action" | "input" | "interaction_id" | "answer"
            ),
            Operation::TaskStart if matches!(key, "task" | "operation_id") => true,
            Operation::TaskStart => match self.backend {
                Backend::Codex => matches!(key, "model" | "effort" | "continuation"),
                #[cfg(feature = "network")]
                Backend::OpenCode => {
                    matches!(key, "model" | "agent" | "variant" | "workspace_requirement")
                }
                Backend::DevinAcp => matches!(key, "model" | "agent" | "cloud"),
                #[cfg(feature = "network")]
                Backend::DevinCloud => matches!(
                    key,
                    "title" | "devin_mode" | "swe_tier" | "repos" | "max_acu_limit"
                ),
            },
        }
    }

    pub(crate) fn validate_session(&self, session: &config::Session) -> Result<()> {
        validate_session_authority(&self.session_id, &self.instance, &self.actor, session)
    }
}

fn validate_authority(
    schema_version: u64,
    session_id: &str,
    instance: &SessionInstance,
    actor: &observation::ActorRef,
) -> Result<()> {
    ensure!(
        schema_version == PROTOCOL_VERSION,
        "unsupported MCP task relay protocol"
    );
    config::validate_session_id(session_id)?;
    ensure!(
        instance.started_at != 0
            && instance.process_id != 0
            && serde_json::to_vec(instance)?.len() <= 64 * 1024,
        "invalid MCP task session instance"
    );
    ensure!(
        matches!(actor.transport.as_str(), "mcp-public" | "mcp-stdio") && actor.principal.is_none(),
        "invalid MCP task actor"
    );
    ensure!(
        actor.transport != "mcp-public" || !instance.permission_mode.is_yolo(),
        "yolo sessions are unavailable on the public MCP endpoint"
    );
    Ok(())
}

fn validate_session_authority(
    session_id: &str,
    instance: &SessionInstance,
    actor: &observation::ActorRef,
    session: &config::Session,
) -> Result<()> {
    ensure!(
        session_id == session.id && instance.matches(session),
        "session instance changed; reconcile retained tasks before another operation"
    );
    ensure!(
        actor.transport != "mcp-public" || !session.yolo(),
        "yolo sessions are unavailable on the public MCP endpoint"
    );
    Ok(())
}

/// Explicit evidence reads are the only relay operation that carries a
/// bounded chunk of child output. An opaque ID cannot address host paths.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EvidencePacket {
    pub schema_version: u64,
    pub session_id: String,
    pub instance: SessionInstance,
    pub actor: observation::ActorRef,
    pub evidence_id: Uuid,
    pub offset_bytes: usize,
    pub max_bytes: usize,
}

impl EvidencePacket {
    pub(crate) fn new(args: &Value, session: &config::Session, public: bool) -> Result<Self> {
        let object = args
            .as_object()
            .context("evidence_read arguments must be an object")?;
        ensure!(
            object.keys().all(|key| matches!(
                key.as_str(),
                "session_id" | "evidence_id" | "offset_bytes" | "max_bytes"
            )),
            "unsupported evidence_read argument"
        );
        if let Some(id) = object.get("session_id") {
            ensure!(
                id.as_str() == Some(session.id.as_str()),
                "evidence_read session_id mismatch"
            );
        }
        let evidence_id = args
            .get("evidence_id")
            .and_then(Value::as_str)
            .context("missing evidence_id")
            .and_then(|id| Uuid::parse_str(id).context("invalid evidence_id"))?;
        let integer = |key: &str, default| -> Result<usize> {
            args.get(key)
                .map(|value| {
                    let number = value
                        .as_u64()
                        .with_context(|| format!("{key} must be a non-negative integer"))?;
                    usize::try_from(number).with_context(|| format!("{key} is too large"))
                })
                .transpose()
                .map(|value| value.unwrap_or(default))
        };
        let packet = Self {
            schema_version: PROTOCOL_VERSION,
            session_id: session.id.clone(),
            instance: session.into(),
            actor: observation::ActorRef::mcp(public),
            evidence_id,
            offset_bytes: integer("offset_bytes", 0)?,
            max_bytes: integer("max_bytes", evidence::DEFAULT_READ_BYTES)?,
        };
        packet.validate()?;
        Ok(packet)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        validate_authority(
            self.schema_version,
            &self.session_id,
            &self.instance,
            &self.actor,
        )?;
        ensure!(
            (1..=evidence::MAX_READ_BYTES).contains(&self.max_bytes),
            "evidence max_bytes must be 1..={}",
            evidence::MAX_READ_BYTES
        );
        ensure!(
            serde_json::to_vec(self)?.len() < MAX_EVIDENCE_REQUEST_BYTES,
            "MCP evidence relay request too large"
        );
        Ok(())
    }

    pub(crate) fn read_for_session(&self, session: &config::Session) -> Result<Value> {
        self.validate()?;
        validate_session_authority(&self.session_id, &self.instance, &self.actor, session)?;
        Ok(serde_json::to_value(evidence::read_for_session(
            session,
            self.evidence_id,
            self.offset_bytes,
            self.max_bytes,
        )?)?)
    }
}

pub(crate) async fn dispatch_evidence(packet: EvidencePacket) -> Result<Value> {
    packet.validate()?;
    let session = config::load_session(&packet.session_id).await?;
    packet.read_for_session(&session)
}

pub(crate) async fn dispatch(packet: TaskPacket) -> Result<Value> {
    packet.validate()?;
    let session = config::load_session(&packet.session_id).await?;
    packet.validate_session(&session)?;
    if packet.operation == Operation::TaskList {
        return orchestration::task_list(&packet.args, &session).await;
    }
    // Frontend owns tool activity and HTTP audit. Only this invocation records
    // the normalized instruction/outcome, using the original transport actor.
    orchestration::invoke(
        packet.backend,
        packet.operation,
        &packet.args,
        &session,
        &packet.actor,
        None,
    )
    .await
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;
    use uuid::Uuid;

    pub(crate) fn session() -> config::Session {
        config::Session {
            id: "relay-test".to_owned(),
            cwd: PathBuf::from("/tmp/relay-test"),
            started_at: 1,
            process_id: 2,
            permission_mode: config::PermissionMode::Agent,
            permitted_directories: vec![PathBuf::from("/tmp/relay-test")],
            grants: config::SessionGrants::default(),
        }
    }

    pub(crate) fn start_packet(task: String) -> TaskPacket {
        TaskPacket::new(
            Backend::Codex,
            Operation::TaskStart,
            &json!({
                "task": task, "model": "gpt-6", "effort": "high", "operation_id": Uuid::new_v4(),
                "continuation": {"type": "previous_task", "task_id": Uuid::new_v4()}
            }),
            &session(),
            true,
        )
        .unwrap()
    }

    #[test]
    fn mcp_relay_preserves_continuation_actor_and_one_mib_contract() {
        for text in ["a", "\u{1}", "\"", "\\", "é"] {
            let task = text.repeat(1024 * 1024 / text.len());
            let packet = start_packet(task.clone());
            let wire = serde_json::to_vec(&packet).unwrap();
            let decoded: TaskPacket = serde_json::from_slice(&wire).unwrap();
            decoded.validate().unwrap();
            assert_eq!(decoded.args["task"], task);
            assert_eq!(decoded.args["continuation"], packet.args["continuation"]);
            assert_eq!(decoded.actor.transport, "mcp-public");
            let mut oversized = decoded;
            oversized.args["task"] = json!("a".repeat(1024 * 1024 + 1));
            assert!(oversized.validate().is_err());
        }
        let mut control = start_packet("first".to_owned());
        control.operation = Operation::TaskControl;
        control.args = json!({"task_id": Uuid::new_v4(), "operation_id": Uuid::new_v4(), "action": "steer", "input": "\u{1}".repeat(1024 * 1024)});
        control.validate().unwrap();
        control.args["input"] = json!("a".repeat(1024 * 1024 + 1));
        assert!(control.validate().is_err());
    }

    #[test]
    fn mcp_relay_rejects_primitives_invalid_capabilities_and_public_yolo() {
        let packet = start_packet("inspect".to_owned());
        for key in [
            "argv",
            "env",
            "executable",
            "network_policy",
            "cwd",
            "agent",
        ] {
            let mut value = serde_json::to_value(&packet).unwrap();
            value["args"][key] = json!("unused-host-input");
            assert!(
                serde_json::from_value::<TaskPacket>(value)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
        let mut packet = packet;
        packet.operation = Operation::TaskControl;
        packet.args = json!({"operation_id": Uuid::new_v4(), "task_id": Uuid::new_v4(), "action": "answer", "interaction_id": Uuid::new_v4(), "answer": {}});
        assert!(packet.validate().is_err()); // Codex has no answer capability.
        packet.operation = Operation::Status;
        packet.args = json!({});
        packet.instance.permission_mode = config::PermissionMode::Yolo;
        assert!(packet.validate().is_err());
        packet.actor = observation::ActorRef::mcp(false);
        packet.validate().unwrap();
        packet.actor.transport = "local-task".to_owned();
        assert!(packet.validate().is_err());
    }

    #[test]
    fn mcp_relay_evidence_validates_typed_id_bounds_and_public_scope() {
        let args = json!({"evidence_id": Uuid::new_v4()});
        let owner = session();
        let packet = EvidencePacket::new(&args, &owner, true).unwrap();
        assert_eq!(packet.max_bytes, evidence::DEFAULT_READ_BYTES);
        assert_eq!(packet.offset_bytes, 0);
        for max_bytes in [0, evidence::MAX_READ_BYTES + 1] {
            let mut args = args.clone();
            args["max_bytes"] = json!(max_bytes);
            assert!(EvidencePacket::new(&args, &owner, true).is_err());
        }
        for key in ["path", "executable", "argv", "env"] {
            let mut args = args.clone();
            args[key] = json!("forbidden");
            assert!(EvidencePacket::new(&args, &owner, true).is_err());
        }
        let mut yolo = owner;
        yolo.permission_mode = config::PermissionMode::Yolo;
        assert!(EvidencePacket::new(&args, &yolo, true).is_err());
        EvidencePacket::new(&args, &yolo, false).unwrap();
    }

    #[test]
    fn mcp_relay_fences_every_session_instance_field() -> noprop::TestResult {
        crate::test_support::run(0x4d43_5052_454c_4159, 256, |ctx| {
            let changes = noprop::sample_usize_in(ctx, 0..=255);
            let packet = start_packet("inspect".to_owned());
            let mut current = session();
            if changes & 1 != 0 {
                current.id.push('x');
            }
            if changes & 2 != 0 {
                current.cwd.push("replacement");
            }
            if changes & 4 != 0 {
                current.started_at += 1;
            }
            if changes & 8 != 0 {
                current.process_id += 1;
            }
            if changes & 16 != 0 {
                current.permission_mode = config::PermissionMode::Ask;
            }
            if changes & 32 != 0 {
                current
                    .permitted_directories
                    .push(PathBuf::from("/tmp/other"));
            }
            if changes & 64 != 0 {
                current.grants.listen_ports.push(4211);
            }
            if changes & 128 != 0 {
                current.grants.ambient_git_credentials = true;
            }
            assert_eq!(packet.validate_session(&current).is_ok(), changes == 0);
            Ok(())
        })
    }
}
