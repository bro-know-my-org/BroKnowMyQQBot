import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { createApp, nextTick } from 'vue';
import AdaptersPage from '../src/components/AdaptersPage.vue';
import { createApi } from '../src/api';
import { readRuntime } from '../src/runtime';
import type { RuntimePhase } from '../src/runtime';
import { runtimeFixture } from './runtimeFixture';

let app: ReturnType<typeof createApp> | undefined;
let hidden = false;
const flush = async () => { await new Promise<void>(resolve => setImmediate(resolve)); await nextTick(); };
const fetcher = vi.fn<typeof fetch>();
const body = () => document.body.textContent ?? '';
const reply = (data = runtimeFixture()) => new Response(JSON.stringify(data));
function mount() {
  const api = createApi(vi.fn());
  const root = document.createElement('div'); document.body.append(root);
  app = createApp(AdaptersPage, { request: api.request }); app.mount(root);
  return api;
}
async function click() { document.querySelector<HTMLButtonElement>('.runtime-panel button')!.click(); await flush(); }
beforeEach(() => {
  vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] });
  hidden = false; vi.spyOn(document, 'hidden', 'get').mockImplementation(() => hidden);
  vi.stubGlobal('fetch', fetcher); fetcher.mockReset(); fetcher.mockResolvedValue(reply());
});
afterEach(() => { app?.unmount(); app = undefined; document.body.replaceChildren(); vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

it('首次读取期间明示未知，真实大计数、积压、未在线与安全错误分类可见', async () => {
  let resolve!: (value: Response) => void;
  fetcher.mockImplementationOnce(() => new Promise<Response>(done => { resolve = done; }));
  mount(); await nextTick();
  expect(body()).toContain('正在读取状态'); expect(body()).not.toContain('运行降级');
  resolve(reply()); await flush();
  expect(body()).toContain('运行降级'); expect(body()).toContain('已配置 · 未在线'); expect(body()).toContain('18446744073709551615');
  expect(body()).toContain('2 / 32'); expect(body()).toContain('传输连接失败');
  expect(fetcher).toHaveBeenCalledTimes(1); expect(fetcher.mock.calls[0]![0]).toBe('/api/runtime');
});

it('初始失败可以重试，后续失败保留旧快照与 DOM，后台成功不清屏', async () => {
  fetcher.mockRejectedValueOnce(new TypeError('network'));
  mount(); await flush(); expect(body()).toContain('状态不可用'); expect(document.querySelector('article')).toBeNull();
  await click(); const article = document.querySelector('article'), values = document.querySelector('.runtime-values');
  fetcher.mockResolvedValueOnce(new Response('{"error":"fixture unavailable"}', { status: 503 }));
  await vi.advanceTimersByTimeAsync(5000); await flush();
  expect(body()).toContain('快照已过期'); expect(body()).toContain('保留上次成功'); expect(body()).toContain('fixture unavailable');
  expect(document.querySelector('article')).toBe(article); expect(document.querySelector('.runtime-values')).toBe(values);
  let resolve!: (value: Response) => void;
  fetcher.mockImplementationOnce(() => new Promise<Response>(done => { resolve = done; }));
  await vi.advanceTimersByTimeAsync(5000); await nextTick();
  expect(document.querySelector('article')).toBe(article); expect(body()).toContain('18446744073709551615');
  resolve(reply()); await flush(); expect(body()).not.toContain('快照已过期'); expect(body()).not.toContain('fixture unavailable');
  expect(document.querySelector('article')).toBe(article);
});

it('等待未完成读取时不重复请求，隐藏时取消并暂停，返回前台拒绝旧响应', async () => {
  const pending: Array<(value: Response) => void> = [];
  fetcher.mockImplementation(() => new Promise<Response>(resolve => { pending.push(resolve); }));
  mount(); await nextTick(); await click(); await vi.advanceTimersByTimeAsync(20000);
  expect(fetcher).toHaveBeenCalledTimes(1);
  hidden = true; document.dispatchEvent(new Event('visibilitychange')); await flush();
  expect(fetcher.mock.calls[0]![1]!.signal!.aborted).toBe(true);
  await vi.advanceTimersByTimeAsync(20000); expect(fetcher).toHaveBeenCalledTimes(1);
  hidden = false; document.dispatchEvent(new Event('visibilitychange')); await flush();
  expect(fetcher).toHaveBeenCalledTimes(2);
  pending[1]!(reply()); await flush();
  const old = runtimeFixture(); old.snapshot.events_received = '999'; pending[0]!(reply(old)); await flush();
  expect(body()).toContain('18446744073709551615'); expect(body()).not.toContain('999');
});

it('后台挂载不读取，卸载取消请求与计时，丢弃迟到响应', async () => {
  hidden = true; mount(); await flush(); expect(fetcher).not.toHaveBeenCalled();
  hidden = false; document.dispatchEvent(new Event('visibilitychange')); await flush();
  let resolve!: (value: Response) => void;
  fetcher.mockImplementationOnce(() => new Promise<Response>(done => { resolve = done; }));
  await vi.advanceTimersByTimeAsync(5000); await nextTick(); expect(fetcher).toHaveBeenCalledTimes(2);
  const signal = fetcher.mock.calls[1]![1]!.signal!;
  app!.unmount(); app = undefined; expect(signal.aborted).toBe(true);
  resolve(reply()); await flush(); await vi.advanceTimersByTimeAsync(20000); expect(fetcher).toHaveBeenCalledTimes(2);
});

it.each<[RuntimePhase, string]>([['starting', '启动中'], ['stopped', '已停止'], ['shutting_down', '正在关闭'], ['failed', '运行失败']])('区分 %s 的运行与连接状态', async (phase, label) => {
  const data = runtimeFixture(); data.health.phase = phase; data.snapshot.phase = phase;
  data.health.status = phase === 'starting' || phase === 'shutting_down' ? 'degraded' : 'unhealthy';
  fetcher.mockResolvedValue(reply(data)); mount(); await flush();
  expect(body()).toContain(label); expect(body()).not.toContain('已配置 · 未在线');
});

it('真实在线后保留历史故障，绝不把未配置的连接填为成功', async () => {
  const data = runtimeFixture(); data.health.status = 'healthy'; data.health.online_adapters = 1; data.snapshot.adapters['qq-fixture']!.online = true;
  fetcher.mockResolvedValueOnce(reply(data)); mount(); await flush(); expect(body()).toContain('当前已在线；历史故障计数保留');
  data.health.configured_adapters = 0; data.health.online_adapters = 0; data.health.status = 'degraded'; data.snapshot.adapters = {};
  fetcher.mockResolvedValueOnce(reply(data)); await click(); expect(body()).toContain('当前没有可确认的在线连接');
});

it.each([
  { ...runtimeFixture(), snapshot: { ...runtimeFixture().snapshot, events_received: 9007199254740992 } },
  { ...runtimeFixture(), snapshot: { ...runtimeFixture().snapshot, events_received: '18446744073709551616' } },
  { ...runtimeFixture(), snapshot: { ...runtimeFixture().snapshot, phase: 'unknown' } },
  { ...runtimeFixture(), snapshot: { ...runtimeFixture().snapshot, phase: ['running'] } },
  { ...runtimeFixture(), health: { ...runtimeFixture().health, phase: ['running'] } },
  { ...runtimeFixture(), health: { ...runtimeFixture().health, status: ['healthy'] } },
  { ...runtimeFixture(), snapshot: { ...runtimeFixture().snapshot, adapters: { bad: { online: true, events_received: '0', events_rejected: '0', failures: '0', last_failure: ['transport'] } } } },
  { ...runtimeFixture(), snapshot: { ...runtimeFixture().snapshot, adapters: { bad: { online: true, events_received: '0', events_rejected: '0', failures: '0', last_failure: 'raw secret' } } } },
])('拒绝畸形运行快照 %#', data => { expect(() => readRuntime(data)).toThrow('运行状态响应格式异常'); });

it('后续畸形快照只标记过期，不替换上次真实状态', async () => {
  mount(); await flush(); const article = document.querySelector('article');
  fetcher.mockResolvedValueOnce(new Response(JSON.stringify({ ...runtimeFixture(), snapshot: { ...runtimeFixture().snapshot, phase: ['running'] } })));
  await click(); expect(body()).toContain('快照已过期'); expect(body()).toContain('运行状态响应格式异常');
  expect(document.querySelector('article')).toBe(article); expect(body()).toContain('已配置 · 未在线');
});
