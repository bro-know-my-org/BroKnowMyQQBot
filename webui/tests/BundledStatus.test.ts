import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { createApp, nextTick, ref } from 'vue';
import BundledPluginsPage from '../src/components/BundledPluginsPage.vue';
import { createManagementRouter } from '../src/navigation';
import { createManagementStatus, managementStatusKey, type PluginStatus } from '../src/status';
import { createNotifications, notificationKey } from '../src/notifications';
import { ApiError } from '../src/api';
import { bundledFixture } from './bundledFixture';
import { runtimeFixture } from './runtimeFixture';

const BOOT = '11111111-1111-4111-8111-111111111111';
const hash = (letter: string) => letter.repeat(64);
class Source extends EventTarget {
  static all: Source[] = [];
  close = vi.fn();
  constructor() { super(); Source.all.push(this); }
}
let app: ReturnType<typeof createApp> | undefined, server = bundledFixture(), sequence = 0;
let shared: ReturnType<typeof createManagementStatus>, notices: ReturnType<typeof createNotifications>;
const request = vi.fn(), dirty = vi.fn(), busy = vi.fn(), saved = vi.fn();
const flush = async () => { await new Promise<void>(resolve => setImmediate(resolve)); await nextTick(); };
const get = <T extends HTMLElement = HTMLElement>(selector: string) => document.querySelector<T>(selector)!;
const field = () => get<HTMLTextAreaElement>('#field-0');
const click = async (selector: string) => { get<HTMLButtonElement>(selector).click(); await flush(); };
const submit = async () => { get('#bundled-config').dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })); await flush(); };
const edit = async (value: string) => { field().value = value; field().dispatchEvent(new Event('input', { bubbles: true })); await nextTick(); };
const posts = () => request.mock.calls.filter(([, body]) => body);
const deferred = () => {
  let resolve!: (value: unknown) => void, reject!: (reason: unknown) => void;
  const promise = new Promise((done, fail) => { resolve = done; reject = fail; });
  return { promise, resolve, reject };
};
function push(bundled_revision: string | null, plugins: PluginStatus[] = [], truncated = false) {
  Source.all[0]!.dispatchEvent(new MessageEvent('status', { data: JSON.stringify({
    boot_id: BOOT, revision: String(++sequence), runtime: { ...runtimeFixture(), boot_id: BOOT },
    plugins, plugins_truncated: truncated, settings_restart_required: null, bundled_revision,
  }) }));
}
const summary = (change: Partial<PluginStatus> = {}): PluginStatus => ({
  instance_id: 'dev.bkm.ping/default', plugin_id: 'dev.bkm.ping', name: 'ping', version: '0.1.0', loaded_version: '0.1.0',
  enabled: true, runtime_state: 'ready', pending: false, restart_required: false, updated_at_ms: 1, ...change,
});
async function mount(locked = ref(false)) {
  const router = createManagementRouter(true);
  await router.push({ name: 'bundled-plugins', query: { instance_id: 'dev.bkm.ping/default' } }); await router.isReady();
  const root = document.createElement('div'); document.body.append(root);
  app = createApp({ components: { BundledPluginsPage }, setup: () => ({ request, dirty, busy, saved, locked }),
    template: '<BundledPluginsPage :request="request" :locked="locked" @dirty="dirty" @busy="busy" @saved="saved" />' })
    .use(router).provide(managementStatusKey, shared).provide(notificationKey, notices);
  app.mount(root); await flush();
}
beforeEach(() => {
  server = bundledFixture(); sequence = 0; Source.all = [];
  request.mockReset(); dirty.mockReset(); busy.mockReset(); saved.mockReset();
  vi.spyOn(document, 'hidden', 'get').mockReturnValue(false); vi.stubGlobal('EventSource', Source);
  vi.spyOn(window, 'confirm').mockReturnValue(true);
  request.mockImplementation(async (_path, body) => {
    if (body) {
      const p = server.plugins.find(p => p.key === body.name)!;
      if (body.config) p.config = body.config;
      if (body.enabled !== undefined) p.enabled = body.enabled;
      server.revision = hash('b'); p.pending = true; p.restart_required = true;
    }
    return structuredClone(server);
  });
  shared = createManagementStatus(request, vi.fn()); shared.start(BOOT); notices = createNotifications();
});
afterEach(() => { app?.unmount(); app = undefined; shared.dispose(); notices.dispose(); document.body.replaceChildren(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

it('revision changes update clean configuration quietly; unrelated runtime frames only merge matching summaries', async () => {
  push(hash('a')); await mount();
  const original = field(); const notify = vi.spyOn(notices, 'notify');
  push(hash('a'), [summary({ runtime_state: 'disabled', pending: true, restart_required: true, enabled: false })]); await flush();
  expect(document.body.textContent).toContain('已禁用'); expect(document.body.textContent).toContain('已启用');
  expect(document.body.textContent).toContain('设置待重启生效'); expect(field()).toBe(original); expect(request).toHaveBeenCalledTimes(1);
  push(hash('a'), [summary({ plugin_id: 'dev.bkm.other', runtime_state: 'draining' })]); await flush();
  expect(document.body.textContent).toContain('运行中');
  server.revision = hash('b'); server.plugins[0]!.config = { greeting: '外部保存' };
  push(hash('b')); await flush(); expect(field().value).toBe('外部保存'); expect(request).toHaveBeenCalledTimes(2);
  push(hash('b'), [summary({ loaded_version: null, runtime_state: 'not_loaded' })], true); await flush();
  expect(document.body.textContent).toContain('未加载'); expect(document.body.textContent).toContain('未注册');
  push(null); push(null); await flush(); expect(request).toHaveBeenCalledTimes(2);
  expect(notify).not.toHaveBeenCalled(); expect(Source.all).toHaveLength(1); expect(posts()).toHaveLength(0);
});

it('external changes preserve drafts and block saves until an explicit successful read', async () => {
  push(hash('a')); await mount(); await edit('我的草稿'); const original = field();
  server.revision = hash('c'); server.plugins[0]!.config = { greeting: '远端设置' };
  push(hash('c')); await flush();
  expect(field()).toBe(original); expect(field().value).toBe('我的草稿'); expect(dirty).toHaveBeenLastCalledWith(true);
  expect(document.body.textContent).toContain('已保存设置发生变化'); expect(get<HTMLButtonElement>('#save-bundled').disabled).toBe(true);
  await submit(); expect(posts()).toHaveLength(0);
  request.mockRejectedValueOnce(new Error('读取失败')); await click('#refresh-bundled'); expect(get<HTMLButtonElement>('#save-bundled').disabled).toBe(true);
  await click('#refresh-bundled'); expect(field()).toBe(original); expect(get<HTMLButtonElement>('#save-bundled').disabled).toBe(false);
  await submit(); expect(posts()[0]![1]).toMatchObject({ expected_revision: hash('c'), config: { greeting: '我的草稿' } });
});

it('unrelated runtime frames preserve a dirty editor and do not block its next save', async () => {
  push(hash('a')); await mount(); await edit('仍可保存的草稿'); const original = field();
  push(hash('a'), [summary({ runtime_state: 'disabled' })]); await flush();
  expect(field()).toBe(original); expect(field().value).toBe('仍可保存的草稿'); expect(request).toHaveBeenCalledOnce();
  expect(get<HTMLButtonElement>('#save-bundled').disabled).toBe(false); expect(document.body.textContent).toContain('已禁用');
  await submit(); expect(posts()[0]![1].config).toEqual({ greeting: '仍可保存的草稿' });
});

it('a refreshed JSON-only schema cannot replace the schema that owns a dirty form draft', async () => {
  push(hash('a')); await mount(); await edit('保留表单草稿'); const original = field();
  server.revision = hash('c'); server.plugins[0]!.schema = null; server.plugins[0]!.config = { greeting: '远端值' };
  push(hash('c')); await flush();
  expect(field()).toBe(original); expect(field().value).toBe('保留表单草稿'); expect(document.querySelector('#config-json')).toBeNull();
  await click('#refresh-bundled');
  expect(field()).toBe(original); expect(field().value).toBe('保留表单草稿');
  await submit(); expect(posts()[0]![1].config).toEqual({ greeting: '保留表单草稿' });
  expect(document.querySelector('#field-0')).toBeNull(); expect(get<HTMLTextAreaElement>('#config-json').value).toContain('保留表单草稿');
});

it('a failed automatic read retries with backoff when the revision stays unchanged and preserves its draft', async () => {
  push(hash('a')); await mount(); await edit('重试保留草稿');
  vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] });
  try {
    server.revision = hash('c'); server.plugins[0]!.config = { greeting: '远端值' };
    request.mockRejectedValueOnce(new Error('暂时断网')); push(hash('c')); await flush();
    expect(request).toHaveBeenCalledTimes(2); expect(get<HTMLButtonElement>('#save-bundled').disabled).toBe(true);
    push(hash('c')); await flush(); await vi.advanceTimersByTimeAsync(999); expect(request).toHaveBeenCalledTimes(2);
    await vi.advanceTimersByTimeAsync(1); await flush();
    expect(request).toHaveBeenCalledTimes(3); expect(field().value).toBe('重试保留草稿');
    expect(document.body.textContent).toContain('已保存设置发生变化'); expect(get<HTMLButtonElement>('#save-bundled').disabled).toBe(true);
    await vi.advanceTimersByTimeAsync(30000); expect(request).toHaveBeenCalledTimes(3);
    await click('#refresh-bundled'); expect(get<HTMLButtonElement>('#save-bundled').disabled).toBe(false);
  } finally { vi.useRealTimers(); }
});

