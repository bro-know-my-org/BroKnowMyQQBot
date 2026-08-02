//! Adapter supervision and event dispatch.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use thiserror::Error;
use tokio::{
    sync::{Semaphore, mpsc, oneshot},
    task::JoinSet,
    time::{sleep, timeout},
};
use tracing::{error, info, warn};

use crate::{
    Adapter, AdapterError, AdapterId, Context, DedupClaim, DedupKey, DedupStore, Event,
    EventSender, MemoryDedupStore, RuntimeObserver, RuntimePhase, ShutdownSignal,
    adapter::is_valid_adapter_id,
    metrics::{HandlerOutcome, MAX_ADAPTER_METRIC_SERIES},
};

const MAX_REGISTERED_HANDLERS: usize = 4096;
const MAX_HANDLER_NAME_BYTES: usize = 128;
const MAX_RUNTIME_QUEUE_CAPACITY: usize = 16_384;
const DEDUP_CLAIM_TIMEOUT: Duration = Duration::from_secs(5);
const DEDUP_COMMIT_MAX_ATTEMPTS: usize = 4;
const DEDUP_COMMIT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(1);
const DEDUP_COMMIT_INITIAL_BACKOFF: Duration = Duration::from_millis(25);
const DEDUP_BACKGROUND_RETRY_DELAY: Duration = Duration::from_secs(1);
const DEDUP_RELEASE_MAX_ATTEMPTS: usize = 4;
const DEDUP_RELEASE_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(1);
const DEDUP_RELEASE_INITIAL_BACKOFF: Duration = Duration::from_millis(25);
const EVENT_ACK_MAX_ATTEMPTS: usize = 4;
const EVENT_ACK_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(1);
const EVENT_ACK_INITIAL_BACKOFF: Duration = Duration::from_millis(25);
const MAX_CONCURRENT_DEDUP_FINALIZATIONS: usize = 32;

#[derive(Debug, Error)]
pub enum HandlerError {
    #[error("handler failed: {0}")]
    Failed(String),
}

#[async_trait]
pub trait EventHandler: Send + Sync + 'static {
    fn name(&self) -> &str;

    async fn handle(&self, context: Context, event: &Event) -> Result<(), HandlerError>;
}

#[derive(Debug, Clone, Default)]
pub struct HandlerPolicy {
    timeout: Option<Duration>,
    max_concurrency: Option<usize>,
}

impl HandlerPolicy {
    /// Sets the end-to-end dispatch budget for this handler, including time
    /// waiting for its concurrency permit. This bounds overloaded handlers
    /// instead of allowing an unbounded admission queue outside the timeout.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    #[must_use]
    pub fn max_concurrency(mut self, max_concurrency: usize) -> Self {
        self.max_concurrency = Some(max_concurrency.max(1));
        self
    }
}

struct RegisteredHandler {
    name: String,
    handler: Arc<dyn EventHandler>,
    policy: HandlerPolicy,
    concurrency: Option<Arc<Semaphore>>,
}

impl std::fmt::Debug for RegisteredHandler {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RegisteredHandler")
            .field("name", &self.name)
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("duplicate adapter ID `{0}`")]
    DuplicateAdapter(AdapterId),
    #[error("adapter at index {index} has an empty, oversized, or control-character ID")]
    InvalidAdapterId { index: usize },
    #[error("runtime requires at least one adapter")]
    NoAdapters,
    #[error("runtime cannot register more than {MAX_ADAPTER_METRIC_SERIES} adapters")]
    TooManyAdapters,
    #[error("runtime observer is already attached to another active runtime")]
    ObserverInUse,
    #[error("runtime cannot register more than {MAX_REGISTERED_HANDLERS} handlers")]
    TooManyHandlers,
    #[error("handler at index {index} has an empty, oversized, or control-character name")]
    InvalidHandlerName { index: usize },
    #[error("duplicate handler name `{0}`")]
    DuplicateHandler(String),
    #[error("all adapters stopped before shutdown")]
    AllAdaptersStopped,
    #[error("adapter task failed to join: {0}")]
    AdapterJoin(String),
    #[error("adapter `{adapter_id}` stopped with an error: {message}")]
    AdapterStopped {
        adapter_id: AdapterId,
        message: String,
    },
    #[error("event dispatch task failed to join: {0}")]
    EventJoin(String),
    #[error("event dispatch drain exceeded the runtime shutdown deadline")]
    EventDrainTimeout,
    #[error("adapter event drain exceeded the runtime shutdown deadline")]
    AdapterDrainTimeout,
    #[error("adapter shutdown exceeded the runtime shutdown deadline")]
    AdapterShutdownTimeout,
    #[error("event concurrency exceeds Tokio's semaphore limit")]
    EventConcurrencyTooLarge,
    #[error("runtime queue capacity cannot exceed {MAX_RUNTIME_QUEUE_CAPACITY}")]
    QueueCapacityTooLarge,
    #[error("handler at index {index} has a concurrency limit above Tokio's semaphore limit")]
    HandlerConcurrencyTooLarge { index: usize },
    #[error("failed to finalize {remaining} deduplication claim(s) during shutdown")]
    DedupFinalization { remaining: usize },
    #[error("deduplication recovery supervisor failed to join: {0}")]
    DedupRecoveryJoin(String),
    #[error("deduplication recovery supervisor exceeded the shutdown deadline")]
    DedupRecoveryTimeout,
}

pub struct RuntimeBuilder {
    queue_capacity: usize,
    handler_timeout: Duration,
    shutdown_timeout: Duration,
    event_concurrency: usize,
    adapters: Vec<Arc<dyn Adapter>>,
    handlers: Vec<(Arc<dyn EventHandler>, HandlerPolicy)>,
    dedup_store: Arc<dyn DedupStore>,
    observer: RuntimeObserver,
}

impl std::fmt::Debug for RuntimeBuilder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RuntimeBuilder")
            .field("queue_capacity", &self.queue_capacity)
            .field("handler_timeout", &self.handler_timeout)
            .field("shutdown_timeout", &self.shutdown_timeout)
            .field("event_concurrency", &self.event_concurrency)
            .field("adapter_count", &self.adapters.len())
            .field("handler_count", &self.handlers.len())
            .field("dedup_store", &"configured")
            .field("observer", &self.observer)
            .finish()
    }
}

impl Default for RuntimeBuilder {
    fn default() -> Self {
        Self {
            queue_capacity: 32,
            handler_timeout: Duration::from_secs(30),
            shutdown_timeout: Duration::from_secs(20),
            event_concurrency: 32,
            adapters: Vec::new(),
            handlers: Vec::new(),
            dedup_store: Arc::new(
                MemoryDedupStore::try_new(32_768).expect("default dedup capacity is non-zero"),
            ),
            observer: RuntimeObserver::new(),
        }
    }
}

