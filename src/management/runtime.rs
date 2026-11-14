//! Safe JSON projection of the process observer, independent of plugin state.

use std::collections::BTreeMap;

use axum::{Json, extract::State};
use bot_core::{AdapterFailureKind, HealthStatus, RuntimeHealth, RuntimeObserver, RuntimePhase};
use serde::Serialize;

use super::plugins::ApiState;

#[derive(Serialize)]
pub(super) struct RuntimeStatus {
    boot_id: &'static str,
    health: RuntimeHealth,
    snapshot: Snapshot,
}

#[derive(Serialize)]
struct Snapshot {
    phase: RuntimePhase,
    queue_capacity: usize,
    queue_depth: usize,
    queue_depth_observed_high_watermark: usize,
    active_events: usize,
    // Decimal strings preserve u64 counters in JavaScript without rounding.
    events_received: String,
    events_completed: String,
    duplicate_events: String,
    rejected_events: String,
    adapters: BTreeMap<String, AdapterStatus>,
}

#[derive(Serialize)]
struct AdapterStatus {
    online: bool,
    events_received: String,
    events_rejected: String,
    failures: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_failure: Option<AdapterFailureKind>,
}

impl RuntimeStatus {
    pub(super) fn read(observer: &RuntimeObserver) -> Self {
        let metrics = observer.snapshot();
        let configured_adapters = metrics.adapters.len();
        let online_adapters = metrics
            .adapters
            .values()
            .filter(|adapter| adapter.online)
            .count();
        // Match RuntimeObserver::health using this same sample, so a readiness
        // transition cannot produce contradictory health and adapter fields.
        let status = match metrics.phase {
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
        Self {
            boot_id: crate::restart::boot_id(),
            health: RuntimeHealth {
                status,
                phase: metrics.phase,
                online_adapters,
                configured_adapters,
            },
            snapshot: Snapshot {
                phase: metrics.phase,
                queue_capacity: metrics.queue_capacity,
                queue_depth: metrics.queue_depth,
                queue_depth_observed_high_watermark: metrics.queue_depth_observed_high_watermark,
                active_events: metrics.active_events,
                events_received: metrics.events_received.to_string(),
                events_completed: metrics.events_completed.to_string(),
                duplicate_events: metrics.duplicate_events.to_string(),
                rejected_events: metrics.rejected_events.to_string(),
                // RuntimeBuilder bounds this map to 256 IDs of at most 128 bytes.
                // Handler names, config, raw errors and transport URLs are omitted.
                adapters: metrics
                    .adapters
                    .into_iter()
                    .map(|(id, adapter)| {
                        (
                            id,
                            AdapterStatus {
                                online: adapter.online,
                                events_received: adapter.events_received.to_string(),
                                events_rejected: adapter.events_rejected.to_string(),
                                failures: adapter.failures.to_string(),
                                last_failure: adapter.last_failure,
                            },
                        )
                    })
                    .collect(),
            },
        }
    }
}

pub(super) async fn get(State(state): State<ApiState>) -> Json<RuntimeStatus> {
    // Degraded/stopped are successful reads; HTTP errors remain API errors.
    Json(RuntimeStatus::read(&state.observer))
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
