//! Bounded in-memory retention for completed activity events.
//!
//! This module deliberately has no transport, synchronization, or session
//! lookup responsibilities. The caller supplies an already constructed
//! [`ActivityEvent`] and the byte length of its serialized envelope.

use std::collections::VecDeque;
use std::fmt;

use super::contract::{ActivityEvent, ContractError, MAX_ACTIVITY_EVENT_BYTES, encode_event};

pub const MAX_ACTIVITY_HISTORY_EVENTS: usize = 4096;
pub const MAX_ACTIVITY_HISTORY_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_ACTIVITY_TAIL: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoryError {
    InvalidCapacity,
    InvalidTail,
    InvalidEvent,
    InvalidSerializedSize,
    SerializedSizeMismatch,
    EventTooLarge,
    InvariantViolation,
}

impl fmt::Display for HistoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::InvalidCapacity => "invalid_capacity",
            Self::InvalidTail => "invalid_tail",
            Self::InvalidEvent => "invalid_event",
            Self::InvalidSerializedSize => "invalid_serialized_size",
            Self::SerializedSizeMismatch => "serialized_size_mismatch",
            Self::EventTooLarge => "event_too_large",
            Self::InvariantViolation => "invariant_violation",
        };
        formatter.write_str(name)
    }
}

impl std::error::Error for HistoryError {}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HistoryEntry {
    event: ActivityEvent,
    serialized_size: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivityHistory {
    max_events: usize,
    max_bytes: usize,
    events: VecDeque<HistoryEntry>,
    serialized_bytes: usize,
    history_truncated: bool,
}

impl Default for ActivityHistory {
    fn default() -> Self {
        Self::new()
    }
}

impl ActivityHistory {
    /// Creates a history using the production count and serialized-byte caps.
    pub fn new() -> Self {
        Self {
            max_events: MAX_ACTIVITY_HISTORY_EVENTS,
            max_bytes: MAX_ACTIVITY_HISTORY_BYTES,
            events: VecDeque::new(),
            serialized_bytes: 0,
            history_truncated: false,
        }
    }

    /// Creates a bounded history with smaller limits suitable for unit tests.
    ///
    /// Both limits must be non-zero and no greater than the production caps.
    /// A zero-capacity history is not representable: callers must choose a
    /// positive retention capacity instead of silently accepting and dropping
    /// every event.
    pub fn with_limits(max_events: usize, max_bytes: usize) -> Result<Self, HistoryError> {
        if !(1..=MAX_ACTIVITY_HISTORY_EVENTS).contains(&max_events)
            || !(1..=MAX_ACTIVITY_HISTORY_BYTES).contains(&max_bytes)
        {
            return Err(HistoryError::InvalidCapacity);
        }
        Ok(Self {
            max_events,
            max_bytes,
            events: VecDeque::new(),
            serialized_bytes: 0,
            history_truncated: false,
        })
    }

    /// Returns the configured maximum number of retained events.
    pub const fn max_events(&self) -> usize {
        self.max_events
    }

    /// Returns the configured maximum serialized-byte total.
    pub const fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// Returns the number of retained events.
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Returns whether no events are currently retained.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Returns the exact serialized-byte total of all retained envelopes.
    pub const fn serialized_bytes(&self) -> usize {
        self.serialized_bytes
    }

    /// Returns whether this history has evicted at least one event.
    ///
    /// This is a global lifetime flag. It is not affected by tail limits,
    /// session filtering, short histories, or read-only operations.
    pub const fn history_truncated(&self) -> bool {
        self.history_truncated
    }

