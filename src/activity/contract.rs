use std::fmt;

use serde::de::{Error as SerdeError, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

pub const ACTIVITY_SCHEMA_VERSION: u64 = 1;
pub const MAX_ACTIVITY_SUMMARY_BYTES: usize = 512;
pub const MAX_ACTIVITY_EVENT_BYTES: usize = 2048;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContractError {
    UnknownSchema,
    InvalidValue,
    InvalidSummary,
    TooLarge,
    InvalidJson,
}

impl fmt::Display for ContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::UnknownSchema => "unknown_schema",
            Self::InvalidValue => "invalid_value",
            Self::InvalidSummary => "invalid_summary",
            Self::TooLarge => "too_large",
            Self::InvalidJson => "invalid_json",
        };
        formatter.write_str(name)
    }
}

impl std::error::Error for ContractError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityOperation {
    SessionStart,
    SessionStop,
    ReadFile,
    WriteFile,
    GitPull,
}

impl<'de> Deserialize<'de> for ActivityOperation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = deserialize_wire_string(deserializer)?;
        match value.as_str() {
            "session_start" => Ok(Self::SessionStart),
            "session_stop" => Ok(Self::SessionStop),
            "read_file" => Ok(Self::ReadFile),
            "write_file" => Ok(Self::WriteFile),
            "git_pull" => Ok(Self::GitPull),
            _ => Err(serde_invalid_json()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityState {
    Started,
    WaitingApproval,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl<'de> Deserialize<'de> for ActivityState {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = deserialize_wire_string(deserializer)?;
        match value.as_str() {
            "started" => Ok(Self::Started),
            "waiting_approval" => Ok(Self::WaitingApproval),
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(serde_invalid_json()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityErrorKind {
    InvalidInput,
    SandboxDenied,
    ApprovalDenied,
    RuntimeUnavailable,
    ChildFailed,
    ProtocolFailed,
    OperationFailed,
}

impl ActivityErrorKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid_input",
            Self::SandboxDenied => "sandbox_denied",
            Self::ApprovalDenied => "approval_denied",
            Self::RuntimeUnavailable => "runtime_unavailable",
            Self::ChildFailed => "child_failed",
            Self::ProtocolFailed => "protocol_failed",
            Self::OperationFailed => "operation_failed",
        }
    }

    fn from_wire_name(value: &str) -> Option<Self> {
        match value {
            "invalid_input" => Some(Self::InvalidInput),
            "sandbox_denied" => Some(Self::SandboxDenied),
            "approval_denied" => Some(Self::ApprovalDenied),
            "runtime_unavailable" => Some(Self::RuntimeUnavailable),
            "child_failed" => Some(Self::ChildFailed),
            "protocol_failed" => Some(Self::ProtocolFailed),
            "operation_failed" => Some(Self::OperationFailed),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for ActivityErrorKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = deserialize_wire_string(deserializer)?;
        Self::from_wire_name(&value).ok_or_else(serde_invalid_json)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityRemote {
    Origin,
    Other,
}

impl<'de> Deserialize<'de> for ActivityRemote {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = deserialize_wire_string(deserializer)?;
        match value.as_str() {
            "origin" => Ok(Self::Origin),
            "other" => Ok(Self::Other),
            _ => Err(serde_invalid_json()),
        }
    }
}

fn deserialize_wire_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    String::deserialize(deserializer).map_err(|_| serde_invalid_json())
}

fn serde_invalid_json<E>() -> E
where
    E: SerdeError,
{
    E::custom("invalid_json")
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ActivitySummary {
    Empty,
    Failure {
        #[serde(rename = "error")]
        kind: ActivityErrorKind,
    },
    Git {
        remote: ActivityRemote,
    },
}

impl<'de> Deserialize<'de> for ActivitySummary {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(ActivitySummaryVisitor)
    }
}

struct ActivitySummaryVisitor;

impl<'de> Visitor<'de> for ActivitySummaryVisitor {
    type Value = ActivitySummary;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an activity summary object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut kind = None;
        let mut error = None;
        let mut remote = None;

        while let Some(field) = map.next_key::<String>().map_err(|_| serde_invalid_json())? {
            match field.as_str() {
                "kind" => {
                    if kind.is_some() {
                        return Err(serde_invalid_json());
                    }
                    kind = Some(
                        map.next_value::<String>()
                            .map_err(|_| serde_invalid_json())?,
                    );
                }
                "error" => {
                    if error.is_some() {
                        return Err(serde_invalid_json());
                    }
                    error = Some(
                        map.next_value::<String>()
                            .map_err(|_| serde_invalid_json())?,
                    );
                }
                "remote" => {
                    if remote.is_some() {
                        return Err(serde_invalid_json());
                    }
                    remote = Some(
                        map.next_value::<String>()
                            .map_err(|_| serde_invalid_json())?,
                    );
                }
                _ => return Err(serde_invalid_json()),
            }
        }

        let kind = kind.ok_or_else(serde_invalid_json)?;
        match kind.as_str() {
            "empty" if error.is_none() && remote.is_none() => Ok(Self::Value::Empty),
            "failure" if error.is_some() && remote.is_none() => {
                let error = error.ok_or_else(serde_invalid_json)?;
                let kind =
                    ActivityErrorKind::from_wire_name(&error).ok_or_else(serde_invalid_json)?;
                Ok(Self::Value::Failure { kind })
            }
            "git" if error.is_none() && remote.is_some() => {
                let remote = remote.ok_or_else(serde_invalid_json)?;
                let remote = match remote.as_str() {
                    "origin" => ActivityRemote::Origin,
                    "other" => ActivityRemote::Other,
                    _ => return Err(serde_invalid_json()),
                };
                Ok(Self::Value::Git { remote })
            }
            _ => Err(serde_invalid_json()),
        }
    }
}

impl ActivitySummary {
    pub const fn empty() -> Self {
        Self::Empty
    }

    pub const fn failure(kind: ActivityErrorKind) -> Self {
        Self::Failure { kind }
    }

    pub const fn git(remote: ActivityRemote) -> Self {
        Self::Git { remote }
    }

    pub fn safe_summary(&self) -> String {
        self.as_safe_summary().to_owned()
    }

    pub fn as_safe_summary(&self) -> &'static str {
        match self {
            Self::Empty => "",
            Self::Failure { kind } => match kind {
                ActivityErrorKind::InvalidInput => "error=invalid_input",
                ActivityErrorKind::SandboxDenied => "error=sandbox_denied",
                ActivityErrorKind::ApprovalDenied => "error=approval_denied",
                ActivityErrorKind::RuntimeUnavailable => "error=runtime_unavailable",
                ActivityErrorKind::ChildFailed => "error=child_failed",
                ActivityErrorKind::ProtocolFailed => "error=protocol_failed",
                ActivityErrorKind::OperationFailed => "error=operation_failed",
            },
            Self::Git {
                remote: ActivityRemote::Origin,
            } => "remote=origin",
            Self::Git {
                remote: ActivityRemote::Other,
            } => "remote=other",
        }
    }
}

impl fmt::Display for ActivitySummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_safe_summary())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivityUpdate {
    schema_version: u64,
    operation_id: Uuid,
    operation: ActivityOperation,
    state: ActivityState,
    duration_ms: Option<u64>,
    summary: ActivitySummary,
}

impl ActivityUpdate {
    pub fn new(
        operation_id: Uuid,
        operation: ActivityOperation,
        state: ActivityState,
        duration_ms: Option<u64>,
        summary: ActivitySummary,
    ) -> Result<Self, ContractError> {
        Self::with_schema_version(
            ACTIVITY_SCHEMA_VERSION,
            operation_id,
            operation,
            state,
            duration_ms,
            summary,
        )
    }

    pub fn with_schema_version(
        schema_version: u64,
        operation_id: Uuid,
        operation: ActivityOperation,
        state: ActivityState,
        duration_ms: Option<u64>,
        summary: ActivitySummary,
    ) -> Result<Self, ContractError> {
        let update = Self {
            schema_version,
            operation_id,
            operation,
            state,
            duration_ms,
            summary,
        };
        update.validate()?;
        Ok(update)
    }

    pub fn validate(&self) -> Result<(), ContractError> {
        validate_update(self)
    }

    pub const fn schema_version(&self) -> u64 {
        self.schema_version
    }

    pub const fn operation_id(&self) -> Uuid {
        self.operation_id
    }

    pub const fn operation(&self) -> ActivityOperation {
        self.operation
    }

    pub const fn state(&self) -> ActivityState {
        self.state
    }

    pub const fn duration_ms(&self) -> Option<u64> {
        self.duration_ms
    }

    pub const fn summary(&self) -> &ActivitySummary {
        &self.summary
    }
}

impl From<&ActivityUpdate> for ActivityUpdate {
    fn from(update: &ActivityUpdate) -> Self {
        update.clone()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventStamp {
    sequence: u64,
    timestamp_ms: u64,
    session_id: Option<String>,
    session_instance: Option<Uuid>,
}

impl EventStamp {
    pub fn new(
        sequence: u64,
        timestamp_ms: u64,
        session_id: Option<String>,
        session_instance: Option<Uuid>,
    ) -> Result<Self, ContractError> {
        let stamp = Self {
            sequence,
            timestamp_ms,
            session_id,
            session_instance,
        };
        stamp.validate()?;
        Ok(stamp)
    }

    pub fn validate(&self) -> Result<(), ContractError> {
        validate_stamp(self)
    }

    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    pub const fn timestamp_ms(&self) -> u64 {
        self.timestamp_ms
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub const fn session_instance(&self) -> Option<Uuid> {
        self.session_instance
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivityEvent {
    schema_version: u64,
    sequence: u64,
    operation_id: Uuid,
    timestamp_ms: u64,
    session_id: Option<String>,
    session_instance: Option<Uuid>,
    operation: ActivityOperation,
    state: ActivityState,
    duration_ms: Option<u64>,
    safe_summary: String,
}

impl ActivityEvent {
    pub fn from_update(
        update: impl Into<ActivityUpdate>,
        stamp: EventStamp,
    ) -> Result<Self, ContractError> {
        let update = update.into();
        update.validate()?;
        stamp.validate()?;

        let event = Self {
            schema_version: update.schema_version,
            sequence: stamp.sequence,
            operation_id: update.operation_id,
            timestamp_ms: stamp.timestamp_ms,
            session_id: stamp.session_id,
            session_instance: stamp.session_instance,
            operation: update.operation,
            state: update.state,
            duration_ms: update.duration_ms,
            safe_summary: update.summary.safe_summary(),
        };
        event.validate()?;
        Ok(event)
    }

    pub fn validate(&self) -> Result<(), ContractError> {
        validate_event(self)
    }

    pub const fn schema_version(&self) -> u64 {
        self.schema_version
    }

    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    pub const fn operation_id(&self) -> Uuid {
        self.operation_id
    }

    pub const fn timestamp_ms(&self) -> u64 {
        self.timestamp_ms
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub const fn session_instance(&self) -> Option<Uuid> {
        self.session_instance
    }

    pub const fn operation(&self) -> ActivityOperation {
        self.operation
    }

    pub const fn state(&self) -> ActivityState {
        self.state
    }

    pub const fn duration_ms(&self) -> Option<u64> {
        self.duration_ms
    }

    pub fn safe_summary(&self) -> &str {
        &self.safe_summary
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct ActivityUpdateWire {
    schema_version: u64,
    operation_id: Uuid,
    operation: ActivityOperation,
    state: ActivityState,
    duration_ms: Option<u64>,
    summary: ActivitySummary,
}

struct ActivityUpdateWireVisitor;

impl<'de> Visitor<'de> for ActivityUpdateWireVisitor {
    type Value = ActivityUpdateWire;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an activity update object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut schema_version = None;
        let mut operation_id = None;
        let mut operation = None;
        let mut state = None;
        let mut duration_ms = None;
        let mut duration_seen = false;
        let mut summary = None;

        while let Some(field) = map.next_key::<String>().map_err(|_| serde_invalid_json())? {
            match field.as_str() {
                "schema_version" => {
                    if schema_version.is_some() {
                        return Err(serde_invalid_json());
                    }
                    schema_version =
                        Some(map.next_value::<u64>().map_err(|_| serde_invalid_json())?);
                }
                "operation_id" => {
                    if operation_id.is_some() {
                        return Err(serde_invalid_json());
                    }
                    operation_id =
                        Some(map.next_value::<Uuid>().map_err(|_| serde_invalid_json())?);
                }
                "operation" => {
                    if operation.is_some() {
                        return Err(serde_invalid_json());
                    }
                    operation = Some(
                        map.next_value::<ActivityOperation>()
                            .map_err(|_| serde_invalid_json())?,
                    );
                }
                "state" => {
                    if state.is_some() {
                        return Err(serde_invalid_json());
                    }
                    state = Some(
                        map.next_value::<ActivityState>()
                            .map_err(|_| serde_invalid_json())?,
                    );
                }
                "duration_ms" => {
                    if duration_seen {
                        return Err(serde_invalid_json());
                    }
                    duration_seen = true;
                    duration_ms = map
                        .next_value::<Option<u64>>()
                        .map_err(|_| serde_invalid_json())?;
                }
                "summary" => {
                    if summary.is_some() {
                        return Err(serde_invalid_json());
                    }
                    summary = Some(
                        map.next_value::<ActivitySummary>()
                            .map_err(|_| serde_invalid_json())?,
                    );
                }
                _ => return Err(serde_invalid_json()),
            }
        }

        let duration_ms = if duration_seen {
            duration_ms
        } else {
            return Err(serde_invalid_json());
        };

        Ok(ActivityUpdateWire {
            schema_version: schema_version.ok_or_else(serde_invalid_json)?,
            operation_id: operation_id.ok_or_else(serde_invalid_json)?,
            operation: operation.ok_or_else(serde_invalid_json)?,
            state: state.ok_or_else(serde_invalid_json)?,
            duration_ms,
            summary: summary.ok_or_else(serde_invalid_json)?,
        })
    }
}

impl<'de> Deserialize<'de> for ActivityUpdateWire {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(ActivityUpdateWireVisitor)
    }
}

impl From<ActivityUpdateWire> for ActivityUpdate {
    fn from(wire: ActivityUpdateWire) -> Self {
        Self {
            schema_version: wire.schema_version,
            operation_id: wire.operation_id,
            operation: wire.operation,
            state: wire.state,
            duration_ms: wire.duration_ms,
            summary: wire.summary,
        }
    }
}

impl From<&ActivityUpdate> for ActivityUpdateWire {
    fn from(update: &ActivityUpdate) -> Self {
        Self {
            schema_version: update.schema_version,
            operation_id: update.operation_id,
            operation: update.operation,
            state: update.state,
            duration_ms: update.duration_ms,
            summary: update.summary.clone(),
        }
    }
}

impl Serialize for ActivityUpdate {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        ActivityUpdateWire::from(self).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ActivityUpdate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let update = Self::from(ActivityUpdateWire::deserialize(deserializer)?);
        update.validate().map_err(|_| serde_invalid_json())?;
        Ok(update)
    }
}

#[derive(Serialize)]
struct ActivityEventWire<'a> {
    schema_version: u64,
    sequence: u64,
    operation_id: Uuid,
    timestamp_ms: u64,
    session_id: Option<&'a str>,
    session_instance: Option<Uuid>,
    operation: ActivityOperation,
    state: ActivityState,
    duration_ms: Option<u64>,
    safe_summary: &'a str,
}

struct ActivityEventWireOwned {
    schema_version: u64,
    sequence: u64,
    operation_id: Uuid,
    timestamp_ms: u64,
    session_id: Option<String>,
    session_instance: Option<Uuid>,
    operation: ActivityOperation,
    state: ActivityState,
    duration_ms: Option<u64>,
    safe_summary: String,
}

struct ActivityEventWireVisitor;

impl<'de> Visitor<'de> for ActivityEventWireVisitor {
    type Value = ActivityEventWireOwned;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an activity event object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut schema_version = None;
        let mut sequence = None;
        let mut operation_id = None;
        let mut timestamp_ms = None;
        let mut session_id = None;
        let mut session_id_seen = false;
        let mut session_instance = None;
        let mut session_instance_seen = false;
        let mut operation = None;
        let mut state = None;
        let mut duration_ms = None;
        let mut duration_seen = false;
        let mut safe_summary = None;

        while let Some(field) = map.next_key::<String>().map_err(|_| serde_invalid_json())? {
            match field.as_str() {
                "schema_version" => set_once(&mut schema_version, map.next_value())?,
                "sequence" => set_once(&mut sequence, map.next_value())?,
                "operation_id" => set_once(&mut operation_id, map.next_value())?,
                "timestamp_ms" => set_once(&mut timestamp_ms, map.next_value())?,
                "session_id" => {
                    if session_id_seen {
                        return Err(serde_invalid_json());
                    }
                    session_id_seen = true;
                    session_id = map.next_value().map_err(|_| serde_invalid_json())?;
                }
                "session_instance" => {
                    if session_instance_seen {
                        return Err(serde_invalid_json());
                    }
                    session_instance_seen = true;
                    session_instance = map.next_value().map_err(|_| serde_invalid_json())?;
                }
                "operation" => set_once(&mut operation, map.next_value())?,
                "state" => set_once(&mut state, map.next_value())?,
                "duration_ms" => {
                    if duration_seen {
                        return Err(serde_invalid_json());
                    }
                    duration_seen = true;
                    duration_ms = map.next_value().map_err(|_| serde_invalid_json())?;
                }
                "safe_summary" => set_once(&mut safe_summary, map.next_value())?,
                _ => return Err(serde_invalid_json()),
            }
        }

        if !session_id_seen || !session_instance_seen || !duration_seen {
            return Err(serde_invalid_json());
        }
        Ok(ActivityEventWireOwned {
            schema_version: schema_version.ok_or_else(serde_invalid_json)?,
            sequence: sequence.ok_or_else(serde_invalid_json)?,
            operation_id: operation_id.ok_or_else(serde_invalid_json)?,
            timestamp_ms: timestamp_ms.ok_or_else(serde_invalid_json)?,
            session_id,
            session_instance,
            operation: operation.ok_or_else(serde_invalid_json)?,
            state: state.ok_or_else(serde_invalid_json)?,
            duration_ms,
            safe_summary: safe_summary.ok_or_else(serde_invalid_json)?,
        })
    }
}

fn set_once<T, E>(slot: &mut Option<T>, value: Result<T, E>) -> Result<(), E>
where
    E: SerdeError,
{
    if slot.is_some() {
        return Err(serde_invalid_json());
    }
    *slot = Some(value.map_err(|_| serde_invalid_json())?);
    Ok(())
}

impl<'de> Deserialize<'de> for ActivityEventWireOwned {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(ActivityEventWireVisitor)
    }
}

struct ActivityEnvelopeOwned {
    event: ActivityEventWireOwned,
}

struct ActivityEnvelopeVisitor;

impl<'de> Visitor<'de> for ActivityEnvelopeVisitor {
    type Value = ActivityEnvelopeOwned;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an activity envelope")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut event_type: Option<String> = None;
        let mut event = None;
        while let Some(field) = map.next_key::<String>().map_err(|_| serde_invalid_json())? {
            match field.as_str() {
                "type" => set_once(&mut event_type, map.next_value())?,
                "event" => set_once(&mut event, map.next_value())?,
                _ => return Err(serde_invalid_json()),
            }
        }
        if event_type.as_deref() != Some("activity") {
            return Err(serde_invalid_json());
        }
        Ok(ActivityEnvelopeOwned {
            event: event.ok_or_else(serde_invalid_json)?,
        })
    }
}

impl<'de> Deserialize<'de> for ActivityEnvelopeOwned {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(ActivityEnvelopeVisitor)
    }
}

impl<'a> From<&'a ActivityEvent> for ActivityEventWire<'a> {
    fn from(event: &'a ActivityEvent) -> Self {
        Self {
            schema_version: event.schema_version,
            sequence: event.sequence,
            operation_id: event.operation_id,
            timestamp_ms: event.timestamp_ms,
            session_id: event.session_id.as_deref(),
            session_instance: event.session_instance,
            operation: event.operation,
            state: event.state,
            duration_ms: event.duration_ms,
            safe_summary: &event.safe_summary,
        }
    }
}

