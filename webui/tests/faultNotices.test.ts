import { expect, it } from 'vitest';
import { createFaultDiff } from '../src/faultNotices';
import type { ManagementSnapshot, PluginStatus } from '../src/status';
import { runtimeFixture } from './runtimeFixture';
export const BOOT = '11111111-1111-4111-8111-111111111111';
export const plugin = (overrides: Partial<PluginStatus> = {}): PluginStatus => ({ instance_id: 'instance/one', plugin_id: 'dev.bkm.one', name: '问候', version: '1', loaded_version: '1', enabled: true, runtime_state: 'ready', pending: false, restart_required: false, updated_at_ms: 1, ...overrides });
export function snapshot(revision: number | string = 1, plugins: PluginStatus[] = []): ManagementSnapshot {
  const runtime = runtimeFixture(); runtime.snapshot.adapters['qq-fixture'] = { online: true, failures: '0', events_received: '0', events_rejected: '0' };
  return { boot_id: BOOT, revision: String(revision), runtime, plugins, plugins_truncated: false, settings_restart_required: null };
}
const offline = (s: ManagementSnapshot, failures = '1') => {
  s.runtime.snapshot.adapters['qq-fixture'] = { online: false, failures, events_received: '0', events_rejected: '0', last_failure: 'transport' }; return s;
};
it('first baseline silently records existing actual failures once, without an alert storm', () => {
  const model = createFaultDiff(), s = offline(snapshot(1, [plugin({ runtime_state: 'disabled' })]));
  const first = model.consume(s, 0); expect(first).toHaveLength(2); expect(first.every(n => n.silent)).toBe(true);
  expect(first[0]?.message).toContain('qq-fixture 当前连接失败（传输连接失败）');
  expect(model.consume(s, 1)).toEqual([]); s.revision = '2'; expect(model.consume(s, 2)).toEqual([]);
});
it('waiting, historical action failures, startup, stopped and expected drain never claim disconnected incidents', () => {
  for (const kind of [undefined, 'action', 'action_unknown'] as const) {
    const s = snapshot(); s.runtime.snapshot.adapters['qq-fixture'] = { online: false, failures: '18446744073709551615', events_received: '0', events_rejected: '0', last_failure: kind };
    expect(createFaultDiff().consume(s, 0)).toEqual([]);
  }
  for (const phase of ['starting', 'shutting_down', 'stopped', 'failed'] as const) {
    const model = createFaultDiff(); model.consume(snapshot(), 0); const s = offline(snapshot(2, [plugin({ runtime_state: 'disabled' })]));
    s.runtime.health.phase = phase; expect(model.consume(s, 1)).toEqual([]);
    s.revision = '3'; s.runtime.health.phase = 'running'; expect(model.consume(s, 2).every(n => n.silent)).toBe(true);
  }
});
it('real drops and recoveries correlate stable adapter IDs and handle large counters exactly', () => {
  const model = createFaultDiff(); model.consume(snapshot('18446744073709551610'), 0);
  let n = model.consume(offline(snapshot('18446744073709551611'), '18446744073709551615'), 1);
  expect(n[0]).toMatchObject({ silent: false, tone: 'warning', destination: { page: 'adapters' } });
  n = model.consume(snapshot('18446744073709551612'), 2); expect(n[0]?.message).toContain('连接已恢复'); expect(n[0]?.coalesce).toBe(true);
  expect(model.consume(offline(snapshot('18446744073709551609')), 3)).toEqual([]);
});
it('an offline adapter first seen without failure does not generate a recovery', () => {
  const model = createFaultDiff(); const s = offline(snapshot(), '0'); model.consume(s, 0);
  expect(model.consume(snapshot(2), 1)).toEqual([]);
});
it('plugin faults distinguish explicit disable, pending apply, not loaded, and rename', () => {
  const model = createFaultDiff(); model.consume(snapshot(1, [plugin()]), 0);
  expect(model.consume(snapshot(2, [plugin({ enabled: false, runtime_state: 'disabled' })]), 1)).toEqual([]);
  expect(model.consume(snapshot(3, [plugin({ pending: true })]), 2)).toEqual([]);
  expect(model.consume(snapshot(4, [plugin({ runtime_state: 'not_loaded' })]), 3)).toEqual([]);
  const fault = model.consume(snapshot(5, [plugin({ name: '改名', runtime_state: 'disabled' })]), 4);
  expect(fault[0]?.message).toContain('改名（instance/one）'); expect(fault[0]?.silent).toBe(false);
  expect(model.consume(snapshot(6, [plugin({ runtime_state: 'disabled', name: '又改名' })]), 5)).toEqual([]);
  expect(model.consume(snapshot(7, [plugin({ runtime_state: 'draining' })]), 6)).toEqual([]);
  expect(model.consume(snapshot(8, [plugin()]), 7)[0]?.message).toContain('已恢复运行');
});
it('explicitly disabling a fault does not announce recovery', () => {
  const model = createFaultDiff(); model.consume(snapshot(1, [plugin({ runtime_state: 'disabled' })]), 0);
  expect(model.consume(snapshot(2, [plugin({ enabled: false, runtime_state: 'disabled' })]), 1)).toEqual([]);
  expect(model.consume(snapshot(3, [plugin()]), 2)).toEqual([]);
});
it('restart false to true differs from pending apply and unknown global settings', () => {
  const model = createFaultDiff(); model.consume(snapshot(1, [plugin()]), 0);
  expect(model.consume(snapshot(2, [plugin({ pending: true })]), 1)).toEqual([]);
  expect(model.consume(snapshot(3, [plugin({ restart_required: true, runtime_state: 'not_loaded' })]), 2)[0]).toMatchObject({ source: '状态 · 插件重启', silent: false });
  expect(model.consume(snapshot(4, [plugin({ restart_required: true })]), 3)).toEqual([]);
});
it.each([false, true])('deleted or truncated entities never imply recovery (truncated %s)', truncated => {
  const model = createFaultDiff(); model.consume(snapshot(1, [plugin({ runtime_state: 'disabled' })]), 0);
  const absent = snapshot(2); absent.plugins_truncated = truncated; expect(model.consume(absent, 1)).toEqual([]);
  expect(model.consume(snapshot(3, [plugin()]), 2)).toEqual([]);
});
it('new boot and session reset baseline all correlations and cooldowns', () => {
  const model = createFaultDiff(); model.consume(offline(snapshot(9, [plugin({ runtime_state: 'disabled' })])), 0);
  const s = snapshot(1, [plugin()]); s.boot_id = '22222222-2222-4222-8222-222222222222'; expect(model.consume(s, 1)).toEqual([]);
  model.reset(); expect(model.consume(offline(snapshot(1)), 2)[0]?.silent).toBe(true);
});
it('thirty second per entity cooldown coalesces flapping without hiding its first failure', () => {
  const model = createFaultDiff(); model.consume(snapshot(), 0);
  expect(model.consume(offline(snapshot(2)), 1)[0]?.silent).toBe(false);
  for (let i = 3; i < 100; i++) {
    const n = model.consume(i % 2 ? snapshot(i) : offline(snapshot(i)), i * 10);
    expect(n).toHaveLength(1); expect(n[0]).toMatchObject({ silent: true, coalesce: true });
  }
  expect(model.consume(offline(snapshot(100)), 30002)[0]).toMatchObject({ silent: false, coalesce: false });
});
it('caps burst delivery and bounds entity history across repeated replacement snapshots', () => {
  const model = createFaultDiff();
  for (let i = 0; i < 20; i++) {
    const many = Array.from({ length: 256 }, (_, j) => plugin({ instance_id: `${i}/${j}`, runtime_state: 'disabled' }));
    const n = model.consume(snapshot(i + 1, many), i); expect(n).toHaveLength(5); expect(n.every(n => n.silent)).toBe(true);
    expect(n[4]?.message).toContain('252 项插件状态变化');
  }
});

