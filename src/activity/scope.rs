//! Synchronous, typed activity state scopes.
//!
//! A scope owns one operation identity and serializes its state transition
//! with the corresponding [`ActivityEmitter::try_emit`] call. It does not
//! assign broker sequence numbers, timestamps, session ownership, or
//! approval decisions. The emitter contract is deliberately non-blocking:
//! `try_emit` reports only whether the update was accepted by its immediate
//! bounded sink, not whether a later delivery completed.
//!
//! Emitters must not synchronously re-enter the same [`ActivityScope`] while
//! `try_emit` is running. The scope holds its state lock through that call so
//! that a terminal transition cannot be overtaken by a concurrent
//! non-terminal update. Implementations must keep `try_emit` synchronous,
//! non-waiting, and free of socket I/O, retry, or unbounded task spawning.

use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use uuid::Uuid;

use super::contract::{ActivityOperation, ActivityState, ActivitySummary, ActivityUpdate};

/// Fixed classifications for immediate emitter rejection.
///
/// These values intentionally contain no raw error or input data. A rejected
/// update is a delivery outcome and does not change the operation outcome.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActivityEmitError {
    Full,
    Closed,
    InvalidInput,
    InvariantViolation,
}

impl fmt::Display for ActivityEmitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Full => "activity_full",
            Self::Closed => "activity_closed",
            Self::InvalidInput => "invalid_input",
            Self::InvariantViolation => "invariant_violation",
        };
        formatter.write_str(name)
    }
}

impl std::error::Error for ActivityEmitError {}

/// Short aliases for callers that name the immediate delivery error simply
/// as an emitter error.
pub type EmitError = ActivityEmitError;
pub type EmitterError = ActivityEmitError;

/// A synchronous emitter for already typed S01 updates.
///
/// Implementations must return without waiting for downstream delivery. The
/// method is called while the owning scope's transition lock is held and must
/// therefore not call back into that same scope.
pub trait ActivityEmitter: Send + Sync {
    fn try_emit(&self, update: ActivityUpdate) -> Result<(), ActivityEmitError>;
}

impl<T> ActivityEmitter for Arc<T>
where
    T: ActivityEmitter + ?Sized,
{
    fn try_emit(&self, update: ActivityUpdate) -> Result<(), ActivityEmitError> {
        (**self).try_emit(update)
    }
}

/// A monotonic `Instant` source used by a scope.
///
/// Production construction uses [`Instant::now`]. Injection is provided so
/// tests and later synchronous callers can control elapsed time without using
/// wall-clock arithmetic.
pub trait ActivityMonotonicClock: Send + Sync {
    fn now(&self) -> Instant;
}

impl<F> ActivityMonotonicClock for F
where
    F: Fn() -> Instant + Send + Sync,
{
    fn now(&self) -> Instant {
        self()
    }
}

impl<T> ActivityMonotonicClock for Arc<T>
where
    T: ActivityMonotonicClock + ?Sized,
{
    fn now(&self) -> Instant {
        (**self).now()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SystemMonotonicClock;

impl ActivityMonotonicClock for SystemMonotonicClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// The result of one scope transition attempt.
///
/// `Rejected` means the typed update was attempted but the emitter's bounded
/// sink refused it. It does not roll back the scope state. `NotAttempted` is
/// used for an idempotent repeated non-terminal state; no emitter call was
/// made in that case.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActivityEmission {
    NotAttempted,
    Accepted,
    Rejected(ActivityEmitError),
}

impl ActivityEmission {
    pub const fn attempted(self) -> bool {
        !matches!(self, Self::NotAttempted)
    }

    pub const fn accepted(self) -> bool {
        matches!(self, Self::Accepted)
    }

    pub const fn rejected(self) -> bool {
        matches!(self, Self::Rejected(_))
    }
}

/// Fixed errors for invalid scope operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScopeError {
    InvalidTransition,
    AlreadyTerminal,
    InvariantViolation,
}

impl fmt::Display for ScopeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::InvalidTransition => "invalid_transition",
            Self::AlreadyTerminal => "already_terminal",
            Self::InvariantViolation => "invariant_violation",
        };
        formatter.write_str(name)
    }
}

impl std::error::Error for ScopeError {}

struct ScopeState {
    current: ActivityState,
    terminal: bool,
}

struct ScopeInner {
    operation: ActivityOperation,
    operation_id: Uuid,
    started_at: Instant,
    default_summary: ActivitySummary,
    clock: Arc<dyn ActivityMonotonicClock>,
    emitter: Arc<dyn ActivityEmitter>,
    state: Mutex<ScopeState>,
    started_emission: OnceLock<ActivityEmission>,
}

