//! Non-blocking activity retention and replay/live subscription coordination.
//!
//! [`ActivityBroker`] owns only bounded in-memory state. It does not resolve
//! sessions, inspect metadata, perform I/O, or assign runtime ownership. The
//! caller supplies the already typed update and session identity; ownership
//! checks belong to the later ingress layer.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};

use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast};
use uuid::Uuid;

use super::contract::{ActivityEvent, ActivityUpdate, ContractError, EventStamp, encode_event};
use super::history::{ActivityHistory, HistoryError, MAX_ACTIVITY_TAIL};

pub const MAX_ACTIVITY_BROADCAST_CAPACITY: usize = 1024;
pub const MAX_ACTIVITY_SUBSCRIBERS: usize = 16;

/// A synchronous timestamp source injected by the broker constructor.
///
/// The broker invokes the clock before acquiring its state lock. Implementors
/// should therefore keep this operation small and non-blocking.
pub trait ActivityClock: Send + Sync {
    fn now_ms(&self) -> u64;
}

impl<F> ActivityClock for F
where
    F: Fn() -> u64 + Send + Sync,
{
    fn now_ms(&self) -> u64 {
        self()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrokerError {
    InvalidCapacity,
    InvalidInput,
    Busy,
    EventTooLarge,
    SequenceExhausted,
    Closed,
    InvariantViolation,
}

impl fmt::Display for BrokerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::InvalidCapacity => "invalid_capacity",
            Self::InvalidInput => "invalid_input",
            Self::Busy => "activity_busy",
            Self::EventTooLarge => "event_too_large",
            Self::SequenceExhausted => "sequence_exhausted",
            Self::Closed => "activity_closed",
            Self::InvariantViolation => "invariant_violation",
        };
        formatter.write_str(name)
    }
}

impl std::error::Error for BrokerError {}

/// A global live-delivery gap caused by broadcast lag.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActivityGap {
    after_sequence: u64,
    through_sequence: u64,
    dropped: u64,
}

impl ActivityGap {
    pub const fn after_sequence(&self) -> u64 {
        self.after_sequence
    }

    pub const fn through_sequence(&self) -> u64 {
        self.through_sequence
    }

    pub const fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// One item observed from a live subscription.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActivityDelivery {
    Event(ActivityEvent),
    Gap(ActivityGap),
}

struct BrokerState {
    history: ActivityHistory,
    live: broadcast::Sender<ActivityEvent>,
    last_sequence: u64,
}

struct PendingGap {
    after_sequence: u64,
    lagged: u64,
}

/// An atomic replay snapshot followed by a bounded live receiver.
///
/// The snapshot and cutoff are captured while the broker lock is held. The
/// receiver starts after that cutoff, so callers can consume the snapshot and
/// then call [`Self::recv`] without replaying cutoff-or-earlier events. The
/// semaphore permit is owned by this value and is released by `Drop`; the
/// type is intentionally not `Clone` so the subscriber limit cannot be
/// bypassed by duplicating receivers.
pub struct ActivitySubscription {
    snapshot: Vec<ActivityEvent>,
    cutoff: u64,
    generation: Uuid,
    history_truncated: bool,
    receiver: broadcast::Receiver<ActivityEvent>,
    _permit: OwnedSemaphorePermit,
    session_id: Option<String>,
    cursor: u64,
    pending_event: Option<ActivityEvent>,
    pending_gap: Option<PendingGap>,
}

impl ActivitySubscription {
    /// Returns the replay snapshot in sequence order.
    pub fn snapshot(&self) -> &[ActivityEvent] {
        &self.snapshot
    }

    /// Consumes the subscription and returns its replay snapshot.
    pub fn into_snapshot(self) -> Vec<ActivityEvent> {
        self.snapshot
    }

    pub const fn cutoff(&self) -> u64 {
        self.cutoff
    }

    pub const fn generation(&self) -> Uuid {
        self.generation
    }

    pub const fn history_truncated(&self) -> bool {
        self.history_truncated
    }

    pub const fn cursor(&self) -> u64 {
        self.cursor
    }