#[derive(Serialize)]
struct ActivityEnvelope<'a> {
    #[serde(rename = "type")]
    event_type: &'static str,
    event: ActivityEventWire<'a>,
}

pub fn encode_update(update: &ActivityUpdate) -> Result<Vec<u8>, ContractError> {
    update.validate()?;
    let wire = ActivityUpdateWire::from(update);
    let bytes = serde_json::to_vec(&wire).map_err(|_| ContractError::InvalidJson)?;
    if bytes.len() > MAX_ACTIVITY_EVENT_BYTES {
        return Err(ContractError::TooLarge);
    }
    Ok(bytes)
}

pub fn decode_update(bytes: &[u8]) -> Result<ActivityUpdate, ContractError> {
    if bytes.len() > MAX_ACTIVITY_EVENT_BYTES {
        return Err(ContractError::TooLarge);
    }

    let wire: ActivityUpdateWire =
        serde_json::from_slice(bytes).map_err(|_| ContractError::InvalidJson)?;
    let update = ActivityUpdate::from(wire);
    update.validate()?;
    Ok(update)
}

pub fn encode_event(event: &ActivityEvent) -> Result<Vec<u8>, ContractError> {
    event.validate()?;
    let envelope = ActivityEnvelope {
        event_type: "activity",
        event: ActivityEventWire::from(event),
    };
    let bytes = serde_json::to_vec(&envelope).map_err(|_| ContractError::InvalidJson)?;
    if bytes.len() > MAX_ACTIVITY_EVENT_BYTES {
        return Err(ContractError::TooLarge);
    }
    Ok(bytes)
}

