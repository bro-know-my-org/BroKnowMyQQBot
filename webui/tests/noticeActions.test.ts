import { afterEach, expect, it, vi } from 'vitest';
import { createApp, nextTick } from 'vue';
import { createNotifications } from '../src/notifications';
import NotificationCenter from '../src/components/NotificationCenter.vue';
import { createManagementRouter } from '../src/navigation';
import { noticeActionRoute, readNoticeAction } from '../src/noticeActions';
import { NOTICE_STORAGE_KEY, noticeStorage } from '../src/noticeStorage';
let app: ReturnType<typeof createApp> | undefined;
let center: ReturnType<typeof createNotifications> | undefined;
const scope = 'a'.repeat(64);
const flush = async () => { await new Promise<void>(resolve => setImmediate(resolve)); await nextTick(); };
afterEach(() => { app?.unmount(); center?.dispose(); app = undefined; center = undefined; document.body.replaceChildren(); sessionStorage.clear(); vi.useRealTimers(); vi.restoreAllMocks(); });
it('only accepts structured destinations and bounded well-formed IDs, with fixed labels', () => {
  for (const invalid of [{ page: 'https://evil.test' }, { page: 'settings', href: 'evil' }, { name: 'settings' }, { page: 'plugin-config', instanceId: '' }, { page: 'plugin-config', instanceId: 'x'.repeat(513) }, { page: 'plugin-overview', instanceId: '\ud800' }, { page: 'logs', label: '<script>' }, { page: 'plugin-config', instanceId: 'a\n' }, []]) {
    expect(readNoticeAction(invalid)).toBeUndefined(); expect(noticeActionRoute(invalid)).toBeUndefined();
  }
  expect(noticeActionRoute({ page: 'plugin-config', instanceId: 'opaque/汉?#%' })).toEqual({ name: 'plugin-config', params: { instanceId: 'opaque/汉?#%' } });
});
it('invalid action metadata is dropped at creation/load without rejecting compatible history', () => {
  center = createNotifications(); center.authenticate(scope, false);
  center.notify('状态', '故障', 'warning', { action: { page: 'adapters' } });
  center.notify('旧操作', '原格式');
  center.notify('无效动作', '安全保留文本', 'warning', { action: { page: 'evil' } as never });
  const payload = JSON.parse(sessionStorage.getItem(NOTICE_STORAGE_KEY)!);
  payload.entries[0].action = { page: 'settings', params: { dirty: false }, label: '伪造' };
  sessionStorage.setItem(NOTICE_STORAGE_KEY, JSON.stringify(payload));
  const loaded = noticeStorage().load(scope);
  expect(loaded).toHaveLength(3); expect(loaded[0]?.action).toBeUndefined(); expect(loaded[1]?.action).toBeUndefined(); expect(loaded[2]?.action).toEqual({ page: 'adapters' });
});
it('coalesces bursts into the same row and refresh restores no toast or duplicate baseline', () => {
  center = createNotifications(); center.authenticate(scope, false);
  const id = center.notify('状态 · 连接', '离线', 'warning', { action: { page: 'adapters' }, key: 'boot:adapter:one' })!;
  for (let i = 0; i < 200; i++) center.notify('状态 · 连接', `变化${i}`, 'success', { silent: true, coalesce: true, key: 'boot:adapter:one', action: { page: 'adapters' } });
  expect(center.state.history).toHaveLength(1); expect(center.state.active).toHaveLength(1); expect(center.state.active[0]?.message).toBe('变化199');
  center.suspendSession(); center.dispose(); center = createNotifications(); center.authenticate(scope, true);
  expect(center.state.history).toHaveLength(1); expect(center.state.active).toEqual([]);
  const restored = center.state.history[0]!.id;
  center.notify('状态 · 连接', '离线', 'warning', { silent: true, baseline: true, key: 'boot:adapter:one' });
  expect(center.state.history[0]?.id).toBe(restored); expect(center.state.history[0]?.message).toBe('变化199'); expect(id).toBeGreaterThan(0);
});
it('coalescing preserves focus and pointer pause when a manual warning becomes an auto-expiring recovery', () => {
  vi.useFakeTimers(); vi.spyOn(document, 'hasFocus').mockReturnValue(true); center = createNotifications();
  const id = center.notify('状态', '离线', 'warning', { key: 'one' })!; center.setPaused(id, 'focus', true); center.setPaused(id, 'pointer', true);
  center.notify('状态', '恢复', 'success', { key: 'one', coalesce: true, silent: true }); vi.advanceTimersByTime(10000); expect(center.state.active).toHaveLength(1);
  center.setPaused(id, 'focus', false); vi.advanceTimersByTime(10000); expect(center.state.active).toHaveLength(1);
  center.setPaused(id, 'pointer', false); vi.advanceTimersByTime(5000); expect(center.state.active).toHaveLength(0);
});
async function mount() {
  vi.spyOn(document, 'hasFocus').mockReturnValue(true);
  const router = createManagementRouter(true); await router.push('/'); await router.isReady();
  center = createNotifications(); const root = document.body.appendChild(document.createElement('div'));
  app = createApp(NotificationCenter, { center }).use(router); app.mount(root); return router;
}
it('history actions use the live guard; cancellation preserves history, unread rows and keyboard focus', async () => {
  const router = await mount(); let allowed = false; const guard = vi.fn(() => allowed); router.beforeEach(guard);
  const other = center!.notify('其他', '未显示条目', 'info', { silent: true })!;
  center!.notify('连接', '连接离线', 'warning', { silent: true, action: { page: 'adapters' } }); center!.setOpen(true); await flush();
  const button = document.querySelector<HTMLButtonElement>('.notification-history .notification-action')!; button.focus(); button.click(); await flush();
  expect(guard).toHaveBeenCalledOnce(); expect(router.currentRoute.value.name).toBe('overview'); expect(center!.state.open).toBe(true); expect(document.activeElement).toBe(button);
  expect(center!.state.history.find(n => n.id === other)?.unread).toBe(true); expect(document.body.textContent).toContain('页面未切换');
  allowed = true; button.click(); await flush(); expect(router.currentRoute.value.name).toBe('adapters'); expect(center!.state.open).toBe(false);
  expect(center!.state.history[0]?.unread).toBe(false); expect(center!.state.history.find(n => n.id === other)?.unread).toBe(true);
});
it('toast actions encode opaque plugin IDs and cannot replay navigation while guards are pending', async () => {
  const router = await mount(); let finish!: (allowed: boolean) => void;
  const guard = vi.fn(() => new Promise<boolean>(resolve => { finish = resolve; })); router.beforeEach(guard);
  center!.notify('插件', '故障', 'warning', { action: { page: 'plugin-overview', instanceId: 'instance/汉?#%' } }); await flush();
  const button = document.querySelector<HTMLButtonElement>('.notification-toast .notification-action')!; button.click(); await flush();
  expect(button.getAttribute('aria-disabled')).toBe('true'); button.click(); await flush(); expect(guard).toHaveBeenCalledOnce();
  finish(true); await flush(); expect(router.currentRoute.value.params.instanceId).toBe('instance/汉?#%'); expect(center!.state.active).toHaveLength(1);
});
it('failed navigation keeps the entry and exposes retry feedback', async () => {
  const router = await mount(); router.onError(() => {}); router.beforeEach(() => { throw new Error('guard'); });
  center!.notify('设置', '待重启', 'warning', { action: { page: 'settings' } }); await flush();
  document.querySelector<HTMLButtonElement>('.notification-action')!.click(); await flush();
  expect(document.body.textContent).toContain('暂时无法打开目标页面'); expect(center!.state.history).toHaveLength(1); expect(router.currentRoute.value.name).toBe('overview');
});