/// A shareable state scope for one activity operation.
///
/// Cloning this handle shares the same operation ID, start `Instant`, state,
/// terminal bit, and emitter. It does not create a second scope or infer any
/// terminal state when a handle is dropped.
#[derive(Clone)]
pub struct ActivityScope {
    inner: Arc<ScopeInner>,
}

impl ActivityScope {
    /// Creates a scope with an empty typed summary and a fresh UUID.
    ///
    /// Construction immediately attempts exactly one `started` emission. The
    /// result can be inspected with [`Self::started_emission`]. A rejected
    /// start does not prevent later valid transitions.
    pub fn new<E>(operation: ActivityOperation, emitter: E) -> Self
    where
        E: ActivityEmitter + 'static,
    {
        Self::with_summary(operation, ActivitySummary::empty(), emitter)
    }

    /// Creates a scope with a typed default summary and a fresh UUID.
    pub fn with_summary<E>(
        operation: ActivityOperation,
        summary: ActivitySummary,
        emitter: E,
    ) -> Self
    where
        E: ActivityEmitter + 'static,
    {
        Self::with_summary_and_clock(operation, summary, emitter, SystemMonotonicClock)
    }

    /// Creates a scope with a fresh UUID and an injected monotonic clock.
    pub fn with_clock<C, E>(operation: ActivityOperation, emitter: E, clock: C) -> Self
    where
        C: ActivityMonotonicClock + 'static,
        E: ActivityEmitter + 'static,
    {
        Self::with_summary_and_clock(operation, ActivitySummary::empty(), emitter, clock)
    }

    /// Creates a scope with a typed default summary, a fresh UUID, and an
    /// injected monotonic clock.
    pub fn with_summary_and_clock<C, E>(
        operation: ActivityOperation,
        summary: ActivitySummary,
        emitter: E,
        clock: C,
    ) -> Self
    where
        C: ActivityMonotonicClock + 'static,
        E: ActivityEmitter + 'static,
    {
        Self::new_with_identity(
            operation,
            Uuid::new_v4(),
            summary,
            Arc::new(clock),
            Arc::new(emitter),
        )
    }

    /// Alias for [`Self::with_summary`] with a constructor-oriented name.
    pub fn new_with_summary<E>(
        operation: ActivityOperation,
        summary: ActivitySummary,
        emitter: E,
    ) -> Self
    where
        E: ActivityEmitter + 'static,
    {
        Self::with_summary(operation, summary, emitter)
    }

    /// Returns the operation identity shared by every emitted update.
    pub fn operation_id(&self) -> Uuid {
        self.inner.operation_id
    }

    pub fn operation(&self) -> ActivityOperation {
        self.inner.operation
    }

    pub fn started_at(&self) -> Instant {
        self.inner.started_at
    }

    /// Returns the result of the one construction-time `started` attempt.
    pub fn started_emission(&self) -> ActivityEmission {
        *self
            .inner
            .started_emission
            .get()
            .expect("started emission is initialized during scope construction")
    }

    pub fn state(&self) -> Result<ActivityState, ScopeError> {
        Ok(self.lock_state()?.current)
    }

    pub fn is_terminal(&self) -> Result<bool, ScopeError> {
        Ok(self.lock_state()?.terminal)
    }

    /// Applies a typed state transition using the supplied S01 summary.
    ///
    /// Repeating the same non-terminal state is an idempotent no-op and
    /// returns [`ActivityEmission::NotAttempted`]. `started` cannot be
    /// reissued, and no transition is accepted after a terminal state.
    pub fn transition(
        &self,
        next: ActivityState,
        summary: ActivitySummary,
    ) -> Result<ActivityEmission, ScopeError> {
        let mut state = self.lock_state()?;
        if state.terminal {
            return Err(ScopeError::AlreadyTerminal);
        }
        if next == ActivityState::Started {
            return Err(ScopeError::InvalidTransition);
        }
        if next == state.current {
            return Ok(ActivityEmission::NotAttempted);
        }
        if !is_valid_transition(state.current, next) {
            return Err(ScopeError::InvalidTransition);
        }

        let duration_ms = terminal_duration(next, self.inner.started_at, &*self.inner.clock);
        let update = ActivityUpdate::new(
            self.inner.operation_id,
            self.inner.operation,
            next,
            duration_ms,
            summary,
        )
        .map_err(|_| ScopeError::InvariantViolation)?;

        state.current = next;
        if is_terminal(next) {
            state.terminal = true;
        }

        Ok(emit(&*self.inner.emitter, update))
    }