pub fn decode_event(bytes: &[u8]) -> Result<ActivityEvent, ContractError> {
    if bytes.len() > MAX_ACTIVITY_EVENT_BYTES {
        return Err(ContractError::TooLarge);
    }
    let wire: ActivityEnvelopeOwned =
        serde_json::from_slice(bytes).map_err(|_| ContractError::InvalidJson)?;
    let wire = wire.event;
    let event = ActivityEvent {
        schema_version: wire.schema_version,
        sequence: wire.sequence,
        operation_id: wire.operation_id,
        timestamp_ms: wire.timestamp_ms,
        session_id: wire.session_id,
        session_instance: wire.session_instance,
        operation: wire.operation,
        state: wire.state,
        duration_ms: wire.duration_ms,
        safe_summary: wire.safe_summary,
    };
    event.validate()?;
    Ok(event)
}

fn validate_update(update: &ActivityUpdate) -> Result<(), ContractError> {
    if update.schema_version != ACTIVITY_SCHEMA_VERSION {
        return Err(ContractError::UnknownSchema);
    }
    validate_duration(update.state, update.duration_ms)?;
    validate_safe_summary(update.summary.as_safe_summary())
}

fn validate_stamp(stamp: &EventStamp) -> Result<(), ContractError> {
    if stamp.sequence == 0 {
        return Err(ContractError::InvalidValue);
    }
    validate_session_identity(stamp.session_id.as_deref(), stamp.session_instance)
}

