import { readonly, ref, shallowRef } from 'vue';
import type { InjectionKey } from 'vue';
import { ApiError } from './api';
import type { Request } from './api';
import { parseSafeJson } from './json';
import { readRuntime } from './runtime';
import type { RuntimeStatus } from './runtime';
import { isObject } from './types';
import { dataRevision } from './pluginManagement';
import type { PluginView, Session } from './types';

export type StatusState = 'connecting' | 'open' | 'reconnecting' | 'closed' | 'stale';
export type PluginStatus = Pick<PluginView, 'instance_id' | 'plugin_id' | 'name' | 'version' | 'loaded_version' | 'enabled' | 'runtime_state' | 'pending' | 'restart_required' | 'updated_at_ms'>;
export interface ManagementSnapshot {
  boot_id: string; revision: string; runtime: RuntimeStatus; plugins: PluginStatus[];
  plugins_truncated: boolean; settings_restart_required: boolean | null; bundled_revision: string | null; plugin_data_revisions?: Record<string, string>;
}
export const managementStatusKey: InjectionKey<ManagementStatus> = Symbol('management-status');
const text = (v: unknown, max: number): v is string => typeof v === 'string' && v.length > 0 && v.length <= max;
const boot = (v: unknown): v is string => typeof v === 'string' && /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(v);
const counter = dataRevision;
export function readManagementSnapshot(raw: unknown): ManagementSnapshot {
  const invalid = () => new ApiError('状态响应格式异常，保留上次快照，请重试。');
  if (!isObject(raw) || !boot(raw.boot_id) || !counter(raw.revision) || !Array.isArray(raw.plugins) || raw.plugins.length > 256
    || typeof raw.plugins_truncated !== 'boolean' || !(raw.settings_restart_required === null || typeof raw.settings_restart_required === 'boolean')
    || !(!Object.hasOwn(raw, 'bundled_revision') || raw.bundled_revision === null || (typeof raw.bundled_revision === 'string' && raw.bundled_revision.length === 64 && /^[0-9a-f]{64}$/.test(raw.bundled_revision)))
    || !isObject(raw.runtime) || raw.runtime.boot_id !== raw.boot_id) throw invalid();
  if (raw.plugin_data_revisions !== undefined && (!isObject(raw.plugin_data_revisions) || Object.keys(raw.plugin_data_revisions).length > 256
    || Object.entries(raw.plugin_data_revisions).some(([id, revision]) => !text(id, 512) || !dataRevision(revision)))) throw invalid();
  const ids = new Set<string>();
  const plugins = raw.plugins.map((p: unknown): PluginStatus => {
    if (!isObject(p) || !text(p.instance_id, 512) || ids.has(p.instance_id) || !text(p.plugin_id, 128) || !text(p.name, 512)
      || !text(p.version, 128) || !(p.loaded_version === null || text(p.loaded_version, 128))
      || typeof p.enabled !== 'boolean' || typeof p.pending !== 'boolean' || typeof p.restart_required !== 'boolean'
      || typeof p.runtime_state !== 'string' || !['ready', 'stopped', 'disabled', 'not_loaded', 'draining'].includes(p.runtime_state)
      || !Number.isSafeInteger(p.updated_at_ms) || Number(p.updated_at_ms) < 0) throw invalid();
    ids.add(p.instance_id);
    return { instance_id: p.instance_id, plugin_id: p.plugin_id, name: p.name, version: p.version, loaded_version: p.loaded_version,
      enabled: p.enabled, runtime_state: p.runtime_state as PluginView['runtime_state'], pending: p.pending,
      restart_required: p.restart_required, updated_at_ms: Number(p.updated_at_ms) };
  });
  return { boot_id: raw.boot_id, revision: raw.revision, runtime: readRuntime(raw.runtime), plugins,
    ...(raw.plugin_data_revisions === undefined ? {} : { plugin_data_revisions: raw.plugin_data_revisions as Record<string, string> }),
    plugins_truncated: raw.plugins_truncated, settings_restart_required: raw.settings_restart_required, bundled_revision: typeof raw.bundled_revision === 'string' ? raw.bundled_revision : null };
}