    pub const fn replayed(&self) -> usize {
        self.snapshot.len()
    }

    /// Receives the next filtered live event or a global broadcast gap.
    ///
    /// A `Lagged` notification is held until the next readable global event
    /// supplies the inclusive `through_sequence` endpoint. That endpoint is
    /// processed before the session filter, so excluded events still preserve
    /// the global gap information. The returned gap precedes that held event;
    /// the event is returned by the next call if it matches the filter.
    pub async fn recv(&mut self) -> Result<ActivityDelivery, BrokerError> {
        loop {
            let event = match self.pending_event.take() {
                Some(event) => event,
                None => match self.receiver.recv().await {
                    Ok(event) => event,
                    Err(broadcast::error::RecvError::Lagged(dropped)) => {
                        self.record_lagged(dropped)?;
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => return Err(BrokerError::Closed),
                },
            };

            if let Some(pending_gap) = self.pending_gap.take() {
                let through_sequence = event
                    .sequence()
                    .checked_sub(1)
                    .ok_or(BrokerError::InvariantViolation)?;
                let dropped = through_sequence
                    .checked_sub(pending_gap.after_sequence)
                    .ok_or(BrokerError::InvariantViolation)?;
                if dropped == 0 || dropped != pending_gap.lagged {
                    return Err(BrokerError::InvariantViolation);
                }
                self.pending_event = Some(event);
                return Ok(ActivityDelivery::Gap(ActivityGap {
                    after_sequence: pending_gap.after_sequence,
                    through_sequence,
                    dropped,
                }));
            }

            if event.sequence() <= self.cursor {
                return Err(BrokerError::InvariantViolation);
            }
            self.cursor = event.sequence();

            let matches = match self.session_id.as_deref() {
                None => true,
                Some(session_id) => event.session_id() == Some(session_id),
            };
            if matches {
                return Ok(ActivityDelivery::Event(event));
            }
        }
    }

    fn record_lagged(&mut self, dropped: u64) -> Result<(), BrokerError> {
        if dropped == 0 {
            return Err(BrokerError::InvariantViolation);
        }
        match &mut self.pending_gap {
            Some(pending) => {
                pending.lagged = pending
                    .lagged
                    .checked_add(dropped)
                    .ok_or(BrokerError::InvariantViolation)?;
            }
            None => {
                self.pending_gap = Some(PendingGap {
                    after_sequence: self.cursor,
                    lagged: dropped,
                });
            }
        }
        Ok(())
    }
}

/// Bounded activity history plus non-blocking replay/live coordination.
pub struct ActivityBroker {
    state: Mutex<BrokerState>,
    clock: Arc<dyn ActivityClock>,
    generation: Uuid,
    subscriber_permits: Arc<Semaphore>,
}

impl ActivityBroker {
    /// Creates a production-sized broker with an injected clock and
    /// generation identity.
    pub fn new<C>(clock: C, generation: Uuid) -> Self
    where
        C: ActivityClock + 'static,
    {
        Self::with_limits(
            ActivityHistory::new(),
            MAX_ACTIVITY_BROADCAST_CAPACITY,
            MAX_ACTIVITY_SUBSCRIBERS,
            clock,
            generation,
        )
        .expect("production broker limits are valid")
    }

    /// Creates a broker with a test-sized history, live channel, and
    /// subscriber limit.
    ///
    /// `history` must be an empty history constructed by
    /// [`ActivityHistory::new`] or [`ActivityHistory::with_limits`]. Requiring
    /// it to be empty keeps the broker's sequence origin private and ensures
    /// every broker starts at sequence one. The live and subscriber limits are
    /// both positive and cannot exceed their production caps.
    pub fn with_limits<C>(
        history: ActivityHistory,
        broadcast_capacity: usize,
        max_subscribers: usize,
        clock: C,
        generation: Uuid,
    ) -> Result<Self, BrokerError>
    where
        C: ActivityClock + 'static,
    {
        if !(1..=MAX_ACTIVITY_BROADCAST_CAPACITY).contains(&broadcast_capacity)
            || !(1..=MAX_ACTIVITY_SUBSCRIBERS).contains(&max_subscribers)
        {
            return Err(BrokerError::InvalidCapacity);
        }
        if !history.is_empty() {
            return Err(BrokerError::InvalidInput);
        }
        let (live, _) = broadcast::channel(broadcast_capacity);
        Ok(Self {
            state: Mutex::new(BrokerState {
                history,
                live,
                last_sequence: 0,
            }),
            clock: Arc::new(clock),
            generation,
            subscriber_permits: Arc::new(Semaphore::new(max_subscribers)),
        })
    }