    /// Appends a validated event and evicts the oldest entries as needed.
    ///
    /// `serialized_size` must be the byte length of the exact output of
    /// [`encode_event`] for `event`; this method verifies that precondition
    /// before changing the history. The event itself is never converted to or
    /// retained as raw JSON. A size that cannot fit in this history is rejected
    /// without changing retained events, byte accounting, or truncation state.
    pub fn push(
        &mut self,
        event: ActivityEvent,
        serialized_size: usize,
    ) -> Result<(), HistoryError> {
        if event.validate().is_err() {
            return Err(HistoryError::InvalidEvent);
        }
        if serialized_size == 0 {
            return Err(HistoryError::InvalidSerializedSize);
        }
        if serialized_size > MAX_ACTIVITY_EVENT_BYTES {
            return Err(HistoryError::EventTooLarge);
        }

        let actual_size = encode_event(&event)
            .map_err(|error| match error {
                ContractError::TooLarge => HistoryError::EventTooLarge,
                _ => HistoryError::InvalidEvent,
            })?
            .len();
        if actual_size != serialized_size {
            return Err(HistoryError::SerializedSizeMismatch);
        }
        if serialized_size > self.max_bytes {
            return Err(HistoryError::EventTooLarge);
        }

        let mut eviction_count = 0;
        let mut remaining_bytes = self.serialized_bytes;
        loop {
            let remaining_count = self
                .events
                .len()
                .checked_sub(eviction_count)
                .ok_or(HistoryError::InvariantViolation)?;
            let fits_bytes = remaining_bytes
                .checked_add(serialized_size)
                .is_some_and(|total| total <= self.max_bytes);
            if remaining_count < self.max_events && fits_bytes {
                break;
            }

            let oldest = self
                .events
                .get(eviction_count)
                .ok_or(HistoryError::InvariantViolation)?;
            remaining_bytes = remaining_bytes
                .checked_sub(oldest.serialized_size)
                .ok_or(HistoryError::InvariantViolation)?;
            eviction_count += 1;
        }
        let final_bytes = remaining_bytes
            .checked_add(serialized_size)
            .ok_or(HistoryError::InvariantViolation)?;

        for _ in 0..eviction_count {
            self.events
                .pop_front()
                .expect("eviction count was checked against retained entries");
        }
        self.events.push_back(HistoryEntry {
            event,
            serialized_size,
        });
        self.serialized_bytes = final_bytes;
        if eviction_count > 0 {
            self.history_truncated = true;
        }
        Ok(())
    }