it('a failed manual refresh preserves an automatic retry that was already pending', async () => {
  push(hash('a')); await mount(); await edit('手动重试仍保留草稿');
  vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] });
  try {
    server.revision = hash('c'); request.mockRejectedValueOnce(new Error('自动读取失败'));
    push(hash('c')); await flush();
    request.mockRejectedValueOnce(new Error('手动读取也失败')); await click('#refresh-bundled');
    expect(request).toHaveBeenCalledTimes(3); push(hash('c'));
    await vi.advanceTimersByTimeAsync(1000); await flush();
    expect(request).toHaveBeenCalledTimes(4); expect(field().value).toBe('手动重试仍保留草稿');
    expect(get<HTMLButtonElement>('#save-bundled').disabled).toBe(true);
    await vi.advanceTimersByTimeAsync(30000); expect(request).toHaveBeenCalledTimes(4);
  } finally { vi.useRealTimers(); }
});

it('a revision reverting to the cached hash resumes pending synchronization without clearing draft protection', async () => {
  push(hash('a')); await mount(); await edit('回退时保留草稿');
  vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] });
  try {
    server.revision = hash('b'); request.mockRejectedValueOnce(new Error('读取失败'));
    push(hash('b')); await flush(); expect(request).toHaveBeenCalledTimes(2);
    server.revision = hash('a'); push(hash('a')); await flush();
    expect(request).toHaveBeenCalledTimes(3); expect(field().value).toBe('回退时保留草稿');
    expect(get<HTMLButtonElement>('#save-bundled').disabled).toBe(true); expect(busy).toHaveBeenLastCalledWith(false);
    await vi.advanceTimersByTimeAsync(30000); expect(request).toHaveBeenCalledTimes(3);
  } finally { vi.useRealTimers(); }
});