    pub const fn generation(&self) -> Uuid {
        self.generation
    }

    /// Returns the last accepted global sequence, or zero for an empty broker.
    pub fn current_sequence(&self) -> Result<u64, BrokerError> {
        Ok(self.try_state()?.last_sequence)
    }

    pub fn history_truncated(&self) -> Result<bool, BrokerError> {
        Ok(self.try_state()?.history.history_truncated())
    }

    /// Publishes a typed update without waiting for the broker lock.
    ///
    /// The update and session identity are validated before lock acquisition.
    /// Once the lock is acquired, sequence assignment, event encoding/history
    /// update, and non-blocking broadcast send occur in one critical section.
    /// No receiver is not an error: the event remains accepted in history.
    /// Runtime instance ownership is deliberately not checked here.
    pub fn publish(
        &self,
        update: impl Into<ActivityUpdate>,
        session_id: Option<&str>,
        session_instance: Option<Uuid>,
    ) -> Result<ActivityEvent, BrokerError> {
        let update = update.into();
        update.validate().map_err(|_| BrokerError::InvalidInput)?;
        let session_id = session_id.map(str::to_owned);
        EventStamp::new(1, 0, session_id.clone(), session_instance)
            .map_err(|_| BrokerError::InvalidInput)?;
        let timestamp_ms = self.clock.now_ms();

        let mut state = self.try_state()?;
        let sequence = state
            .last_sequence
            .checked_add(1)
            .ok_or(BrokerError::SequenceExhausted)?;
        let stamp = EventStamp::new(sequence, timestamp_ms, session_id, session_instance)
            .map_err(|_| BrokerError::InvalidInput)?;
        let event =
            ActivityEvent::from_update(update, stamp).map_err(|_| BrokerError::InvalidInput)?;
        let serialized_size = encode_event(&event)
            .map_err(|error| match error {
                ContractError::TooLarge => BrokerError::EventTooLarge,
                _ => BrokerError::InvalidInput,
            })?
            .len();

        state
            .history
            .push(event.clone(), serialized_size)
            .map_err(map_history_error)?;
        state.last_sequence = sequence;
        let _ = state.live.send(event.clone());
        Ok(event)
    }

    /// Atomically registers a subscriber, captures the cutoff, and copies its
    /// filtered history tail while holding the broker lock.
    ///
    /// The returned receiver observes only events published after `cutoff`.
    /// `tail` is delegated to S02's bounded implementation and is restricted
    /// to `0..=MAX_ACTIVITY_TAIL`.
    pub fn subscribe_snapshot(
        &self,
        session_id: Option<&str>,
        tail: usize,
    ) -> Result<ActivitySubscription, BrokerError> {
        if tail > MAX_ACTIVITY_TAIL {
            return Err(BrokerError::InvalidInput);
        }
        let session_id = session_id.map(str::to_owned);
        EventStamp::new(1, 0, session_id.clone(), None).map_err(|_| BrokerError::InvalidInput)?;
        let permit = self
            .subscriber_permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| BrokerError::Busy)?;
        let state = self.try_state()?;

        let receiver = state.live.subscribe();
        let cutoff = state.last_sequence;
        let snapshot = state
            .history
            .tail(session_id.as_deref(), tail)
            .map_err(map_history_error)?;
        let history_truncated = state.history.history_truncated();

        Ok(ActivitySubscription {
            snapshot,
            cutoff,
            generation: self.generation,
            history_truncated,
            receiver,
            _permit: permit,
            session_id,
            cursor: cutoff,
            pending_event: None,
            pending_gap: None,
        })
    }