fn validate_event(event: &ActivityEvent) -> Result<(), ContractError> {
    if event.schema_version != ACTIVITY_SCHEMA_VERSION {
        return Err(ContractError::UnknownSchema);
    }
    if event.sequence == 0 {
        return Err(ContractError::InvalidValue);
    }
    validate_duration(event.state, event.duration_ms)?;
    validate_session_identity(event.session_id.as_deref(), event.session_instance)?;
    validate_safe_summary(&event.safe_summary)
}

fn validate_duration(state: ActivityState, duration_ms: Option<u64>) -> Result<(), ContractError> {
    let terminal = matches!(
        state,
        ActivityState::Completed | ActivityState::Failed | ActivityState::Cancelled
    );
    if terminal == duration_ms.is_none() {
        return Err(ContractError::InvalidValue);
    }
    Ok(())
}

fn validate_session_identity(
    session_id: Option<&str>,
    session_instance: Option<Uuid>,
) -> Result<(), ContractError> {
    let Some(session_id) = session_id else {
        return if session_instance.is_none() {
            Ok(())
        } else {
            Err(ContractError::InvalidValue)
        };
    };

    let length = session_id.len();
    if !(1..=64).contains(&length)
        || session_id == "."
        || session_id == ".."
        || !session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(ContractError::InvalidValue);
    }
    Ok(())
}

