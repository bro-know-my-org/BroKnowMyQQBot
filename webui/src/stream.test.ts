import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { createApp, nextTick } from 'vue';
import App from './App.vue';
import { ApiError, createApi } from './api';
import { createManagementRouter, pluginRoute } from './navigation';
import type { LogPage, LogQuery, LogStreamHandlers } from './types';

class MockEventSource extends EventTarget {
  static instances: MockEventSource[] = [];
  static CLOSED = 2;
  readyState = 0;
  onopen: (() => void) | null = null;
  onerror: (() => void) | null = null;
  close = vi.fn();
  constructor(readonly url: string, readonly options?: EventSourceInit) { super(); MockEventSource.instances.push(this); }
  send(data: unknown, type = 'logs') { this.dispatchEvent(new MessageEvent(type, { data: typeof data === 'string' ? data : JSON.stringify(data) })); }
}
const source = () => MockEventSource.instances.at(-1)!;
const row = (id = 1) => ({ id, timestamp: '2026-11-13T10:00:00Z', level: 'INFO', target: 'runtime', message: `日志 ${id}`, instance_id: null, fields: {} });
const page = (overrides: Partial<LogPage> = {}): LogPage => ({ session_id: 'first', entries: [row()], next_cursor: 1, dropped: false, has_more: false, ...overrides });
const handlers = (): LogStreamHandlers => ({ onPage: vi.fn(), onState: vi.fn(), onError: vi.fn() });
const response = (data: unknown, status = 200) => new Response(JSON.stringify(data), { status });
const fetcher = vi.fn<typeof fetch>();
const flush = async () => { await new Promise<void>(resolve => setTimeout(resolve, 25)); await nextTick(); };
let app: ReturnType<typeof createApp> | undefined;
let api: ReturnType<typeof createApi> | undefined;
beforeEach(() => {
  MockEventSource.instances = []; vi.stubGlobal('EventSource', MockEventSource); vi.stubGlobal('fetch', fetcher);
  fetcher.mockReset(); fetcher.mockResolvedValue(response({ mode: 'token', authenticated: true }));
});
afterEach(() => { app?.unmount(); app = undefined; api?.clear(); api = undefined; document.body.replaceChildren(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

describe('Cookie SSE API', () => {
  it('永久关闭立即显示停止，关闭订阅会取消尚未返回的会话复核', async () => {
    api = createApi(vi.fn()); const callbacks = handlers();
    const close = api.subscribeLogs({ level: 'INFO' }, callbacks);
    let signal: AbortSignal | undefined;
    fetcher.mockImplementationOnce((_input, init) => new Promise((_resolve, reject) => {
      signal = init?.signal as AbortSignal;
      signal.addEventListener('abort', () => reject(new DOMException('aborted', 'AbortError')));
    }));
    source().readyState = MockEventSource.CLOSED; source().onerror!();
    expect(callbacks.onState).toHaveBeenLastCalledWith('closed'); expect(signal!.aborted).toBe(false);
    close(); expect(signal!.aborted).toBe(true); await flush(); expect(callbacks.onError).not.toHaveBeenCalled();
  });
  it('原生连接永久关闭时停止伪重连并提供手动重试', async () => {
    api = createApi(vi.fn()); const callbacks = handlers(); api.subscribeLogs({ level: 'INFO' }, callbacks);
    source().readyState = MockEventSource.CLOSED; source().onerror!(); await flush();
    expect(source().close).toHaveBeenCalledOnce();
    expect(callbacks.onError).toHaveBeenCalledWith(expect.objectContaining({ message: expect.stringContaining('手动刷新重试') }));
  });
  it('构造同源URL与成对游标，原生连接持续收页并原地自动重连', async () => {
    api = createApi(vi.fn()); const callbacks = handlers();
    api.subscribeLogs({ level: 'WARN', limit: 200, instance_id: 'sample / #中文', after: 0, session_id: 'first' }, callbacks);
    const stream = source(), url = new URL(stream.url, 'http://localhost');
    expect(url.pathname).toBe('/api/logs/stream'); expect(url.searchParams.get('instance_id')).toBe('sample / #中文');
    expect(url.searchParams.get('after')).toBe('0'); expect(url.searchParams.get('session_id')).toBe('first'); expect(url.searchParams.get('limit')).toBe('200');
    expect(stream.options).toBeUndefined(); expect(callbacks.onState).toHaveBeenLastCalledWith('connecting');
    stream.onopen!(); stream.send(page()); stream.send(page({ entries: [], next_cursor: 3 }));
    expect(callbacks.onPage).toHaveBeenCalledTimes(2); stream.onerror!(); await flush();
    expect(callbacks.onState).toHaveBeenLastCalledWith('reconnecting'); expect(MockEventSource.instances).toHaveLength(1); expect(stream.close).not.toHaveBeenCalled();
    expect(fetcher).toHaveBeenCalledWith('/api/session', expect.objectContaining({ credentials: 'same-origin' }));
    stream.onopen!(); stream.send(page({ entries: [row(4)], next_cursor: 4 })); expect(callbacks.onPage).toHaveBeenCalledTimes(3);
  });
  it.each([
    { level: 'INFO', after: 1 }, { level: 'INFO', session_id: 'first' },
    { level: 'INFO', limit: 201 }, { level: 'INFO', after: Number.MAX_SAFE_INTEGER + 1, session_id: 'first' },
  ])('拒绝无效续接参数且不创建连接', query => {
    api = createApi(vi.fn()); expect(() => api!.subscribeLogs(query as LogQuery, handlers())).toThrow(ApiError);
    expect(MockEventSource.instances).toHaveLength(0);
  });
  it('clear关闭所有活动流、abort HTTP，忽略排队事件与延迟会话复核', async () => {
    const logout = vi.fn(); api = createApi(logout); const first = handlers(), second = handlers();
    api.subscribeLogs({ level: 'INFO' }, first); const old = source(); api.subscribeLogs({ level: 'ERROR' }, second);
    let resolve!: (response: Response) => void, signal: AbortSignal | undefined;
    fetcher.mockImplementationOnce(async (_input, init) => { signal = init?.signal as AbortSignal; return await new Promise(finish => { resolve = finish; }); });
    old.onerror!(); const close = source().close; api.clear();
    expect(old.close).toHaveBeenCalledTimes(1); expect(close).toHaveBeenCalledTimes(1); expect(signal!.aborted).toBe(true);
    old.send(page()); old.onopen!(); old.onerror!(); old.send('', 'auth-required');
    resolve(response({ mode: 'token', authenticated: false })); await flush();
    expect(first.onPage).not.toHaveBeenCalled(); expect(logout).not.toHaveBeenCalled(); expect(fetcher).toHaveBeenCalledTimes(1);
  });
  it('单独关闭或损坏后迟到事件无效，其他活动流仍可接收', () => {
    api = createApi(vi.fn()); const first = handlers(), second = handlers();
    const close = api.subscribeLogs({ level: 'INFO' }, first), old = source(); api.subscribeLogs({ level: 'INFO' }, second);
    close(); old.send(page()); old.onerror!(); source().send(page());
    expect(first.onPage).not.toHaveBeenCalled(); expect(second.onPage).toHaveBeenCalledOnce(); expect(fetcher).not.toHaveBeenCalled();
  });
  it('旧订阅关闭后的迟到401复核不结束新订阅', async () => {
    const logout = vi.fn(); api = createApi(logout);
    const close = api.subscribeLogs({ level: 'INFO' }, handlers()), old = source();
    let resolve!: (response: Response) => void;
    fetcher.mockImplementationOnce(() => new Promise(finish => { resolve = finish; }));
    old.onerror!(); close(); close();
    const callbacks = handlers(); api.subscribeLogs({ level: 'INFO' }, callbacks);
    resolve(response({}, 401)); await flush(); source().send(page());
    expect(logout).not.toHaveBeenCalled(); expect(callbacks.onPage).toHaveBeenCalledOnce(); expect(old.close).toHaveBeenCalledOnce();
  });
  it.each([
    '{broken', JSON.stringify(page()).replace('"next_cursor":1', '"next_cursor":9007199254740993'),
    page({ entries: [], has_more: true }), page({ entries: [row(2), row(1)], next_cursor: 2 }),
    page({ entries: [{ ...row(), fields: { detail: 42 } }] } as unknown as Partial<LogPage>),
    page({ entries: [{ ...row(), level: 'bad' }] }), page({ entries: [{ ...row(), timestamp: 'invalid' }] }),
    page({ entries: Array.from({ length: 201 }, (_, index) => row(index + 1)), next_cursor: 201 }),
  ])('无效JSON或LogPage明确停止，可重新订阅', invalid => {
    api = createApi(vi.fn()); const callbacks = handlers(); api.subscribeLogs({ level: 'INFO' }, callbacks); const old = source();
    old.send(invalid); expect(old.close).toHaveBeenCalledOnce(); expect(callbacks.onPage).not.toHaveBeenCalled();
    expect(callbacks.onError).toHaveBeenCalledWith(expect.objectContaining({ message: expect.stringContaining('手动刷新重试') }));
    old.send(page()); expect(callbacks.onPage).not.toHaveBeenCalled();
    api.subscribeLogs({ level: 'INFO' }, callbacks); source().send(page()); expect(callbacks.onPage).toHaveBeenCalledOnce();
  });
  it('重放ID可去重，拒绝游标回退，进程重启可接受更小游标', () => {
    api = createApi(vi.fn()); const callbacks = handlers(); api.subscribeLogs({ level: 'INFO' }, callbacks);
    source().send(page()); source().send(page()); expect(callbacks.onPage).toHaveBeenCalledTimes(2);
    source().send(page({ session_id: 'second', dropped: true, entries: [], next_cursor: 0 }));
    expect(callbacks.onPage).toHaveBeenCalledTimes(3);
    source().send(page({ session_id: 'second', entries: [], next_cursor: 2 }));
    source().send(page({ session_id: 'second', entries: [], next_cursor: 1 })); expect(callbacks.onError).toHaveBeenCalledOnce();
  });
  it.each(['auth-required', 'unauthenticated', '401'])('%s统一logout并关闭全部流', async reason => {
    const logout = vi.fn(); api = createApi(logout); api.subscribeLogs({ level: 'INFO' }, handlers()); const first = source();
    api.subscribeLogs({ level: 'INFO' }, handlers()); const second = source();
    if (reason === 'auth-required') first.send('', 'auth-required');
    else { fetcher.mockResolvedValueOnce(reason === '401' ? response({}, 401) : response({ mode: 'token', authenticated: false })); first.onerror!(); await flush(); }
    expect(logout).toHaveBeenCalledOnce(); expect(first.close).toHaveBeenCalledOnce(); expect(second.close).toHaveBeenCalledOnce();
    first.onerror!(); first.send('', 'auth-required'); await flush(); expect(logout).toHaveBeenCalledOnce();
  });
  it('网络错误保持自动重连，同一时刻只发一次会话复核', async () => {
    api = createApi(vi.fn()); const callbacks = handlers(); api.subscribeLogs({ level: 'INFO' }, callbacks);
    let reject!: (reason: unknown) => void;
    fetcher.mockImplementationOnce(() => new Promise((_resolve, fail) => { reject = fail; }));
    source().onerror!(); source().onerror!(); expect(fetcher).toHaveBeenCalledOnce();
    reject(new TypeError('offline')); await flush(); expect(source().close).not.toHaveBeenCalled();
    source().onopen!(); source().send(page()); expect(callbacks.onPage).toHaveBeenCalledOnce();
  });
});

describe('应用真实DOM与SSE接线', () => {
  async function mount(plugin = false) {
    fetcher.mockImplementation(async input => response(String(input) === '/api/plugins'
      ? { plugins: plugin ? [{ instance_id: 'sample', plugin_id: 'dev.bkm.sample', name: '测试插件', runtime_state: 'ready', requested_permissions: [], granted_permissions: [] }] : [] }
      : { mode: 'token', authenticated: true }));
    const router = createManagementRouter(true); await router.push(plugin ? pluginRoute('sample', 'logs') : { name: 'logs' }); await router.isReady();
    const root = document.createElement('div'); document.body.append(root); app = createApp(App).use(router); app.mount(root); await flush(); return router;
  }
  it.each(['auth-required', '401'])('%s将日志页切回登录并释放流', async reason => {
    await mount(); source().send(page()); await flush(); expect(document.querySelector('.terminal-message')?.textContent).toBe('日志 1');
    const stream = source();
    if (reason === 'auth-required') stream.send('', 'auth-required');
    else { fetcher.mockResolvedValueOnce(response({}, 401)); stream.onerror!(); }
    await flush(); expect(document.querySelector('#login-form')).not.toBeNull(); expect(document.querySelector('.terminal-panel')).toBeNull();
    expect(document.body.textContent).toContain('登录已失效'); expect(stream.close).toHaveBeenCalled();
  });
  it('插件详情锁定实例，离开路由与显式退出都关闭订阅', async () => {
    const router = await mount(true); expect(new URL(source().url, 'http://localhost').searchParams.get('instance_id')).toBe('sample');
    const first = source(); await router.push({ name: 'logs' }); await flush(); expect(first.close).toHaveBeenCalled();
    const second = source(); expect(new URL(second.url, 'http://localhost').searchParams.has('instance_id')).toBe(false);
    document.querySelector<HTMLButtonElement>('#logout')!.click(); await flush();
    expect(second.close).toHaveBeenCalled(); expect(document.querySelector('#login-form')).not.toBeNull();
  });
});