it('known global restart false to true notifies while unknown stays unknown', () => {
  const model = createFaultDiff(); const s = snapshot(); model.consume(s, 0);
  s.revision = '2'; s.settings_restart_required = false; expect(model.consume(s, 1)).toEqual([]);
  s.revision = '3'; s.settings_restart_required = true; expect(model.consume(s, 2)[0]).toMatchObject({ silent: false, destination: { page: 'settings' } });
  s.revision = '4'; expect(model.consume(s, 3)).toEqual([]);
});
it('adapter-only burst summaries link to adapters', () => {
  const model = createFaultDiff(), s = snapshot();
  s.runtime.snapshot.adapters = Object.fromEntries(Array.from({ length: 10 }, (_, i) => [String(i), { online: true, failures: '0', events_received: '0', events_rejected: '0' }]));
  model.consume(s, 0); s.revision = '2';
  for (const a of Object.values(s.runtime.snapshot.adapters)) a.online = false;
  expect(model.consume(s, 1).at(-1)).toMatchObject({ destination: { page: 'adapters' } });
});
it('truncated reappearance of a persistent fault does not duplicate its baseline', () => {
  const model = createFaultDiff(); model.consume(snapshot(1, [plugin({ runtime_state: 'disabled' })]), 0);
  const s = snapshot(2); s.plugins_truncated = true; model.consume(s, 1);
  expect(model.consume(snapshot(3, [plugin({ runtime_state: 'disabled' })]), 2)).toEqual([]);
});

