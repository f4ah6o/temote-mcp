//! Process-wide best-effort delivery of typed activity updates.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(not(test))]
use std::sync::OnceLock;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::approvals::{
    self, ACTIVITY_ACK_ACCEPTED, ACTIVITY_ACK_DISCARDED, ActivityExpectedSession,
};
use crate::config::{self, Session};
use temote_mcp::activity::contract::{ActivityUpdate, encode_update};
use temote_mcp::activity::scope::{ActivityEmitError, ActivityEmitter};

#[cfg_attr(test, allow(dead_code))]
const ACTIVITY_PRODUCER_QUEUE_CAPACITY: usize = 256;
#[cfg_attr(test, allow(dead_code))]
const ACTIVITY_DELIVERY_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_ACTIVITY_ACK_BYTES: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActivityDeliveryError {
    Connect,
    Encode,
    Write,
    Ack,
    Timeout,
}

impl fmt::Display for ActivityDeliveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Connect => "connect_failed",
            Self::Encode => "encode_failed",
            Self::Write => "write_failed",
            Self::Ack => "ack_failed",
            Self::Timeout => "delivery_timeout",
        })
    }
}

struct PendingActivity {
    socket_path: PathBuf,
    expected_session: ActivityExpectedSession,
    update: ActivityUpdate,
}

struct ActivityRuntimeEmitter {
    socket_path: PathBuf,
    expected_session: ActivityExpectedSession,
    sender: mpsc::Sender<PendingActivity>,
}

impl ActivityEmitter for ActivityRuntimeEmitter {
    fn try_emit(&self, update: ActivityUpdate) -> Result<(), ActivityEmitError> {
        encode_update(&update).map_err(|_| ActivityEmitError::InvalidInput)?;
        self.sender
            .try_send(PendingActivity {
                socket_path: self.socket_path.clone(),
                expected_session: self.expected_session.clone(),
                update,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => ActivityEmitError::Full,
                mpsc::error::TrySendError::Closed(_) => ActivityEmitError::Closed,
            })
    }
}

struct ActivityProducer {
    sender: mpsc::Sender<PendingActivity>,
}

impl ActivityProducer {
    #[cfg_attr(test, allow(dead_code))]
    fn spawn() -> Self {
        let (producer, receiver) = Self::channel(ACTIVITY_PRODUCER_QUEUE_CAPACITY);
        tokio::spawn(run_worker(receiver, ACTIVITY_DELIVERY_TIMEOUT));
        producer
    }

    fn channel(capacity: usize) -> (Self, mpsc::Receiver<PendingActivity>) {
        let (sender, receiver) = mpsc::channel(capacity);
        (Self { sender }, receiver)
    }

    fn emitter(&self, socket_path: PathBuf, session: &Session) -> Arc<ActivityRuntimeEmitter> {
        Arc::new(ActivityRuntimeEmitter {
            socket_path,
            expected_session: ActivityExpectedSession::from_session(session),
            sender: self.sender.clone(),
        })
    }
}

#[cfg(not(test))]
static PROCESS_ACTIVITY_PRODUCER: OnceLock<ActivityProducer> = OnceLock::new();

#[allow(dead_code)]
pub(crate) fn emitter(session: &Session) -> Result<Arc<dyn ActivityEmitter>, ActivityEmitError> {
    let socket_path =
        config::socket_path(&session.id).map_err(|_| ActivityEmitError::InvalidInput)?;
    #[cfg(not(test))]
    let producer = PROCESS_ACTIVITY_PRODUCER.get_or_init(ActivityProducer::spawn);
    #[cfg(test)]
    let producer = ActivityProducer::spawn();
    Ok(producer.emitter(socket_path, session))
}

async fn run_worker(mut receiver: mpsc::Receiver<PendingActivity>, timeout: Duration) {
    while let Some(pending) = receiver.recv().await {
        let _ = deliver_one(pending, timeout).await;
    }
}