impl RuntimeBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn queue_capacity(mut self, capacity: usize) -> Self {
        self.queue_capacity = capacity.max(1);
        self
    }

    #[must_use]
    pub fn handler_timeout(mut self, duration: Duration) -> Self {
        self.handler_timeout = duration;
        self
    }

    #[must_use]
    pub fn shutdown_timeout(mut self, duration: Duration) -> Self {
        self.shutdown_timeout = duration;
        self
    }

    #[must_use]
    pub fn event_concurrency(mut self, concurrency: usize) -> Self {
        self.event_concurrency = concurrency.max(1);
        self
    }

    #[must_use]
    pub fn adapter(mut self, adapter: Arc<dyn Adapter>) -> Self {
        self.adapters.push(adapter);
        self
    }

    #[must_use]
    pub fn handler(mut self, handler: Arc<dyn EventHandler>) -> Self {
        self.handlers.push((handler, HandlerPolicy::default()));
        self
    }

    #[must_use]
    pub fn handler_with_policy(
        mut self,
        handler: Arc<dyn EventHandler>,
        policy: HandlerPolicy,
    ) -> Self {
        self.handlers.push((handler, policy));
        self
    }

    #[must_use]
    pub fn dedup_store(mut self, store: Arc<dyn DedupStore>) -> Self {
        self.dedup_store = store;
        self
    }

    #[must_use]
    pub fn observer(mut self, observer: RuntimeObserver) -> Self {
        self.observer = observer;
        self
    }

    pub fn build(self) -> Result<Runtime, RuntimeError> {
        if self.adapters.is_empty() {
            return Err(RuntimeError::NoAdapters);
        }
        if self.adapters.len() > MAX_ADAPTER_METRIC_SERIES {
            return Err(RuntimeError::TooManyAdapters);
        }
        let mut ids = HashSet::new();
        for (index, adapter) in self.adapters.iter().enumerate() {
            let id = adapter.id();
            if !is_valid_adapter_id(id) {
                return Err(RuntimeError::InvalidAdapterId { index });
            }
            if !ids.insert(adapter.id().clone()) {
                return Err(RuntimeError::DuplicateAdapter(adapter.id().clone()));
            }
        }
        if self.handlers.len() > MAX_REGISTERED_HANDLERS {
            return Err(RuntimeError::TooManyHandlers);
        }
        if self.event_concurrency > Semaphore::MAX_PERMITS {
            return Err(RuntimeError::EventConcurrencyTooLarge);
        }
        if self.queue_capacity > MAX_RUNTIME_QUEUE_CAPACITY {
            return Err(RuntimeError::QueueCapacityTooLarge);
        }
        let mut handler_names = HashSet::new();
        let handlers = self
            .handlers
            .into_iter()
            .enumerate()
            .map(|(index, (handler, policy))| {
                let name = handler.name();
                if name.is_empty()
                    || name.len() > MAX_HANDLER_NAME_BYTES
                    || name.chars().any(char::is_control)
                {
                    return Err(RuntimeError::InvalidHandlerName { index });
                }
                let name = name.to_owned();
                if !handler_names.insert(name.clone()) {
                    return Err(RuntimeError::DuplicateHandler(name));
                }
                if policy
                    .max_concurrency
                    .is_some_and(|limit| limit > Semaphore::MAX_PERMITS)
                {
                    return Err(RuntimeError::HandlerConcurrencyTooLarge { index });
                }
                let concurrency = policy
                    .max_concurrency
                    .map(|limit| Arc::new(Semaphore::new(limit)));
                Ok(RegisteredHandler {
                    name,
                    handler,
                    policy,
                    concurrency,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Runtime {
            queue_capacity: self.queue_capacity,
            handler_timeout: self.handler_timeout,
            shutdown_timeout: self.shutdown_timeout,
            event_concurrency: self.event_concurrency,
            adapters: self.adapters,
            handlers,
            dedup_store: self.dedup_store,
            observer: self.observer,
        })
    }
}

pub struct Runtime {
    queue_capacity: usize,
    handler_timeout: Duration,
    shutdown_timeout: Duration,
    event_concurrency: usize,
    adapters: Vec<Arc<dyn Adapter>>,
    handlers: Vec<RegisteredHandler>,
    dedup_store: Arc<dyn DedupStore>,
    observer: RuntimeObserver,
}

type PartitionTails = Arc<StdMutex<HashMap<String, (u64, oneshot::Receiver<()>)>>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutstandingClaimState {
    Processing,
    CommitPending,
    AcknowledgementPending,
}

#[derive(Clone)]
struct OutstandingClaim {
    state: OutstandingClaimState,
    event: crate::EventEnvelope,
    adapter: Arc<dyn Adapter>,
}

type OutstandingClaims = Arc<StdMutex<HashMap<DedupClaim, OutstandingClaim>>>;

#[derive(Clone)]
struct DedupCoordinator {
    store: Arc<dyn DedupStore>,
    outstanding: OutstandingClaims,
    recovery: mpsc::UnboundedSender<RecoveryCommand>,
}

enum RecoveryCommand {
    Recover(DedupClaim),
    Shutdown(oneshot::Sender<()>),
}

#[derive(Clone)]
struct RecoveryContext {
    store: Arc<dyn DedupStore>,
    outstanding: OutstandingClaims,
}

struct DedupRecoverySupervisor {
    commands: mpsc::UnboundedSender<RecoveryCommand>,
    task: tokio::task::JoinHandle<()>,
}

impl DedupRecoverySupervisor {
    async fn stop(&mut self, shutdown_timeout: Duration) -> Result<(), RuntimeError> {
        let (stopped, confirmed) = oneshot::channel();
        let stopping = async {
            if self
                .commands
                .send(RecoveryCommand::Shutdown(stopped))
                .is_err()
            {
                return (&mut self.task)
                    .await
                    .map_err(|error| RuntimeError::DedupRecoveryJoin(error.to_string()));
            }
            confirmed
                .await
                .map_err(|error| RuntimeError::DedupRecoveryJoin(error.to_string()))?;
            (&mut self.task)
                .await
                .map_err(|error| RuntimeError::DedupRecoveryJoin(error.to_string()))
        };
        if let Ok(result) = timeout(shutdown_timeout, stopping).await {
            return result;
        }
        self.task.abort();
        let _ = (&mut self.task).await;
        Err(RuntimeError::DedupRecoveryTimeout)
    }
}

struct PendingDedupRetry {
    event: crate::EventEnvelope,
    adapter: Arc<dyn Adapter>,
    dedup: DedupCoordinator,
    claim: DedupClaim,
    state: OutstandingClaimState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DedupFinalizationResult {
    Succeeded,
    RetryableFailure,
    PermanentFailure,
}

impl PendingDedupRetry {
    async fn retry(self) {
        loop {
            sleep(DEDUP_BACKGROUND_RETRY_DELAY).await;
            let recovered = match self.state {
                OutstandingClaimState::Processing => {
                    release_claim_with_retry(
                        &self.dedup.store,
                        &self.claim,
                        &self.event.adapter,
                        &self.event.id,
                    )
                    .await
                }
                OutstandingClaimState::CommitPending => {
                    commit_claim_with_retry(
                        &self.dedup.store,
                        &self.claim,
                        &self.event.adapter,
                        &self.event.id,
                    )
                    .await
                }
                OutstandingClaimState::AcknowledgementPending => {
                    unreachable!("acknowledgement retries use PendingAcknowledgement")
                }
            };
            match recovered {
                DedupFinalizationResult::Succeeded => {}
                DedupFinalizationResult::RetryableFailure => continue,
                DedupFinalizationResult::PermanentFailure => return,
            }
            if self.state == OutstandingClaimState::CommitPending {
                mark_acknowledgement_pending(&self.dedup.outstanding, &self.claim);
                while !acknowledge_event_with_retry(&self.adapter, &self.event).await {
                    sleep(DEDUP_BACKGROUND_RETRY_DELAY).await;
                }
            }
            untrack_claim(&self.dedup.outstanding, &self.claim);
            return;
        }
    }
}

struct PendingAcknowledgement {
    event: crate::EventEnvelope,
    adapter: Arc<dyn Adapter>,
    claimed: Option<(DedupCoordinator, DedupClaim)>,
}

enum PendingEventRetry {
    Dedup(PendingDedupRetry),
    Acknowledgement(PendingAcknowledgement),
}

impl PendingEventRetry {
    async fn retry(self) {
        match self {
            Self::Dedup(retry) => retry.retry().await,
            Self::Acknowledgement(retry) => loop {
                sleep(DEDUP_BACKGROUND_RETRY_DELAY).await;
                if acknowledge_event_with_retry(&retry.adapter, &retry.event).await {
                    if let Some((dedup, claim)) = &retry.claimed {
                        untrack_claim(&dedup.outstanding, claim);
                    }
                    return;
                }
            },
        }
    }
}

struct EventDispatcher {
    queue_capacity: usize,
    handler_timeout: Duration,
    shutdown_timeout: Duration,
    adapters: Arc<HashMap<AdapterId, Arc<dyn Adapter>>>,
    handlers: Arc<Vec<RegisteredHandler>>,
    dedup: DedupCoordinator,
    observer: RuntimeObserver,
    active_events: Arc<Semaphore>,
    claim_admissions: Arc<Semaphore>,
    partition_tails: PartitionTails,
    dispatch_generation: u64,
}

#[derive(Debug, Clone, Copy)]
struct DispatcherLimits {
    queue_capacity: usize,
    handler_timeout: Duration,
    shutdown_timeout: Duration,
    event_concurrency: usize,
}

impl EventDispatcher {
    fn new(
        limits: DispatcherLimits,
        adapters: Arc<HashMap<AdapterId, Arc<dyn Adapter>>>,
        handlers: Arc<Vec<RegisteredHandler>>,
        dedup: DedupCoordinator,
        observer: RuntimeObserver,
    ) -> Self {
        Self {
            queue_capacity: limits.queue_capacity,
            handler_timeout: limits.handler_timeout,
            shutdown_timeout: limits.shutdown_timeout,
            adapters,
            handlers,
            dedup,
            observer,
            active_events: Arc::new(Semaphore::new(limits.event_concurrency)),
            claim_admissions: Arc::new(Semaphore::new(limits.event_concurrency)),
            partition_tails: Arc::new(StdMutex::new(HashMap::new())),
            dispatch_generation: 0,
        }
    }

    async fn run(
        mut self,
        mut event_receiver: mpsc::Receiver<crate::EventEnvelope>,
        shutdown: &mut ShutdownSignal,
        adapter_tasks: &mut JoinSet<(AdapterId, Result<(), AdapterError>)>,
    ) -> (JoinSet<()>, Option<RuntimeError>, Option<Instant>) {
        let mut event_tasks = JoinSet::new();
        let mut shutdown_deadline = None;
        let mut shutdown_error = None;
        loop {
            // Normal dispatch retains at most `queue_capacity` task-owned
            // envelopes plus the bounded channel's `queue_capacity` envelopes.
            if event_tasks.len() >= self.queue_capacity {
                tokio::select! {
                    () = shutdown.cancelled() => {
                        let (error, deadline) = self.prepare_shutdown(
                            &mut event_receiver,
                            &mut event_tasks,
                            adapter_tasks,
                        ).await;
                        shutdown_error = error;
                        shutdown_deadline = Some(deadline);
                        break;
                    },
                    joined = event_tasks.join_next() => {
                        if let Err(error) = check_event_join(joined) {
                            return (event_tasks, Some(error), shutdown_deadline);
                        }
                    }
                    joined = adapter_tasks.join_next(), if !adapter_tasks.is_empty() => {
                        if let Err(error) = check_adapter_join(joined) {
                            return (event_tasks, Some(error), shutdown_deadline);
                        }
                    }
                }
                continue;
            }
            tokio::select! {
                () = shutdown.cancelled() => {
                    let (error, deadline) = self.prepare_shutdown(
                        &mut event_receiver,
                        &mut event_tasks,
                        adapter_tasks,
                    ).await;
                    shutdown_error = error;
                    shutdown_deadline = Some(deadline);
                    break;
                },
                joined = event_tasks.join_next(), if !event_tasks.is_empty() => {
                    if let Err(error) = check_event_join(joined) {
                        return (event_tasks, Some(error), shutdown_deadline);
                    }
                }
                joined = adapter_tasks.join_next(), if !adapter_tasks.is_empty() => {
                    if let Err(error) = check_adapter_join(joined) {
                        return (event_tasks, Some(error), shutdown_deadline);
                    }
                }
                event = event_receiver.recv() => {
                    let Some(event) = event else {
                        if shutdown.is_shutdown() {
                            break;
                        }
                        while let Some(joined) = adapter_tasks.try_join_next() {
                            if let Err(error) = check_adapter_join(Some(joined)) {
                                return (event_tasks, Some(error), shutdown_deadline);
                            }
                        }
                        return (
                            event_tasks,
                            Some(RuntimeError::AllAdaptersStopped),
                            shutdown_deadline,
                        );
                    };
                    self.observer.record_queue_depth(event_receiver.len());
                    self.spawn_event(&mut event_tasks, event);
                }
            }
        }
        (event_tasks, shutdown_error, shutdown_deadline)
    }

    async fn prepare_shutdown(
        &mut self,
        event_receiver: &mut mpsc::Receiver<crate::EventEnvelope>,
        event_tasks: &mut JoinSet<()>,
        adapter_tasks: &mut JoinSet<(AdapterId, Result<(), AdapterError>)>,
    ) -> (Option<RuntimeError>, Instant) {
        let deadline = Instant::now() + self.shutdown_timeout;
        // Preserve half of the shared deadline for already accepted events,
        // handler cancellation, acknowledgement, and claim finalization.
        let dispatch_reserve = self.shutdown_timeout / 2;
        let adapter_budget = deadline
            .saturating_duration_since(Instant::now())
            .saturating_sub(dispatch_reserve);
        let error = match timeout(
            adapter_budget,
            self.drain_adapters(event_receiver, event_tasks, adapter_tasks),
        )
        .await
        {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error),
            Err(_) => {
                warn!("adapter event drain exceeded the runtime shutdown deadline");
                adapter_tasks.abort_all();
                while adapter_tasks.join_next().await.is_some() {}
                Some(RuntimeError::AdapterDrainTimeout)
            }
        };
        event_receiver.close();
        while let Some(event) = event_receiver.recv().await {
            while event_tasks.len() >= self.queue_capacity {
                let remaining = deadline.saturating_duration_since(Instant::now());
                let Ok(joined) = timeout(remaining, event_tasks.join_next()).await else {
                    return (Some(RuntimeError::EventDrainTimeout), deadline);
                };
                if let Err(error) = check_event_join(joined) {
                    return (Some(error), deadline);
                }
            }
            self.observer.record_queue_depth(event_receiver.len());
            self.spawn_event(event_tasks, event);
        }
        (error, deadline)
    }

    async fn drain_adapters(
        &mut self,
        event_receiver: &mut mpsc::Receiver<crate::EventEnvelope>,
        event_tasks: &mut JoinSet<()>,
        adapter_tasks: &mut JoinSet<(AdapterId, Result<(), AdapterError>)>,
    ) -> Result<(), RuntimeError> {
        let mut receiver_open = true;
        while !adapter_tasks.is_empty() {
            if event_tasks.len() >= self.queue_capacity {
                tokio::select! {
                    joined = event_tasks.join_next() => check_event_join(joined)?,
                    joined = adapter_tasks.join_next() => check_adapter_join(joined)?,
                }
                continue;
            }
            tokio::select! {
                event = event_receiver.recv(), if receiver_open => {
                    if let Some(event) = event {
                        self.observer.record_queue_depth(event_receiver.len());
                        self.spawn_event(event_tasks, event);
                    } else {
                        receiver_open = false;
                    }
                }
                joined = event_tasks.join_next(), if !event_tasks.is_empty() => {
                    check_event_join(joined)?;
                }
                joined = adapter_tasks.join_next() => check_adapter_join(joined)?,
            }
        }
        Ok(())
    }

    fn spawn_event(&mut self, event_tasks: &mut JoinSet<()>, event: crate::EventEnvelope) {
        let Some(adapter) = self.adapters.get(&event.adapter).cloned() else {
            self.observer.record_unattributed_rejection();
            warn!(adapter_id = %event.adapter, event_id = %event.id, "dropping event from unknown adapter");
            return;
        };
        let handlers = Arc::clone(&self.handlers);
        let dedup = self.dedup.clone();
        let observer = self.observer.clone();
        let active_events = Arc::clone(&self.active_events);
        let claim_admissions = Arc::clone(&self.claim_admissions);
        let partition_tails = Arc::clone(&self.partition_tails);
        let handler_timeout = self.handler_timeout;
        let partition_key = runtime_partition_key(&event);
        self.dispatch_generation = self.dispatch_generation.wrapping_add(1);
        let generation = self.dispatch_generation;
        let (finished_sender, finished_receiver) = oneshot::channel();
        let previous = {
            let mut tails = self
                .partition_tails
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let previous = tails.remove(&partition_key).map(|(_, receiver)| receiver);
            tails.insert(partition_key.clone(), (generation, finished_receiver));
            previous
        };
        event_tasks.spawn(async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            {
                let Ok(claim_admission) = claim_admissions.acquire_owned().await else {
                    return;
                };
                observer.record_event_started();
                let _activity = EventActivity(observer.clone());
                let claimed = claim_event(&event, &adapter, &dedup, &observer).await;
                drop(claim_admission);
                let mut cancellation_guard = None;
                let pending_retry = match claimed {
                    Err(()) => None,
                    Ok(None) => process_duplicate_event(event, adapter).await,
                    Ok(Some(claim)) => {
                        cancellation_guard = Some(ClaimCancellationGuard {
                            dedup: dedup.clone(),
                            claim: claim.duplicate(),
                        });
                        let Ok(permit) = active_events.acquire_owned().await else {
                            return;
                        };
                        let pending_retry = process_claimed_event(
                            event,
                            adapter,
                            handlers,
                            handler_timeout,
                            dedup.clone(),
                            observer,
                            claim,
                        )
                        .await;
                        // Recovery retains the partition tail and bounded task
                        // slot, but not global handler concurrency.
                        drop(permit);
                        pending_retry
                    }
                };
                if let Some(pending_retry) = pending_retry {
                    pending_retry.retry().await;
                }
                drop(cancellation_guard);
            }
            let _ = finished_sender.send(());
            if let Ok(mut tails) = partition_tails.lock()
                && tails
                    .get(&partition_key)
                    .is_some_and(|(current, _)| *current == generation)
            {
                tails.remove(&partition_key);
            }
        });
    }
}

struct EventActivity(RuntimeObserver);

struct ClaimCancellationGuard {
    dedup: DedupCoordinator,
    claim: DedupClaim,
}

impl Drop for ClaimCancellationGuard {
    fn drop(&mut self) {
        let tracked = self
            .dedup
            .outstanding
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&self.claim);
        if !tracked {
            return;
        }
        if self
            .dedup
            .recovery
            .send(RecoveryCommand::Recover(self.claim.duplicate()))
            .is_err()
        {
            error!(
                adapter_id = %self.claim.key().adapter,
                event_id = %self.claim.key().event,
                "cancelled claim recovery supervisor is unavailable"
            );
        }
    }
}

impl Drop for EventActivity {
    fn drop(&mut self) {
        self.0.record_event_completed();
    }
}

struct ObserverRunGuard(RuntimeObserver);

impl Drop for ObserverRunGuard {
    fn drop(&mut self) {
        self.0.end_run();
    }
}

