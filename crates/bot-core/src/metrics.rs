//! Runtime metrics and health snapshots without a required exporter backend.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use serde::Serialize;

use crate::{AdapterError, AdapterId};

pub(crate) const MAX_ADAPTER_METRIC_SERIES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimePhase {
    Starting,
    Running,
    ShuttingDown,
    Stopped,
    Failed,
}

impl RuntimePhase {
    const fn code(self) -> u8 {
        match self {
            Self::Starting => 0,
            Self::Running => 1,
            Self::ShuttingDown => 2,
            Self::Stopped => 3,
            Self::Failed => 4,
        }
    }

    const fn from_code(code: u8) -> Self {
        match code {
            0 => Self::Starting,
            1 => Self::Running,
            2 => Self::ShuttingDown,
            3 => Self::Stopped,
            _ => Self::Failed,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AdapterMetrics {
    pub online: bool,
    pub events_received: u64,
    pub events_rejected: u64,
    pub enqueue_wait_micros: u64,
    pub enqueue_wait_micros_max: u64,
    pub failures: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<AdapterFailureKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterFailureKind {
    Configuration,
    Transport,
    Action,
    ActionUnknown,
    EventQueueClosed,
    EventAdapterMismatch,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct HandlerMetrics {
    pub invocations: u64,
    pub successes: u64,
    pub failures: u64,
    pub timeouts: u64,
    pub duration_micros: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeMetricsSnapshot {
    pub phase: RuntimePhase,
    pub queue_capacity: usize,
    /// Queue depth sampled immediately after enqueue/dequeue operations.
    pub queue_depth: usize,
    /// Highest sampled queue depth; concurrent receivers can make this lower
    /// than the instantaneous channel occupancy.
    pub queue_depth_observed_high_watermark: usize,
    pub active_events: usize,
    pub events_received: u64,
    pub events_completed: u64,
    pub duplicate_events: u64,
    pub rejected_events: u64,
    pub enqueue_wait_micros: u64,
    pub enqueue_wait_micros_max: u64,
    pub adapters: BTreeMap<String, AdapterMetrics>,
    pub handlers: BTreeMap<String, HandlerMetrics>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Healthy,
    Degraded,
    Unhealthy,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeHealth {
    pub status: HealthStatus,
    pub phase: RuntimePhase,
    pub online_adapters: usize,
    pub configured_adapters: usize,
}

#[derive(Debug, Clone)]
pub struct RuntimeObserver {
    inner: Arc<ObserverInner>,
}

#[derive(Debug)]
struct ObserverInner {
    active_run: AtomicBool,
    phase: AtomicU8,
    queue_capacity: AtomicUsize,
    queue_depth: AtomicUsize,
    queue_depth_observed_high_watermark: AtomicUsize,
    active_events: AtomicUsize,
    events_received: AtomicU64,
    events_completed: AtomicU64,
    duplicate_events: AtomicU64,
    rejected_events: AtomicU64,
    enqueue_wait_micros: AtomicU64,
    enqueue_wait_micros_max: AtomicU64,
    adapters: Mutex<BTreeMap<String, AdapterMetrics>>,
    handlers: Mutex<BTreeMap<String, HandlerMetrics>>,
}

impl Default for RuntimeObserver {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeObserver {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(ObserverInner {
                active_run: AtomicBool::new(false),
                phase: AtomicU8::new(RuntimePhase::Stopped.code()),
                queue_capacity: AtomicUsize::new(0),
                queue_depth: AtomicUsize::new(0),
                queue_depth_observed_high_watermark: AtomicUsize::new(0),
                active_events: AtomicUsize::new(0),
                events_received: AtomicU64::new(0),
                events_completed: AtomicU64::new(0),
                duplicate_events: AtomicU64::new(0),
                rejected_events: AtomicU64::new(0),
                enqueue_wait_micros: AtomicU64::new(0),
                enqueue_wait_micros_max: AtomicU64::new(0),
                adapters: Mutex::new(BTreeMap::new()),
                handlers: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    pub fn snapshot(&self) -> RuntimeMetricsSnapshot {
        RuntimeMetricsSnapshot {
            phase: self.phase(),
            queue_capacity: self.inner.queue_capacity.load(Ordering::Relaxed),
            queue_depth: self.inner.queue_depth.load(Ordering::Relaxed),
            queue_depth_observed_high_watermark: self
                .inner
                .queue_depth_observed_high_watermark
                .load(Ordering::Relaxed),
            active_events: self.inner.active_events.load(Ordering::Relaxed),
            events_received: self.inner.events_received.load(Ordering::Relaxed),
            events_completed: self.inner.events_completed.load(Ordering::Relaxed),
            duplicate_events: self.inner.duplicate_events.load(Ordering::Relaxed),
            rejected_events: self.inner.rejected_events.load(Ordering::Relaxed),
            enqueue_wait_micros: self.inner.enqueue_wait_micros.load(Ordering::Relaxed),
            enqueue_wait_micros_max: self.inner.enqueue_wait_micros_max.load(Ordering::Relaxed),
            adapters: self
                .inner
                .adapters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            handlers: self
                .inner
                .handlers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        }
    }

    pub fn health(&self) -> RuntimeHealth {
        let snapshot = self.snapshot();
        let online_adapters = snapshot
            .adapters
            .values()
            .filter(|adapter| adapter.online)
            .count();
        let configured_adapters = snapshot.adapters.len();
        let status = match snapshot.phase {
            RuntimePhase::Running
                if configured_adapters > 0 && online_adapters == configured_adapters =>
            {
                HealthStatus::Healthy
            }
            RuntimePhase::Running | RuntimePhase::Starting | RuntimePhase::ShuttingDown => {
                HealthStatus::Degraded
            }
            RuntimePhase::Stopped | RuntimePhase::Failed => HealthStatus::Unhealthy,
        };
        RuntimeHealth {
            status,
            phase: snapshot.phase,
            online_adapters,
            configured_adapters,
        }
    }

    pub fn render_prometheus(&self) -> String {
        let snapshot = self.snapshot();
        let mut output = format!(
            "# HELP bkm_runtime_queue_depth Queue depth sampled after enqueue and dequeue operations.\n# HELP bkm_runtime_queue_depth_observed_high_watermark Highest sampled queue depth; concurrent receives can make it lower than instantaneous occupancy.\nbkm_runtime_queue_capacity {}\nbkm_runtime_queue_depth {}\nbkm_runtime_queue_depth_observed_high_watermark {}\nbkm_runtime_enqueue_wait_microseconds_total {}\nbkm_runtime_enqueue_wait_microseconds_max {}\nbkm_runtime_active_events {}\nbkm_runtime_events_received_total {}\nbkm_runtime_events_completed_total {}\nbkm_runtime_duplicate_events_total {}\nbkm_runtime_rejected_events_total {}\n",
            snapshot.queue_capacity,
            snapshot.queue_depth,
            snapshot.queue_depth_observed_high_watermark,
            snapshot.enqueue_wait_micros,
            snapshot.enqueue_wait_micros_max,
            snapshot.active_events,
            snapshot.events_received,
            snapshot.events_completed,
            snapshot.duplicate_events,
            snapshot.rejected_events,
        );
        for (adapter, metrics) in snapshot.adapters {
            let adapter = prometheus_label(&adapter);
            let _ = write!(
                output,
                "bkm_runtime_adapter_online{{adapter=\"{adapter}\"}} {}\nbkm_runtime_adapter_events_total{{adapter=\"{adapter}\"}} {}\nbkm_runtime_adapter_rejected_events_total{{adapter=\"{adapter}\"}} {}\nbkm_runtime_adapter_enqueue_wait_microseconds_total{{adapter=\"{adapter}\"}} {}\nbkm_runtime_adapter_enqueue_wait_microseconds_max{{adapter=\"{adapter}\"}} {}\nbkm_runtime_adapter_failures_total{{adapter=\"{adapter}\"}} {}\n",
                u8::from(metrics.online),
                metrics.events_received,
                metrics.events_rejected,
                metrics.enqueue_wait_micros,
                metrics.enqueue_wait_micros_max,
                metrics.failures,
            );
        }
        for (handler, metrics) in snapshot.handlers {
            let handler = prometheus_label(&handler);
            let _ = write!(
                output,
                "bkm_runtime_handler_invocations_total{{handler=\"{handler}\"}} {}\nbkm_runtime_handler_successes_total{{handler=\"{handler}\"}} {}\nbkm_runtime_handler_failures_total{{handler=\"{handler}\"}} {}\nbkm_runtime_handler_timeouts_total{{handler=\"{handler}\"}} {}\nbkm_runtime_handler_duration_microseconds_total{{handler=\"{handler}\"}} {}\n",
                metrics.invocations,
                metrics.successes,
                metrics.failures,
                metrics.timeouts,
                metrics.duration_micros,
            );
        }
        output
    }

    pub fn phase(&self) -> RuntimePhase {
        RuntimePhase::from_code(self.inner.phase.load(Ordering::Acquire))
    }

    pub(crate) fn configure(&self, queue_capacity: usize, adapters: &[Arc<dyn crate::Adapter>]) {
        self.inner
            .queue_capacity
            .store(queue_capacity, Ordering::Relaxed);
        self.inner.queue_depth.store(0, Ordering::Relaxed);
        self.inner
            .queue_depth_observed_high_watermark
            .store(0, Ordering::Relaxed);
        self.inner.active_events.store(0, Ordering::Relaxed);
        self.inner.events_received.store(0, Ordering::Relaxed);
        self.inner.events_completed.store(0, Ordering::Relaxed);
        self.inner.duplicate_events.store(0, Ordering::Relaxed);
        self.inner.rejected_events.store(0, Ordering::Relaxed);
        self.inner.enqueue_wait_micros.store(0, Ordering::Relaxed);
        self.inner
            .enqueue_wait_micros_max
            .store(0, Ordering::Relaxed);
        let mut metrics = self
            .inner
            .adapters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        metrics.clear();
        for adapter in adapters {
            metrics.entry(adapter.id().to_string()).or_default();
        }
        self.inner
            .handlers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    pub(crate) fn begin_run(
        &self,
        queue_capacity: usize,
        adapters: &[Arc<dyn crate::Adapter>],
    ) -> bool {
        if self
            .inner
            .active_run
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        self.configure(queue_capacity, adapters);
        true
    }

    pub(crate) fn end_run(&self) {
        let phase = self.phase();
        if matches!(
            phase,
            RuntimePhase::Starting | RuntimePhase::Running | RuntimePhase::ShuttingDown
        ) {
            self.set_phase(RuntimePhase::Stopped);
        }
        self.inner.active_run.store(false, Ordering::Release);
    }

    pub(crate) fn set_phase(&self, phase: RuntimePhase) {
        self.inner.phase.store(phase.code(), Ordering::Release);
    }

    pub(crate) fn set_adapter_online(&self, adapter: &AdapterId, online: bool) {
        self.update_adapter_metrics(adapter, |metrics| metrics.online = online);
    }

    pub(crate) fn record_adapter_failure(&self, adapter: &AdapterId, error: &AdapterError) {
        self.update_adapter_metrics(adapter, |metrics| {
            metrics.online = false;
            metrics.failures = metrics.failures.saturating_add(1);
            metrics.last_failure = Some(match error {
                AdapterError::Configuration(_) => AdapterFailureKind::Configuration,
                AdapterError::Transport(_) => AdapterFailureKind::Transport,
                AdapterError::Action(_) => AdapterFailureKind::Action,
                AdapterError::ActionUnknown(_) => AdapterFailureKind::ActionUnknown,
                AdapterError::EventQueueClosed => AdapterFailureKind::EventQueueClosed,
                AdapterError::EventAdapterMismatch { .. } => {
                    AdapterFailureKind::EventAdapterMismatch
                }
            });
        });
    }

    pub(crate) fn record_event_enqueued(
        &self,
        adapter: &AdapterId,
        enqueue_wait: Duration,
        queue_depth: usize,
    ) {
        let wait_micros = u64::try_from(enqueue_wait.as_micros()).unwrap_or(u64::MAX);
        saturating_fetch_add_u64(&self.inner.events_received, 1);
        saturating_fetch_add_u64(&self.inner.enqueue_wait_micros, wait_micros);
        self.inner
            .enqueue_wait_micros_max
            .fetch_max(wait_micros, Ordering::Relaxed);
        self.inner.queue_depth.store(queue_depth, Ordering::Relaxed);
        self.inner
            .queue_depth_observed_high_watermark
            .fetch_max(queue_depth, Ordering::Relaxed);
        self.update_adapter_metrics(adapter, |metrics| {
            metrics.events_received = metrics.events_received.saturating_add(1);
            metrics.enqueue_wait_micros = metrics.enqueue_wait_micros.saturating_add(wait_micros);
            metrics.enqueue_wait_micros_max = metrics.enqueue_wait_micros_max.max(wait_micros);
        });
    }

    pub(crate) fn record_queue_depth(&self, queue_depth: usize) {
        self.inner.queue_depth.store(queue_depth, Ordering::Relaxed);
    }

    pub(crate) fn record_event_started(&self) {
        saturating_fetch_add_usize(&self.inner.active_events, 1);
    }

    pub(crate) fn record_event_completed(&self) {
        let _ =
            self.inner
                .active_events
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                    Some(value.saturating_sub(1))
                });
        saturating_fetch_add_u64(&self.inner.events_completed, 1);
    }

    pub(crate) fn record_duplicate(&self) {
        saturating_fetch_add_u64(&self.inner.duplicate_events, 1);
    }

    pub(crate) fn record_event_rejected(&self, adapter: &AdapterId) {
        saturating_fetch_add_u64(&self.inner.rejected_events, 1);
        self.update_adapter_metrics(adapter, |metrics| {
            metrics.events_rejected = metrics.events_rejected.saturating_add(1);
        });
    }

    pub(crate) fn record_unattributed_rejection(&self) {
        saturating_fetch_add_u64(&self.inner.rejected_events, 1);
    }

    pub(crate) fn record_handler(&self, name: &str, duration: Duration, outcome: HandlerOutcome) {
        let mut handlers = self
            .inner
            .handlers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let metrics = handlers.entry(name.to_owned()).or_default();
        metrics.invocations = metrics.invocations.saturating_add(1);
        metrics.duration_micros = metrics
            .duration_micros
            .saturating_add(u64::try_from(duration.as_micros()).unwrap_or(u64::MAX));
        match outcome {
            HandlerOutcome::Success => {
                metrics.successes = metrics.successes.saturating_add(1);
            }
            HandlerOutcome::Failure => {
                metrics.failures = metrics.failures.saturating_add(1);
            }
            HandlerOutcome::Timeout => {
                metrics.timeouts = metrics.timeouts.saturating_add(1);
            }
        }
    }

    fn update_adapter_metrics(
        &self,
        adapter: &AdapterId,
        update: impl FnOnce(&mut AdapterMetrics),
    ) {
        let mut guard = self
            .inner
            .adapters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(metrics) = guard.get_mut(adapter.as_str()) {
            update(metrics);
        } else if guard.len() < MAX_ADAPTER_METRIC_SERIES {
            let mut metrics = AdapterMetrics::default();
            update(&mut metrics);
            guard.insert(adapter.to_string(), metrics);
        }
    }
}

fn saturating_fetch_add_u64(value: &AtomicU64, increment: u64) {
    let _ = value.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(increment))
    });
}

fn saturating_fetch_add_usize(value: &AtomicUsize, increment: usize) {
    let _ = value.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(increment))
    });
}

#[derive(Clone, Copy)]
pub(crate) enum HandlerOutcome {
    Success,
    Failure,
    Timeout,
}

fn prometheus_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::{HealthStatus, RuntimeObserver, RuntimePhase};
    use crate::AdapterId;

    #[test]
    fn observer_exposes_health_and_prometheus_metrics() {
        let observer = RuntimeObserver::new();
        observer.set_phase(RuntimePhase::Running);
        let health = observer.health();
        assert_eq!(health.status, HealthStatus::Degraded);
        assert!(
            observer
                .render_prometheus()
                .contains("bkm_runtime_queue_depth")
        );
    }

    #[test]
    fn configuring_a_new_run_clears_stale_metrics() {
        let observer = RuntimeObserver::new();
        observer.record_event_rejected(&AdapterId::new("old"));
        assert_eq!(observer.snapshot().rejected_events, 1);
        assert!(observer.snapshot().adapters.contains_key("old"));

        observer.configure(8, &[]);
        let snapshot = observer.snapshot();
        assert_eq!(snapshot.rejected_events, 0);
        assert!(snapshot.adapters.is_empty());
        assert_eq!(snapshot.queue_capacity, 8);
    }

    #[test]
    fn adapter_metric_cardinality_is_bounded() {
        let observer = RuntimeObserver::new();
        for index in 0..super::MAX_ADAPTER_METRIC_SERIES + 32 {
            observer.record_event_rejected(&AdapterId::new(format!("adapter-{index}")));
        }
        assert_eq!(
            observer.snapshot().adapters.len(),
            super::MAX_ADAPTER_METRIC_SERIES
        );
    }

    #[test]
    fn observer_rejects_overlapping_runtime_generations() {
        let observer = RuntimeObserver::new();
        assert!(observer.begin_run(8, &[]));
        assert!(!observer.begin_run(16, &[]));
        observer.end_run();
        assert_eq!(observer.phase(), RuntimePhase::Stopped);
        assert!(observer.begin_run(16, &[]));
        observer.end_run();
    }
}