it.each([false, true])('burst summaries preserve recovery-only and mixed tones (%s)', mixed => {
  const model = createFaultDiff();
  const many = Array.from({ length: 8 }, (_, i) => plugin({ instance_id: String(i), runtime_state: 'disabled' }));
  if (mixed) many[7]!.runtime_state = 'ready';
  model.consume(snapshot(1, many), 0);
  const changed = many.map(p => ({ ...p, runtime_state: p.runtime_state === 'ready' ? 'disabled' as const : 'ready' as const }));
  expect(model.consume(snapshot(2, changed), 1).at(-1)?.tone).toBe(mixed ? 'warning' : 'success');
});

it('healthy instance intentionally retired by recover/permissions only announces its restart boundary', () => {
  const model = createFaultDiff(); model.consume(snapshot(1, [plugin()]), 0);
  const notices = model.consume(snapshot(2, [plugin({ runtime_state: 'disabled', enabled: true, restart_required: true })]), 1);
  expect(notices).toHaveLength(1); expect(notices[0]?.source).toBe('状态 · 插件重启'); expect(notices[0]?.message).not.toContain('故障');
});

it('a resolved incident can record a new fault after omission instead of swallowing it', () => {
  const model = createFaultDiff(); model.consume(snapshot(1, [plugin({ runtime_state: 'disabled' })]), 0);
  model.consume(snapshot(2, [plugin()]), 1); const absent = snapshot(3); absent.plugins_truncated = true; model.consume(absent, 2);
  const next = model.consume(snapshot(4, [plugin({ runtime_state: 'disabled' })]), 40000);
  expect(next).toHaveLength(1); expect(next[0]).toMatchObject({ tone: 'warning', silent: true });
  expect(model.consume(snapshot(5, [plugin({ runtime_state: 'disabled' })]), 50000)).toEqual([]);
});

it('cleared restart then omitted then pending again records a fresh bounded hint', () => {
  const model = createFaultDiff(); model.consume(snapshot(1, [plugin({ restart_required: true })]), 0);
  expect(model.consume(snapshot(2, [plugin()]), 1)).toEqual([]);
  const absent = snapshot(3); absent.plugins_truncated = true; model.consume(absent, 2);
  expect(model.consume(snapshot(4, [plugin({ restart_required: true })]), 40000)[0]).toMatchObject({ source: '状态 · 插件重启', silent: true });
});
