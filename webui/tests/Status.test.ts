import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { createManagementStatus, readManagementSnapshot } from '../src/status';
import type { Request } from '../src/api';
import { runtimeFixture } from './runtimeFixture';
const BOOT = '11111111-1111-4111-8111-111111111111', NEXT = '22222222-2222-4222-8222-222222222222';
class Source extends EventTarget {
  static CLOSED = 2; static all: Source[] = [];
  readyState = 1; onerror: (() => void) | null = null; onopen: (() => void) | null = null;
  close = vi.fn(() => { this.readyState = 2; });
  constructor(public url: string) { super(); Source.all.push(this); }
  emit(type: string, value: unknown) { this.dispatchEvent(new MessageEvent(type, { data: JSON.stringify(value) })); }
}
const snapshot = (revision = '1', boot = BOOT) => ({ boot_id: boot, revision, runtime: { ...runtimeFixture(), boot_id: boot }, plugins: [], plugins_truncated: false, settings_restart_required: null });
let hidden = false, store: ReturnType<typeof createManagementStatus>;
const request = vi.fn<Request>(), unauthorized = vi.fn();
const flush = async () => { await new Promise<void>(resolve => setImmediate(resolve)); };
beforeEach(() => {
  hidden = false; Source.all = []; vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout', 'Date'] });
  vi.spyOn(document, 'hidden', 'get').mockImplementation(() => hidden);
  vi.stubGlobal('EventSource', Source); request.mockReset(); unauthorized.mockReset();
  request.mockResolvedValue({ authenticated: true, mode: 'none', boot_id: BOOT });
  store = createManagementStatus(request as Request, unauthorized);
});
afterEach(() => { store.dispose(); vi.restoreAllMocks(); vi.unstubAllGlobals(); vi.useRealTimers(); });
it('one root source preserves data identity on idle, stale and old revisions without writes', () => {
  store.start(BOOT); store.start(BOOT); expect(Source.all).toHaveLength(1);
  const source = Source.all[0]!; source.emit('status', snapshot('18446744073709551615'));
  const original = store.snapshot.value; expect(store.state.value).toBe('open');
  source.emit('status', snapshot('1')); expect(store.snapshot.value).toBe(original);
  source.emit('stale', {}); expect(store.state.value).toBe('stale'); expect(store.snapshot.value).toBe(original);
  source.emit('status', snapshot('18446744073709551615')); expect(store.state.value).toBe('open'); expect(store.snapshot.value).toBe(original);
  expect(request).not.toHaveBeenCalled();
});
it('background closes the source and ignores late generations, foreground receives newest full data', () => {
  store.start(BOOT); const old = Source.all[0]!; old.emit('status', snapshot());
  hidden = true; document.dispatchEvent(new Event('visibilitychange')); expect(old.close).toHaveBeenCalledOnce();
  old.emit('status', snapshot('99')); expect(store.snapshot.value!.revision).toBe('1');
  hidden = false; document.dispatchEvent(new Event('visibilitychange')); expect(Source.all).toHaveLength(2);
  Source.all[1]!.emit('status', snapshot('2')); expect(store.snapshot.value!.revision).toBe('2');
  store.stop(); Source.all[1]!.emit('status', snapshot('100')); expect(store.snapshot.value).toBeNull();
});
it('failed handshake probes session once, aborts probes and retry timers on teardown', async () => {
  let finish!: (value: unknown) => void;
  request.mockImplementation(() => new Promise(resolve => { finish = resolve; }));
  store.start(BOOT); const source = Source.all[0]!; source.readyState = Source.CLOSED;
  source.onerror!(); source.onerror!(); expect(request).toHaveBeenCalledOnce();
  const signal = request.mock.calls[0]![2]!;
  store.stop(); expect(signal.aborted).toBe(true);
  finish({ authenticated: false }); await flush(); await vi.advanceTimersByTimeAsync(60000);
  expect(unauthorized).not.toHaveBeenCalled(); expect(Source.all).toHaveLength(1); expect(store.state.value).toBe('closed');
});
it('401-equivalent auth event and false session clear old data and never reconnect', async () => {
  store.start(BOOT); Source.all[0]!.emit('status', snapshot()); Source.all[0]!.emit('auth-required', {});
  expect(unauthorized).toHaveBeenCalledOnce(); expect(store.snapshot.value).toBeNull();
  store.start(BOOT); request.mockResolvedValue({ authenticated: false, mode: 'token', boot_id: BOOT });
  Source.all[1]!.onerror!(); await flush(); expect(unauthorized).toHaveBeenCalledTimes(2);
  await vi.advanceTimersByTimeAsync(60000); expect(Source.all).toHaveLength(2);
});
it('boot reset requires matching live session and rejects late old-source frames', async () => {
  store.start(BOOT); const old = Source.all[0]!; old.emit('status', snapshot('9'));
  request.mockResolvedValue({ authenticated: true, mode: 'none', boot_id: NEXT });
  old.emit('status', snapshot('1', NEXT)); await flush();
  expect(Source.all).toHaveLength(2); expect(old.close).toHaveBeenCalledOnce();
  Source.all[1]!.emit('status', snapshot('1', NEXT)); old.emit('status', snapshot('1000'));
  expect(store.snapshot.value!.boot_id).toBe(NEXT); expect(store.snapshot.value!.revision).toBe('1');
});
it('malformed frames preserve visible old data and expose a stale error until explicit retry', () => {
  store.start(BOOT); const source = Source.all[0]!; source.emit('status', snapshot());
  const original = store.snapshot.value; source.emit('status', { ...snapshot('2'), revision: 9007199254740992 });
  expect(store.snapshot.value).toBe(original); expect(store.state.value).toBe('stale'); expect(store.error.value).toContain('安全整数范围');
  expect(source.close).toHaveBeenCalledOnce(); store.refresh(); expect(Source.all).toHaveLength(2);
});
it.each(['18446744073709551616', '-1', '01', 1])('rejects invalid u64 revision %s', revision => {
  expect(() => readManagementSnapshot({ ...snapshot(), revision })).toThrow();
});
it('rejects duplicate plugin IDs, unsafe timestamps and excessive arrays', () => {
  const plugin = { instance_id: 'one', plugin_id: 'dev.bkm.one', name: 'One', version: '1', loaded_version: null,
    enabled: false, runtime_state: 'not_loaded', pending: false, restart_required: false, updated_at_ms: 1 };
  expect(() => readManagementSnapshot({ ...snapshot(), plugins: [plugin, plugin] })).toThrow();
  expect(() => readManagementSnapshot({ ...snapshot(), plugins: [{ ...plugin, updated_at_ms: 9007199254740992 }] })).toThrow();
  expect(() => readManagementSnapshot({ ...snapshot(), plugins: Array(257).fill(plugin) })).toThrow();
  expect(readManagementSnapshot({ ...snapshot(), plugins: [{ ...plugin, config: { private: 'drop' } }] }).plugins[0]).not.toHaveProperty('config');
});