    /// Finishes the scope with an explicitly terminal S01 state.
    ///
    /// Non-terminal values are rejected before the scope state or emitter is
    /// touched. The caller supplies the typed summary, including
    /// `ActivitySummary::failure(ActivityErrorKind::ApprovalDenied)` when
    /// approval denial is the explicit operation result.
    pub fn finish(
        &self,
        terminal_state: ActivityState,
        summary: ActivitySummary,
    ) -> Result<ActivityEmission, ScopeError> {
        if !is_terminal(terminal_state) {
            return Err(ScopeError::InvalidTransition);
        }
        self.transition(terminal_state, summary)
    }

    pub fn running(&self) -> Result<ActivityEmission, ScopeError> {
        self.transition(ActivityState::Running, self.inner.default_summary.clone())
    }

    pub fn waiting_approval(&self) -> Result<ActivityEmission, ScopeError> {
        self.transition(
            ActivityState::WaitingApproval,
            self.inner.default_summary.clone(),
        )
    }

    pub fn complete(&self) -> Result<ActivityEmission, ScopeError> {
        self.finish(ActivityState::Completed, self.inner.default_summary.clone())
    }

    pub fn fail(&self) -> Result<ActivityEmission, ScopeError> {
        self.finish(ActivityState::Failed, self.inner.default_summary.clone())
    }

    pub fn cancel(&self) -> Result<ActivityEmission, ScopeError> {
        self.finish(ActivityState::Cancelled, self.inner.default_summary.clone())
    }

    pub fn complete_with_summary(
        &self,
        summary: ActivitySummary,
    ) -> Result<ActivityEmission, ScopeError> {
        self.finish(ActivityState::Completed, summary)
    }

    pub fn fail_with_summary(
        &self,
        summary: ActivitySummary,
    ) -> Result<ActivityEmission, ScopeError> {
        self.finish(ActivityState::Failed, summary)
    }

    pub fn cancel_with_summary(
        &self,
        summary: ActivitySummary,
    ) -> Result<ActivityEmission, ScopeError> {
        self.finish(ActivityState::Cancelled, summary)
    }

    fn new_with_identity(
        operation: ActivityOperation,
        operation_id: Uuid,
        default_summary: ActivitySummary,
        clock: Arc<dyn ActivityMonotonicClock>,
        emitter: Arc<dyn ActivityEmitter>,
    ) -> Self {
        let started_at = clock.now();
        let inner = Arc::new(ScopeInner {
            operation,
            operation_id,
            started_at,
            default_summary: default_summary.clone(),
            clock,
            emitter,
            state: Mutex::new(ScopeState {
                current: ActivityState::Started,
                terminal: false,
            }),
            started_emission: OnceLock::new(),
        });
        let scope = Self { inner };

        let _state = scope
            .inner
            .state
            .lock()
            .expect("new scope state cannot be poisoned");
        let update = ActivityUpdate::new(
            scope.inner.operation_id,
            scope.inner.operation,
            ActivityState::Started,
            None,
            default_summary,
        )
        .expect("typed scope start must satisfy the S01 contract");
        let emission = emit(&*scope.inner.emitter, update);
        drop(_state);
        scope
            .inner
            .started_emission
            .set(emission)
            .expect("started emission is set exactly once");
        scope
    }