    fn try_state(&self) -> Result<MutexGuard<'_, BrokerState>, BrokerError> {
        match self.state.try_lock() {
            Ok(state) => Ok(state),
            Err(TryLockError::WouldBlock) => Err(BrokerError::Busy),
            Err(TryLockError::Poisoned(_)) => Err(BrokerError::InvariantViolation),
        }
    }
}

fn map_history_error(error: HistoryError) -> BrokerError {
    match error {
        HistoryError::EventTooLarge => BrokerError::EventTooLarge,
        HistoryError::InvalidCapacity => BrokerError::InvalidCapacity,
        HistoryError::InvalidTail | HistoryError::InvalidEvent => BrokerError::InvalidInput,
        HistoryError::InvalidSerializedSize
        | HistoryError::SerializedSizeMismatch
        | HistoryError::InvariantViolation => BrokerError::InvariantViolation,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    };
    use std::thread;

    use super::*;
    use crate::activity::contract::{
        ActivityOperation, ActivityRemote, ActivityState, ActivitySummary,
    };
    use crate::activity::history::MAX_ACTIVITY_HISTORY_BYTES;

    const GENERATION: Uuid = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0201);
    const OPERATION_ID: Uuid = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0202);

    fn update(state: ActivityState, duration_ms: Option<u64>) -> ActivityUpdate {
        ActivityUpdate::new(
            OPERATION_ID,
            ActivityOperation::GitPull,
            state,
            duration_ms,
            ActivitySummary::git(ActivityRemote::Origin),
        )
        .unwrap()
    }

    fn broker() -> ActivityBroker {
        ActivityBroker::new(|| 1_780_000_000_000, GENERATION)
    }

    fn small_broker(
        history_events: usize,
        history_bytes: usize,
        broadcast_capacity: usize,
        max_subscribers: usize,
    ) -> ActivityBroker {
        ActivityBroker::with_limits(
            ActivityHistory::with_limits(history_events, history_bytes).unwrap(),
            broadcast_capacity,
            max_subscribers,
            || 1_780_000_000_000,
            GENERATION,
        )
        .unwrap()
    }

    fn event_sequences(events: &[ActivityEvent]) -> Vec<u64> {
        events.iter().map(ActivityEvent::sequence).collect()
    }

    async fn next_event(subscription: &mut ActivitySubscription) -> ActivityEvent {
        match subscription.recv().await.unwrap() {
            ActivityDelivery::Event(event) => event,
            ActivityDelivery::Gap(_) => panic!("unexpected live gap"),
        }
    }

    #[test]
    fn fixed_constructor_starts_at_one_and_keeps_typed_event_data() {
        let broker = broker();
        assert_eq!(broker.current_sequence().unwrap(), 0);
        assert_eq!(broker.generation(), GENERATION);

        let event = broker
            .publish(
                update(ActivityState::Completed, Some(1832)),
                Some("sf"),
                None,
            )
            .unwrap();
        assert_eq!(event.sequence(), 1);
        assert_eq!(event.timestamp_ms(), 1_780_000_000_000);
        assert_eq!(event.operation_id(), OPERATION_ID);
        assert_eq!(event.operation(), ActivityOperation::GitPull);
        assert_eq!(event.state(), ActivityState::Completed);
        assert_eq!(event.safe_summary(), "remote=origin");
    }

    #[test]
    fn publish_without_subscribers_remains_in_history() {
        let broker = broker();
        broker
            .publish(update(ActivityState::Started, None), None, None)
            .unwrap();

        let subscription = broker.subscribe_snapshot(None, MAX_ACTIVITY_TAIL).unwrap();
        assert_eq!(subscription.cutoff(), 1);
        assert_eq!(event_sequences(subscription.snapshot()), [1]);
        assert_eq!(subscription.cursor(), 1);
    }

    #[test]
    fn clock_can_move_back_without_changing_sequence_order() {
        let calls = Arc::new(AtomicUsize::new(0));
        let clock_calls = Arc::clone(&calls);
        let broker = ActivityBroker::new(
            move || {
                if clock_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    500
                } else {
                    400
                }
            },
            GENERATION,
        );
        broker
            .publish(update(ActivityState::Started, None), None, None)
            .unwrap();
        broker
            .publish(update(ActivityState::Running, None), None, None)
            .unwrap();

        let subscription = broker.subscribe_snapshot(None, MAX_ACTIVITY_TAIL).unwrap();
        assert_eq!(event_sequences(subscription.snapshot()), [1, 2]);
        assert_eq!(
            subscription
                .snapshot()
                .iter()
                .map(ActivityEvent::timestamp_ms)
                .collect::<Vec<_>>(),
            [500, 400]
        );
    }

    #[tokio::test]
    async fn replay_then_live_has_each_sequence_once() {
        let broker = broker();
        for state in [
            ActivityState::Started,
            ActivityState::WaitingApproval,
            ActivityState::Running,
        ] {
            broker.publish(update(state, None), None, None).unwrap();
        }
        let mut subscription = broker.subscribe_snapshot(None, MAX_ACTIVITY_TAIL).unwrap();
        assert_eq!(event_sequences(subscription.snapshot()), [1, 2, 3]);

        broker
            .publish(update(ActivityState::Completed, Some(1)), None, None)
            .unwrap();
        broker
            .publish(update(ActivityState::Failed, Some(2)), None, None)
            .unwrap();
        let live = vec![
            next_event(&mut subscription).await,
            next_event(&mut subscription).await,
        ];
        assert_eq!(event_sequences(subscription.snapshot()), [1, 2, 3]);
        assert_eq!(event_sequences(&live), [4, 5]);
        assert_eq!(subscription.cutoff(), 3);
        assert_eq!(subscription.cursor(), 5);
    }

    #[tokio::test]
    async fn snapshot_and_publish_barrier_have_no_duplicate_or_missing_event() {
        struct BlockingClock {
            entered: Arc<Barrier>,
            release: Arc<Barrier>,
        }

        impl ActivityClock for BlockingClock {
            fn now_ms(&self) -> u64 {
                self.entered.wait();
                self.release.wait();
                1_780_000_000_000
            }
        }

        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let broker = Arc::new(ActivityBroker::new(
            BlockingClock {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
            },
            GENERATION,
        ));
        let publishing = Arc::clone(&broker);
        let handle = thread::spawn(move || {
            publishing
                .publish(update(ActivityState::Started, None), None, None)
                .unwrap()
        });

        entered.wait();
        let mut subscription = broker.subscribe_snapshot(None, MAX_ACTIVITY_TAIL).unwrap();
        assert!(subscription.snapshot().is_empty());
        assert_eq!(subscription.cutoff(), 0);
        release.wait();
        let event = handle.join().unwrap();
        assert_eq!(event.sequence(), 1);
        assert_eq!(next_event(&mut subscription).await.sequence(), 1);
    }

    #[tokio::test]
    async fn filtered_live_events_advance_global_cursor_without_a_false_gap() {
        let broker = broker();
        for state in [
            ActivityState::Started,
            ActivityState::WaitingApproval,
            ActivityState::Running,
        ] {
            broker
                .publish(update(state, None), Some("target"), None)
                .unwrap();
        }
        let mut subscription = broker
            .subscribe_snapshot(Some("target"), MAX_ACTIVITY_TAIL)
            .unwrap();
        assert_eq!(event_sequences(subscription.snapshot()), [1, 2, 3]);

        broker
            .publish(
                update(ActivityState::Completed, Some(4)),
                Some("other"),
                None,
            )
            .unwrap();
        broker
            .publish(update(ActivityState::Failed, Some(5)), Some("target"), None)
            .unwrap();
        assert_eq!(next_event(&mut subscription).await.sequence(), 5);
        assert_eq!(subscription.cursor(), 5);
    }

    #[tokio::test]
    async fn small_broadcast_capacity_reports_global_gap_and_following_event() {
        let broker = small_broker(32, MAX_ACTIVITY_HISTORY_BYTES, 2, MAX_ACTIVITY_SUBSCRIBERS);
        let mut subscription = broker.subscribe_snapshot(None, 0).unwrap();
        for _ in 0..4 {
            broker
                .publish(update(ActivityState::Started, None), None, None)
                .unwrap();
        }

        assert_eq!(
            subscription.recv().await.unwrap(),
            ActivityDelivery::Gap(ActivityGap {
                after_sequence: 0,
                through_sequence: 2,
                dropped: 2,
            })
        );
        assert_eq!(next_event(&mut subscription).await.sequence(), 3);
        assert_eq!(next_event(&mut subscription).await.sequence(), 4);
        assert_eq!(subscription.cursor(), 4);
    }

    #[tokio::test]
    async fn repeated_lag_is_reported_without_duplicate_counting() {
        let broker = small_broker(32, MAX_ACTIVITY_HISTORY_BYTES, 2, MAX_ACTIVITY_SUBSCRIBERS);
        let mut subscription = broker.subscribe_snapshot(None, 0).unwrap();
        for _ in 0..4 {
            broker
                .publish(update(ActivityState::Started, None), None, None)
                .unwrap();
        }
        assert_eq!(
            subscription.recv().await.unwrap(),
            ActivityDelivery::Gap(ActivityGap {
                after_sequence: 0,
                through_sequence: 2,
                dropped: 2,
            })
        );
        assert_eq!(next_event(&mut subscription).await.sequence(), 3);
        for _ in 0..4 {
            broker
                .publish(update(ActivityState::Started, None), None, None)
                .unwrap();
        }
        assert_eq!(
            subscription.recv().await.unwrap(),
            ActivityDelivery::Gap(ActivityGap {
                after_sequence: 3,
                through_sequence: 6,
                dropped: 3,
            })
        );
        assert_eq!(next_event(&mut subscription).await.sequence(), 7);
        assert_eq!(next_event(&mut subscription).await.sequence(), 8);
    }

    #[tokio::test]
    async fn history_truncation_is_snapshot_state_not_a_live_gap() {
        let broker = small_broker(1, MAX_ACTIVITY_HISTORY_BYTES, 4, MAX_ACTIVITY_SUBSCRIBERS);
        broker
            .publish(update(ActivityState::Started, None), None, None)
            .unwrap();
        broker
            .publish(update(ActivityState::Running, None), None, None)
            .unwrap();
        assert!(broker.history_truncated().unwrap());
        let subscription = broker.subscribe_snapshot(None, MAX_ACTIVITY_TAIL).unwrap();
        assert!(subscription.history_truncated());
        assert_eq!(event_sequences(subscription.snapshot()), [2]);
        let mut subscription = subscription;
        broker
            .publish(update(ActivityState::Completed, Some(3)), None, None)
            .unwrap();
        assert_eq!(next_event(&mut subscription).await.sequence(), 3);
    }

    #[test]
    fn subscriber_limit_is_bounded_and_drop_releases_permit() {
        let broker = small_broker(8, MAX_ACTIVITY_HISTORY_BYTES, 4, 16);
        let mut subscriptions = Vec::new();
        for _ in 0..16 {
            subscriptions.push(broker.subscribe_snapshot(None, 0).unwrap());
        }
        assert_eq!(
            broker.subscribe_snapshot(None, 0).err(),
            Some(BrokerError::Busy)
        );
        drop(subscriptions.pop());
        assert!(broker.subscribe_snapshot(None, 0).is_ok());
    }

    #[test]
    fn invalid_subscribe_and_lock_contention_do_not_leak_or_mutate_state() {
        let broker = small_broker(8, MAX_ACTIVITY_HISTORY_BYTES, 2, 1);
        assert_eq!(
            broker.subscribe_snapshot(None, MAX_ACTIVITY_TAIL + 1).err(),
            Some(BrokerError::InvalidInput)
        );
        assert_eq!(
            broker.subscribe_snapshot(Some("contains space"), 0).err(),
            Some(BrokerError::InvalidInput)
        );
        assert_eq!(broker.current_sequence().unwrap(), 0);
        let guard = broker.state.lock().unwrap();
        assert_eq!(
            broker.subscribe_snapshot(None, 0).err(),
            Some(BrokerError::Busy)
        );
        assert_eq!(
            broker.publish(update(ActivityState::Started, None), None, None),
            Err(BrokerError::Busy)
        );
        drop(guard);
        assert!(broker.subscribe_snapshot(None, 0).is_ok());
        assert_eq!(broker.current_sequence().unwrap(), 0);
    }

    #[test]
    fn invalid_publish_is_rejected_before_sequence_or_history_change() {
        let broker = broker();
        assert_eq!(
            broker.publish(
                update(ActivityState::Started, None),
                Some("contains space"),
                None,
            ),
            Err(BrokerError::InvalidInput)
        );
        assert_eq!(broker.current_sequence().unwrap(), 0);
        let subscription = broker.subscribe_snapshot(None, MAX_ACTIVITY_TAIL).unwrap();
        assert!(subscription.snapshot().is_empty());
    }

    #[tokio::test]
    async fn replay_lag_reports_global_gap_before_an_excluded_event() {
        let broker = small_broker(32, MAX_ACTIVITY_HISTORY_BYTES, 2, MAX_ACTIVITY_SUBSCRIBERS);
        broker
            .publish(update(ActivityState::Started, None), Some("target"), None)
            .unwrap();
        let mut subscription = broker.subscribe_snapshot(Some("target"), 1).unwrap();
        assert_eq!(event_sequences(subscription.snapshot()), [1]);

        broker
            .publish(update(ActivityState::Running, None), Some("other"), None)
            .unwrap();
        broker
            .publish(
                update(ActivityState::Completed, Some(3)),
                Some("other"),
                None,
            )
            .unwrap();
        broker
            .publish(update(ActivityState::Failed, Some(4)), Some("target"), None)
            .unwrap();

        assert_eq!(
            subscription.recv().await.unwrap(),
            ActivityDelivery::Gap(ActivityGap {
                after_sequence: 1,
                through_sequence: 2,
                dropped: 1,
            })
        );
        assert_eq!(next_event(&mut subscription).await.sequence(), 4);
        assert_eq!(subscription.cursor(), 4);
    }

    #[tokio::test]
    async fn consecutive_lagged_notifications_are_accumulated_once() {
        let broker = small_broker(32, MAX_ACTIVITY_HISTORY_BYTES, 2, MAX_ACTIVITY_SUBSCRIBERS);
        let mut subscription = broker.subscribe_snapshot(None, 0).unwrap();
        for _ in 0..4 {
            broker
                .publish(update(ActivityState::Started, None), None, None)
                .unwrap();
        }
        assert!(matches!(
            subscription.receiver.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(2))
        ));
        subscription.record_lagged(2).unwrap();

        for _ in 0..2 {
            broker
                .publish(update(ActivityState::Running, None), None, None)
                .unwrap();
        }
        assert!(matches!(
            subscription.receiver.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(2))
        ));
        subscription.record_lagged(2).unwrap();

        assert_eq!(
            subscription.recv().await.unwrap(),
            ActivityDelivery::Gap(ActivityGap {
                after_sequence: 0,
                through_sequence: 4,
                dropped: 4,
            })
        );
        assert_eq!(next_event(&mut subscription).await.sequence(), 5);
        assert_eq!(next_event(&mut subscription).await.sequence(), 6);
    }

    #[tokio::test]
    async fn multiple_subscribers_receive_each_sequence_once() {
        let broker = broker();
        let mut first = broker.subscribe_snapshot(None, 0).unwrap();
        let mut second = broker.subscribe_snapshot(None, 0).unwrap();

        for _ in 0..3 {
            broker
                .publish(update(ActivityState::Started, None), None, None)
                .unwrap();
        }
        for subscription in [&mut first, &mut second] {
            assert_eq!(next_event(subscription).await.sequence(), 1);
            assert_eq!(next_event(subscription).await.sequence(), 2);
            assert_eq!(next_event(subscription).await.sequence(), 3);
        }
    }

    #[test]
    fn invalid_capacities_are_rejected() {
        let history = ActivityHistory::with_limits(8, MAX_ACTIVITY_HISTORY_BYTES).unwrap();
        assert_eq!(
            ActivityBroker::with_limits(history.clone(), 0, 1, || 0, GENERATION).err(),
            Some(BrokerError::InvalidCapacity)
        );
        assert_eq!(
            ActivityBroker::with_limits(
                history.clone(),
                MAX_ACTIVITY_BROADCAST_CAPACITY + 1,
                1,
                || 0,
                GENERATION
            )
            .err(),
            Some(BrokerError::InvalidCapacity)
        );
        assert_eq!(
            ActivityBroker::with_limits(history.clone(), 1, 0, || 0, GENERATION).err(),
            Some(BrokerError::InvalidCapacity)
        );
        assert_eq!(
            ActivityBroker::with_limits(history, 1, MAX_ACTIVITY_SUBSCRIBERS + 1, || 0, GENERATION)
                .err(),
            Some(BrokerError::InvalidCapacity)
        );
    }

    #[test]
    fn nonempty_history_cannot_set_the_broker_sequence_origin() {
        let (event, serialized_size) = {
            let update = update(ActivityState::Started, None);
            let stamp = EventStamp::new(99, 0, None, None).unwrap();
            let event = ActivityEvent::from_update(update, stamp).unwrap();
            let serialized_size = encode_event(&event).unwrap().len();
            (event, serialized_size)
        };
        let mut history = ActivityHistory::with_limits(8, MAX_ACTIVITY_HISTORY_BYTES).unwrap();
        history.push(event, serialized_size).unwrap();

        assert_eq!(
            ActivityBroker::with_limits(history, 1, 1, || 0, GENERATION).err(),
            Some(BrokerError::InvalidInput)
        );
    }

    #[test]
    fn sequence_overflow_stops_without_wrap_or_partial_update() {
        let broker = broker();
        {
            let mut state = broker.state.lock().unwrap();
            state.last_sequence = u64::MAX - 1;
        }
        let first = broker
            .publish(update(ActivityState::Started, None), None, None)
            .unwrap();
        assert_eq!(first.sequence(), u64::MAX);
        assert_eq!(
            broker.publish(update(ActivityState::Running, None), None, None),
            Err(BrokerError::SequenceExhausted)
        );
        assert_eq!(broker.current_sequence().unwrap(), u64::MAX);
        let subscription = broker.subscribe_snapshot(None, MAX_ACTIVITY_TAIL).unwrap();
        assert_eq!(event_sequences(subscription.snapshot()), [u64::MAX]);
    }

    #[tokio::test]
    async fn session_filter_and_tail_boundaries_are_read_only() {
        let broker = broker();
        for (session_id, state) in [
            (Some("target"), ActivityState::Started),
            (None, ActivityState::WaitingApproval),
            (Some("other"), ActivityState::Running),
        ] {
            broker
                .publish(update(state, None), session_id, None)
                .unwrap();
        }
        let before = broker.current_sequence().unwrap();
        let empty = broker.subscribe_snapshot(Some("missing"), 0).unwrap();
        assert!(empty.snapshot().is_empty());
        let target = broker.subscribe_snapshot(Some("target"), 1).unwrap();
        assert_eq!(event_sequences(target.snapshot()), [1]);
        let target_all = broker
            .subscribe_snapshot(Some("target"), MAX_ACTIVITY_TAIL)
            .unwrap();
        assert_eq!(event_sequences(target_all.snapshot()), [1]);
        let all = broker.subscribe_snapshot(None, MAX_ACTIVITY_TAIL).unwrap();
        assert_eq!(event_sequences(all.snapshot()), [1, 2, 3]);
        assert_eq!(broker.current_sequence().unwrap(), before);
        assert!(!broker.history_truncated().unwrap());
        assert_eq!(
            broker.subscribe_snapshot(None, MAX_ACTIVITY_TAIL + 1).err(),
            Some(BrokerError::InvalidInput)
        );
    }
}