it('pagehide closes and pageshow restores one full snapshot subscription', () => {
  store.start(BOOT); const source = Source.all[0]!; source.emit('status', snapshot());
  window.dispatchEvent(new PageTransitionEvent('pagehide'));
  expect(source.close).toHaveBeenCalledOnce(); expect(store.snapshot.value!.revision).toBe('1');
  window.dispatchEvent(new PageTransitionEvent('pageshow')); expect(Source.all).toHaveLength(2);
  store.dispose(); window.dispatchEvent(new PageTransitionEvent('pageshow')); expect(Source.all).toHaveLength(2);
});
it('a stalled session probe times out without leaving a permanent in-flight probe', async () => {
  request.mockImplementation(() => new Promise(() => {}));
  store.start(BOOT); const source = Source.all[0]!; source.onerror!();
  await vi.advanceTimersByTimeAsync(5000); expect(request.mock.calls[0]![2]!.aborted).toBe(true);
  expect(store.error.value).toContain('超时'); source.onerror!(); expect(request).toHaveBeenCalledTimes(2);
});

it.each(['reject', 'timeout'])('obsolete session probe %s cannot mark a recovered idle stream stale', async outcome => {
  let reject!: (error: Error) => void;
  request.mockImplementation(() => new Promise((_resolve, fail) => { reject = fail; }));
  store.start(BOOT); const source = Source.all[0]!; source.onerror!(); source.emit('status', snapshot());
  if (outcome === 'reject') { reject(new Error('old probe failure')); await flush(); }
  else await vi.advanceTimersByTimeAsync(5000);
  expect(store.state.value).toBe('open'); expect(store.error.value).toBe('');
});
it('invalid successful session response preserves data and authentication', async () => {
  store.start(BOOT); const source = Source.all[0]!; source.emit('status', snapshot());
  request.mockResolvedValue({}); source.onerror!(); await flush();
  expect(unauthorized).not.toHaveBeenCalled(); expect(store.snapshot.value!.revision).toBe('1');
  expect(store.error.value).toContain('格式异常'); expect(store.state.value).toBe('stale');
});

