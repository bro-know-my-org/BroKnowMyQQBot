import type { DeepReadonly } from 'vue';
import type { ManagementSnapshot, PluginStatus } from './status';
import { failureNames } from './runtime';
import type { AdapterStatus } from './runtime';

export type FaultDestination = { page: 'adapters' | 'plugins' | 'settings' } | { page: 'plugin-overview'; instanceId: string };
export interface FaultNotice {
  key: string; source: string; message: string; tone: 'warning' | 'success';
  destination: FaultDestination; silent: boolean; coalesce: boolean;
}
const COOLDOWN_MS = 30000, MAX_DELIVERIES = 768, INDIVIDUAL_BURST_BUDGET = 4;
interface Observed { fault: boolean; restart: boolean; online: boolean }
const pluginFault = (p: PluginStatus) => p.enabled && p.runtime_state === 'disabled' && !p.restart_required;
const adapterFault = (a: AdapterStatus) => !a.online && BigInt(a.failures) > 0n && (a.last_failure === 'configuration' || a.last_failure === 'transport');
const label = (value: string) => value.replace(/[\u0000-\u001f\u007f]/g, '').slice(0, 160);

/** Consumes validated full snapshots only. No IO, subscriptions or mutation of shared state. */
export function createFaultDiff() {
  let boot = '', revision = -1n, ready = false, settings: boolean | null = null;
  let entities = new Map<string, Observed>();
  const delivery = new Map<string, { at: number; tone: FaultNotice['tone'] }>();
  function reset() { boot = ''; revision = -1n; ready = false; settings = null; entities.clear(); delivery.clear(); }
  function consume(next: DeepReadonly<ManagementSnapshot>, now: number): FaultNotice[] {
    if (boot !== next.boot_id) { reset(); boot = next.boot_id; }
    if (BigInt(next.revision) <= revision) return [];
    revision = BigInt(next.revision);
    // Graceful drain/stopped/startup is a new baseline, never a disconnection.
    if (next.runtime.health.phase !== 'running' || next.runtime.snapshot.phase !== 'running') { ready = false; settings = null; entities.clear(); delivery.clear(); return []; }
    const notices: FaultNotice[] = [], current = new Map<string, Observed>();
    function emit(key: string, source: string, message: string, tone: FaultNotice['tone'], destination: FaultDestination, initial: boolean) {
      const last = delivery.get(key);
      if (initial && last?.tone === 'warning') return;
      const coalesce = last !== undefined && now - last.at < COOLDOWN_MS;
      delivery.set(key, { at: coalesce ? last!.at : now, tone });
      notices.push({ key, source, message, tone, destination, silent: initial || coalesce, coalesce });
    }
    function resolve(key: string) { const entry = delivery.get(key); if (entry) entry.tone = 'success'; }
    for (const [id, a] of Object.entries(next.runtime.snapshot.adapters)) {
      const key = `adapter:${id}`, old = ready ? entities.get(key) : undefined;
      const fault = adapterFault(a);
      // Online -> offline is real even when no specific failure class was published.
      const incident = !a.online && (old?.online === true || old?.fault === true || fault);
      current.set(key, { fault: incident, restart: false, online: a.online });
      if (incident && !old?.fault) {
        const reason = a.last_failure ? `（${failureNames[a.last_failure]}）` : '';
        emit(key, '状态 · 连接', `${label(id)} ${old?.online ? '连接已离线' : '当前连接失败'}${reason}。`, 'warning', { page: 'adapters' }, !old);
      } else if (a.online && old?.fault) emit(key, '状态 · 连接', `${label(id)} 连接已恢复。`, 'success', { page: 'adapters' }, false);
      if (a.online) resolve(key);
    }
    for (const p of next.plugins) {
      const key = `plugin:${p.instance_id}`, old = ready ? entities.get(key) : undefined;
      const fault = pluginFault(p), destination: FaultDestination = { page: 'plugin-overview', instanceId: p.instance_id };
      const name = `${label(p.name)}（${label(p.instance_id)}）`;
      current.set(key, { fault: fault || !!(old?.fault && p.enabled && p.runtime_state !== 'ready'), restart: p.restart_required, online: false });
      if (fault && !old?.fault) emit(`${key}:fault`, '状态 · 插件运行', `${name} 已启用，但运行实例已禁用，请检查插件状态。`, 'warning', destination, !old);
      else if (old?.fault && p.enabled && p.runtime_state === 'ready') emit(`${key}:fault`, '状态 · 插件运行', `${name} 已恢复运行。`, 'success', destination, false);
      if (!p.enabled || p.runtime_state === 'ready') resolve(`${key}:fault`);
      if (!p.restart_required) resolve(`${key}:restart`);
      if (p.restart_required && !old?.restart) emit(`${key}:restart`, '状态 · 插件重启', `${name} 需要重启 Bot 后加载已保存版本或恢复状态。`, 'warning', destination, !old);
    }
    if (next.settings_restart_required === true && settings !== true) emit('settings:restart', '状态 · 设置重启', '已保存的普通设置需要重启 Bot 后读取，环境变量覆盖仍优先。', 'warning', { page: 'settings' }, settings === null);
    if (next.settings_restart_required === false) resolve('settings:restart');
    settings = next.settings_restart_required;
    // Omission breaks recovery correlation; bounded delivery memory prevents repeated silent baselines.
    entities = current; ready = true;
    while (delivery.size > MAX_DELIVERIES) delivery.delete(delivery.keys().next().value!);
    if (notices.length <= INDIVIDUAL_BURST_BUDGET + 1) return notices;
    const shown = notices.slice(0, INDIVIDUAL_BURST_BUDGET), omitted = notices.slice(INDIVIDUAL_BURST_BUDGET);
    for (const page of ['adapters', 'plugins', 'settings'] as const) {
      const group = omitted.filter(n => page === 'plugins' ? n.destination.page.startsWith('plugin') : n.destination.page === page);
      const names = { adapters: '连接', plugins: '插件', settings: '设置' };
      const tone = group.every(n => n.tone === 'success') ? 'success' : 'warning';
      if (group.length) shown.push({ key: `status:burst:${page}`, source: '状态 · 汇总', message: `另有 ${group.length} 项${names[page]}状态变化，请查看对应页面。`, tone, destination: { page }, silent: true, coalesce: true });
    }
    return shown;
  }
  return { consume, reset };
}