struct RuntimeShutdown {
    event_tasks: JoinSet<()>,
    adapter_tasks: JoinSet<(AdapterId, Result<(), AdapterError>)>,
    adapters: Arc<Vec<Arc<dyn Adapter>>>,
    dedup: DedupCoordinator,
    recovery: DedupRecoverySupervisor,
    observer: RuntimeObserver,
    deadline: Instant,
    timeout: Duration,
    error: Option<RuntimeError>,
}

impl RuntimeShutdown {
    async fn finish(mut self) -> Result<(), RuntimeError> {
        self.observer.set_phase(RuntimePhase::ShuttingDown);
        info!("bot runtime is shutting down");
        let finalization_reserve = self.timeout / 4;
        let event_deadline = self
            .deadline
            .checked_sub(finalization_reserve)
            .unwrap_or(self.deadline);
        if self.error.is_some() {
            abort_event_tasks(&mut self.event_tasks).await;
            abort_adapter_tasks(&mut self.adapter_tasks).await;
        } else if let Err(error) = finish_event_tasks(
            &mut self.event_tasks,
            event_deadline.saturating_duration_since(Instant::now()),
        )
        .await
        {
            self.error.get_or_insert(error);
            abort_adapter_tasks(&mut self.adapter_tasks).await;
        }
        if let Err(error) = self
            .recovery
            .stop(self.deadline.saturating_duration_since(Instant::now()))
            .await
        {
            self.error.get_or_insert(error);
        }
        if let Err(error) = finalize_outstanding_claims(
            &self.dedup,
            self.deadline.saturating_duration_since(Instant::now()),
        )
        .await
        {
            self.error.get_or_insert(error);
        }
        for adapter in self.adapters.iter() {
            self.observer.set_adapter_online(adapter.id(), false);
        }
        if self.error.is_some() {
            abort_adapter_tasks(&mut self.adapter_tasks).await;
        }
        if let Err(error) = finish_adapter_tasks(
            &mut self.adapter_tasks,
            self.deadline.saturating_duration_since(Instant::now()),
        )
        .await
        {
            self.error.get_or_insert(error);
        }
        if self.error.is_some() {
            self.observer.set_phase(RuntimePhase::Failed);
        } else {
            self.observer.set_phase(RuntimePhase::Stopped);
        }
        self.error.map_or(Ok(()), Err)
    }
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Runtime")
            .field("queue_capacity", &self.queue_capacity)
            .field("handler_timeout", &self.handler_timeout)
            .field("shutdown_timeout", &self.shutdown_timeout)
            .field("event_concurrency", &self.event_concurrency)
            .field("adapter_count", &self.adapters.len())
            .field("handler_count", &self.handlers.len())
            .field("dedup_store", &"configured")
            .field("observer", &self.observer)
            .finish()
    }
}

impl Runtime {
    pub async fn run(self, mut shutdown: ShutdownSignal) -> Result<(), RuntimeError> {
        if !self.observer.begin_run(self.queue_capacity, &self.adapters) {
            return Err(RuntimeError::ObserverInUse);
        }
        let _observer_run = ObserverRunGuard(self.observer.clone());
        self.observer.set_phase(RuntimePhase::Starting);
        let (event_sender, event_receiver) = mpsc::channel(self.queue_capacity);
        let mut adapter_tasks = JoinSet::new();

        for adapter in &self.adapters {
            let adapter = Arc::clone(adapter);
            let events = EventSender::new_unchecked(
                event_sender.clone(),
                adapter.id().clone(),
                self.observer.clone(),
            );
            let adapter_shutdown = shutdown.clone();
            let observer = self.observer.clone();
            adapter_tasks.spawn(async move {
                let id = adapter.id().clone();
                let result = adapter.run(events, adapter_shutdown).await;
                match &result {
                    Ok(()) => observer.set_adapter_online(&id, false),
                    Err(error) => observer.record_adapter_failure(&id, error),
                }
                (id, result)
            });
        }
        drop(event_sender);
        self.observer.set_phase(RuntimePhase::Running);

        let adapters = Arc::new(self.adapters);
        let adapter_index = Arc::new(
            adapters
                .iter()
                .map(|adapter| (adapter.id().clone(), Arc::clone(adapter)))
                .collect(),
        );
        let handlers = Arc::new(self.handlers);
        let outstanding = Arc::new(StdMutex::new(HashMap::new()));
        let (recovery_sender, recovery_receiver) = mpsc::unbounded_channel();
        let recovery_context = RecoveryContext {
            store: Arc::clone(&self.dedup_store),
            outstanding: Arc::clone(&outstanding),
        };
        let recovery = DedupRecoverySupervisor {
            commands: recovery_sender.clone(),
            task: tokio::spawn(run_recovery_supervisor(recovery_receiver, recovery_context)),
        };
        let dedup = DedupCoordinator {
            store: Arc::clone(&self.dedup_store),
            outstanding,
            recovery: recovery_sender,
        };
        info!(
            adapter_count = adapters.len(),
            event_concurrency = self.event_concurrency,
            "bot runtime started"
        );
        let dispatcher = EventDispatcher::new(
            DispatcherLimits {
                queue_capacity: self.queue_capacity,
                handler_timeout: self.handler_timeout,
                shutdown_timeout: self.shutdown_timeout,
                event_concurrency: self.event_concurrency,
            },
            adapter_index,
            handlers,
            dedup.clone(),
            self.observer.clone(),
        );
        let (event_tasks, runtime_error, shutdown_deadline) = dispatcher
            .run(event_receiver, &mut shutdown, &mut adapter_tasks)
            .await;
        let shutdown_deadline =
            shutdown_deadline.unwrap_or_else(|| Instant::now() + self.shutdown_timeout);
        RuntimeShutdown {
            event_tasks,
            adapter_tasks,
            adapters,
            dedup,
            recovery,
            observer: self.observer,
            deadline: shutdown_deadline,
            timeout: self.shutdown_timeout,
            error: runtime_error,
        }
        .finish()
        .await
    }
}

async fn abort_event_tasks(event_tasks: &mut JoinSet<()>) {
    event_tasks.abort_all();
    while event_tasks.join_next().await.is_some() {}
}

async fn abort_adapter_tasks(adapter_tasks: &mut JoinSet<(AdapterId, Result<(), AdapterError>)>) {
    adapter_tasks.abort_all();
    while adapter_tasks.join_next().await.is_some() {}
}

async fn finish_adapter_tasks(
    adapter_tasks: &mut JoinSet<(AdapterId, Result<(), AdapterError>)>,
    shutdown_timeout: Duration,
) -> Result<(), RuntimeError> {
    let finish = async {
        while let Some(joined) = adapter_tasks.join_next().await {
            let (adapter_id, result) = match joined {
                Ok(result) => result,
                Err(error) => {
                    let error = RuntimeError::AdapterJoin(error.to_string());
                    adapter_tasks.abort_all();
                    while adapter_tasks.join_next().await.is_some() {}
                    return Err(error);
                }
            };
            if let Err(error) = result {
                log_adapter_error(&adapter_id, &error);
                let runtime_error = RuntimeError::AdapterStopped {
                    adapter_id,
                    message: error.to_string(),
                };
                adapter_tasks.abort_all();
                while adapter_tasks.join_next().await.is_some() {}
                return Err(runtime_error);
            }
        }
        Ok(())
    };
    if let Ok(result) = timeout(shutdown_timeout, finish).await {
        return result;
    }
    warn!("adapter shutdown timed out; aborting remaining tasks");
    adapter_tasks.abort_all();
    while adapter_tasks.join_next().await.is_some() {}
    Err(RuntimeError::AdapterShutdownTimeout)
}

async fn finish_event_tasks(
    event_tasks: &mut JoinSet<()>,
    shutdown_timeout: Duration,
) -> Result<(), RuntimeError> {
    let finish = async {
        while let Some(joined) = event_tasks.join_next().await {
            joined.map_err(|error| RuntimeError::EventJoin(error.to_string()))?;
        }
        Ok(())
    };
    match timeout(shutdown_timeout, finish).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            event_tasks.abort_all();
            while event_tasks.join_next().await.is_some() {}
            Err(error)
        }
        Err(_) => {
            warn!("event dispatch shutdown timed out; aborting remaining tasks");
            event_tasks.abort_all();
            while event_tasks.join_next().await.is_some() {}
            Err(RuntimeError::EventDrainTimeout)
        }
    }
}

fn check_event_join(
    joined: Option<Result<(), tokio::task::JoinError>>,
) -> Result<(), RuntimeError> {
    if let Some(joined) = joined {
        joined.map_err(|error| RuntimeError::EventJoin(error.to_string()))?;
    }
    Ok(())
}

type AdapterTaskResult = Result<(AdapterId, Result<(), AdapterError>), tokio::task::JoinError>;

fn check_adapter_join(joined: Option<AdapterTaskResult>) -> Result<(), RuntimeError> {
    let Some(joined) = joined else {
        return Ok(());
    };
    let (adapter_id, result) =
        joined.map_err(|error| RuntimeError::AdapterJoin(error.to_string()))?;
    match result {
        Ok(()) => {
            info!(%adapter_id, "adapter stopped");
            Ok(())
        }
        Err(error) => {
            log_adapter_error(&adapter_id, &error);
            Err(RuntimeError::AdapterStopped {
                adapter_id,
                message: error.to_string(),
            })
        }
    }
}

fn runtime_partition_key(event: &crate::EventEnvelope) -> String {
    let adapter = event.adapter.as_str();
    match &event.event {
        Event::Message(message) => match &message.target {
            crate::MessageTarget::Group { group_id } => format!("{adapter}:group:{group_id}"),
            crate::MessageTarget::Private { user_id } => format!("{adapter}:private:{user_id}"),
            crate::MessageTarget::Channel { channel_id } => {
                format!("{adapter}:channel:{channel_id}")
            }
        },
        _ => format!("{adapter}:event:{}", event.id),
    }
}

async fn claim_event(
    event: &crate::EventEnvelope,
    adapter: &Arc<dyn Adapter>,
    dedup: &DedupCoordinator,
    observer: &RuntimeObserver,
) -> Result<Option<DedupClaim>, ()> {
    loop {
        let key = DedupKey::new(event.adapter.clone(), event.id.clone());
        match timeout(DEDUP_CLAIM_TIMEOUT, dedup.store.claim(key)).await {
            Ok(Ok(None)) => {
                observer.record_duplicate();
                return Ok(None);
            }
            Ok(Ok(Some(claim))) => {
                track_claim(
                    &dedup.outstanding,
                    claim.duplicate(),
                    event.clone(),
                    Arc::clone(adapter),
                );
                return Ok(Some(claim));
            }
            Ok(Err(error)) if error.is_retryable() => {
                warn!(
                    adapter_id = %event.adapter,
                    event_id = %event.id,
                    error = %error,
                    "event deduplication claim admission is retrying"
                );
            }
            Ok(Err(error)) => {
                observer.record_event_rejected(&event.adapter);
                error!(
                    adapter_id = %event.adapter,
                    event_id = %event.id,
                    error = %error,
                    "event rejected because its deduplication claim failed permanently"
                );
                return Err(());
            }
            Err(_) => {
                warn!(
                    adapter_id = %event.adapter,
                    event_id = %event.id,
                    "event deduplication claim timed out and will retry"
                );
            }
        }
        sleep(DEDUP_BACKGROUND_RETRY_DELAY).await;
    }
}

async fn process_duplicate_event(
    event: crate::EventEnvelope,
    adapter: Arc<dyn Adapter>,
) -> Option<PendingEventRetry> {
    if acknowledge_event_with_retry(&adapter, &event).await {
        None
    } else {
        Some(PendingEventRetry::Acknowledgement(PendingAcknowledgement {
            event,
            adapter,
            claimed: None,
        }))
    }
}

async fn process_claimed_event(
    event: crate::EventEnvelope,
    adapter: Arc<dyn Adapter>,
    handlers: Arc<Vec<RegisteredHandler>>,
    handler_timeout: Duration,
    dedup: DedupCoordinator,
    observer: RuntimeObserver,
    claim: DedupClaim,
) -> Option<PendingEventRetry> {
    let context = Context::new(&event, Arc::clone(&adapter));
    let mut handled_successfully = true;
    for registered in handlers.iter() {
        if !run_handler(
            registered,
            context.clone(),
            &event,
            handler_timeout,
            &observer,
        )
        .await
        {
            handled_successfully = false;
            break;
        }
    }
    if !handled_successfully {
        match release_claim_with_retry(&dedup.store, &claim, &event.adapter, &event.id).await {
            DedupFinalizationResult::Succeeded => {
                untrack_claim(&dedup.outstanding, &claim);
                return None;
            }
            DedupFinalizationResult::PermanentFailure => return None,
            DedupFinalizationResult::RetryableFailure => {}
        }
        return Some(PendingEventRetry::Dedup(PendingDedupRetry {
            event,
            adapter,
            dedup,
            claim,
            state: OutstandingClaimState::Processing,
        }));
    }
    // Handler effects have completed. From this point onward the claim must
    // only be committed, never released, even if this task is cancelled.
    mark_commit_pending(&dedup.outstanding, &claim);
    match commit_claim_with_retry(&dedup.store, &claim, &event.adapter, &event.id).await {
        DedupFinalizationResult::Succeeded => {}
        DedupFinalizationResult::RetryableFailure => {
            return Some(PendingEventRetry::Dedup(PendingDedupRetry {
                event,
                adapter,
                dedup,
                claim,
                state: OutstandingClaimState::CommitPending,
            }));
        }
        DedupFinalizationResult::PermanentFailure => {
            return None;
        }
    }
    mark_acknowledgement_pending(&dedup.outstanding, &claim);
    // Persist successful processing before source acknowledgement. Failed
    // acknowledgements remain under bounded dispatcher supervision and never
    // repeat Handler effects.
    if acknowledge_event_with_retry(&adapter, &event).await {
        untrack_claim(&dedup.outstanding, &claim);
        None
    } else {
        Some(PendingEventRetry::Acknowledgement(PendingAcknowledgement {
            event,
            adapter,
            claimed: Some((dedup, claim)),
        }))
    }
}