    #[cfg(test)]
    fn for_test<C, E>(
        operation: ActivityOperation,
        operation_id: Uuid,
        started_at: Instant,
        default_summary: ActivitySummary,
        clock: Arc<C>,
        emitter: Arc<E>,
    ) -> Self
    where
        C: ActivityMonotonicClock + 'static,
        E: ActivityEmitter + 'static,
    {
        let inner = Arc::new(ScopeInner {
            operation,
            operation_id,
            started_at,
            default_summary: default_summary.clone(),
            clock: Arc::new(clock),
            emitter: Arc::new(emitter),
            state: Mutex::new(ScopeState {
                current: ActivityState::Started,
                terminal: false,
            }),
            started_emission: OnceLock::new(),
        });
        let scope = Self { inner };
        let _state = scope.inner.state.lock().unwrap();
        let update = ActivityUpdate::new(
            scope.inner.operation_id,
            scope.inner.operation,
            ActivityState::Started,
            None,
            default_summary,
        )
        .unwrap();
        let emission = emit(&*scope.inner.emitter, update);
        drop(_state);
        scope.inner.started_emission.set(emission).unwrap();
        scope
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, ScopeState>, ScopeError> {
        self.inner
            .state
            .lock()
            .map_err(|_| ScopeError::InvariantViolation)
    }
}

fn emit(emitter: &dyn ActivityEmitter, update: ActivityUpdate) -> ActivityEmission {
    match emitter.try_emit(update) {
        Ok(()) => ActivityEmission::Accepted,
        Err(error) => ActivityEmission::Rejected(error),
    }
}

fn is_terminal(state: ActivityState) -> bool {
    matches!(
        state,
        ActivityState::Completed | ActivityState::Failed | ActivityState::Cancelled
    )
}

fn is_valid_transition(current: ActivityState, next: ActivityState) -> bool {
    match current {
        ActivityState::Started => matches!(
            next,
            ActivityState::WaitingApproval
                | ActivityState::Running
                | ActivityState::Completed
                | ActivityState::Failed
                | ActivityState::Cancelled
        ),
        ActivityState::WaitingApproval => matches!(
            next,
            ActivityState::Running | ActivityState::Failed | ActivityState::Cancelled
        ),
        ActivityState::Running => matches!(
            next,
            ActivityState::WaitingApproval
                | ActivityState::Completed
                | ActivityState::Failed
                | ActivityState::Cancelled
        ),
        ActivityState::Completed | ActivityState::Failed | ActivityState::Cancelled => false,
    }
}

fn terminal_duration(
    state: ActivityState,
    started_at: Instant,
    clock: &dyn ActivityMonotonicClock,
) -> Option<u64> {
    is_terminal(state).then(|| elapsed_millis_saturating(started_at, clock.now()))
}

fn elapsed_millis_saturating(started_at: Instant, now: Instant) -> u64 {
    let elapsed = now
        .checked_duration_since(started_at)
        .unwrap_or(Duration::ZERO);
    duration_millis_saturating(elapsed)
}

fn duration_millis_saturating(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier, Mutex, mpsc};
    use std::thread;

    use super::*;
    use crate::activity::contract::{
        ActivityErrorKind, ActivityOperation, ActivityRemote, ActivityState, ActivitySummary,
    };