it('CLOSED source waits for delayed session authentication before reconnecting', async () => {
  let finish!: (session: unknown) => void;
  request.mockImplementation(() => new Promise(resolve => { finish = resolve; }));
  store.start(BOOT); const source = Source.all[0]!; source.readyState = Source.CLOSED; source.onerror!();
  await vi.advanceTimersByTimeAsync(2000); expect(Source.all).toHaveLength(1);
  expect(request.mock.calls[0]![2]!.aborted).toBe(false);
  finish({ authenticated: false, mode: 'token', boot_id: BOOT }); await flush();
  expect(unauthorized).toHaveBeenCalledOnce(); await vi.advanceTimersByTimeAsync(10000); expect(Source.all).toHaveLength(1);
});
it('failed boot confirmation retries even when new server sends no further changed snapshots', async () => {
  store.start(BOOT); const source = Source.all[0]!; source.emit('status', snapshot('9'));
  request.mockRejectedValueOnce(new Error('offline')); source.emit('status', snapshot('1', NEXT)); await flush();
  request.mockResolvedValue({ authenticated: true, mode: 'none', boot_id: NEXT });
  await vi.advanceTimersByTimeAsync(1000); expect(Source.all).toHaveLength(2);
  Source.all[1]!.emit('status', snapshot('1', NEXT)); await flush();
  Source.all.at(-1)!.emit('status', snapshot('1', NEXT)); expect(store.snapshot.value!.boot_id).toBe(NEXT);
});

it('repeated mismatched frames cannot bypass boot verification backoff', async () => {
  store.start(BOOT); const source = Source.all[0]!; source.emit('status', snapshot('9'));
  request.mockRejectedValue(new Error('offline')); source.emit('status', snapshot('1', NEXT)); await flush();
  for (let i = 2; i < 20; i++) { source.emit('status', snapshot(String(i), NEXT)); await flush(); }
  expect(request).toHaveBeenCalledOnce(); await vi.advanceTimersByTimeAsync(999); expect(request).toHaveBeenCalledOnce();
  await vi.advanceTimersByTimeAsync(1); Source.all[1]!.emit('status', snapshot('20', NEXT)); await flush();
  expect(request).toHaveBeenCalledTimes(2);
});
it('valid recovery cancels a scheduled reconnect rather than closing the healthy source', async () => {
  store.start(BOOT); const source = Source.all[0]!; source.emit('status', snapshot('9'));
  request.mockRejectedValue(new Error('offline')); source.emit('status', snapshot('1', NEXT)); await flush();
  source.emit('status', snapshot('10')); await vi.advanceTimersByTimeAsync(60000);
  expect(store.state.value).toBe('open'); expect(source.close).not.toHaveBeenCalled(); expect(Source.all).toHaveLength(1);
});

it('alternating boot frames cannot force multiple probes on one connection', async () => {
  store.start(BOOT); const source = Source.all[0]!; source.emit('status', snapshot('1'));
  request.mockRejectedValue(new Error('offline'));
  for (let i = 2; i < 12; i++) {
    source.emit('status', snapshot(String(i), NEXT)); await flush();
    source.emit('status', snapshot(String(i))); await flush();
  }
  expect(request).toHaveBeenCalledOnce(); await vi.advanceTimersByTimeAsync(60000); expect(Source.all).toHaveLength(1);
});
