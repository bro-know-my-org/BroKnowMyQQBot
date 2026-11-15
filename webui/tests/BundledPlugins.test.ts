import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { createApp, nextTick, ref } from 'vue';
import BundledPluginsPage from '../src/components/BundledPluginsPage.vue';
import { createManagementRouter } from '../src/navigation';
import { readBundledCatalogue } from '../src/bundledPlugins';
import { ApiError } from '../src/api';
import { bundledFixture } from './bundledFixture';

let app: ReturnType<typeof createApp> | undefined, server = bundledFixture();
const request = vi.fn(), dirty = vi.fn(), busy = vi.fn(), saved = vi.fn();
const flush = async () => { await new Promise<void>(resolve => setImmediate(resolve)); await nextTick(); };
const get = <T extends HTMLElement = HTMLElement>(selector: string) => document.querySelector<T>(selector)!;
const click = async (selector: string) => { get<HTMLButtonElement>(selector).click(); await flush(); };
const edit = async (value: string, selector = '#field-0') => { get<HTMLTextAreaElement>(selector).value = value; get(selector).dispatchEvent(new Event('input', { bubbles: true })); await nextTick(); };
const submit = async () => { get('#bundled-config').dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })); await flush(); };
const posts = () => request.mock.calls.filter(([, body]) => body);
async function mount(key = 'ping', locked = ref(false)) {
  const router = createManagementRouter(true); await router.push({ name: 'bundled-plugins', query: key ? { instance_id: `dev.bkm.${key}/default` } : {} }); await router.isReady();
  const root = document.createElement('div'); document.body.append(root);
  app = createApp({ components: { BundledPluginsPage }, setup: () => ({ request, dirty, busy, saved, locked }), template: '<BundledPluginsPage :request="request" :locked="locked" @dirty="dirty" @busy="busy" @saved="saved" />' }).use(router); app.mount(root); await flush(); return router;
}
beforeEach(() => {
  server = bundledFixture(); request.mockReset(); dirty.mockReset(); busy.mockReset(); saved.mockReset();
  request.mockImplementation(async (_path, body) => {
    if (body) { const p = server.plugins.find(p => p.key === body.name)!; if (body.config) p.config = body.config; if (body.enabled !== undefined) p.enabled = body.enabled; p.pending = true; p.restart_required = true; server.revision = 'b'.repeat(64); }
    return structuredClone(server);
  });
  vi.spyOn(window, 'confirm').mockReturnValue(false);
});
afterEach(() => { app?.unmount(); app = undefined; document.body.replaceChildren(); vi.restoreAllMocks(); });

it('目录包含未启用 admin，列表与详情互斥，查询实例可直达', async () => {
  const router = await mount(''); expect(document.querySelectorAll('section.card')).toHaveLength(6);
  expect(document.body.textContent).toContain('admin'); expect(document.querySelector('#bundled-config')).toBeNull();
  await router.push({ name: 'bundled-plugins', query: { instance_id: 'dev.bkm.admin/default' } }); await flush();
  expect(document.querySelectorAll('section.card')).toHaveLength(0); expect(document.body.textContent).toContain('未加载'); expect(request).toHaveBeenCalledTimes(1);
  expect(document.activeElement?.textContent).toBe('admin');
  await router.push({ name: 'bundled-plugins' }); await flush(); expect((document.activeElement as HTMLElement).dataset.bundledInstance).toBe('dev.bkm.admin/default');
});

it('启停取消不写入，确认说明重启及保留数据，保存不伪造当前运行态', async () => {
  await mount(); await click('#toggle-bundled'); expect(posts()).toHaveLength(0);
  vi.mocked(window.confirm).mockReturnValue(true); await click('#toggle-bundled');
  expect(posts()).toHaveLength(1); expect(posts()[0]![1]).toEqual({ name: 'ping', expected_revision: 'a'.repeat(64), confirm: true, enabled: false });
  expect(window.confirm).toHaveBeenLastCalledWith(expect.stringMatching(/重启 Bot.*私有数据保留/));
  expect(document.body.textContent).toContain('已停用'); expect(document.body.textContent).toContain('运行中'); expect(document.body.textContent).toContain('设置待重启生效');
  expect(document.querySelector('#apply')).toBeNull(); expect(saved).toHaveBeenCalledOnce();
});

it('配置保存复用 schema 并保留换行，启停不能抹去草稿', async () => {
  await mount(); await edit('新值\n下一行'); expect(get<HTMLButtonElement>('#toggle-bundled').disabled).toBe(true);
  await submit(); expect(posts()).toHaveLength(0); expect(get<HTMLTextAreaElement>('#field-0').value).toBe('新值\n下一行');
  vi.mocked(window.confirm).mockReturnValue(true); await submit(); expect(posts()[0]![1].config).toEqual({ greeting: '新值\n下一行' });
  expect(dirty).toHaveBeenLastCalledWith(false); expect(server.plugins[0]!.runtime_state).toBe('ready');
});