it('changes during a read coalesce into a follow-up and obsolete responses never replace the editor', async () => {
  push(hash('a')); await mount(); const original = field(), old = structuredClone(server);
  const first = deferred(), second = deferred(); request.mockReturnValueOnce(first.promise).mockReturnValueOnce(second.promise);
  await click('#refresh-bundled'); server.revision = hash('c'); server.plugins[0]!.config = { greeting: '最新设置' };
  push(hash('b')); push(hash('c')); await flush(); expect(request).toHaveBeenCalledTimes(2);
  old.plugins[0]!.config = { greeting: '迟到的旧设置' }; first.resolve(old); await flush();
  expect(request).toHaveBeenCalledTimes(3); expect(field()).toBe(original); expect(field().value).toBe('原值'); expect(busy).toHaveBeenLastCalledWith(true);
  second.resolve(structuredClone(server)); await flush(); expect(field().value).toBe('最新设置'); expect(busy).toHaveBeenLastCalledWith(false);
  push(hash('c')); await flush(); expect(request).toHaveBeenCalledTimes(3);
});

it('a failed obsolete read cannot replace the feedback for the latest successful automatic read', async () => {
  push(hash('a')); await mount(); const first = deferred(); request.mockReturnValueOnce(first.promise);
  const notify = vi.spyOn(notices, 'notify'); push(hash('b')); await flush();
  server.revision = hash('c'); server.plugins[0]!.config = { greeting: '最新设置' }; push(hash('c'));
  first.reject(new Error('过时的读取错误')); await flush();
  expect(request).toHaveBeenCalledTimes(3); expect(field().value).toBe('最新设置'); expect(notify).not.toHaveBeenCalled();
});