    /// Returns the newest `limit` matching events in sequence order.
    ///
    /// `session_id = None` includes all events. A session filter is an exact
    /// match and excludes session-independent events. No session existence,
    /// metadata, startup, or probe is consulted. `limit` must be in
    /// `0..=MAX_ACTIVITY_TAIL`; zero returns an empty vector without mutation.
    pub fn tail(
        &self,
        session_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ActivityEvent>, HistoryError> {
        self.tail_with(session_id, limit, ActivityEvent::clone)
    }

    fn tail_with<T, F>(
        &self,
        session_id: Option<&str>,
        limit: usize,
        mut clone_event: F,
    ) -> Result<Vec<T>, HistoryError>
    where
        F: FnMut(&ActivityEvent) -> T,
    {
        if limit > MAX_ACTIVITY_TAIL {
            return Err(HistoryError::InvalidTail);
        }
        if limit == 0 {
            return Ok(Vec::new());
        }

        let mut matching: Vec<&HistoryEntry> = self
            .events
            .iter()
            .filter(|entry| match session_id {
                None => true,
                Some(session_id) => entry.event.session_id() == Some(session_id),
            })
            .collect();
        matching.sort_by_key(|entry| entry.event.sequence());
        let skip = matching.len().saturating_sub(limit);
        Ok(matching
            .into_iter()
            .skip(skip)
            .map(|entry| clone_event(&entry.event))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::contract::{
        ActivityOperation, ActivityState, ActivitySummary, EventStamp,
    };
    use uuid::Uuid;

    const OPERATION_ID: Uuid = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0101);

    fn event(sequence: u64, timestamp_ms: u64, session_id: Option<&str>) -> (ActivityEvent, usize) {
        let update = crate::activity::contract::ActivityUpdate::new(
            OPERATION_ID,
            ActivityOperation::ReadFile,
            ActivityState::Started,
            None,
            ActivitySummary::Empty,
        )
        .unwrap();
        let stamp =
            EventStamp::new(sequence, timestamp_ms, session_id.map(str::to_owned), None).unwrap();
        let event = ActivityEvent::from_update(update, stamp).unwrap();
        let serialized_size = encode_event(&event).unwrap().len();
        (event, serialized_size)
    }

    fn sequences(events: &[ActivityEvent]) -> Vec<u64> {
        events.iter().map(ActivityEvent::sequence).collect()
    }

    #[test]
    fn count_capacity_evicts_only_the_oldest_events() {
        let mut history = ActivityHistory::with_limits(3, MAX_ACTIVITY_HISTORY_BYTES).unwrap();
        for sequence in 1..=4 {
            let (event, serialized_size) = event(sequence, sequence, None);
            history.push(event, serialized_size).unwrap();
        }

        let retained = history.tail(None, MAX_ACTIVITY_TAIL).unwrap();
        assert_eq!(sequences(&retained), [2, 3, 4]);
        assert_eq!(history.len(), 3);
        assert_eq!(history.serialized_bytes(), {
            let (_, size) = event(2, 2, None);
            size * 3
        });
        assert!(history.history_truncated());
    }

    #[test]
    fn byte_capacity_evicts_until_the_new_event_fits() {
        let (_, event_size) = event(1, 1, None);
        let mut history = ActivityHistory::with_limits(10, event_size * 2).unwrap();
        for sequence in 1..=3 {
            let (event, serialized_size) = event(sequence, sequence, None);
            history.push(event, serialized_size).unwrap();
        }

        assert_eq!(
            sequences(&history.tail(None, MAX_ACTIVITY_TAIL).unwrap()),
            [2, 3]
        );
        assert_eq!(history.serialized_bytes(), event_size * 2);
        assert!(history.history_truncated());
    }

    #[test]
    fn one_append_can_evict_multiple_oldest_events() {
        let (_, small_size) = event(1, 1, None);
        let long_session = "s".repeat(64);
        let (large_event, large_size) = event(10, 10, Some(&long_session));
        assert!(large_size > small_size);

        let small_count = large_size.div_ceil(small_size);
        assert!(small_count >= 2);
        let mut history =
            ActivityHistory::with_limits(small_count + 1, small_count * small_size).unwrap();
        for sequence in 1..=small_count as u64 {
            let (event, serialized_size) = event(sequence, sequence, None);
            history.push(event, serialized_size).unwrap();
        }
        history.push(large_event, large_size).unwrap();

        assert_eq!(history.len(), 1);
        assert_eq!(
            sequences(&history.tail(None, MAX_ACTIVITY_TAIL).unwrap()),
            [10]
        );
        assert_eq!(history.serialized_bytes(), large_size);
        assert!(history.history_truncated());
    }

    #[test]
    fn exact_byte_limit_is_accepted_and_single_event_overflow_is_rejected_without_mutation() {
        let (first_event, event_size) = event(1, 1, None);
        let mut exact = ActivityHistory::with_limits(2, event_size).unwrap();
        exact.push(first_event, event_size).unwrap();
        assert_eq!(exact.serialized_bytes(), event_size);
        assert!(!exact.history_truncated());

        let before = exact.clone();
        let long_session = "l".repeat(64);
        let (too_large, too_large_size) = event(2, 2, Some(&long_session));
        assert!(too_large_size > event_size);
        assert_eq!(
            exact.push(too_large, too_large_size),
            Err(HistoryError::EventTooLarge)
        );
        assert_eq!(exact, before);
    }

    #[test]
    fn invalid_capacity_and_serialized_size_are_rejected_without_mutation() {
        let (_, event_size) = event(1, 1, None);
        assert_eq!(
            ActivityHistory::with_limits(0, MAX_ACTIVITY_HISTORY_BYTES),
            Err(HistoryError::InvalidCapacity)
        );
        assert_eq!(
            ActivityHistory::with_limits(1, 0),
            Err(HistoryError::InvalidCapacity)
        );

        let (event, _) = event(1, 1, None);
        let mut history = ActivityHistory::new();
        let before = history.clone();
        assert_eq!(
            history.push(event.clone(), 0),
            Err(HistoryError::InvalidSerializedSize)
        );
        assert_eq!(history, before);
        assert_eq!(
            history.push(event.clone(), event_size + 1),
            Err(HistoryError::SerializedSizeMismatch)
        );
        assert_eq!(history, before);
        assert_eq!(
            history.push(event, MAX_ACTIVITY_EVENT_BYTES + 1),
            Err(HistoryError::EventTooLarge)
        );
        assert_eq!(history, before);
    }

    #[test]
    fn filtered_tail_uses_exact_session_matches_and_sequence_order() {
        let mut history = ActivityHistory::with_limits(10, MAX_ACTIVITY_HISTORY_BYTES).unwrap();
        for (sequence, timestamp_ms, session_id) in [
            (1, 500, Some("sf")),
            (2, 400, Some("dagu")),
            (3, 300, None),
            (4, 200, Some("sf")),
            (5, 100, Some("dagu")),
        ] {
            let (event, serialized_size) = event(sequence, timestamp_ms, session_id);
            history.push(event, serialized_size).unwrap();
        }

        let all = history.tail(None, MAX_ACTIVITY_TAIL).unwrap();
        assert_eq!(sequences(&all), [1, 2, 3, 4, 5]);
        assert_eq!(
            all.iter()
                .map(ActivityEvent::timestamp_ms)
                .collect::<Vec<_>>(),
            [500, 400, 300, 200, 100]
        );
        assert_eq!(
            sequences(&history.tail(Some("sf"), MAX_ACTIVITY_TAIL).unwrap()),
            [1, 4]
        );
        assert_eq!(sequences(&history.tail(Some("sf"), 1).unwrap()), [4]);
        assert_eq!(
            sequences(&history.tail(Some("missing"), 1024).unwrap()),
            Vec::<u64>::new()
        );
        assert_eq!(
            sequences(&history.tail(None, 0).unwrap()),
            Vec::<u64>::new()
        );
        assert_eq!(sequences(&history.tail(None, 3).unwrap()), [3, 4, 5]);
        assert_eq!(
            history.tail(None, MAX_ACTIVITY_TAIL + 1),
            Err(HistoryError::InvalidTail)
        );
    }

    #[test]
    fn tail_selects_before_cloning_when_history_exceeds_the_replay_limit() {
        let mut history = ActivityHistory::with_limits(2048, MAX_ACTIVITY_HISTORY_BYTES).unwrap();
        for sequence in 1..=1025 {
            let (event, serialized_size) = event(sequence, sequence, None);
            history.push(event, serialized_size).unwrap();
        }

        let retained = history.tail(None, 1).unwrap();
        assert_eq!(sequences(&retained), [1025]);

        let mut clone_count = 0;
        let retained = history
            .tail_with(None, 1, |event| {
                clone_count += 1;
                event.clone()
            })
            .unwrap();
        assert_eq!(sequences(&retained), [1025]);
        assert_eq!(clone_count, 1);

        let mut clone_count = 0;
        let retained = history
            .tail_with(None, MAX_ACTIVITY_TAIL, |event| {
                clone_count += 1;
                event.clone()
            })
            .unwrap();
        assert_eq!(retained.len(), MAX_ACTIVITY_TAIL);
        assert_eq!(clone_count, MAX_ACTIVITY_TAIL);
        assert_eq!(retained.first().map(ActivityEvent::sequence), Some(2));
        assert_eq!(retained.last().map(ActivityEvent::sequence), Some(1025));
        assert!(history.len() > MAX_ACTIVITY_TAIL);
    }

    #[test]
    fn truncation_is_global_and_read_operations_do_not_mutate_history() {
        let empty = ActivityHistory::new();
        assert!(empty.is_empty());
        assert_eq!(empty.tail(None, 0).unwrap(), Vec::<ActivityEvent>::new());
        assert_eq!(empty.tail(None, 1).unwrap(), Vec::<ActivityEvent>::new());
        assert_eq!(
            empty.tail(Some("missing"), MAX_ACTIVITY_TAIL).unwrap(),
            Vec::<ActivityEvent>::new()
        );
        assert!(!empty.history_truncated());

        let (_, event_size) = event(1, 1, None);
        let mut history = ActivityHistory::with_limits(3, event_size * 3).unwrap();
        for sequence in 1..=3 {
            let (event, serialized_size) = event(sequence, sequence, None);
            history.push(event, serialized_size).unwrap();
        }
        assert!(!history.history_truncated());

        let before_reads = history.clone();
        let _ = history.tail(Some("sf"), 1).unwrap();
        let _ = history.tail(None, 0).unwrap();
        assert_eq!(history, before_reads);
        assert!(!history.history_truncated());

        let (event, serialized_size) = event(4, 4, None);
        history.push(event, serialized_size).unwrap();
        assert!(history.history_truncated());
        let before_more_reads = history.clone();
        let _ = history.tail(None, MAX_ACTIVITY_TAIL).unwrap();
        let _ = history.tail(Some("missing"), MAX_ACTIVITY_TAIL).unwrap();
        assert_eq!(history, before_more_reads);
        assert!(history.history_truncated());
    }
}