fn track_claim(
    outstanding: &OutstandingClaims,
    claim: DedupClaim,
    event: crate::EventEnvelope,
    adapter: Arc<dyn Adapter>,
) {
    outstanding
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            claim,
            OutstandingClaim {
                state: OutstandingClaimState::Processing,
                event,
                adapter,
            },
        );
}

fn mark_commit_pending(outstanding: &OutstandingClaims, claim: &DedupClaim) {
    if let Some(state) = outstanding
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_mut(claim)
    {
        state.state = OutstandingClaimState::CommitPending;
    }
}

fn mark_acknowledgement_pending(outstanding: &OutstandingClaims, claim: &DedupClaim) {
    if let Some(state) = outstanding
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_mut(claim)
    {
        state.state = OutstandingClaimState::AcknowledgementPending;
    }
}

fn untrack_claim(outstanding: &OutstandingClaims, claim: &DedupClaim) {
    outstanding
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(claim);
}

async fn commit_claim_with_retry(
    store: &Arc<dyn DedupStore>,
    claim: &DedupClaim,
    adapter_id: &AdapterId,
    event_id: &crate::EventId,
) -> DedupFinalizationResult {
    let mut backoff = DEDUP_COMMIT_INITIAL_BACKOFF;
    for attempt in 1..=DEDUP_COMMIT_MAX_ATTEMPTS {
        match timeout(DEDUP_COMMIT_ATTEMPT_TIMEOUT, store.commit(claim)).await {
            Ok(Ok(())) => return DedupFinalizationResult::Succeeded,
            Ok(Err(error)) => {
                let retryable = error.is_retryable();
                warn!(
                    adapter_id = %adapter_id,
                    event_id = %event_id,
                    attempt,
                    error = %error,
                    "event deduplication commit attempt failed"
                );
                if !retryable {
                    error!(
                        adapter_id = %adapter_id,
                        event_id = %event_id,
                        error = %error,
                        "event deduplication commit failed permanently"
                    );
                    return DedupFinalizationResult::PermanentFailure;
                }
            }
            Err(_) => {
                warn!(
                    adapter_id = %adapter_id,
                    event_id = %event_id,
                    attempt,
                    "event deduplication commit attempt timed out"
                );
            }
        }
        if attempt < DEDUP_COMMIT_MAX_ATTEMPTS {
            sleep(backoff).await;
            backoff = backoff.saturating_mul(2);
        }
    }
    warn!(
        adapter_id = %adapter_id,
        event_id = %event_id,
        attempts = DEDUP_COMMIT_MAX_ATTEMPTS,
        "event deduplication commit retries exhausted; claim remains outstanding"
    );
    DedupFinalizationResult::RetryableFailure
}

async fn release_claim_with_retry(
    store: &Arc<dyn DedupStore>,
    claim: &DedupClaim,
    adapter_id: &AdapterId,
    event_id: &crate::EventId,
) -> DedupFinalizationResult {
    let mut backoff = DEDUP_RELEASE_INITIAL_BACKOFF;
    for attempt in 1..=DEDUP_RELEASE_MAX_ATTEMPTS {
        match timeout(DEDUP_RELEASE_ATTEMPT_TIMEOUT, store.release(claim)).await {
            Ok(Ok(())) => return DedupFinalizationResult::Succeeded,
            Ok(Err(error)) => {
                let retryable = error.is_retryable();
                warn!(
                    adapter_id = %adapter_id,
                    event_id = %event_id,
                    attempt,
                    error = %error,
                    "event deduplication release attempt failed"
                );
                if !retryable {
                    error!(
                        adapter_id = %adapter_id,
                        event_id = %event_id,
                        error = %error,
                        "event deduplication release failed permanently"
                    );
                    return DedupFinalizationResult::PermanentFailure;
                }
            }
            Err(_) => {
                warn!(
                    adapter_id = %adapter_id,
                    event_id = %event_id,
                    attempt,
                    "event deduplication release attempt timed out"
                );
            }
        }
        if attempt < DEDUP_RELEASE_MAX_ATTEMPTS {
            sleep(backoff).await;
            backoff = backoff.saturating_mul(2);
        }
    }
    warn!(
        adapter_id = %adapter_id,
        event_id = %event_id,
        attempts = DEDUP_RELEASE_MAX_ATTEMPTS,
        "event deduplication release retries exhausted; claim remains outstanding"
    );
    DedupFinalizationResult::RetryableFailure
}

async fn acknowledge_event_with_retry(
    adapter: &Arc<dyn Adapter>,
    event: &crate::EventEnvelope,
) -> bool {
    let mut backoff = EVENT_ACK_INITIAL_BACKOFF;
    for attempt in 1..=EVENT_ACK_MAX_ATTEMPTS {
        match timeout(EVENT_ACK_ATTEMPT_TIMEOUT, adapter.event_handled(event)).await {
            Ok(Ok(())) => return true,
            Ok(Err(error)) => {
                warn!(
                    adapter_id = %event.adapter,
                    event_id = %event.id,
                    attempt,
                    error = %error,
                    "event source acknowledgement attempt failed"
                );
            }
            Err(_) => {
                warn!(
                    adapter_id = %event.adapter,
                    event_id = %event.id,
                    attempt,
                    "event source acknowledgement attempt timed out"
                );
            }
        }
        if attempt < EVENT_ACK_MAX_ATTEMPTS {
            sleep(backoff).await;
            backoff = backoff.saturating_mul(2);
        }
    }
    error!(
        adapter_id = %event.adapter,
        event_id = %event.id,
        attempts = EVENT_ACK_MAX_ATTEMPTS,
        "event source acknowledgement retries exhausted"
    );
    false
}

async fn run_recovery_supervisor(
    mut commands: mpsc::UnboundedReceiver<RecoveryCommand>,
    context: RecoveryContext,
) {
    let mut scheduled = HashSet::new();
    let mut pending: VecDeque<DedupClaim> = VecDeque::new();
    let mut tasks = JoinSet::new();
    let mut commands_open = true;
    loop {
        while tasks.len() < MAX_CONCURRENT_DEDUP_FINALIZATIONS
            && let Some(claim) = pending.pop_front()
        {
            let context = context.clone();
            tasks.spawn(async move {
                let retryable = recover_cancelled_claim(context, claim.duplicate()).await;
                if retryable {
                    sleep(DEDUP_BACKGROUND_RETRY_DELAY).await;
                }
                (claim, retryable)
            });
        }
        if !commands_open && pending.is_empty() && tasks.is_empty() {
            return;
        }
        tokio::select! {
            command = commands.recv(), if commands_open => match command {
                Some(RecoveryCommand::Recover(claim)) => {
                    if scheduled.insert(claim.duplicate()) {
                        pending.push_back(claim);
                    }
                }
                Some(RecoveryCommand::Shutdown(stopped)) => {
                    tasks.abort_all();
                    while tasks.join_next().await.is_some() {}
                    let _ = stopped.send(());
                    return;
                }
                None => {
                    commands_open = false;
                }
            },
            joined = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Ok((claim, retryable))) = joined {
                    if retryable {
                        pending.push_back(claim);
                    } else {
                        scheduled.remove(&claim);
                    }
                }
            }
        }
    }
}

async fn recover_cancelled_claim(context: RecoveryContext, claim: DedupClaim) -> bool {
    let Some(outstanding) = context
        .outstanding
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&claim)
        .cloned()
    else {
        return false;
    };
    let recovered = match outstanding.state {
        OutstandingClaimState::Processing => {
            release_claim_with_retry(
                &context.store,
                &claim,
                &outstanding.event.adapter,
                &outstanding.event.id,
            )
            .await
        }
        OutstandingClaimState::CommitPending => {
            let committed = commit_claim_with_retry(
                &context.store,
                &claim,
                &outstanding.event.adapter,
                &outstanding.event.id,
            )
            .await;
            match committed {
                DedupFinalizationResult::Succeeded => {
                    mark_acknowledgement_pending(&context.outstanding, &claim);
                    if acknowledge_event_with_retry(&outstanding.adapter, &outstanding.event).await
                    {
                        DedupFinalizationResult::Succeeded
                    } else {
                        DedupFinalizationResult::RetryableFailure
                    }
                }
                failure => failure,
            }
        }
        OutstandingClaimState::AcknowledgementPending => {
            if acknowledge_event_with_retry(&outstanding.adapter, &outstanding.event).await {
                DedupFinalizationResult::Succeeded
            } else {
                DedupFinalizationResult::RetryableFailure
            }
        }
    };
    if recovered == DedupFinalizationResult::Succeeded {
        untrack_claim(&context.outstanding, &claim);
    }
    recovered == DedupFinalizationResult::RetryableFailure
}

async fn finalize_outstanding_claim(
    store: Arc<dyn DedupStore>,
    tracked: OutstandingClaims,
    claim: DedupClaim,
    outstanding: OutstandingClaim,
) -> (DedupClaim, OutstandingClaimState, DedupFinalizationResult) {
    let state = outstanding.state;
    let finalized = match state {
        OutstandingClaimState::Processing => {
            release_claim_with_retry(&store, &claim, &claim.key().adapter, &claim.key().event).await
        }
        OutstandingClaimState::CommitPending => {
            let committed =
                commit_claim_with_retry(&store, &claim, &claim.key().adapter, &claim.key().event)
                    .await;
            match committed {
                DedupFinalizationResult::Succeeded => {
                    mark_acknowledgement_pending(&tracked, &claim);
                    if acknowledge_event_with_retry(&outstanding.adapter, &outstanding.event).await
                    {
                        DedupFinalizationResult::Succeeded
                    } else {
                        DedupFinalizationResult::RetryableFailure
                    }
                }
                failure => failure,
            }
        }
        OutstandingClaimState::AcknowledgementPending => {
            if acknowledge_event_with_retry(&outstanding.adapter, &outstanding.event).await {
                DedupFinalizationResult::Succeeded
            } else {
                DedupFinalizationResult::RetryableFailure
            }
        }
    };
    (claim, state, finalized)
}

async fn finalize_outstanding_claims(
    dedup: &DedupCoordinator,
    operation_timeout: Duration,
) -> Result<(), RuntimeError> {
    let claims = dedup
        .outstanding
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .map(|(claim, outstanding)| (claim.duplicate(), outstanding.clone()))
        .collect::<Vec<_>>();
    let mut claims = claims.into_iter();
    let mut tasks = JoinSet::new();
    let finalize = async {
        loop {
            while tasks.len() < MAX_CONCURRENT_DEDUP_FINALIZATIONS {
                let Some((claim, outstanding)) = claims.next() else {
                    break;
                };
                let store = Arc::clone(&dedup.store);
                let tracked = Arc::clone(&dedup.outstanding);
                tasks.spawn(finalize_outstanding_claim(
                    store,
                    tracked,
                    claim,
                    outstanding,
                ));
            }
            let Some(joined) = tasks.join_next().await else {
                break;
            };
            match joined {
                Ok((claim, _state, DedupFinalizationResult::Succeeded)) => {
                    untrack_claim(&dedup.outstanding, &claim);
                }
                Ok((claim, state, result)) => {
                    warn!(
                        adapter_id = %claim.key().adapter,
                        event_id = %claim.key().event,
                        operation = claim_finalization_operation(state),
                        permanent = result == DedupFinalizationResult::PermanentFailure,
                        "failed to finalize an outstanding deduplication claim during shutdown retries"
                    );
                }
                Err(error) => {
                    warn!(error = %error, "deduplication claim finalization task failed");
                }
            }
        }
    };
    if timeout(operation_timeout, finalize).await.is_err() {
        warn!(
            remaining_claims = outstanding_claim_count(&dedup.outstanding),
            "deduplication claim finalization exceeded the shutdown deadline"
        );
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
    let remaining = outstanding_claim_count(&dedup.outstanding);
    if remaining == 0 {
        Ok(())
    } else {
        Err(RuntimeError::DedupFinalization { remaining })
    }
}

fn outstanding_claim_count(outstanding: &OutstandingClaims) -> usize {
    outstanding
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .len()
}

const fn claim_finalization_operation(state: OutstandingClaimState) -> &'static str {
    match state {
        OutstandingClaimState::Processing => "release",
        OutstandingClaimState::CommitPending => "commit",
        OutstandingClaimState::AcknowledgementPending => "acknowledgement",
    }
}

