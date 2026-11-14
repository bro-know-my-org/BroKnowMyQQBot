import { afterEach, expect, it, vi } from 'vitest';
import { readonly, ref, shallowRef } from 'vue';
import { createFaultNotices } from '../src/faultNoticeEngine';
import { createNotifications } from '../src/notifications';
import type { ManagementSnapshot, StatusState } from '../src/status';
import { runtimeFixture } from './runtimeFixture';
const BOOT = '11111111-1111-4111-8111-111111111111', NEXT = '22222222-2222-4222-8222-222222222222';
const data = (revision: number, online: boolean, boot = BOOT): ManagementSnapshot => {
  const runtime = runtimeFixture(); runtime.snapshot.adapters['qq-fixture']!.online = online;
  return { boot_id: boot, revision: String(revision), runtime, plugins: [], plugins_truncated: false, settings_restart_required: null };
};
const active = ref(true), boot = ref(BOOT), snapshot = shallowRef<ManagementSnapshot | null>(null), state = ref<StatusState>('open');
let center = createNotifications(), engine: ReturnType<typeof createFaultNotices>;
function setup() { center.dispose(); center = createNotifications(); active.value = true; boot.value = BOOT; snapshot.value = null; state.value = 'open'; engine = createFaultNotices({ snapshot: readonly(snapshot), state: readonly(state) }, center, () => active.value, () => boot.value); }
afterEach(() => { engine.dispose(); center.dispose(); vi.restoreAllMocks(); sessionStorage.clear(); });
it('one engine consumes readonly singleton data with zero network/subscription operations', () => {
  const fetch = vi.spyOn(globalThis, 'fetch'); setup(); snapshot.value = data(1, true); snapshot.value = data(2, false);
  expect(center.state.history).toHaveLength(1); expect(center.state.active[0]?.message).toContain('连接已离线');
  snapshot.value = data(2, false); state.value = 'stale'; state.value = 'open'; expect(center.state.history).toHaveLength(1); expect(fetch).not.toHaveBeenCalled();
  snapshot.value = data(3, true); expect(center.state.history).toHaveLength(1); expect(center.state.history[0]?.message).toContain('恢复');
});
it('bounded flap history, boot reset and expected process control never report graceful stop as failure', () => {
  setup(); snapshot.value = data(1, true); snapshot.value = data(2, false);
  for (let i = 3; i < 200; i++) snapshot.value = data(i, i % 2 === 1);
  expect(center.state.history).toHaveLength(1); active.value = false; snapshot.value = data(200, false); expect(center.state.active).toHaveLength(0);
  boot.value = NEXT; snapshot.value = data(1, true, NEXT); active.value = true; expect(center.state.active).toHaveLength(0);
  snapshot.value = data(2, false, NEXT); expect(center.state.active).toHaveLength(1);
});
it('confirmed ordinary saves persist boot-scoped hints across route/refresh and reject stale or unknown boots', () => {
  setup(); center.authenticate('a'.repeat(64), false);
  engine.settingsSaved('', 'global'); engine.settingsSaved(NEXT, 'connections'); expect(center.state.history).toHaveLength(0);
  engine.settingsSaved(BOOT, 'connections'); engine.settingsSaved(BOOT, 'connections'); expect(center.state.history).toHaveLength(1);
  expect(center.state.history[0]?.message).toContain('环境变量覆盖仍优先'); expect(center.state.history[0]?.message).toContain('不判断当前实际生效');
  expect(center.state.history[0]?.action).toEqual({ page: 'adapters' });
  center.suspendSession(); center.authenticate('a'.repeat(64), true); expect(center.state.history).toHaveLength(1); expect(center.state.active).toHaveLength(0);
  boot.value = NEXT; snapshot.value = data(1, true, NEXT); engine.settingsSaved(BOOT, 'connections'); expect(center.state.active).toHaveLength(0);
  active.value = false; engine.settingsSaved(NEXT, 'global'); expect(center.state.history).toHaveLength(1);
  center.reset(); expect(center.state.history).toHaveLength(0);
});
it('new external boot dismisses old pending/fault toasts without claiming settings applied', () => {
  setup(); engine.settingsSaved(BOOT, 'global'); snapshot.value = data(1, false); expect(center.state.active).toHaveLength(1);
  boot.value = NEXT; snapshot.value = data(1, true, NEXT); expect(center.state.active).toHaveLength(0);
  expect(center.state.history.some(n => n.message.includes('已生效'))).toBe(false);
});
it('reload deduplicates existing boot baselines while restoring no toast', () => {
  setup(); center.authenticate('a'.repeat(64), false); snapshot.value = data(1, false); expect(center.state.history).toHaveLength(1);
  engine.dispose(); center.suspendSession(); center.authenticate('a'.repeat(64), true); snapshot.value = null;
  engine = createFaultNotices({ snapshot: readonly(snapshot), state: readonly(state) }, center, () => true, () => boot.value);
  snapshot.value = data(2, false); expect(center.state.history).toHaveLength(1); expect(center.state.active).toHaveLength(0);
  engine.dispose(); snapshot.value = data(3, true); expect(center.state.history[0]?.message).not.toContain('恢复');
});

it('a reload records a currently faulted baseline after a persisted recovery instead of hiding the new incident', () => {
  setup(); center.authenticate('a'.repeat(64), false); snapshot.value = data(1, false); snapshot.value = data(2, true);
  expect(center.state.history[0]?.tone).toBe('success'); engine.dispose(); center.suspendSession(); center.authenticate('a'.repeat(64), true); snapshot.value = null;
  engine = createFaultNotices({ snapshot: readonly(snapshot), state: readonly(state) }, center, () => true, () => boot.value);
  snapshot.value = data(3, false); expect(center.state.history).toHaveLength(2); expect(center.state.history[0]?.tone).toBe('warning'); expect(center.state.active).toHaveLength(0);
});
