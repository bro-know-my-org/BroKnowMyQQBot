import type { RuntimeStatus } from '../src/runtime';

export const runtimeFixture = (): RuntimeStatus => ({
  health: { status: 'degraded', phase: 'running', configured_adapters: 1, online_adapters: 0 },
  snapshot: {
    phase: 'running', queue_capacity: 32, queue_depth: 2, queue_depth_observed_high_watermark: 4, active_events: 1,
    events_received: '18446744073709551615', events_completed: '4', duplicate_events: '1', rejected_events: '2',
    adapters: { 'qq-fixture': { online: false, events_received: '7', events_rejected: '2', failures: '3', last_failure: 'transport' } },
  },
});