async fn run_handler(
    registered: &RegisteredHandler,
    context: Context,
    event: &crate::EventEnvelope,
    default_timeout: Duration,
    observer: &RuntimeObserver,
) -> bool {
    let started = Instant::now();
    let result = timeout(
        registered.policy.timeout.unwrap_or(default_timeout),
        async {
            let _concurrency = if let Some(semaphore) = &registered.concurrency {
                Some(semaphore.clone().acquire_owned().await.map_err(|_| {
                    HandlerError::Failed("handler concurrency limiter closed".to_owned())
                })?)
            } else {
                None
            };
            registered.handler.handle(context, &event.event).await
        },
    )
    .await;
    match result {
        Ok(Ok(())) => {
            observer.record_handler(&registered.name, started.elapsed(), HandlerOutcome::Success);
            true
        }
        Ok(Err(error)) => {
            observer.record_handler(&registered.name, started.elapsed(), HandlerOutcome::Failure);
            error!(
                handler = registered.name.as_str(),
                event_id = %event.id,
                error = %error,
                "event handler failed"
            );
            false
        }
        Err(_) => {
            observer.record_handler(&registered.name, started.elapsed(), HandlerOutcome::Timeout);
            error!(
                handler = registered.name.as_str(),
                event_id = %event.id,
                "event handler timed out"
            );
            false
        }
    }
}

fn log_adapter_error(adapter_id: &AdapterId, error: &AdapterError) {
    error!(adapter_id = %adapter_id, error = %error, "adapter stopped with an error");
}

#[cfg(test)]
mod tests {
    use super::{
        DEDUP_COMMIT_MAX_ATTEMPTS, DEDUP_RELEASE_MAX_ATTEMPTS, DedupCoordinator,
        EVENT_ACK_MAX_ATTEMPTS, finalize_outstanding_claims, finish_adapter_tasks,
        finish_event_tasks, track_claim,
    };
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use async_trait::async_trait;
    use serde_json::{Value, json};
    use tokio::{
        sync::{Mutex, Notify, mpsc},
        time::{sleep, timeout},
    };

    use crate::{
        Action, ActionResult, Adapter, AdapterError, AdapterId, CommonMessage, Context, DedupClaim,
        DedupError, DedupKey, DedupStore, Event, EventEnvelope, EventHandler, EventId, EventSender,
        HandlerError, HandlerPolicy, MemoryDedupStore, MessageSegment, MessageTarget,
        RuntimeBuilder, RuntimeError, RuntimeObserver, Sender, ShutdownHandle, ShutdownSignal,
        shutdown_channel,
    };

    #[derive(Debug)]
    struct MockAdapter {
        id: AdapterId,
        actions: mpsc::Sender<Action>,
        event: Mutex<Option<EventEnvelope>>,
    }

    #[async_trait]
    impl Adapter for MockAdapter {
        fn id(&self) -> &AdapterId {
            &self.id
        }

        fn platform(&self) -> &'static str {
            "mock"
        }

        async fn run(
            &self,
            events: EventSender,
            mut shutdown: ShutdownSignal,
        ) -> Result<(), AdapterError> {
            events.mark_ready();
            if let Some(event) = self.event.lock().await.take() {
                events
                    .send(event)
                    .await
                    .map_err(|_| AdapterError::EventQueueClosed)?;
            }
            shutdown.cancelled().await;
            Ok(())
        }