export function createManagementStatus(request: Request, onUnauthorized: () => void) {
  const snapshot = shallowRef<ManagementSnapshot | null>(null), state = ref<StatusState>('closed'), error = ref(''), updatedAt = ref('');
  let source: EventSource | undefined, generation = 0, enabled = false, disposed = false, expectedBoot = '', verifyingBoot = false;
  let probe: AbortController | undefined, retry: ReturnType<typeof setTimeout> | undefined;
  let retryDelay = 1000, lastProbe = -Infinity, received = 0;
  let probeTimer: ReturnType<typeof setTimeout> | undefined;
  const live = (current: number) => enabled && !disposed && current === generation && !document.hidden;
  function disconnect() { generation++; source?.close(); source = undefined; probe?.abort(); probe = undefined; clearTimeout(probeTimer); probeTimer = undefined; clearTimeout(retry); retry = undefined; }
  function unavailable(message: string) { state.value = 'stale'; error.value = message; }
  function reconnectLater() {
    if (retry || !enabled || document.hidden || disposed) return;
    retry = setTimeout(() => { retry = undefined; if (!probe) connect(); }, retryDelay);
    retryDelay = Math.min(retryDelay * 2, 30000);
  }
  async function checkSession(current: number, force = false) {
    if (probe || (!force && Date.now() - lastProbe < 5000)) return;
    const observed = received, controller = new AbortController(); probe = controller; lastProbe = Date.now();
    probeTimer = setTimeout(() => { if (probe === controller && live(current)) { controller.abort(); probe = undefined; if (received === observed) unavailable('会话核对超时，请重试。'); if (verifyingBoot || source?.readyState === EventSource.CLOSED) reconnectLater(); } }, 5000);
    try {
      const session = await request<Session>('/api/session', undefined, controller.signal);
      if (!live(current) || controller.signal.aborted) return;
      if (!isObject(session) || typeof session.authenticated !== 'boolean' || !['token', 'none'].includes(session.mode) || !boot(session.boot_id)) throw new ApiError('会话响应格式异常，请重试。');
      if (session.authenticated === false) { stop(); onUnauthorized(); return; }
      if (session.boot_id !== expectedBoot) { expectedBoot = session.boot_id; connect(); }
    } catch (e) { if (live(current) && !controller.signal.aborted) { if (received === observed) unavailable(e instanceof Error ? e.message : '无法核对会话，请重试。'); if (verifyingBoot || source?.readyState === EventSource.CLOSED) reconnectLater(); } }
    finally { if (probe === controller) { probe = undefined; clearTimeout(probeTimer); probeTimer = undefined; if (live(current) && (verifyingBoot || source?.readyState === EventSource.CLOSED)) reconnectLater(); } }
  }
  function connect() {
    disconnect();
    if (!enabled || disposed || document.hidden) return;
    const current = generation;
    state.value = snapshot.value ? 'reconnecting' : 'connecting';
    try { source = new EventSource('/api/status/stream'); }
    catch { unavailable('无法订阅运行状态，请重试。'); reconnectLater(); return; }
    const stream = source; let verificationAttempted = false;
    stream.onopen = () => { if (live(current)) state.value = snapshot.value ? 'reconnecting' : 'connecting'; };
    stream.addEventListener('status', event => {
      if (!live(current)) return;
      try {
        const data = (event as MessageEvent).data;
        if (typeof data !== 'string' || data.length > 512 * 1024) throw new ApiError('状态响应过大，请重试。');
        const next = readManagementSnapshot(parseSafeJson(data));
        if (expectedBoot && next.boot_id !== expectedBoot) { verifyingBoot = true; unavailable('管理进程已变化，正在核对会话。'); if (!verificationAttempted) { verificationAttempted = true; void checkSession(current, true); } else if (!probe) reconnectLater(); return; }
        expectedBoot = next.boot_id; verifyingBoot = false;
        const previous = snapshot.value;
        if (previous?.boot_id === next.boot_id && BigInt(next.revision) < BigInt(previous.revision)) return;
        if (previous?.boot_id !== next.boot_id || previous.revision !== next.revision) snapshot.value = next;
        clearTimeout(retry); retry = undefined; received++; state.value = 'open'; error.value = ''; retryDelay = 1000;
        updatedAt.value = new Date().toLocaleTimeString('zh-CN', { hour12: false });
      } catch (e) { if (live(current)) { disconnect(); unavailable(e instanceof Error ? e.message : '状态响应格式异常，请重试。'); } }
    });
    stream.addEventListener('stale', () => { if (live(current)) unavailable('状态暂时无法读取，保留上次快照。'); });
    stream.addEventListener('auth-required', () => { if (live(current)) { stop(); onUnauthorized(); } });
    stream.onerror = () => {
      if (!live(current)) return;
      state.value = 'reconnecting'; error.value = '状态连接中断，保留上次快照。';
      void checkSession(current);
      if (!probe && stream.readyState === EventSource.CLOSED) reconnectLater();
    };
  }
  function start(bootId = '') { if (disposed) return; const changed = bootId && expectedBoot !== bootId; expectedBoot = bootId || expectedBoot; enabled = true; if (!source || changed) connect(); }
  function stop(clear = true) { enabled = false; disconnect(); state.value = 'closed'; error.value = ''; if (clear) { snapshot.value = null; updatedAt.value = ''; expectedBoot = ''; verifyingBoot = false; } }
  function visibility() { if (document.hidden) { disconnect(); if (enabled) state.value = 'stale'; } else if (enabled) connect(); }
  function refresh() { if (enabled) connect(); }
  function pagehide() { disconnect(); if (enabled) state.value = 'stale'; }
  function pageshow() { if (enabled && !document.hidden && !source) connect(); }
  function dispose() { stop(); disposed = true; document.removeEventListener('visibilitychange', visibility); window.removeEventListener('pagehide', pagehide); window.removeEventListener('pageshow', pageshow); }
  document.addEventListener('visibilitychange', visibility);
  window.addEventListener('pagehide', pagehide); window.addEventListener('pageshow', pageshow);
  return { snapshot: readonly(snapshot), state: readonly(state), error: readonly(error), updatedAt: readonly(updatedAt), start, stop, refresh, dispose };
}
export type ManagementStatus = ReturnType<typeof createManagementStatus>;
