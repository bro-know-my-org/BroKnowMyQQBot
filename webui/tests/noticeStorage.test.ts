import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { createNotifications, type NotificationCenter } from '../src/notifications';
import { NOTICE_STORAGE_KEY, noticeStorage } from '../src/noticeStorage';

const scope = 'a'.repeat(64);
let center: NotificationCenter;
beforeEach(() => { sessionStorage.clear(); vi.spyOn(document, 'hasFocus').mockReturnValue(true); center = createNotifications(); });
afterEach(() => { center.dispose(); sessionStorage.clear(); vi.restoreAllMocks(); });
it('restores history only after authentication, with read/silent state and no clocks or toasts', () => {
  center.authenticate(scope, false);
  const id = center.notify('控制台', '完成')!; center.setVisible(id, 'toast', true);
  center.notify('后台', '待查看', 'warning', { silent: true }); center.notify('操作', '进行中', 'progress');
  center.suspendSession(); expect(center.state.history).toEqual([]);
  center.dispose(); center = createNotifications(); expect(center.state.history).toEqual([]);
  center.authenticate(scope, true);
  expect(center.state.history.map(e => [e.message, e.unread])).toEqual([['待查看', true], ['完成', false]]);
  expect(center.state.active).toEqual([]);
  center.markAllRead(); center.dispose(); center = createNotifications(); center.authenticate(scope, true);
  expect(center.unread.value).toBe(0); center.clearHistory(); expect(noticeStorage().load(scope)).toEqual([]);
});
it('logout, expired auth and new login clear old storage; a different session cannot restore it', () => {
  center.authenticate(scope, false); center.notify('控制台', '旧登录'); center.reset();
  expect(sessionStorage.getItem(NOTICE_STORAGE_KEY)).toBeNull();
  center.authenticate(scope, false); center.notify('控制台', '旧登录'); center.suspendSession();
  center.authenticate('b'.repeat(64), true); expect(center.state.history).toEqual([]);
  center.notify('控制台', '同登录旧历史'); center.authenticate('b'.repeat(64), false);
  expect(center.state.history).toEqual([]);
});
it('bounds history and serialized properties, excludes progress and credential-shaped values', () => {
  center.authenticate(scope, false);
  for (let i = 0; i < 105; i++) center.notify('控制台', `通知${i}`, 'info', { silent: true });
  center.notify('控制台', 'token=private-value Bearer sensitive-value');
  center.notify('控制台', '进行中', 'progress');
  const raw = sessionStorage.getItem(NOTICE_STORAGE_KEY)!;
  expect(raw).not.toContain('private-value'); expect(raw).not.toContain('sensitive-value'); expect(raw).not.toContain('进行中');
  expect(noticeStorage().load(scope)).toHaveLength(100);
  expect(Object.keys(JSON.parse(raw).entries[0])).toEqual(['id', 'source', 'message', 'tone', 'createdAt', 'unread']);
});
it('rejects corrupt, oversized, duplicate and invalid entries before restoring', () => {
  for (const raw of ['{', 'x'.repeat(524289), JSON.stringify({ version: 1, scope, entries: Array(101).fill({}) }),
    JSON.stringify({ version: 1, scope, entries: [{ id: 1, tone: 'progress' }] }), JSON.stringify(null)]) {
    sessionStorage.setItem(NOTICE_STORAGE_KEY, raw); expect(noticeStorage().load(scope)).toEqual([]);
    expect(sessionStorage.getItem(NOTICE_STORAGE_KEY)).toBeNull();
  }
});
it('storage denial and quota errors degrade without breaking notification delivery', () => {
  const store = noticeStorage(() => { throw new Error('denied'); });
  expect(store.load(scope)).toEqual([]); expect(() => store.save(scope, [])).not.toThrow();
  vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => { throw new Error('quota'); });
  center.authenticate(scope, false); expect(() => center.notify('控制台', '仍然可见')).not.toThrow();
  expect(center.state.active[0]?.message).toBe('仍然可见');
});

it('restored IDs cannot overflow the live sequence or reuse old visibility callbacks', () => {
  noticeStorage().save(scope, [{ id: Number.MAX_SAFE_INTEGER, source: '控制台', message: '旧历史', tone: 'info', createdAt: 1, unread: true }]);
  center.authenticate(scope, true);
  const restored = center.state.history[0]!.id;
  const next = center.notify('控制台', '新通知')!;
  expect(Number.isSafeInteger(next)).toBe(true); expect(next).toBeGreaterThan(restored);
  center.suspendSession(); center.authenticate(scope, true);
  center.setVisible(restored, 'history', true); center.setOpen(true);
  expect(center.unread.value).toBe(2);
});