it.each(['before', 'after'])('a save’s own SSE arriving %s its response never reports a draft conflict', async order => {
  push(hash('a')); await mount(); await edit('已保存草稿');
  const pending = deferred(); request.mockReturnValueOnce(pending.promise); await submit();
  server.revision = hash('b'); server.plugins[0]!.config = { greeting: '已保存草稿' }; server.plugins[0]!.pending = true;
  if (order === 'before') push(hash('b'));
  pending.resolve(structuredClone(server)); await flush();
  if (order === 'after') { await edit('下一份草稿'); push(hash('b')); await flush(); }
  expect(document.body.textContent).not.toContain('已保存设置发生变化');
  if (order === 'after') expect(get<HTMLButtonElement>('#save-bundled').disabled).toBe(false);
  else expect(get<HTMLButtonElement>('#toggle-bundled').disabled).toBe(false);
  expect(field().value).toBe(order === 'after' ? '下一份草稿' : '已保存草稿'); expect(saved).toHaveBeenCalledOnce();
  expect(request).toHaveBeenCalledTimes(order === 'before' ? 3 : 2); expect(notices.state.history.filter(n => n.tone === 'success')).toHaveLength(1);
});

it('an external revision received during a save is re-read after the delayed save response', async () => {
  push(hash('a')); await mount(); await edit('本次保存');
  const pending = deferred(); request.mockReturnValueOnce(pending.promise); await submit();
  const response = structuredClone(server); response.revision = hash('b'); response.plugins[0]!.config = { greeting: '本次保存' };
  server.revision = hash('c'); server.plugins[0]!.config = { greeting: '随后外部保存' }; push(hash('b')); push(hash('c'));
  pending.resolve(response); await flush();
  expect(field().value).toBe('随后外部保存'); expect(request).toHaveBeenCalledTimes(3); expect(saved).toHaveBeenCalledOnce();
  expect(document.body.textContent).not.toContain('已保存设置发生变化');
});

it.each(['config', 'enabled'])('automatic reads never clear unknown %s write protection, including changes queued during that write', async kind => {
  push(hash('a')); await mount(); if (kind === 'config') await edit('未知结果的草稿');
  const pending = deferred(); request.mockReturnValueOnce(pending.promise);
  if (kind === 'config') await submit(); else await click('#toggle-bundled');
  server.revision = hash('b'); server.plugins[0]!.config = { greeting: '远端设置' }; push(hash('b'));
  pending.reject(new ApiError('请求超时', 408)); await flush();
  expect(request).toHaveBeenCalledTimes(3); expect(document.body.textContent).toContain('保存发生冲突或结果未知');
  if (kind === 'config') expect(field().value).toBe('未知结果的草稿');
  expect(get<HTMLButtonElement>('#toggle-bundled').disabled).toBe(true); await submit(); expect(posts()).toHaveLength(1);
  server.revision = hash('c'); push(hash('c')); await flush(); expect(get<HTMLButtonElement>('#toggle-bundled').disabled).toBe(true);
  await click('#refresh-bundled'); expect(document.body.textContent).not.toContain('保存发生冲突或结果未知');
  if (kind === 'config') expect(get<HTMLButtonElement>('#save-bundled').disabled).toBe(false);
  else expect(get<HTMLButtonElement>('#toggle-bundled').disabled).toBe(false);
});

it('locked pages defer and coalesce synchronization without reads or writes until unlocked', async () => {
  const locked = ref(true); push(hash('a')); await mount(locked); push(hash('b')); push(hash('c')); await flush(); expect(request).not.toHaveBeenCalled();
  server.revision = hash('c'); locked.value = false; await flush(); expect(request).toHaveBeenCalledOnce();
  locked.value = true; await nextTick(); server.revision = hash('d'); server.plugins[0]!.config = { greeting: '解锁后读取' };
  push(hash('d')); await click('#toggle-bundled'); expect(request).toHaveBeenCalledOnce(); expect(window.confirm).not.toHaveBeenCalled();
  locked.value = false; await flush(); expect(request).toHaveBeenCalledTimes(2); expect(field().value).toBe('解锁后读取');
});

it('a lock arriving during a read defers its queued follow-up, and unmount aborts that read and drops late success', async () => {
  const locked = ref(false); push(hash('a')); await mount(locked); const first = deferred(), second = deferred();
  request.mockReturnValueOnce(first.promise).mockReturnValueOnce(second.promise);
  push(hash('b')); await flush(); locked.value = true; push(hash('c')); first.resolve(structuredClone(server)); await flush();
  expect(request).toHaveBeenCalledTimes(2); locked.value = false; await flush(); expect(request).toHaveBeenCalledTimes(3);
  const signal = request.mock.calls.at(-1)![2] as AbortSignal; const notify = vi.spyOn(notices, 'notify');
  app!.unmount(); app = undefined; expect(signal.aborted).toBe(true); second.resolve(structuredClone(server)); await flush();
  expect(notify).not.toHaveBeenCalled(); expect(saved).not.toHaveBeenCalled(); expect(busy).toHaveBeenLastCalledWith(false);
});