it('admin owners 422 保留草稿并可修正，所有确认包括全局权限影响', async () => {
  server.plugins[5]!.config = { owners: [] }; server.plugins[5]!.schema = { type: 'object', properties: { owners: { type: 'array' } } };
  await mount('admin'); await edit('{"owners":["draft"]}', '#config-json'); vi.mocked(window.confirm).mockReturnValue(true);
  request.mockRejectedValueOnce(new ApiError('owners 无效', 422)); await submit();
  expect(get<HTMLTextAreaElement>('#config-json').value).toContain('draft'); expect(get<HTMLButtonElement>('#save-bundled').disabled).toBe(false);
  expect(window.confirm).toHaveBeenLastCalledWith(expect.stringMatching(/owner\/admin.*下次重启|下次重启.*owner\/admin/));
  await edit('{"owners":["qq:owner"]}', '#config-json'); await submit();
  await click('#toggle-bundled'); expect(posts().at(-1)![1].enabled).toBe(true);
  expect(document.body.textContent).toContain('未加载'); expect(document.body.textContent).toContain('未注册');
});

it.each([new ApiError('冲突', 409), new ApiError('结果未知'), new ApiError('请求超时', 408), new ApiError('服务错误', 503), new Error('无效响应')])('失败 %s 锁定重复写入，显式读前后都保留草稿', async error => {
  await mount(); await edit('保留我'); vi.mocked(window.confirm).mockReturnValue(true); request.mockRejectedValueOnce(error); await submit();
  expect(get<HTMLButtonElement>('#save-bundled').disabled).toBe(true); await submit(); expect(posts()).toHaveLength(1);
  expect(get<HTMLTextAreaElement>('#field-0').value).toBe('保留我');
  server.plugins[0]!.config = { greeting: '远端更新' }; server.revision = 'c'.repeat(64);
  await click('#refresh-bundled'); expect(get<HTMLTextAreaElement>('#field-0').value).toBe('保留我'); expect(get<HTMLButtonElement>('#save-bundled').disabled).toBe(false);
  await submit(); expect(posts().at(-1)![1].expected_revision).toBe('c'.repeat(64));
});

it('刷新取消不读取，刷新失败不丢草稿；确认并成功后才替换编辑器', async () => {
  await mount(); await edit('草稿'); await click('#refresh-bundled'); expect(request).toHaveBeenCalledTimes(1);
  vi.mocked(window.confirm).mockReturnValue(true); request.mockRejectedValueOnce(new Error('读取失败')); await click('#refresh-bundled');
  expect(get<HTMLTextAreaElement>('#field-0').value).toBe('草稿'); await click('#refresh-bundled'); expect(get<HTMLTextAreaElement>('#field-0').value).toBe('原值');
});

it('写入期间重复 submit 不 POST，卸载中止请求且迟到结果不发成功事件', async () => {
  await mount(); await edit('草稿'); vi.mocked(window.confirm).mockReturnValue(true);
  let finish!: (value: unknown) => void; request.mockImplementationOnce(() => new Promise(resolve => { finish = resolve; }));
  await submit(); await submit(); expect(posts()).toHaveLength(1); expect(busy).toHaveBeenLastCalledWith(true);
  const signal = posts()[0]![2] as AbortSignal; app!.unmount(); app = undefined; expect(signal.aborted).toBe(true);
  finish(structuredClone(server)); await flush(); expect(saved).not.toHaveBeenCalled(); expect(busy).toHaveBeenLastCalledWith(false);
});

it('读取在锁解除后启动，卸载清理读取，迟到错误不产生通知', async () => {
  const locked = ref(true); await mount('ping', locked); expect(request).not.toHaveBeenCalled();
  let reject!: (reason: Error) => void; request.mockImplementationOnce(() => new Promise((_resolve, fail) => { reject = fail; }));
  locked.value = false; await nextTick(); const signal = request.mock.calls[0]![2] as AbortSignal;
  app!.unmount(); app = undefined; expect(signal.aborted).toBe(true); reject(new Error('迟到')); await flush(); expect(document.body.textContent).not.toContain('迟到');
});

it('整对象 JSON 拒绝不安全数字，未确认不发送', async () => {
  server.plugins[0]!.schema = null; await mount(); await edit('{"n":9007199254740993}', '#config-json'); await submit();
  expect(posts()).toHaveLength(0); expect(document.body.textContent).toContain('安全整数范围'); expect(window.confirm).not.toHaveBeenCalled();
});

it.each([
  (v: any) => { v.revision = 'wrong'; }, (v: any) => { v.plugins.push(v.plugins[0]); },
  (v: any) => { v.plugins[1].instance_id = v.plugins[0].instance_id; }, (v: any) => { delete v.plugins[0].loaded; },
  (v: any) => { v.plugins[0].enabled = 1; }, (v: any) => { v.plugins[0].runtime_state = 'toString'; },
  (v: any) => { v.plugins[0].config = []; }, (v: any) => { v.plugins[0].config = { n: Number.MAX_SAFE_INTEGER + 1 }; },
  (v: any) => { v.plugins[0].schema = true; }, (v: any) => { v.plugins[0].requested_permissions = [true]; },
])('目录解析拒绝无效结构、重复标识及不安全数字 %s', mutate => {
  const value = structuredClone(server); mutate(value); expect(() => readBundledCatalogue(value)).toThrow();
});

it('诊断插件只展示后端提供目录，合法嵌套 JSON 可读', () => {
  server.plugins.push({ ...server.plugins[0]!, key: 'diagnostic', instance_id: 'dev.bkm.probe/default', diagnostic: true, config: { items: [null, true, { number: 3 }] } });
  expect(readBundledCatalogue(server).plugins).toHaveLength(7);
});