async fn deliver_one(
    pending: PendingActivity,
    timeout: Duration,
) -> Result<(), ActivityDeliveryError> {
    match tokio::time::timeout(timeout, deliver_with_io(pending)).await {
        Ok(result) => result,
        Err(_) => Err(ActivityDeliveryError::Timeout),
    }
}

async fn deliver_with_io(pending: PendingActivity) -> Result<(), ActivityDeliveryError> {
    let mut stream = UnixStream::connect(&pending.socket_path)
        .await
        .map_err(|_| ActivityDeliveryError::Connect)?;
    let bytes = approvals::encode_activity_update_message(pending.expected_session, pending.update)
        .map_err(|_| ActivityDeliveryError::Encode)?;
    stream
        .write_all(&bytes)
        .await
        .map_err(|_| ActivityDeliveryError::Write)?;
    stream
        .shutdown()
        .await
        .map_err(|_| ActivityDeliveryError::Write)?;

    let mut ack = Vec::new();
    let read = BufReader::new(stream)
        .take((MAX_ACTIVITY_ACK_BYTES + 1) as u64)
        .read_until(b'\n', &mut ack)
        .await
        .map_err(|_| ActivityDeliveryError::Ack)?;
    if read == 0
        || (ack.as_slice() != ACTIVITY_ACK_ACCEPTED && ack.as_slice() != ACTIVITY_ACK_DISCARDED)
    {
        return Err(ActivityDeliveryError::Ack);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use tokio::net::UnixListener;
    use uuid::Uuid;

    use temote_mcp::activity::contract::{ActivityOperation, ActivityState, ActivitySummary};

    fn session(root: &Path) -> Session {
        Session {
            id: "s06".to_owned(),
            cwd: root.to_owned(),
            permitted_directories: vec![root.to_owned()],
            started_at: 123,
            process_id: 456,
            permission_mode: config::PermissionMode::Agent,
        }
    }

    fn update(operation_id: Uuid) -> ActivityUpdate {
        ActivityUpdate::new(
            operation_id,
            ActivityOperation::ReadFile,
            ActivityState::Started,
            None,
            ActivitySummary::Empty,
        )
        .unwrap()
    }

    fn listener() -> (tempfile::TempDir, PathBuf, UnixListener) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("activity.sock");
        let listener = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
        (directory, path, listener)
    }

    async fn read_frame(stream: UnixStream) -> (UnixStream, serde_json::Value) {
        let mut line = String::new();
        let mut reader = BufReader::new(stream);
        reader.read_line(&mut line).await.unwrap();
        let stream = reader.into_inner();
        (stream, serde_json::from_str(&line).unwrap())
    }

    #[tokio::test]
    async fn activity_producer_is_ordered_and_captures_expected_instance_once() {
        let (_directory, path, listener) = listener();
        let root = tempfile::tempdir().unwrap();
        let mut session = session(root.path());
        let (producer, receiver) = ActivityProducer::channel(256);
        let emitter = producer.emitter(path, &session);
        session.started_at = 999;
        session.process_id = 888;
        let first_id = Uuid::new_v4();
        let second_id = Uuid::new_v4();
        emitter.try_emit(update(first_id)).unwrap();
        emitter.try_emit(update(second_id)).unwrap();
        let worker = tokio::spawn(run_worker(receiver, Duration::from_secs(1)));

        let (first, _) = listener.accept().await.unwrap();
        let (mut first, first_frame) = read_frame(first).await;
        assert_eq!(first_frame["expected_session"]["started_at"], 123);
        assert_eq!(first_frame["expected_session"]["process_id"], 456);
        assert_eq!(first_frame["update"]["operation_id"], first_id.to_string());
        assert!(
            tokio::time::timeout(Duration::from_millis(25), listener.accept())
                .await
                .is_err(),
            "worker connected the second item before the first ACK"
        );
        first.write_all(ACTIVITY_ACK_ACCEPTED).await.unwrap();

        let (second, _) = listener.accept().await.unwrap();
        let (mut second, second_frame) = read_frame(second).await;
        assert_eq!(
            second_frame["update"]["operation_id"],
            second_id.to_string()
        );
        second.write_all(ACTIVITY_ACK_DISCARDED).await.unwrap();
        drop(emitter);
        drop(producer);
        worker.await.unwrap();
    }

    #[test]
    fn activity_producer_queue_is_bounded_and_try_emit_never_waits() {
        assert_eq!(ACTIVITY_PRODUCER_QUEUE_CAPACITY, 256);
        assert_eq!(ACTIVITY_DELIVERY_TIMEOUT, Duration::from_secs(1));
        let root = tempfile::tempdir().unwrap();
        let (producer, receiver) = ActivityProducer::channel(1);
        let emitter = producer.emitter(root.path().join("unused.sock"), &session(root.path()));

        assert_eq!(emitter.try_emit(update(Uuid::new_v4())), Ok(()));
        assert_eq!(
            emitter.try_emit(update(Uuid::new_v4())),
            Err(ActivityEmitError::Full)
        );
        drop(receiver);
        assert_eq!(
            emitter.try_emit(update(Uuid::new_v4())),
            Err(ActivityEmitError::Closed)
        );
    }

    #[tokio::test]
    async fn activity_producer_timeout_is_fixed_and_never_retries() {
        let (_directory, path, listener) = listener();
        let root = tempfile::tempdir().unwrap();
        let (producer, mut receiver) = ActivityProducer::channel(1);
        let emitter = producer.emitter(path, &session(root.path()));
        emitter.try_emit(update(Uuid::new_v4())).unwrap();
        let pending = receiver.recv().await.unwrap();
        let delivery = tokio::spawn(deliver_one(pending, Duration::from_millis(25)));

        let (stream, _) = listener.accept().await.unwrap();
        let (_stream, _frame) = read_frame(stream).await;
        assert_eq!(delivery.await.unwrap(), Err(ActivityDeliveryError::Timeout));
        assert!(
            tokio::time::timeout(Duration::from_millis(25), listener.accept())
                .await
                .is_err(),
            "timed out update was retried"
        );
    }

    #[tokio::test]
    async fn activity_producer_fixed_errors_never_include_raw_paths() {
        let root = tempfile::tempdir().unwrap();
        let sentinel = root.path().join("secret-sentinel.sock");
        let (producer, mut receiver) = ActivityProducer::channel(1);
        let emitter = producer.emitter(sentinel, &session(root.path()));
        emitter.try_emit(update(Uuid::new_v4())).unwrap();

        let error = deliver_one(receiver.recv().await.unwrap(), Duration::from_secs(1))
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "connect_failed");
        assert!(!error.to_string().contains("secret-sentinel"));
    }

    #[tokio::test]
    async fn activity_producer_bad_ack_drops_one_item_and_continues() {
        let (_directory, path, listener) = listener();
        let root = tempfile::tempdir().unwrap();
        let (producer, receiver) = ActivityProducer::channel(2);
        let emitter = producer.emitter(path, &session(root.path()));
        emitter.try_emit(update(Uuid::new_v4())).unwrap();
        emitter.try_emit(update(Uuid::new_v4())).unwrap();
        let worker = tokio::spawn(run_worker(receiver, Duration::from_secs(1)));

        let (first, _) = listener.accept().await.unwrap();
        let (mut first, _) = read_frame(first).await;
        first.write_all(b"invalid\n").await.unwrap();
        let (second, _) = listener.accept().await.unwrap();
        let (mut second, _) = read_frame(second).await;
        second.write_all(ACTIVITY_ACK_ACCEPTED).await.unwrap();

        drop(emitter);
        drop(producer);
        worker.await.unwrap();
    }
}
