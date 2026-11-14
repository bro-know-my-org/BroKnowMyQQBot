import { onBeforeUnmount, onMounted, ref, shallowRef } from 'vue';
import { ApiError } from './api';
import type { Request } from './api';
import { isObject } from './types';

export type RuntimePhase = 'starting' | 'running' | 'shutting_down' | 'stopped' | 'failed';
export type FailureKind = 'configuration' | 'transport' | 'action' | 'action_unknown' | 'event_queue_closed' | 'event_adapter_mismatch';
export interface AdapterStatus { online: boolean; events_received: string; events_rejected: string; failures: string; last_failure?: FailureKind }
export interface RuntimeStatus {
  health: { status: 'healthy' | 'degraded' | 'unhealthy'; phase: RuntimePhase; online_adapters: number; configured_adapters: number };
  snapshot: {
    phase: RuntimePhase; queue_capacity: number; queue_depth: number; queue_depth_observed_high_watermark: number; active_events: number;
    events_received: string; events_completed: string; duplicate_events: string; rejected_events: string;
    adapters: Record<string, AdapterStatus>;
  };
}
export const phaseNames: Record<RuntimePhase, string> = { starting: '启动中', running: '运行中', shutting_down: '正在关闭', stopped: '已停止', failed: '运行失败' };
export const failureNames: Record<FailureKind, string> = {
  configuration: '连接配置异常', transport: '传输连接失败', action: '动作执行失败', action_unknown: '动作结果未知',
  event_queue_closed: '事件队列已关闭', event_adapter_mismatch: '事件来源不匹配',
};
const phases = Object.keys(phaseNames), failures = Object.keys(failureNames);
const count = (value: unknown, max = Number.MAX_SAFE_INTEGER) => Number.isSafeInteger(value) && Number(value) >= 0 && Number(value) <= max;
const counter = (value: unknown) => typeof value === 'string' && /^(0|[1-9]\d{0,19})$/.test(value) && (value.length < 20 || value <= '18446744073709551615');
export function readRuntime(value: unknown): RuntimeStatus {
  const invalid = () => new ApiError('运行状态响应格式异常，请重试。');
  if (!isObject(value) || !isObject(value.health) || !isObject(value.snapshot)) throw invalid();
  const h = value.health, s = value.snapshot;
  if (typeof h.status !== 'string' || !['healthy', 'degraded', 'unhealthy'].includes(h.status)
    || typeof h.phase !== 'string' || !phases.includes(h.phase) || typeof s.phase !== 'string' || !phases.includes(s.phase)
    || !count(h.online_adapters, 256) || !count(h.configured_adapters, 256) || !isObject(s.adapters) || Object.keys(s.adapters).length > 256
    || !['queue_capacity', 'queue_depth', 'queue_depth_observed_high_watermark', 'active_events'].every(key => count(s[key]))
    || !['events_received', 'events_completed', 'duplicate_events', 'rejected_events'].every(key => counter(s[key]))) throw invalid();
  for (const [id, a] of Object.entries(s.adapters)) {
    if (!id || id.length > 128 || /[\u0000-\u001f\u007f]/.test(id) || !isObject(a) || typeof a.online !== 'boolean'
      || !['events_received', 'events_rejected', 'failures'].every(key => counter(a[key]))
      || (a.last_failure !== undefined && (typeof a.last_failure !== 'string' || !failures.includes(a.last_failure)))) throw invalid();
  }
  return value as unknown as RuntimeStatus;
}

export function useRuntime(request: Request) {
  const data = shallowRef<RuntimeStatus | null>(null), loading = ref(false), stale = ref(false), error = ref(''), updatedAt = ref('');
  let alive = false, generation = 0, active: AbortController | undefined, timer: ReturnType<typeof setTimeout> | undefined;
  function cancel() { generation++; clearTimeout(timer); active?.abort(); active = undefined; loading.value = false; }
  async function refresh() {
    if (!alive || document.hidden || active) return;
    clearTimeout(timer);
    const current = generation, controller = new AbortController(); active = controller; loading.value = true;
    const valid = () => alive && current === generation && !controller.signal.aborted;
    try {
      const next = readRuntime(await request<unknown>('/api/runtime', undefined, controller.signal));
      if (!valid()) return;
      data.value = next; stale.value = false; error.value = ''; updatedAt.value = new Date().toLocaleTimeString('zh-CN', { hour12: false });
    } catch (e) {
      if (valid()) { stale.value = true; error.value = e instanceof Error ? e.message : '运行状态读取失败，请重试。'; }
    } finally {
      if (valid()) { active = undefined; loading.value = false; timer = setTimeout(() => { void refresh(); }, 5000); }
    }
  }
  function visibility() {
    if (document.hidden) { cancel(); if (data.value) stale.value = true; }
    else void refresh();
  }
  onMounted(() => { alive = true; document.addEventListener('visibilitychange', visibility); void refresh(); });
  onBeforeUnmount(() => { alive = false; cancel(); document.removeEventListener('visibilitychange', visibility); });
  return { data, loading, stale, error, updatedAt, refresh };
}