it('ephemeral progress cannot coalesce into retained history', () => {
  center = createNotifications(); center.notify('状态', '故障', 'warning', { key: 'one' });
  center.notify('状态', '进行中', 'progress', { key: 'one', coalesce: true });
  expect(center.state.history[0]?.tone).toBe('warning'); center.clearHistory(); expect(center.state.active).toEqual([]);
});
it('failed action feedback disappears with its originating toast without exceeding the toast cap', async () => {
  const router = await mount(); router.beforeEach(() => false);
  center!.notify('状态', '故障', 'warning', { action: { page: 'adapters' } }); await flush();
  document.querySelector<HTMLButtonElement>('.notification-action')!.click(); await flush();
  expect(document.querySelectorAll('.notification-toast')).toHaveLength(1);
  document.querySelector<HTMLButtonElement>('.notification-close')!.click(); await flush(); expect(document.body.textContent).not.toContain('页面未切换');
});

it('an already reached destination closes history and marks only the selected row read', async () => {
  const router = await mount(); await router.push('/adapters');
  center!.notify('其他', '未展示', 'info', { silent: true }); center!.notify('连接', '故障', 'warning', { silent: true, action: { page: 'adapters' } });
  center!.setOpen(true); await flush(); document.querySelector<HTMLButtonElement>('.notification-history .notification-action')!.click(); await flush();
  expect(center!.state.open).toBe(false); expect(center!.state.history[0]?.unread).toBe(false); expect(center!.state.history[1]?.unread).toBe(true);
  expect(document.body.textContent).not.toContain('页面未切换');
});