fn validate_safe_summary(summary: &str) -> Result<(), ContractError> {
    if summary.len() > MAX_ACTIVITY_SUMMARY_BYTES
        || summary.chars().any(|character| {
            character.is_control()
                || character == '\u{061c}'
                || character == '\u{200e}'
                || character == '\u{200f}'
                || matches!(character, '\u{202a}'..='\u{202e}')
                || matches!(character, '\u{2066}'..='\u{2069}')
        })
    {
        return Err(ContractError::InvalidSummary);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;

    const OPERATION_ID: Uuid = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0002);
    const SESSION_INSTANCE: Uuid = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0003);

    fn update(
        operation: ActivityOperation,
        state: ActivityState,
        duration_ms: Option<u64>,
        summary: ActivitySummary,
    ) -> ActivityUpdate {
        ActivityUpdate::new(OPERATION_ID, operation, state, duration_ms, summary).unwrap()
    }

    fn valid_update_value() -> Value {
        json!({
            "schema_version": ACTIVITY_SCHEMA_VERSION,
            "operation_id": OPERATION_ID,
            "operation": "git_pull",
            "state": "completed",
            "duration_ms": 1832,
            "summary": {"kind": "git", "remote": "origin"}
        })
    }

    #[test]
    fn serializes_all_fixed_enum_names() {
        let operations = [
            (ActivityOperation::SessionStart, "session_start"),
            (ActivityOperation::SessionStop, "session_stop"),
            (ActivityOperation::ReadFile, "read_file"),
            (ActivityOperation::WriteFile, "write_file"),
            (ActivityOperation::GitPull, "git_pull"),
        ];
        for (operation, expected) in operations {
            let encoded = encode_update(&update(
                operation,
                ActivityState::Started,
                None,
                ActivitySummary::Empty,
            ))
            .unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&encoded).unwrap()["operation"],
                expected
            );
        }

        let states = [
            (ActivityState::Started, "started", None),
            (ActivityState::WaitingApproval, "waiting_approval", None),
            (ActivityState::Running, "running", None),
            (ActivityState::Completed, "completed", Some(0)),
            (ActivityState::Failed, "failed", Some(1832)),
            (ActivityState::Cancelled, "cancelled", Some(0)),
        ];
        for (state, expected, duration_ms) in states {
            let encoded = encode_update(&update(
                ActivityOperation::ReadFile,
                state,
                duration_ms,
                ActivitySummary::Empty,
            ))
            .unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&encoded).unwrap()["state"],
                expected
            );
        }
    }

    #[test]
    fn enforces_duration_by_state() {
        for state in [
            ActivityState::Started,
            ActivityState::WaitingApproval,
            ActivityState::Running,
        ] {
            assert!(
                ActivityUpdate::new(
                    OPERATION_ID,
                    ActivityOperation::ReadFile,
                    state,
                    None,
                    ActivitySummary::Empty,
                )
                .is_ok()
            );
            assert_eq!(
                ActivityUpdate::new(
                    OPERATION_ID,
                    ActivityOperation::ReadFile,
                    state,
                    Some(0),
                    ActivitySummary::Empty,
                ),
                Err(ContractError::InvalidValue)
            );
        }

        for state in [
            ActivityState::Completed,
            ActivityState::Failed,
            ActivityState::Cancelled,
        ] {
            assert!(
                ActivityUpdate::new(
                    OPERATION_ID,
                    ActivityOperation::ReadFile,
                    state,
                    Some(0),
                    ActivitySummary::Empty,
                )
                .is_ok()
            );
            assert!(
                ActivityUpdate::new(
                    OPERATION_ID,
                    ActivityOperation::ReadFile,
                    state,
                    Some(1832),
                    ActivitySummary::Empty,
                )
                .is_ok()
            );
            assert_eq!(
                ActivityUpdate::new(
                    OPERATION_ID,
                    ActivityOperation::ReadFile,
                    state,
                    None,
                    ActivitySummary::Empty,
                ),
                Err(ContractError::InvalidValue)
            );
        }
    }

    #[test]
    fn renders_only_the_three_safe_summary_forms() {
        assert_eq!(ActivitySummary::Empty.safe_summary(), "");
        assert_eq!(
            ActivitySummary::failure(ActivityErrorKind::ApprovalDenied).safe_summary(),
            "error=approval_denied"
        );
        assert_eq!(
            ActivitySummary::git(ActivityRemote::Origin).safe_summary(),
            "remote=origin"
        );
        assert_eq!(
            ActivitySummary::git(ActivityRemote::Other).safe_summary(),
            "remote=other"
        );

        let value =
            serde_json::to_value(ActivitySummary::failure(ActivityErrorKind::ApprovalDenied))
                .unwrap();
        assert_eq!(
            value,
            json!({"kind": "failure", "error": "approval_denied"})
        );
    }

    #[test]
    fn update_round_trip_preserves_typed_summary_and_null_duration() {
        let update = update(
            ActivityOperation::WriteFile,
            ActivityState::Running,
            None,
            ActivitySummary::failure(ActivityErrorKind::SandboxDenied),
        );
        let encoded = encode_update(&update).unwrap();
        assert!(!encoded.ends_with(b"\n"));
        let value: Value = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(value.get("duration_ms"), Some(&Value::Null));
        assert!(value.get("safe_summary").is_none());
        assert_eq!(decode_update(&encoded).unwrap(), update);
        assert_eq!(encode_update(&update).unwrap(), encoded);
    }

    #[test]
    fn golden_event_has_the_expected_envelope_and_safe_summary() {
        let update = update(
            ActivityOperation::GitPull,
            ActivityState::Completed,
            Some(1832),
            ActivitySummary::git(ActivityRemote::Origin),
        );
        let stamp = EventStamp::new(
            1,
            1_780_000_000_000,
            Some("sf".to_owned()),
            Some(SESSION_INSTANCE),
        )
        .unwrap();
        let event = ActivityEvent::from_update(update, stamp).unwrap();
        assert_eq!(event.safe_summary(), "remote=origin");
        let encoded = encode_event(&event).unwrap();
        assert!(!encoded.ends_with(b"\n"));
        assert_eq!(decode_event(&encoded), Ok(event.clone()));
        let actual: Value = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(
            actual,
            json!({
                "type": "activity",
                "event": {
                    "schema_version": 1,
                    "sequence": 1,
                    "operation_id": "00000000-0000-4000-8000-000000000002",
                    "timestamp_ms": 1780000000000u64,
                    "session_id": "sf",
                    "session_instance": "00000000-0000-4000-8000-000000000003",
                    "operation": "git_pull",
                    "state": "completed",
                    "duration_ms": 1832,
                    "safe_summary": "remote=origin"
                }
            })
        );
    }

    #[test]
    fn preserves_explicit_nulls_in_event() {
        let event = ActivityEvent::from_update(
            update(
                ActivityOperation::SessionStart,
                ActivityState::Started,
                None,
                ActivitySummary::Empty,
            ),
            EventStamp::new(1, 0, None, None).unwrap(),
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&encode_event(&event).unwrap()).unwrap();
        let event = value.get("event").and_then(Value::as_object).unwrap();
        assert_eq!(event.get("session_id"), Some(&Value::Null));
        assert_eq!(event.get("session_instance"), Some(&Value::Null));
        assert_eq!(event.get("duration_ms"), Some(&Value::Null));
    }

    #[test]
    fn missing_event_nullable_key_fails_explicit_null_check() {
        let event = ActivityEvent::from_update(
            update(
                ActivityOperation::SessionStart,
                ActivityState::Started,
                None,
                ActivitySummary::Empty,
            ),
            EventStamp::new(1, 0, None, None).unwrap(),
        )
        .unwrap();
        let mut value: Value = serde_json::from_slice(&encode_event(&event).unwrap()).unwrap();
        let event = value
            .get("event")
            .and_then(Value::as_object)
            .unwrap()
            .clone();

        for field in ["session_id", "session_instance", "duration_ms"] {
            let mut missing = event.clone();
            missing.remove(field);
            assert_ne!(missing.get(field), Some(&Value::Null));
            value["event"] = Value::Object(missing);
            assert_eq!(
                decode_event(&serde_json::to_vec(&value).unwrap()),
                Err(ContractError::InvalidJson)
            );
        }
    }

    #[test]
    fn decode_event_rejects_duplicate_unknown_and_semantically_invalid_fields() {
        let valid = br#"{"type":"activity","event":{"schema_version":1,"sequence":1,"operation_id":"00000000-0000-4000-8000-000000000002","timestamp_ms":1780000000000,"session_id":"sf","session_instance":"00000000-0000-4000-8000-000000000003","operation":"git_pull","state":"completed","duration_ms":1832,"safe_summary":"remote=origin"}}"#;
        assert!(decode_event(valid).is_ok());
        for invalid in [
            br#"{"type":"activity","type":"activity","event":{"schema_version":1,"sequence":1,"operation_id":"00000000-0000-4000-8000-000000000002","timestamp_ms":1780000000000,"session_id":"sf","session_instance":null,"operation":"git_pull","state":"completed","duration_ms":1,"safe_summary":""}}"#.as_slice(),
            br#"{"type":"activity","event":{"schema_version":1,"sequence":1,"sequence":2,"operation_id":"00000000-0000-4000-8000-000000000002","timestamp_ms":1780000000000,"session_id":"sf","session_instance":null,"operation":"git_pull","state":"completed","duration_ms":1,"safe_summary":""}}"#.as_slice(),
            br#"{"type":"activity","event":{"schema_version":1,"sequence":1,"operation_id":"00000000-0000-4000-8000-000000000002","timestamp_ms":1780000000000,"session_id":"sf","session_instance":null,"operation":"git_pull","state":"completed","duration_ms":1,"safe_summary":"","extra":true}}"#.as_slice(),
        ] {
            assert_eq!(decode_event(invalid), Err(ContractError::InvalidJson));
        }

        let unknown_schema = std::str::from_utf8(valid).unwrap().replacen(
            "\"schema_version\":1",
            "\"schema_version\":2",
            1,
        );
        assert_eq!(
            decode_event(unknown_schema.as_bytes()),
            Err(ContractError::UnknownSchema)
        );
        let oversized = vec![b' '; MAX_ACTIVITY_EVENT_BYTES + 1];
        assert_eq!(decode_event(&oversized), Err(ContractError::TooLarge));
    }

    #[test]
    fn validates_session_identity_and_sequence() {
        assert!(EventStamp::new(1, 0, None, None).is_ok());
        assert!(EventStamp::new(1, 0, Some("normal_id.v1-2".to_owned()), None).is_ok());
        assert!(EventStamp::new(1, 0, Some("a".repeat(64)), None).is_ok());

        for session_id in ["", ".", "..", "a/b", "a b", "é"] {
            assert_eq!(
                EventStamp::new(1, 0, Some(session_id.to_owned()), None),
                Err(ContractError::InvalidValue),
                "accepted invalid session id {session_id:?}"
            );
        }
        assert_eq!(
            EventStamp::new(1, 0, Some("a".repeat(65)), None),
            Err(ContractError::InvalidValue)
        );
        assert_eq!(
            EventStamp::new(0, 0, None, None),
            Err(ContractError::InvalidValue)
        );
        assert_eq!(
            EventStamp::new(1, 0, None, Some(SESSION_INSTANCE)),
            Err(ContractError::InvalidValue)
        );
        assert!(EventStamp::new(1, 0, Some("sf".to_owned()), None).is_ok());
    }

    #[test]
    fn rejects_unknown_fields_missing_fields_and_wrong_values() {
        let mut value = valid_update_value();
        value["session_id"] = json!("sf");
        assert_eq!(
            decode_update(&serde_json::to_vec(&value).unwrap()),
            Err(ContractError::InvalidJson)
        );

        for field in [
            "schema_version",
            "operation_id",
            "operation",
            "state",
            "duration_ms",
            "summary",
        ] {
            let mut missing = valid_update_value();
            missing.as_object_mut().unwrap().remove(field);
            assert_eq!(
                decode_update(&serde_json::to_vec(&missing).unwrap()),
                Err(ContractError::InvalidJson),
                "accepted missing field {field}"
            );
        }

        let mut wrong_summary_field = valid_update_value();
        wrong_summary_field["summary"]["secret"] = json!("SECRET_SENTINEL_ACTIVITY");
        assert_eq!(
            decode_update(&serde_json::to_vec(&wrong_summary_field).unwrap()),
            Err(ContractError::InvalidJson)
        );

        let mut wrong_operation = valid_update_value();
        wrong_operation["operation"] = json!("unknown");
        assert_eq!(
            decode_update(&serde_json::to_vec(&wrong_operation).unwrap()),
            Err(ContractError::InvalidJson)
        );

        let mut wrong_duration = valid_update_value();
        wrong_duration["duration_ms"] = json!("1832");
        assert_eq!(
            decode_update(&serde_json::to_vec(&wrong_duration).unwrap()),
            Err(ContractError::InvalidJson)
        );
        wrong_duration["duration_ms"] = json!(-1);
        assert_eq!(
            decode_update(&serde_json::to_vec(&wrong_duration).unwrap()),
            Err(ContractError::InvalidJson)
        );

        let mut unknown_schema = valid_update_value();
        unknown_schema["schema_version"] = json!(2);
        assert_eq!(
            decode_update(&serde_json::to_vec(&unknown_schema).unwrap()),
            Err(ContractError::UnknownSchema)
        );
    }

    #[test]
    fn review_reproduces_non_string_operation_and_state_acceptance() {
        let operation_object = br#"{"schema_version":1,"operation_id":"00000000-0000-4000-8000-000000000002","operation":{"git_pull":null},"state":"started","duration_ms":null,"summary":{"kind":"empty"}}"#;
        assert_eq!(
            decode_update(operation_object),
            Err(ContractError::InvalidJson)
        );

        let state_object = br#"{"schema_version":1,"operation_id":"00000000-0000-4000-8000-000000000002","operation":"git_pull","state":{"completed":null},"duration_ms":1832,"summary":{"kind":"empty"}}"#;
        assert_eq!(decode_update(state_object), Err(ContractError::InvalidJson));
    }

    #[test]
    fn review_reproduces_duplicate_top_level_and_summary_keys() {
        let duplicate_schema = br#"{"schema_version":2,"schema_version":1,"operation_id":"00000000-0000-4000-8000-000000000002","operation":"git_pull","state":"completed","duration_ms":1832,"summary":{"kind":"git","remote":"origin"}}"#;
        assert_eq!(
            decode_update(duplicate_schema),
            Err(ContractError::InvalidJson)
        );

        let duplicate_operation = br#"{"schema_version":1,"operation_id":"00000000-0000-4000-8000-000000000002","operation":"git_pull","operation":"git_pull","state":"completed","duration_ms":1832,"summary":{"kind":"git","remote":"origin"}}"#;
        assert_eq!(
            decode_update(duplicate_operation),
            Err(ContractError::InvalidJson)
        );

        let duplicate_summary_same = br#"{"schema_version":1,"operation_id":"00000000-0000-4000-8000-000000000002","operation":"git_pull","state":"completed","duration_ms":1832,"summary":{"kind":"git","remote":"origin","remote":"origin"}}"#;
        assert_eq!(
            decode_update(duplicate_summary_same),
            Err(ContractError::InvalidJson)
        );

        let duplicate_summary_different = br#"{"schema_version":1,"operation_id":"00000000-0000-4000-8000-000000000002","operation":"git_pull","state":"completed","duration_ms":1832,"summary":{"kind":"git","remote":"origin","remote":"other"}}"#;
        assert_eq!(
            decode_update(duplicate_summary_different),
            Err(ContractError::InvalidJson)
        );
    }

    #[test]
    fn rejects_all_non_string_operation_and_state_values() {
        for value in ["{}", "[]", "1", "true", "null"] {
            let operation = format!(
                r#"{{"schema_version":1,"operation_id":"00000000-0000-4000-8000-000000000002","operation":{value},"state":"started","duration_ms":null,"summary":{{"kind":"empty"}}}}"#
            );
            assert_eq!(
                decode_update(operation.as_bytes()),
                Err(ContractError::InvalidJson),
                "accepted non-string operation value {value}"
            );

            let state = format!(
                r#"{{"schema_version":1,"operation_id":"00000000-0000-4000-8000-000000000002","operation":"git_pull","state":{value},"duration_ms":1832,"summary":{{"kind":"empty"}}}}"#
            );
            assert_eq!(
                decode_update(state.as_bytes()),
                Err(ContractError::InvalidJson),
                "accepted non-string state value {value}"
            );
        }
    }

    #[test]
    fn rejects_non_object_updates() {
        for value in ["[]", "1", "true", "null"] {
            assert_eq!(
                decode_update(value.as_bytes()),
                Err(ContractError::InvalidJson),
                "accepted non-object update {value}"
            );
        }
    }

    #[test]
    fn rejects_missing_nullable_duration_key() {
        let mut value = valid_update_value();
        value.as_object_mut().unwrap().remove("duration_ms");
        assert_eq!(
            decode_update(&serde_json::to_vec(&value).unwrap()),
            Err(ContractError::InvalidJson)
        );
    }

    #[test]
    fn checks_input_size_before_json_parsing() {
        let oversized = vec![b'{'; MAX_ACTIVITY_EVENT_BYTES + 1];
        assert_eq!(decode_update(&oversized), Err(ContractError::TooLarge));

        let boundary_invalid = vec![b'{'; MAX_ACTIVITY_EVENT_BYTES];
        assert_eq!(
            decode_update(&boundary_invalid),
            Err(ContractError::InvalidJson)
        );
    }

    #[test]
    fn validates_summary_bytes_and_control_characters() {
        assert!(validate_safe_summary(&"a".repeat(512)).is_ok());
        assert_eq!(
            validate_safe_summary(&"a".repeat(513)),
            Err(ContractError::InvalidSummary)
        );
        assert!(validate_safe_summary(&"é".repeat(256)).is_ok());
        assert_eq!(
            validate_safe_summary(&"é".repeat(257)),
            Err(ContractError::InvalidSummary)
        );

        for control in [
            "line\nfeed",
            "carriage\rreturn",
            "escape\u{001b}",
            "c1\u{0085}",
            "arabic\u{061c}",
            "left\u{200e}",
            "right\u{200f}",
            "embed\u{202a}",
            "isolate\u{2066}",
        ] {
            assert_eq!(
                validate_safe_summary(control),
                Err(ContractError::InvalidSummary),
                "accepted control-containing summary {control:?}"
            );
        }
    }

    #[test]
    fn errors_are_fixed_and_do_not_echo_input() {
        let sentinel = "SECRET_SENTINEL_ACTIVITY";
        let malformed = format!("{{\"operation\":\"{sentinel}\"}}");
        let error = decode_update(malformed.as_bytes()).unwrap_err();
        assert_eq!(error, ContractError::InvalidJson);
        assert!(!format!("{error}").contains(sentinel));
        assert!(!format!("{error:?}").contains(sentinel));
    }
}