        async fn execute(&self, action: Action) -> Result<ActionResult, AdapterError> {
            self.actions
                .send(action)
                .await
                .map_err(|_| AdapterError::Action("action observer closed".to_owned()))?;
            Ok(ActionResult {
                message_id: Some("sent-message".to_owned()),
                raw: Value::Null,
            })
        }
    }

    #[derive(Debug)]
    struct PingHandler {
        name: String,
        shutdown: ShutdownHandle,
    }

    #[async_trait]
    impl EventHandler for PingHandler {
        fn name(&self) -> &str {
            &self.name
        }

        async fn handle(&self, context: Context, event: &Event) -> Result<(), HandlerError> {
            if let Event::Message(message) = event
                && message.text.trim() == "/ping"
            {
                context
                    .reply("pong")
                    .await
                    .map_err(|error| HandlerError::Failed(error.to_string()))?;
                self.shutdown.shutdown();
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn routes_reply_to_source_adapter() {
        let (action_sender, mut actions) = mpsc::channel(1);
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let adapter = Arc::new(MockAdapter {
            id: AdapterId::new("mock"),
            actions: action_sender,
            event: Mutex::new(Some(EventEnvelope {
                id: EventId::new("event-1"),
                adapter: AdapterId::new("mock"),
                delivery_id: None,
                timestamp: None,
                event: Event::Message(CommonMessage {
                    message_id: "source-message".to_owned(),
                    target: MessageTarget::Group {
                        group_id: "group".to_owned(),
                    },
                    sender: Sender {
                        id: "user".to_owned(),
                        display_name: None,
                    },
                    text: "/ping".to_owned(),
                    segments: vec![MessageSegment::Text {
                        text: "/ping".to_owned(),
                    }],
                    reply_to: None,
                }),
                raw: json!({"platform":"mock"}),
            })),
        });
        let runtime = RuntimeBuilder::new()
            .adapter(adapter)
            .handler(Arc::new(PingHandler {
                name: "ping".to_owned(),
                shutdown: shutdown_handle,
            }))
            .build()
            .unwrap();

        runtime.run(shutdown_signal).await.unwrap();
        let action = actions.recv().await.unwrap();
        let Action::Reply(reply) = action else {
            panic!("expected reply action");
        };
        assert_eq!(reply.source_message_id, "source-message");
        assert_eq!(reply.content, "pong");
    }

    #[derive(Debug)]
    struct BurstAdapter {
        id: AdapterId,
        events: Mutex<Vec<EventEnvelope>>,
    }

    #[derive(Debug)]
    struct ShutdownDrainAdapter {
        id: AdapterId,
        event: Mutex<Option<EventEnvelope>>,
    }

    #[async_trait]
    impl Adapter for ShutdownDrainAdapter {
        fn id(&self) -> &AdapterId {
            &self.id
        }

        fn platform(&self) -> &'static str {
            "mock"
        }

        async fn run(
            &self,
            events: EventSender,
            mut shutdown: ShutdownSignal,
        ) -> Result<(), AdapterError> {
            events.mark_ready();
            shutdown.cancelled().await;
            if let Some(event) = self.event.lock().await.take() {
                events
                    .send(event)
                    .await
                    .map_err(|_| AdapterError::EventQueueClosed)?;
            }
            Ok(())
        }

        async fn execute(&self, _action: Action) -> Result<ActionResult, AdapterError> {
            Err(AdapterError::Action("not supported".to_owned()))
        }
    }

    #[async_trait]
    impl Adapter for BurstAdapter {
        fn id(&self) -> &AdapterId {
            &self.id
        }

        fn platform(&self) -> &'static str {
            "mock"
        }

        async fn run(
            &self,
            events: EventSender,
            mut shutdown: ShutdownSignal,
        ) -> Result<(), AdapterError> {
            events.mark_ready();
            for event in self.events.lock().await.drain(..) {
                events
                    .send(event)
                    .await
                    .map_err(|_| AdapterError::EventQueueClosed)?;
            }
            shutdown.cancelled().await;
            Ok(())
        }

        async fn execute(&self, _action: Action) -> Result<ActionResult, AdapterError> {
            Err(AdapterError::Action("not supported".to_owned()))
        }
    }

    #[derive(Debug, Default)]
    struct CountingHandler {
        invocations: AtomicUsize,
    }

    #[derive(Debug)]
    struct FailingHandler;

    #[derive(Debug, Default)]
    struct FailFirstHandler {
        successful_calls: AtomicUsize,
    }

    #[async_trait]
    impl EventHandler for FailingHandler {
        fn name(&self) -> &'static str {
            "failing"
        }

        async fn handle(&self, _context: Context, _event: &Event) -> Result<(), HandlerError> {
            Err(HandlerError::Failed("expected failure".to_owned()))
        }
    }

    #[async_trait]
    impl EventHandler for FailFirstHandler {
        fn name(&self) -> &'static str {
            "fail-first"
        }

        async fn handle(&self, _context: Context, event: &Event) -> Result<(), HandlerError> {
            let Event::Message(message) = event else {
                return Ok(());
            };
            if message.message_id == "release-first" {
                return Err(HandlerError::Failed("expected first failure".to_owned()));
            }
            self.successful_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[derive(Debug)]
    struct ObservedAdapter {
        id: AdapterId,
        events: Mutex<Vec<EventEnvelope>>,
        handled: AtomicUsize,
    }

    #[derive(Debug)]
    struct FailingAckAdapter {
        id: AdapterId,
        events: Mutex<Vec<EventEnvelope>>,
        failures_before_success: usize,
        ack_calls: AtomicUsize,
    }

    #[async_trait]
    impl Adapter for FailingAckAdapter {
        fn id(&self) -> &AdapterId {
            &self.id
        }

        fn platform(&self) -> &'static str {
            "mock"
        }

        async fn run(
            &self,
            events: EventSender,
            mut shutdown: ShutdownSignal,
        ) -> Result<(), AdapterError> {
            events.mark_ready();
            for event in self.events.lock().await.drain(..) {
                events
                    .send(event)
                    .await
                    .map_err(|_| AdapterError::EventQueueClosed)?;
            }
            shutdown.cancelled().await;
            Ok(())
        }

        async fn execute(&self, _action: Action) -> Result<ActionResult, AdapterError> {
            Err(AdapterError::Action("not supported".to_owned()))
        }

        async fn event_handled(&self, _event: &EventEnvelope) -> Result<(), AdapterError> {
            if self.ack_calls.fetch_add(1, Ordering::SeqCst) < self.failures_before_success {
                Err(AdapterError::Transport(
                    "temporary acknowledgement failure".to_owned(),
                ))
            } else {
                Ok(())
            }
        }
    }

    #[async_trait]
    impl Adapter for ObservedAdapter {
        fn id(&self) -> &AdapterId {
            &self.id
        }

        fn platform(&self) -> &'static str {
            "mock"
        }

        async fn run(
            &self,
            events: EventSender,
            mut shutdown: ShutdownSignal,
        ) -> Result<(), AdapterError> {
            events.mark_ready();
            for event in self.events.lock().await.drain(..) {
                events
                    .send(event)
                    .await
                    .map_err(|_| AdapterError::EventQueueClosed)?;
            }
            shutdown.cancelled().await;
            Ok(())
        }

        async fn execute(&self, _action: Action) -> Result<ActionResult, AdapterError> {
            Err(AdapterError::Action("not supported".to_owned()))
        }

        async fn event_handled(&self, _event: &EventEnvelope) -> Result<(), AdapterError> {
            self.handled.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[derive(Debug)]
    struct FailingClaimStore;

    #[async_trait]
    impl DedupStore for FailingClaimStore {
        async fn claim(&self, _key: DedupKey) -> Result<Option<DedupClaim>, DedupError> {
            Err(DedupError::permanent("claim unavailable"))
        }

        async fn commit(&self, _claim: &DedupClaim) -> Result<(), DedupError> {
            unreachable!("a failed claim cannot be committed")
        }

        async fn release(&self, _claim: &DedupClaim) -> Result<(), DedupError> {
            unreachable!("a failed claim cannot be released")
        }
    }

    #[derive(Debug)]
    struct FlakyClaimStore {
        inner: MemoryDedupStore,
        failures_before_success: usize,
        claim_calls: AtomicUsize,
    }

    #[derive(Debug)]
    struct ClaimConcurrencyStore {
        inner: MemoryDedupStore,
        active: Arc<AtomicUsize>,
        maximum: AtomicUsize,
    }

    struct ClaimActivity(Arc<AtomicUsize>);

    impl Drop for ClaimActivity {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl DedupStore for ClaimConcurrencyStore {
        async fn claim(&self, key: DedupKey) -> Result<Option<DedupClaim>, DedupError> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            let _activity = ClaimActivity(Arc::clone(&self.active));
            self.maximum.fetch_max(active, Ordering::SeqCst);
            sleep(Duration::from_millis(30)).await;
            self.inner.claim(key).await
        }

        async fn commit(&self, claim: &DedupClaim) -> Result<(), DedupError> {
            self.inner.commit(claim).await
        }

        async fn release(&self, claim: &DedupClaim) -> Result<(), DedupError> {
            self.inner.release(claim).await
        }
    }

    #[async_trait]
    impl DedupStore for FlakyClaimStore {
        async fn claim(&self, key: DedupKey) -> Result<Option<DedupClaim>, DedupError> {
            if self.claim_calls.fetch_add(1, Ordering::SeqCst) < self.failures_before_success {
                Err(DedupError::retryable("claim admission saturated"))
            } else {
                self.inner.claim(key).await
            }
        }

        async fn commit(&self, claim: &DedupClaim) -> Result<(), DedupError> {
            self.inner.commit(claim).await
        }

        async fn release(&self, claim: &DedupClaim) -> Result<(), DedupError> {
            self.inner.release(claim).await
        }
    }

    #[derive(Debug)]
    struct FlakyCommitStore {
        inner: MemoryDedupStore,
        failures_before_success: usize,
        commit_calls: AtomicUsize,
        release_calls: AtomicUsize,
    }

    #[async_trait]
    impl DedupStore for FlakyCommitStore {
        async fn claim(&self, key: DedupKey) -> Result<Option<DedupClaim>, DedupError> {
            self.inner.claim(key).await
        }

        async fn commit(&self, claim: &DedupClaim) -> Result<(), DedupError> {
            if self.commit_calls.fetch_add(1, Ordering::SeqCst) < self.failures_before_success {
                Err(DedupError::retryable("temporary commit failure"))
            } else {
                self.inner.commit(claim).await
            }
        }

        async fn release(&self, claim: &DedupClaim) -> Result<(), DedupError> {
            self.release_calls.fetch_add(1, Ordering::SeqCst);
            self.inner.release(claim).await
        }
    }

    #[derive(Debug)]
    struct FlakyReleaseStore {
        inner: MemoryDedupStore,
        failures_before_success: usize,
        release_calls: AtomicUsize,
    }

    #[derive(Debug)]
    struct FailingReleaseStore {
        inner: MemoryDedupStore,
        release_calls: AtomicUsize,
    }

    #[derive(Debug)]
    struct PermanentFinalizationStore {
        inner: MemoryDedupStore,
        fail_commit: bool,
    }

    #[async_trait]
    impl DedupStore for PermanentFinalizationStore {
        async fn claim(&self, key: DedupKey) -> Result<Option<DedupClaim>, DedupError> {
            self.inner.claim(key).await
        }

        async fn commit(&self, claim: &DedupClaim) -> Result<(), DedupError> {
            if self.fail_commit {
                Err(DedupError::permanent("commit requires operator recovery"))
            } else {
                self.inner.commit(claim).await
            }
        }

        async fn release(&self, claim: &DedupClaim) -> Result<(), DedupError> {
            if self.fail_commit {
                self.inner.release(claim).await
            } else {
                Err(DedupError::permanent("release requires operator recovery"))
            }
        }
    }

    #[async_trait]
    impl DedupStore for FailingReleaseStore {
        async fn claim(&self, key: DedupKey) -> Result<Option<DedupClaim>, DedupError> {
            self.inner.claim(key).await
        }

        async fn commit(&self, claim: &DedupClaim) -> Result<(), DedupError> {
            self.inner.commit(claim).await
        }

        async fn release(&self, _claim: &DedupClaim) -> Result<(), DedupError> {
            self.release_calls.fetch_add(1, Ordering::SeqCst);
            Err(DedupError::retryable("release remains unavailable"))
        }
    }

    #[async_trait]
    impl DedupStore for FlakyReleaseStore {
        async fn claim(&self, key: DedupKey) -> Result<Option<DedupClaim>, DedupError> {
            self.inner.claim(key).await
        }

        async fn commit(&self, claim: &DedupClaim) -> Result<(), DedupError> {
            self.inner.commit(claim).await
        }

        async fn release(&self, claim: &DedupClaim) -> Result<(), DedupError> {
            if self.release_calls.fetch_add(1, Ordering::SeqCst) < self.failures_before_success {
                Err(DedupError::retryable("temporary release failure"))
            } else {
                self.inner.release(claim).await
            }
        }
    }

    #[async_trait]
    impl EventHandler for CountingHandler {
        fn name(&self) -> &'static str {
            "counting"
        }

        async fn handle(&self, _context: Context, _event: &Event) -> Result<(), HandlerError> {
            self.invocations.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn message_event(id: &str, group: &str) -> EventEnvelope {
        EventEnvelope {
            id: EventId::new(id),
            adapter: AdapterId::new("burst"),
            delivery_id: None,
            timestamp: None,
            event: Event::Message(CommonMessage {
                message_id: id.to_owned(),
                target: MessageTarget::Group {
                    group_id: group.to_owned(),
                },
                sender: Sender {
                    id: "user".to_owned(),
                    display_name: None,
                },
                text: "hello".to_owned(),
                segments: Vec::new(),
                reply_to: None,
            }),
            raw: json!({"id":id}),
        }
    }

    async fn wait_for_completed(observer: &RuntimeObserver, expected: u64) {
        timeout(Duration::from_secs(2), async {
            loop {
                if observer.snapshot().events_completed >= expected {
                    break;
                }
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    async fn wait_for_active(observer: &RuntimeObserver, expected: usize) {
        timeout(Duration::from_secs(2), async {
            loop {
                if observer.snapshot().active_events >= expected {
                    break;
                }
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn runtime_rejects_duplicate_handler_names() {
        let adapter = Arc::new(ObservedAdapter {
            id: AdapterId::new("burst"),
            events: Mutex::new(Vec::new()),
            handled: AtomicUsize::new(0),
        });
        let result = RuntimeBuilder::new()
            .adapter(adapter)
            .handler(Arc::new(CountingHandler::default()))
            .handler(Arc::new(CountingHandler::default()))
            .build();
        assert!(matches!(
            result,
            Err(RuntimeError::DuplicateHandler(name)) if name == "counting"
        ));
    }

    #[test]
    fn runtime_rejects_control_characters_in_adapter_ids() {
        let adapter = Arc::new(ObservedAdapter {
            id: AdapterId::new("invalid\rid"),
            events: Mutex::new(Vec::new()),
            handled: AtomicUsize::new(0),
        });
        assert!(matches!(
            RuntimeBuilder::new().adapter(adapter).build(),
            Err(RuntimeError::InvalidAdapterId { index: 0 })
        ));
    }

    #[test]
    fn runtime_rejects_concurrency_above_tokio_semaphore_limit() {
        let adapter = || {
            Arc::new(ObservedAdapter {
                id: AdapterId::new("burst"),
                events: Mutex::new(Vec::new()),
                handled: AtomicUsize::new(0),
            })
        };
        assert!(matches!(
            RuntimeBuilder::new()
                .adapter(adapter())
                .event_concurrency(usize::MAX)
                .build(),
            Err(RuntimeError::EventConcurrencyTooLarge)
        ));
        assert!(matches!(
            RuntimeBuilder::new()
                .adapter(adapter())
                .queue_capacity(super::MAX_RUNTIME_QUEUE_CAPACITY + 1)
                .build(),
            Err(RuntimeError::QueueCapacityTooLarge)
        ));
        assert!(matches!(
            RuntimeBuilder::new()
                .adapter(adapter())
                .handler_with_policy(
                    Arc::new(CountingHandler::default()),
                    HandlerPolicy::default().max_concurrency(usize::MAX),
                )
                .build(),
            Err(RuntimeError::HandlerConcurrencyTooLarge { index: 0 })
        ));
    }

    #[test]
    fn runtime_rejects_adapter_counts_above_metric_cardinality_limit() {
        let mut builder = RuntimeBuilder::new();
        for index in 0..=super::MAX_ADAPTER_METRIC_SERIES {
            builder = builder.adapter(Arc::new(ObservedAdapter {
                id: AdapterId::new(format!("adapter-{index}")),
                events: Mutex::new(Vec::new()),
                handled: AtomicUsize::new(0),
            }));
        }
        assert!(matches!(
            builder.build(),
            Err(RuntimeError::TooManyAdapters)
        ));
    }

    #[tokio::test]
    async fn runtime_stops_handler_chain_after_first_failure() {
        let adapter = Arc::new(ObservedAdapter {
            id: AdapterId::new("burst"),
            events: Mutex::new(vec![message_event("failed", "group")]),
            handled: AtomicUsize::new(0),
        });
        let later = Arc::new(CountingHandler::default());
        let observer = RuntimeObserver::new();
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let runtime = RuntimeBuilder::new()
            .adapter(adapter.clone())
            .handler(Arc::new(FailingHandler))
            .handler(later.clone())
            .observer(observer.clone())
            .build()
            .unwrap();
        let task = tokio::spawn(runtime.run(shutdown_signal));

        wait_for_completed(&observer, 1).await;
        shutdown_handle.shutdown();
        task.await.unwrap().unwrap();
        assert_eq!(later.invocations.load(Ordering::SeqCst), 0);
        assert_eq!(adapter.handled.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn exhausted_source_acknowledgement_retries_recover_online() {
        let adapter = Arc::new(FailingAckAdapter {
            id: AdapterId::new("burst"),
            events: Mutex::new(vec![message_event("ack-failure", "group")]),
            failures_before_success: EVENT_ACK_MAX_ATTEMPTS,
            ack_calls: AtomicUsize::new(0),
        });
        let observer = RuntimeObserver::new();
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let runtime = RuntimeBuilder::new()
            .adapter(adapter.clone())
            .handler(Arc::new(CountingHandler::default()))
            .observer(observer.clone())
            .build()
            .unwrap();
        let task = tokio::spawn(runtime.run(shutdown_signal));

        wait_for_completed(&observer, 1).await;
        assert_eq!(
            adapter.ack_calls.load(Ordering::SeqCst),
            EVENT_ACK_MAX_ATTEMPTS + 1
        );
        shutdown_handle.shutdown();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn shutdown_finalizes_committed_but_unacknowledged_events() {
        let adapter = Arc::new(FailingAckAdapter {
            id: AdapterId::new("burst"),
            events: Mutex::new(vec![message_event("ack-shutdown", "group")]),
            failures_before_success: EVENT_ACK_MAX_ATTEMPTS,
            ack_calls: AtomicUsize::new(0),
        });
        let dedup = Arc::new(MemoryDedupStore::try_new(8).unwrap());
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let runtime = RuntimeBuilder::new()
            .adapter(adapter.clone())
            .handler(Arc::new(CountingHandler::default()))
            .dedup_store(dedup.clone())
            .shutdown_timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let task = tokio::spawn(runtime.run(shutdown_signal));

        timeout(Duration::from_secs(1), async {
            while adapter.ack_calls.load(Ordering::SeqCst) < EVENT_ACK_MAX_ATTEMPTS {
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        shutdown_handle.shutdown();
        assert!(matches!(
            task.await.unwrap(),
            Err(RuntimeError::EventDrainTimeout)
        ));
        assert_eq!(
            adapter.ack_calls.load(Ordering::SeqCst),
            EVENT_ACK_MAX_ATTEMPTS + 1
        );
        assert!(
            dedup
                .claim(DedupKey::new(
                    AdapterId::new("burst"),
                    EventId::new("ack-shutdown"),
                ))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn shutdown_drains_events_already_accepted_by_adapters() {
        let adapter = Arc::new(ShutdownDrainAdapter {
            id: AdapterId::new("burst"),
            event: Mutex::new(Some(message_event("shutdown-drain", "group"))),
        });
        let handler = Arc::new(CountingHandler::default());
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let runtime = RuntimeBuilder::new()
            .adapter(adapter)
            .handler(handler.clone())
            .build()
            .unwrap();
        let task = tokio::spawn(runtime.run(shutdown_signal));

        shutdown_handle.shutdown();
        task.await.unwrap().unwrap();
        assert_eq!(handler.invocations.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn transient_deduplication_release_failure_is_retried_while_running() {
        let adapter = Arc::new(ObservedAdapter {
            id: AdapterId::new("burst"),
            events: Mutex::new(vec![message_event("failed", "group")]),
            handled: AtomicUsize::new(0),
        });
        let store = Arc::new(FlakyReleaseStore {
            inner: MemoryDedupStore::try_new(8).unwrap(),
            failures_before_success: 1,
            release_calls: AtomicUsize::new(0),
        });
        let observer = RuntimeObserver::new();
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let runtime = RuntimeBuilder::new()
            .adapter(adapter)
            .handler(Arc::new(FailingHandler))
            .dedup_store(store.clone())
            .observer(observer.clone())
            .build()
            .unwrap();
        let task = tokio::spawn(runtime.run(shutdown_signal));

        wait_for_completed(&observer, 1).await;
        assert_eq!(store.release_calls.load(Ordering::SeqCst), 2);
        assert!(store.inner.is_empty());
        shutdown_handle.shutdown();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn permanent_release_failure_remains_outstanding_for_shutdown() {
        let adapter = Arc::new(ObservedAdapter {
            id: AdapterId::new("burst"),
            events: Mutex::new(vec![message_event("permanent-release", "group")]),
            handled: AtomicUsize::new(0),
        });
        let store = Arc::new(PermanentFinalizationStore {
            inner: MemoryDedupStore::try_new(8).unwrap(),
            fail_commit: false,
        });
        let observer = RuntimeObserver::new();
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let runtime = RuntimeBuilder::new()
            .adapter(adapter)
            .handler(Arc::new(FailingHandler))
            .dedup_store(store.clone())
            .observer(observer.clone())
            .build()
            .unwrap();
        let task = tokio::spawn(runtime.run(shutdown_signal));

        wait_for_completed(&observer, 1).await;
        shutdown_handle.shutdown();
        assert!(matches!(
            task.await.unwrap(),
            Err(RuntimeError::DedupFinalization { remaining: 1 })
        ));
        assert!(!store.inner.is_empty());
    }

    #[tokio::test]
    async fn permanent_commit_failure_remains_outstanding_for_shutdown() {
        let adapter = Arc::new(ObservedAdapter {
            id: AdapterId::new("burst"),
            events: Mutex::new(vec![message_event("permanent-commit", "group")]),
            handled: AtomicUsize::new(0),
        });
        let store = Arc::new(PermanentFinalizationStore {
            inner: MemoryDedupStore::try_new(8).unwrap(),
            fail_commit: true,
        });
        let observer = RuntimeObserver::new();
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let runtime = RuntimeBuilder::new()
            .adapter(adapter.clone())
            .handler(Arc::new(CountingHandler::default()))
            .dedup_store(store.clone())
            .observer(observer.clone())
            .build()
            .unwrap();
        let task = tokio::spawn(runtime.run(shutdown_signal));

        wait_for_completed(&observer, 1).await;
        shutdown_handle.shutdown();
        assert!(matches!(
            task.await.unwrap(),
            Err(RuntimeError::DedupFinalization { remaining: 1 })
        ));
        assert!(!store.inner.is_empty());
        assert_eq!(adapter.handled.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn exhausted_release_retries_continue_under_runtime_supervision() {
        let adapter = Arc::new(ObservedAdapter {
            id: AdapterId::new("burst"),
            events: Mutex::new(vec![message_event("release-pending", "group")]),
            handled: AtomicUsize::new(0),
        });
        let store = Arc::new(FlakyReleaseStore {
            inner: MemoryDedupStore::try_new(8).unwrap(),
            failures_before_success: DEDUP_RELEASE_MAX_ATTEMPTS,
            release_calls: AtomicUsize::new(0),
        });
        let observer = RuntimeObserver::new();
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let runtime = RuntimeBuilder::new()
            .adapter(adapter.clone())
            .handler(Arc::new(FailingHandler))
            .dedup_store(store.clone())
            .observer(observer.clone())
            .build()
            .unwrap();
        let task = tokio::spawn(runtime.run(shutdown_signal));

        wait_for_completed(&observer, 1).await;
        timeout(Duration::from_secs(3), async {
            while !store.inner.is_empty() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            store.release_calls.load(Ordering::SeqCst),
            DEDUP_RELEASE_MAX_ATTEMPTS + 1
        );
        assert_eq!(adapter.handled.load(Ordering::SeqCst), 0);

        shutdown_handle.shutdown();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn release_recovery_preserves_partition_order() {
        let adapter = Arc::new(ObservedAdapter {
            id: AdapterId::new("burst"),
            events: Mutex::new(vec![
                message_event("release-first", "group"),
                message_event("release-second", "group"),
            ]),
            handled: AtomicUsize::new(0),
        });
        let store = Arc::new(FlakyReleaseStore {
            inner: MemoryDedupStore::try_new(8).unwrap(),
            failures_before_success: DEDUP_RELEASE_MAX_ATTEMPTS,
            release_calls: AtomicUsize::new(0),
        });
        let handler = Arc::new(FailFirstHandler::default());
        let observer = RuntimeObserver::new();
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let runtime = RuntimeBuilder::new()
            .adapter(adapter.clone())
            .handler(handler.clone())
            .dedup_store(store.clone())
            .observer(observer.clone())
            .build()
            .unwrap();
        let task = tokio::spawn(runtime.run(shutdown_signal));

        timeout(Duration::from_secs(1), async {
            while store.release_calls.load(Ordering::SeqCst) < DEDUP_RELEASE_MAX_ATTEMPTS {
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(handler.successful_calls.load(Ordering::SeqCst), 0);

        wait_for_completed(&observer, 2).await;
        assert_eq!(handler.successful_calls.load(Ordering::SeqCst), 1);
        assert_eq!(adapter.handled.load(Ordering::SeqCst), 1);
        shutdown_handle.shutdown();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn release_recovery_does_not_block_unrelated_partitions() {
        let adapter = Arc::new(ObservedAdapter {
            id: AdapterId::new("burst"),
            events: Mutex::new(vec![
                message_event("release-first", "blocked-group"),
                message_event("release-second", "independent-group"),
            ]),
            handled: AtomicUsize::new(0),
        });
        let store = Arc::new(FlakyReleaseStore {
            inner: MemoryDedupStore::try_new(8).unwrap(),
            failures_before_success: DEDUP_RELEASE_MAX_ATTEMPTS,
            release_calls: AtomicUsize::new(0),
        });
        let handler = Arc::new(FailFirstHandler::default());
        let observer = RuntimeObserver::new();
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let runtime = RuntimeBuilder::new()
            .adapter(adapter)
            .handler(handler.clone())
            .dedup_store(store.clone())
            .event_concurrency(1)
            .observer(observer.clone())
            .build()
            .unwrap();
        let task = tokio::spawn(runtime.run(shutdown_signal));

        timeout(Duration::from_secs(1), async {
            while store.release_calls.load(Ordering::SeqCst) < DEDUP_RELEASE_MAX_ATTEMPTS {
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        timeout(Duration::from_millis(250), async {
            while handler.successful_calls.load(Ordering::SeqCst) == 0 {
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();

        wait_for_completed(&observer, 2).await;
        shutdown_handle.shutdown();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn deduplication_claim_failure_rejects_event_without_running_handlers() {
        let adapter = Arc::new(ObservedAdapter {
            id: AdapterId::new("burst"),
            events: Mutex::new(vec![message_event("unclaimed", "group")]),
            handled: AtomicUsize::new(0),
        });
        let handler = Arc::new(CountingHandler::default());
        let observer = RuntimeObserver::new();
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let runtime = RuntimeBuilder::new()
            .adapter(adapter.clone())
            .handler(handler.clone())
            .dedup_store(Arc::new(FailingClaimStore))
            .observer(observer.clone())
            .build()
            .unwrap();
        let task = tokio::spawn(runtime.run(shutdown_signal));

        wait_for_completed(&observer, 1).await;
        shutdown_handle.shutdown();
        task.await.unwrap().unwrap();
        assert_eq!(handler.invocations.load(Ordering::SeqCst), 0);
        assert_eq!(adapter.handled.load(Ordering::SeqCst), 0);
        assert_eq!(observer.snapshot().rejected_events, 1);
    }

    #[tokio::test]
    async fn retryable_claim_admission_retains_the_event_until_capacity_returns() {
        let adapter = Arc::new(ObservedAdapter {
            id: AdapterId::new("burst"),
            events: Mutex::new(vec![message_event("claim-retry", "group")]),
            handled: AtomicUsize::new(0),
        });
        let handler = Arc::new(CountingHandler::default());
        let store = Arc::new(FlakyClaimStore {
            inner: MemoryDedupStore::try_new(8).unwrap(),
            failures_before_success: 1,
            claim_calls: AtomicUsize::new(0),
        });
        let observer = RuntimeObserver::new();
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let runtime = RuntimeBuilder::new()
            .adapter(adapter.clone())
            .handler(handler.clone())
            .dedup_store(store.clone())
            .observer(observer.clone())
            .build()
            .unwrap();
        let task = tokio::spawn(runtime.run(shutdown_signal));

        wait_for_completed(&observer, 1).await;
        assert_eq!(store.claim_calls.load(Ordering::SeqCst), 2);
        assert_eq!(handler.invocations.load(Ordering::SeqCst), 1);
        assert_eq!(adapter.handled.load(Ordering::SeqCst), 1);
        assert_eq!(observer.snapshot().rejected_events, 0);
        shutdown_handle.shutdown();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn deduplication_claim_admission_respects_event_concurrency() {
        let adapter = Arc::new(ObservedAdapter {
            id: AdapterId::new("burst"),
            events: Mutex::new(vec![
                message_event("claim-one", "group-one"),
                message_event("claim-two", "group-two"),
            ]),
            handled: AtomicUsize::new(0),
        });
        let store = Arc::new(ClaimConcurrencyStore {
            inner: MemoryDedupStore::try_new(8).unwrap(),
            active: Arc::new(AtomicUsize::new(0)),
            maximum: AtomicUsize::new(0),
        });
        let observer = RuntimeObserver::new();
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let runtime = RuntimeBuilder::new()
            .adapter(adapter)
            .handler(Arc::new(CountingHandler::default()))
            .event_concurrency(1)
            .dedup_store(store.clone())
            .observer(observer.clone())
            .build()
            .unwrap();
        let task = tokio::spawn(runtime.run(shutdown_signal));

        wait_for_completed(&observer, 2).await;
        shutdown_handle.shutdown();
        task.await.unwrap().unwrap();
        assert_eq!(store.maximum.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn transient_deduplication_commit_failure_is_retried_before_acknowledging() {
        let adapter = Arc::new(ObservedAdapter {
            id: AdapterId::new("burst"),
            events: Mutex::new(vec![message_event("commit-pending", "group")]),
            handled: AtomicUsize::new(0),
        });
        let store = Arc::new(FlakyCommitStore {
            inner: MemoryDedupStore::try_new(8).unwrap(),
            failures_before_success: 1,
            commit_calls: AtomicUsize::new(0),
            release_calls: AtomicUsize::new(0),
        });
        let handler = Arc::new(CountingHandler::default());
        let observer = RuntimeObserver::new();
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let runtime = RuntimeBuilder::new()
            .adapter(adapter.clone())
            .handler(handler.clone())
            .dedup_store(store.clone())
            .observer(observer.clone())
            .build()
            .unwrap();
        let task = tokio::spawn(runtime.run(shutdown_signal));

        wait_for_completed(&observer, 1).await;
        assert_eq!(handler.invocations.load(Ordering::SeqCst), 1);
        assert_eq!(adapter.handled.load(Ordering::SeqCst), 1);
        assert_eq!(store.commit_calls.load(Ordering::SeqCst), 2);
        assert_eq!(store.release_calls.load(Ordering::SeqCst), 0);

        shutdown_handle.shutdown();
        task.await.unwrap().unwrap();
        assert_eq!(store.commit_calls.load(Ordering::SeqCst), 2);
        assert_eq!(store.release_calls.load(Ordering::SeqCst), 0);
        assert!(
            store
                .inner
                .claim(DedupKey::new(
                    AdapterId::new("burst"),
                    EventId::new("commit-pending"),
                ))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn shutdown_waits_for_pending_commit_recovery_before_adapter_ack() {
        let adapter = Arc::new(ObservedAdapter {
            id: AdapterId::new("burst"),
            events: Mutex::new(vec![message_event("commit-pending", "group")]),
            handled: AtomicUsize::new(0),
        });
        let store = Arc::new(FlakyCommitStore {
            inner: MemoryDedupStore::try_new(8).unwrap(),
            failures_before_success: DEDUP_COMMIT_MAX_ATTEMPTS,
            commit_calls: AtomicUsize::new(0),
            release_calls: AtomicUsize::new(0),
        });
        let observer = RuntimeObserver::new();
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let runtime = RuntimeBuilder::new()
            .adapter(adapter.clone())
            .handler(Arc::new(CountingHandler::default()))
            .dedup_store(store.clone())
            .observer(observer.clone())
            .build()
            .unwrap();
        let task = tokio::spawn(runtime.run(shutdown_signal));

        timeout(Duration::from_secs(1), async {
            while store.commit_calls.load(Ordering::SeqCst) < DEDUP_COMMIT_MAX_ATTEMPTS {
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(adapter.handled.load(Ordering::SeqCst), 0);
        assert_eq!(
            store.commit_calls.load(Ordering::SeqCst),
            DEDUP_COMMIT_MAX_ATTEMPTS
        );
        assert_eq!(store.release_calls.load(Ordering::SeqCst), 0);

        shutdown_handle.shutdown();
        task.await.unwrap().unwrap();
        assert_eq!(
            store.commit_calls.load(Ordering::SeqCst),
            DEDUP_COMMIT_MAX_ATTEMPTS + 1
        );
        assert_eq!(store.release_calls.load(Ordering::SeqCst), 0);
        assert_eq!(adapter.handled.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn exhausted_commit_retries_recover_and_ack_while_runtime_stays_online() {
        let adapter = Arc::new(ObservedAdapter {
            id: AdapterId::new("burst"),
            events: Mutex::new(vec![message_event("commit-recovery", "group")]),
            handled: AtomicUsize::new(0),
        });
        let store = Arc::new(FlakyCommitStore {
            inner: MemoryDedupStore::try_new(8).unwrap(),
            failures_before_success: DEDUP_COMMIT_MAX_ATTEMPTS,
            commit_calls: AtomicUsize::new(0),
            release_calls: AtomicUsize::new(0),
        });
        let observer = RuntimeObserver::new();
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let runtime = RuntimeBuilder::new()
            .adapter(adapter.clone())
            .handler(Arc::new(CountingHandler::default()))
            .dedup_store(store.clone())
            .observer(observer.clone())
            .build()
            .unwrap();
        let task = tokio::spawn(runtime.run(shutdown_signal));

        wait_for_completed(&observer, 1).await;
        timeout(Duration::from_secs(3), async {
            while adapter.handled.load(Ordering::SeqCst) == 0 {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            store.commit_calls.load(Ordering::SeqCst),
            DEDUP_COMMIT_MAX_ATTEMPTS + 1
        );
        assert_eq!(store.release_calls.load(Ordering::SeqCst), 0);

        shutdown_handle.shutdown();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn runtime_deduplicates_successfully_handled_events() {
        let adapter = Arc::new(BurstAdapter {
            id: AdapterId::new("burst"),
            events: Mutex::new(vec![
                message_event("duplicate", "group-a"),
                message_event("duplicate", "group-b"),
            ]),
        });
        let handler = Arc::new(CountingHandler::default());
        let observer = RuntimeObserver::new();
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let runtime = RuntimeBuilder::new()
            .event_concurrency(2)
            .adapter(adapter)
            .handler(handler.clone())
            .observer(observer.clone())
            .build()
            .unwrap();
        let task = tokio::spawn(runtime.run(shutdown_signal));

        wait_for_completed(&observer, 2).await;
        shutdown_handle.shutdown();
        task.await.unwrap().unwrap();
        assert_eq!(handler.invocations.load(Ordering::SeqCst), 1);
        assert_eq!(observer.snapshot().duplicate_events, 1);
    }

    #[derive(Debug, Default)]
    struct ConcurrencyHandler {
        active: AtomicUsize,
        maximum: AtomicUsize,
    }

    #[async_trait]
    impl EventHandler for ConcurrencyHandler {
        fn name(&self) -> &'static str {
            "concurrency"
        }

        async fn handle(&self, _context: Context, _event: &Event) -> Result<(), HandlerError> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.maximum.fetch_max(active, Ordering::SeqCst);
            sleep(Duration::from_millis(30)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn per_handler_concurrency_policy_limits_cross_partition_work() {
        let adapter = Arc::new(BurstAdapter {
            id: AdapterId::new("burst"),
            events: Mutex::new(vec![message_event("one", "a"), message_event("two", "b")]),
        });
        let handler = Arc::new(ConcurrencyHandler::default());
        let observer = RuntimeObserver::new();
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let runtime = RuntimeBuilder::new()
            .event_concurrency(2)
            .adapter(adapter)
            .handler_with_policy(handler.clone(), HandlerPolicy::default().max_concurrency(1))
            .observer(observer.clone())
            .build()
            .unwrap();
        let task = tokio::spawn(runtime.run(shutdown_signal));

        wait_for_completed(&observer, 2).await;
        shutdown_handle.shutdown();
        task.await.unwrap().unwrap();
        assert_eq!(handler.maximum.load(Ordering::SeqCst), 1);
    }

    #[derive(Debug)]
    struct SlowHandler;

    #[async_trait]
    impl EventHandler for SlowHandler {
        fn name(&self) -> &'static str {
            "slow"
        }

        async fn handle(&self, _context: Context, _event: &Event) -> Result<(), HandlerError> {
            sleep(Duration::from_secs(1)).await;
            Ok(())
        }
    }

    #[tokio::test]
    async fn per_handler_timeout_is_observed_and_not_deduplicated() {
        let adapter = Arc::new(BurstAdapter {
            id: AdapterId::new("burst"),
            events: Mutex::new(vec![message_event("slow", "group")]),
        });
        let dedup = Arc::new(MemoryDedupStore::try_new(8).unwrap());
        let observer = RuntimeObserver::new();
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let runtime = RuntimeBuilder::new()
            .adapter(adapter)
            .handler_with_policy(
                Arc::new(SlowHandler),
                HandlerPolicy::default().timeout(Duration::from_millis(10)),
            )
            .dedup_store(dedup.clone())
            .observer(observer.clone())
            .build()
            .unwrap();
        let task = tokio::spawn(runtime.run(shutdown_signal));

        wait_for_completed(&observer, 1).await;
        shutdown_handle.shutdown();
        task.await.unwrap().unwrap();
        assert!(dedup.is_empty());
        assert_eq!(observer.snapshot().handlers["slow"].timeouts, 1);
    }

    #[tokio::test]
    async fn forced_shutdown_releases_outstanding_deduplication_claims() {
        let adapter = Arc::new(BurstAdapter {
            id: AdapterId::new("burst"),
            events: Mutex::new(vec![message_event("cancelled", "group")]),
        });
        let dedup = Arc::new(MemoryDedupStore::try_new(8).unwrap());
        let observer = RuntimeObserver::new();
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let runtime = RuntimeBuilder::new()
            .adapter(adapter)
            .handler(Arc::new(SlowHandler))
            .handler_timeout(Duration::from_secs(10))
            .shutdown_timeout(Duration::from_millis(10))
            .dedup_store(dedup.clone())
            .observer(observer.clone())
            .build()
            .unwrap();
        let task = tokio::spawn(runtime.run(shutdown_signal));

        wait_for_active(&observer, 1).await;
        shutdown_handle.shutdown();
        assert!(matches!(
            task.await.unwrap(),
            Err(RuntimeError::EventDrainTimeout)
        ));
        assert!(dedup.is_empty());
    }

    #[tokio::test]
    async fn aborting_runtime_future_recovers_in_flight_memory_claims() {
        let adapter = Arc::new(BurstAdapter {
            id: AdapterId::new("burst"),
            events: Mutex::new(vec![message_event("aborted-runtime", "group")]),
        });
        let dedup = Arc::new(MemoryDedupStore::try_new(8).unwrap());
        let observer = RuntimeObserver::new();
        let (_shutdown_handle, shutdown_signal) = shutdown_channel();
        let runtime = RuntimeBuilder::new()
            .adapter(adapter)
            .handler(Arc::new(SlowHandler))
            .handler_timeout(Duration::from_secs(10))
            .dedup_store(dedup.clone())
            .observer(observer.clone())
            .build()
            .unwrap();
        let task = tokio::spawn(runtime.run(shutdown_signal));

        wait_for_active(&observer, 1).await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let replacement = timeout(
            Duration::from_secs(1),
            dedup.claim(DedupKey::new(
                AdapterId::new("burst"),
                EventId::new("aborted-runtime"),
            )),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        dedup.release(&replacement).await.unwrap();
    }

    #[tokio::test]
    async fn aborting_runtime_keeps_retryable_claim_recovery_owned() {
        let adapter = Arc::new(BurstAdapter {
            id: AdapterId::new("burst"),
            events: Mutex::new(vec![message_event("aborted-retry", "group")]),
        });
        let store = Arc::new(FlakyReleaseStore {
            inner: MemoryDedupStore::try_new(8).unwrap(),
            failures_before_success: DEDUP_RELEASE_MAX_ATTEMPTS,
            release_calls: AtomicUsize::new(0),
        });
        let observer = RuntimeObserver::new();
        let (_shutdown_handle, shutdown_signal) = shutdown_channel();
        let runtime = RuntimeBuilder::new()
            .adapter(adapter)
            .handler(Arc::new(SlowHandler))
            .handler_timeout(Duration::from_secs(10))
            .dedup_store(store.clone())
            .observer(observer.clone())
            .build()
            .unwrap();
        let task = tokio::spawn(runtime.run(shutdown_signal));

        wait_for_active(&observer, 1).await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        timeout(Duration::from_secs(3), async {
            while !store.inner.is_empty() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            store.release_calls.load(Ordering::SeqCst),
            DEDUP_RELEASE_MAX_ATTEMPTS + 1
        );
    }

    #[tokio::test]
    async fn shutdown_joins_cancelled_claim_recovery_jobs() {
        let adapter = Arc::new(BurstAdapter {
            id: AdapterId::new("burst"),
            events: Mutex::new(vec![message_event("bounded-recovery", "group")]),
        });
        let store = Arc::new(FailingReleaseStore {
            inner: MemoryDedupStore::try_new(8).unwrap(),
            release_calls: AtomicUsize::new(0),
        });
        let observer = RuntimeObserver::new();
        let (shutdown_handle, shutdown_signal) = shutdown_channel();
        let runtime = RuntimeBuilder::new()
            .adapter(adapter)
            .handler(Arc::new(SlowHandler))
            .handler_timeout(Duration::from_secs(10))
            .shutdown_timeout(Duration::from_millis(100))
            .dedup_store(store.clone())
            .observer(observer.clone())
            .build()
            .unwrap();
        let task = tokio::spawn(runtime.run(shutdown_signal));

        wait_for_active(&observer, 1).await;
        shutdown_handle.shutdown();
        assert!(task.await.unwrap().is_err());
        let calls_after_shutdown = store.release_calls.load(Ordering::SeqCst);
        sleep(Duration::from_millis(250)).await;
        assert_eq!(
            store.release_calls.load(Ordering::SeqCst),
            calls_after_shutdown
        );
    }

    #[tokio::test]
    async fn finalization_reports_claims_that_remain_after_its_deadline() {
        let store = Arc::new(FailingReleaseStore {
            inner: MemoryDedupStore::try_new(8).unwrap(),
            release_calls: AtomicUsize::new(0),
        });
        let claim = store
            .claim(DedupKey::new(
                AdapterId::new("burst"),
                EventId::new("unfinalized"),
            ))
            .await
            .unwrap()
            .unwrap();
        let outstanding = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        track_claim(
            &outstanding,
            claim.duplicate(),
            message_event("unfinalized", "group"),
            Arc::new(ObservedAdapter {
                id: AdapterId::new("burst"),
                events: Mutex::new(Vec::new()),
                handled: AtomicUsize::new(0),
            }),
        );
        let (recovery, _commands) = mpsc::unbounded_channel();
        let coordinator = DedupCoordinator {
            store,
            outstanding,
            recovery,
        };
        assert!(matches!(
            finalize_outstanding_claims(&coordinator, Duration::from_millis(10)).await,
            Err(RuntimeError::DedupFinalization { remaining: 1 })
        ));
    }

    #[tokio::test]
    async fn event_join_failure_aborts_and_joins_sibling_tasks() {
        struct Dropped(Arc<AtomicBool>);

        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let sibling_dropped = Arc::new(AtomicBool::new(false));
        let sibling_started = Arc::new(Notify::new());
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn({
            let sibling_started = Arc::clone(&sibling_started);
            async move {
                sibling_started.notified().await;
                panic!("expected event task panic");
            }
        });
        tasks.spawn({
            let sibling_dropped = Arc::clone(&sibling_dropped);
            let sibling_started = Arc::clone(&sibling_started);
            async move {
                let _dropped = Dropped(sibling_dropped);
                sibling_started.notify_one();
                std::future::pending::<()>().await;
            }
        });

        assert!(matches!(
            finish_event_tasks(&mut tasks, Duration::from_secs(1)).await,
            Err(RuntimeError::EventJoin(_))
        ));
        assert!(tasks.is_empty());
        assert!(sibling_dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn adapter_shutdown_timeout_is_reported_after_forced_abort() {
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(async {
            std::future::pending::<()>().await;
            (AdapterId::new("pending"), Ok(()))
        });

        assert!(matches!(
            finish_adapter_tasks(&mut tasks, Duration::from_millis(1)).await,
            Err(RuntimeError::AdapterShutdownTimeout)
        ));
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn adapter_error_aborts_and_joins_sibling_tasks() {
        struct Dropped(Arc<AtomicBool>);

        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let sibling_started = Arc::new(Notify::new());
        let sibling_dropped = Arc::new(AtomicBool::new(false));
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn({
            let sibling_started = Arc::clone(&sibling_started);
            async move {
                sibling_started.notified().await;
                (
                    AdapterId::new("failed"),
                    Err(AdapterError::Transport("expected failure".to_owned())),
                )
            }
        });
        tasks.spawn({
            let sibling_started = Arc::clone(&sibling_started);
            let sibling_dropped = Arc::clone(&sibling_dropped);
            async move {
                let _dropped = Dropped(sibling_dropped);
                sibling_started.notify_one();
                std::future::pending::<()>().await;
                (AdapterId::new("sibling"), Ok(()))
            }
        });

        assert!(matches!(
            finish_adapter_tasks(&mut tasks, Duration::from_secs(1)).await,
            Err(RuntimeError::AdapterStopped { .. })
        ));
        assert!(tasks.is_empty());
        assert!(sibling_dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn adapter_join_failure_aborts_and_joins_sibling_tasks() {
        struct Dropped(Arc<AtomicBool>);

        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let sibling_started = Arc::new(Notify::new());
        let sibling_dropped = Arc::new(AtomicBool::new(false));
        let mut tasks = tokio::task::JoinSet::<(AdapterId, Result<(), AdapterError>)>::new();
        tasks.spawn({
            let sibling_started = Arc::clone(&sibling_started);
            async move {
                sibling_started.notified().await;
                panic!("expected adapter task panic");
            }
        });
        tasks.spawn({
            let sibling_started = Arc::clone(&sibling_started);
            let sibling_dropped = Arc::clone(&sibling_dropped);
            async move {
                let _dropped = Dropped(sibling_dropped);
                sibling_started.notify_one();
                std::future::pending::<()>().await;
                (AdapterId::new("sibling"), Ok(()))
            }
        });

        assert!(matches!(
            finish_adapter_tasks(&mut tasks, Duration::from_secs(1)).await,
            Err(RuntimeError::AdapterJoin(_))
        ));
        assert!(tasks.is_empty());
        assert!(sibling_dropped.load(Ordering::SeqCst));
    }
}