    const OPERATION_ID: Uuid = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0401);

    struct EmissionGate {
        state: ActivityState,
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }

    #[derive(Default)]
    struct RecordingEmitter {
        attempts: Mutex<Vec<ActivityUpdate>>,
        accepted: Mutex<Vec<ActivityUpdate>>,
        rejection: Mutex<Option<ActivityEmitError>>,
        gate: Mutex<Option<EmissionGate>>,
    }

    impl RecordingEmitter {
        fn attempts(&self) -> Vec<ActivityUpdate> {
            self.attempts.lock().unwrap().clone()
        }

        fn accepted(&self) -> Vec<ActivityUpdate> {
            self.accepted.lock().unwrap().clone()
        }

        fn reject_with(&self, error: ActivityEmitError) {
            *self.rejection.lock().unwrap() = Some(error);
        }

        fn accept(&self) {
            *self.rejection.lock().unwrap() = None;
        }

        fn gate(&self, state: ActivityState) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
            let (entered_sender, entered_receiver) = mpsc::channel();
            let (release_sender, release_receiver) = mpsc::channel();
            *self.gate.lock().unwrap() = Some(EmissionGate {
                state,
                entered: entered_sender,
                release: release_receiver,
            });
            (entered_receiver, release_sender)
        }
    }

    impl ActivityEmitter for RecordingEmitter {
        fn try_emit(&self, update: ActivityUpdate) -> Result<(), ActivityEmitError> {
            self.attempts.lock().unwrap().push(update.clone());
            let gate = {
                let mut configured_gate = self.gate.lock().unwrap();
                configured_gate
                    .as_ref()
                    .is_some_and(|gate| gate.state == update.state())
                    .then(|| configured_gate.take().unwrap())
            };
            if let Some(gate) = gate {
                gate.entered.send(()).unwrap();
                gate.release.recv().unwrap();
            }
            if let Some(error) = *self.rejection.lock().unwrap() {
                return Err(error);
            }
            self.accepted.lock().unwrap().push(update);
            Ok(())
        }
    }

    struct FixedClock {
        now: Mutex<Instant>,
    }

    impl FixedClock {
        fn new(now: Instant) -> Self {
            Self {
                now: Mutex::new(now),
            }
        }

        fn set(&self, now: Instant) {
            *self.now.lock().unwrap() = now;
        }
    }

    impl ActivityMonotonicClock for FixedClock {
        fn now(&self) -> Instant {
            *self.now.lock().unwrap()
        }
    }

    fn make_scope(emitter: Arc<RecordingEmitter>, clock: Arc<FixedClock>) -> ActivityScope {
        let started_at = clock.now();
        ActivityScope::for_test(
            ActivityOperation::GitPull,
            OPERATION_ID,
            started_at,
            ActivitySummary::git(ActivityRemote::Origin),
            clock,
            emitter,
        )
    }

    fn states(events: &[ActivityUpdate]) -> Vec<ActivityState> {
        events.iter().map(ActivityUpdate::state).collect()
    }

    #[test]
    fn started_is_attempted_once_and_shared_handles_keep_identity() {
        let emitter = Arc::new(RecordingEmitter::default());
        let clock = Arc::new(FixedClock::new(Instant::now()));
        let scope = make_scope(Arc::clone(&emitter), Arc::clone(&clock));
        let shared = scope.clone();

        assert_eq!(scope.started_emission(), ActivityEmission::Accepted);
        assert_eq!(scope.operation_id(), OPERATION_ID);
        assert_eq!(shared.operation_id(), OPERATION_ID);
        assert_eq!(scope.operation(), ActivityOperation::GitPull);
        assert_eq!(scope.state().unwrap(), ActivityState::Started);
        assert_eq!(emitter.attempts().len(), 1);
        assert_eq!(states(&emitter.attempts()), [ActivityState::Started]);

        shared.running().unwrap();
        scope.complete().unwrap();
        let attempts = emitter.attempts();
        assert_eq!(attempts.len(), 3);
        assert!(attempts.iter().all(|event| {
            event.operation_id() == OPERATION_ID && event.operation() == ActivityOperation::GitPull
        }));
        drop(shared);
        drop(scope);
        assert_eq!(emitter.attempts().len(), 3);
    }

    #[test]
    fn normal_constructor_uses_fresh_identity_and_clones_share_it() {
        let first_emitter = Arc::new(RecordingEmitter::default());
        let first = ActivityScope::new(ActivityOperation::ReadFile, Arc::clone(&first_emitter));
        let first_clone = first.clone();
        let second_emitter = Arc::new(RecordingEmitter::default());
        let second = ActivityScope::new(ActivityOperation::ReadFile, Arc::clone(&second_emitter));

        assert_ne!(first.operation_id(), second.operation_id());
        assert_eq!(first_clone.operation_id(), first.operation_id());
        assert_eq!(first_clone.started_at(), first.started_at());
        assert_eq!(first.started_emission(), ActivityEmission::Accepted);
        assert_eq!(second.started_emission(), ActivityEmission::Accepted);
        assert_eq!(first_emitter.attempts().len(), 1);
        assert_eq!(second_emitter.attempts().len(), 1);
        assert_eq!(first_emitter.attempts()[0].state(), ActivityState::Started);
        assert_eq!(second_emitter.attempts()[0].state(), ActivityState::Started);
    }

    #[test]
    fn normal_and_short_terminal_paths_have_typed_states_and_durations() {
        let emitter = Arc::new(RecordingEmitter::default());
        let clock = Arc::new(FixedClock::new(Instant::now()));
        let scope = make_scope(Arc::clone(&emitter), Arc::clone(&clock));
        scope.running().unwrap();
        clock.set(scope.started_at() + Duration::from_millis(27));
        scope.complete().unwrap();
        assert_eq!(
            states(&emitter.accepted()),
            [
                ActivityState::Started,
                ActivityState::Running,
                ActivityState::Completed
            ]
        );
        assert_eq!(emitter.accepted()[0].duration_ms(), None);
        assert_eq!(emitter.accepted()[1].duration_ms(), None);
        assert_eq!(emitter.accepted()[2].duration_ms(), Some(27));

        let short_failed_emitter = Arc::new(RecordingEmitter::default());
        let short_failed_clock = Arc::new(FixedClock::new(Instant::now()));
        let short_failed = make_scope(
            Arc::clone(&short_failed_emitter),
            Arc::clone(&short_failed_clock),
        );
        short_failed
            .fail_with_summary(ActivitySummary::failure(ActivityErrorKind::OperationFailed))
            .unwrap();
        assert_eq!(
            short_failed_emitter.accepted()[1].state(),
            ActivityState::Failed
        );
        assert_eq!(short_failed_emitter.accepted()[1].duration_ms(), Some(0));

        let cancelled_emitter = Arc::new(RecordingEmitter::default());
        let cancelled_clock = Arc::new(FixedClock::new(Instant::now()));
        let cancelled = make_scope(Arc::clone(&cancelled_emitter), cancelled_clock);
        cancelled.cancel().unwrap();
        assert_eq!(
            cancelled_emitter.accepted()[1].state(),
            ActivityState::Cancelled
        );
        assert_eq!(cancelled_emitter.accepted()[1].duration_ms(), Some(0));
    }

    #[test]
    fn approval_waiting_running_and_denial_paths_are_explicit() {
        let emitter = Arc::new(RecordingEmitter::default());
        let clock = Arc::new(FixedClock::new(Instant::now()));
        let scope = make_scope(Arc::clone(&emitter), Arc::clone(&clock));
        scope.waiting_approval().unwrap();
        scope.running().unwrap();
        scope.waiting_approval().unwrap();
        scope.running().unwrap();
        scope.complete().unwrap();
        assert_eq!(
            states(&emitter.accepted()),
            [
                ActivityState::Started,
                ActivityState::WaitingApproval,
                ActivityState::Running,
                ActivityState::WaitingApproval,
                ActivityState::Running,
                ActivityState::Completed
            ]
        );

        let denied_emitter = Arc::new(RecordingEmitter::default());
        let denied = make_scope(
            Arc::clone(&denied_emitter),
            Arc::new(FixedClock::new(Instant::now())),
        );
        denied.waiting_approval().unwrap();
        denied
            .finish(
                ActivityState::Failed,
                ActivitySummary::failure(ActivityErrorKind::ApprovalDenied),
            )
            .unwrap();
        assert_eq!(
            states(&denied_emitter.accepted()),
            [
                ActivityState::Started,
                ActivityState::WaitingApproval,
                ActivityState::Failed
            ]
        );
    }

    #[test]
    fn repeated_nonterminal_state_is_a_noop_and_finish_rejects_nonterminal() {
        let emitter = Arc::new(RecordingEmitter::default());
        let scope = make_scope(
            Arc::clone(&emitter),
            Arc::new(FixedClock::new(Instant::now())),
        );
        scope.running().unwrap();
        assert_eq!(scope.running().unwrap(), ActivityEmission::NotAttempted);
        assert_eq!(emitter.attempts().len(), 2);
        assert_eq!(
            scope.finish(ActivityState::WaitingApproval, ActivitySummary::empty()),
            Err(ScopeError::InvalidTransition)
        );
        assert_eq!(scope.state().unwrap(), ActivityState::Running);
        assert_eq!(emitter.attempts().len(), 2);
    }

    #[test]
    fn terminal_state_suppresses_all_later_updates() {
        let emitter = Arc::new(RecordingEmitter::default());
        let scope = make_scope(
            Arc::clone(&emitter),
            Arc::new(FixedClock::new(Instant::now())),
        );
        scope.complete().unwrap();
        let before = emitter.attempts().len();
        assert_eq!(scope.waiting_approval(), Err(ScopeError::AlreadyTerminal));
        assert_eq!(scope.running(), Err(ScopeError::AlreadyTerminal));
        assert_eq!(
            scope.finish(ActivityState::Failed, ActivitySummary::empty()),
            Err(ScopeError::AlreadyTerminal)
        );
        assert_eq!(emitter.attempts().len(), before);
        assert_eq!(scope.state().unwrap(), ActivityState::Completed);
        assert!(scope.is_terminal().unwrap());
    }

    #[test]
    fn emitter_rejection_is_reported_without_changing_scope_policy() {
        let emitter = Arc::new(RecordingEmitter::default());
        emitter.reject_with(ActivityEmitError::Full);
        let scope = make_scope(
            Arc::clone(&emitter),
            Arc::new(FixedClock::new(Instant::now())),
        );
        assert_eq!(
            scope.started_emission(),
            ActivityEmission::Rejected(ActivityEmitError::Full)
        );
        assert_eq!(
            scope.running().unwrap(),
            ActivityEmission::Rejected(ActivityEmitError::Full)
        );

        emitter.accept();
        assert_eq!(scope.complete().unwrap(), ActivityEmission::Accepted);
        assert_eq!(scope.state().unwrap(), ActivityState::Completed);
        assert_eq!(
            states(&emitter.attempts()),
            [
                ActivityState::Started,
                ActivityState::Running,
                ActivityState::Completed
            ]
        );

        emitter.reject_with(ActivityEmitError::Closed);
        let terminal_rejected = make_scope(
            Arc::clone(&emitter),
            Arc::new(FixedClock::new(Instant::now())),
        );
        assert_eq!(
            terminal_rejected.finish(ActivityState::Failed, ActivitySummary::empty()),
            Ok(ActivityEmission::Rejected(ActivityEmitError::Closed))
        );
        let attempts = emitter.attempts();
        let terminal_count = attempts
            .iter()
            .filter(|event| event.state() == ActivityState::Failed)
            .count();
        assert_eq!(terminal_count, 1);
        assert_eq!(
            terminal_rejected.finish(ActivityState::Cancelled, ActivitySummary::empty()),
            Err(ScopeError::AlreadyTerminal)
        );
        assert_eq!(emitter.attempts().len(), attempts.len());
        assert!(
            !emitter
                .accepted()
                .iter()
                .any(|event| event.state() == ActivityState::Failed)
        );
    }

    fn terminal_race_order_is_controlled_for_both_winners(
        first: ActivityState,
        second: ActivityState,
    ) {
        let emitter = Arc::new(RecordingEmitter::default());
        let (entered, release) = emitter.gate(first);
        let scope = Arc::new(make_scope(
            Arc::clone(&emitter),
            Arc::new(FixedClock::new(Instant::now())),
        ));
        let first_scope = Arc::clone(&scope);
        let first_handle =
            thread::spawn(move || first_scope.finish(first, ActivitySummary::empty()));
        entered.recv().unwrap();

        let second_scope = Arc::clone(&scope);
        let (second_started_sender, second_started_receiver) = mpsc::channel();
        let second_handle = thread::spawn(move || {
            second_started_sender.send(()).unwrap();
            second_scope.finish(second, ActivitySummary::empty())
        });
        second_started_receiver.recv().unwrap();
        release.send(()).unwrap();

        assert_eq!(first_handle.join().unwrap(), Ok(ActivityEmission::Accepted));
        assert_eq!(
            second_handle.join().unwrap(),
            Err(ScopeError::AlreadyTerminal)
        );
        assert_eq!(
            emitter
                .attempts()
                .iter()
                .filter(|event| matches!(
                    event.state(),
                    ActivityState::Completed | ActivityState::Cancelled
                ))
                .count(),
            1
        );
        assert_eq!(
            emitter.attempts().last().map(ActivityUpdate::state),
            Some(first)
        );
    }

    #[test]
    fn completed_wins_a_controlled_terminal_race() {
        terminal_race_order_is_controlled_for_both_winners(
            ActivityState::Completed,
            ActivityState::Cancelled,
        );
    }

    #[test]
    fn cancelled_wins_a_controlled_terminal_race() {
        terminal_race_order_is_controlled_for_both_winners(
            ActivityState::Cancelled,
            ActivityState::Completed,
        );
    }

    #[test]
    fn concurrent_nonterminal_and_finish_never_emits_nonterminal_after_terminal() {
        let emitter = Arc::new(RecordingEmitter::default());
        let scope = Arc::new(make_scope(
            Arc::clone(&emitter),
            Arc::new(FixedClock::new(Instant::now())),
        ));
        scope.running().unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let waiting_scope = Arc::clone(&scope);
        let waiting_barrier = Arc::clone(&barrier);
        let waiting = thread::spawn(move || {
            waiting_barrier.wait();
            waiting_scope.waiting_approval()
        });
        let finishing_scope = Arc::clone(&scope);
        let finishing_barrier = Arc::clone(&barrier);
        let finishing = thread::spawn(move || {
            finishing_barrier.wait();
            finishing_scope.cancel()
        });
        barrier.wait();
        let waiting_result = waiting.join().unwrap();
        let finishing_result = finishing.join().unwrap();
        assert!(matches!(
            waiting_result,
            Ok(ActivityEmission::Accepted) | Err(ScopeError::AlreadyTerminal)
        ));
        assert_eq!(finishing_result, Ok(ActivityEmission::Accepted));
        let attempts = emitter.attempts();
        let terminal_index = attempts
            .iter()
            .position(|event| event.state() == ActivityState::Cancelled)
            .unwrap();
        assert!(
            attempts[terminal_index + 1..]
                .iter()
                .all(|event| is_terminal(event.state()))
        );
    }

    #[test]
    fn controlled_nonterminal_and_terminal_order_is_serialized_in_both_directions() {
        let waiting_first_emitter = Arc::new(RecordingEmitter::default());
        let waiting_first_scope = Arc::new(make_scope(
            Arc::clone(&waiting_first_emitter),
            Arc::new(FixedClock::new(Instant::now())),
        ));
        waiting_first_scope.running().unwrap();
        let (waiting_entered, waiting_release) =
            waiting_first_emitter.gate(ActivityState::WaitingApproval);
        let waiting_scope = Arc::clone(&waiting_first_scope);
        let waiting_handle = thread::spawn(move || waiting_scope.waiting_approval());
        waiting_entered.recv().unwrap();
        let terminal_scope = Arc::clone(&waiting_first_scope);
        let (terminal_started_sender, terminal_started_receiver) = mpsc::channel();
        let terminal_handle = thread::spawn(move || {
            terminal_started_sender.send(()).unwrap();
            terminal_scope.cancel()
        });
        terminal_started_receiver.recv().unwrap();
        waiting_release.send(()).unwrap();
        assert_eq!(
            waiting_handle.join().unwrap(),
            Ok(ActivityEmission::Accepted)
        );
        assert_eq!(
            terminal_handle.join().unwrap(),
            Ok(ActivityEmission::Accepted)
        );
        assert_eq!(
            states(&waiting_first_emitter.attempts()),
            [
                ActivityState::Started,
                ActivityState::Running,
                ActivityState::WaitingApproval,
                ActivityState::Cancelled
            ]
        );

        let terminal_first_emitter = Arc::new(RecordingEmitter::default());
        let terminal_first_scope = Arc::new(make_scope(
            Arc::clone(&terminal_first_emitter),
            Arc::new(FixedClock::new(Instant::now())),
        ));
        terminal_first_scope.running().unwrap();
        let (terminal_entered, terminal_release) =
            terminal_first_emitter.gate(ActivityState::Cancelled);
        let terminal_scope = Arc::clone(&terminal_first_scope);
        let terminal_handle = thread::spawn(move || terminal_scope.cancel());
        terminal_entered.recv().unwrap();
        let waiting_scope = Arc::clone(&terminal_first_scope);
        let (waiting_started_sender, waiting_started_receiver) = mpsc::channel();
        let waiting_handle = thread::spawn(move || {
            waiting_started_sender.send(()).unwrap();
            waiting_scope.waiting_approval()
        });
        waiting_started_receiver.recv().unwrap();
        terminal_release.send(()).unwrap();
        assert_eq!(
            terminal_handle.join().unwrap(),
            Ok(ActivityEmission::Accepted)
        );
        assert_eq!(
            waiting_handle.join().unwrap(),
            Err(ScopeError::AlreadyTerminal)
        );
        let attempts = terminal_first_emitter.attempts();
        assert_eq!(
            states(&attempts),
            [
                ActivityState::Started,
                ActivityState::Running,
                ActivityState::Cancelled
            ]
        );
        let terminal_index = attempts
            .iter()
            .position(|event| event.state() == ActivityState::Cancelled)
            .unwrap();
        assert!(
            attempts[terminal_index + 1..]
                .iter()
                .all(|event| is_terminal(event.state()))
        );
    }

    #[test]
    fn duration_uses_monotonic_time_and_saturates_without_instant_overflow() {
        let start = Instant::now();
        assert_eq!(elapsed_millis_saturating(start, start), 0);
        assert_eq!(
            elapsed_millis_saturating(start + Duration::from_millis(10), start),
            0
        );
        assert_eq!(
            duration_millis_saturating(Duration::from_secs(u64::MAX)),
            u64::MAX
        );

        let emitter = Arc::new(RecordingEmitter::default());
        let clock = Arc::new(FixedClock::new(start + Duration::from_millis(9)));
        let scope = ActivityScope::for_test(
            ActivityOperation::ReadFile,
            OPERATION_ID,
            start,
            ActivitySummary::empty(),
            Arc::clone(&clock),
            Arc::clone(&emitter),
        );
        scope.complete().unwrap();
        assert_eq!(emitter.accepted()[1].duration_ms(), Some(9));
    }

    #[test]
    fn drop_does_not_infer_a_terminal_event() {
        let emitter = Arc::new(RecordingEmitter::default());
        let scope = make_scope(
            Arc::clone(&emitter),
            Arc::new(FixedClock::new(Instant::now())),
        );
        let shared = scope.clone();
        drop(scope);
        drop(shared);
        assert_eq!(states(&emitter.attempts()), [ActivityState::Started]);
    }
}
