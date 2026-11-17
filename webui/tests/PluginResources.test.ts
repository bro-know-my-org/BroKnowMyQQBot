import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { createApp, h, nextTick, reactive } from 'vue';
import PluginResources from '../src/components/PluginResources.vue';
import { ApiError, createApi, type Request } from '../src/api';
const resource = () => ({ id: 'repositories', title: '仓库别名', schema: { type: 'object', properties: { alias: { type: 'string', title: '别名' } }, required: ['alias'], additionalProperties: false }, columns: [{ key: 'alias', title: '别名' }], operations: ['list', 'create', 'update', 'delete'] });
const management = () => ({ instance_id: 'sample/a', plugin_updated_at_ms: 1, revision: '1', descriptor: { schema_version: 1, resources: [resource()] }, editable: true, unavailable_reason: null });
const page = (revision = '1') => ({ instance_id: 'sample/a', plugin_updated_at_ms: 1, revision, data: { records: [{ id: 'A', values: { alias: 'A' } }], next_cursor: null as string | null } });
let app: ReturnType<typeof createApp>, meta = management(), records = page();
const props = reactive({ instanceId: 'sample/a', updatedAt: 1, revision: '1', bootId: 'boot-a', busy: false });
const request = vi.fn<Request>(), dirty = vi.fn(), busy = vi.fn();
const flush = async () => { await new Promise<void>(resolve => setImmediate(resolve)); await nextTick(); };
const get = <T extends HTMLElement = HTMLButtonElement>(selector: string) => document.querySelector<T>(selector)!;
const button = (name: string) => `[data-resource="${name}"]`;
async function click(name: string) { get(button(name)).click(); await flush(); }
async function fill(value: string) { const input = get<HTMLTextAreaElement>('#resource-field-0'); input.value = value; input.dispatchEvent(new Event('input', { bubbles: true })); await nextTick(); }
async function mount(custom: Request = request as Request) {
  const root = document.createElement('div'); document.body.append(root);
  app = createApp({ render: () => h(PluginResources, { ...props, request: custom, onDirty: dirty, onBusy: busy }) }); app.mount(root); await flush();
}
beforeEach(() => {
  vi.clearAllMocks(); meta = management(); records = page();
  Object.assign(props, { instanceId: 'sample/a', updatedAt: 1, revision: '1', bootId: 'boot-a', busy: false });
  request.mockImplementation(async (path, body) => {
    if (body) { records = page(String(Number(records.revision) + 1)); meta.revision = records.revision; return { ...records, data: { records: [], next_cursor: null } } as never; }
    return (path.includes('/resource?') ? records : meta) as never;
  });
  vi.spyOn(window, 'confirm').mockReturnValue(true);
});
afterEach(() => { app?.unmount(); document.body.replaceChildren(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });
it('CRUD uses both CAS tokens, confirms deletes once and rereads after empty write responses', async () => {
  await mount(); expect(document.body.textContent).toContain('仓库别名');
  await click('create'); await fill('B'); await click('save');
  expect(request.mock.calls.find(c => c[1])?.slice(0, 2)).toEqual(['/api/plugins/management/resource', { instance_id: 'sample/a', resource: 'repositories', operation: 'create', record_id: null, input: { alias: 'B' }, expected_revision: '1', expected_updated_at_ms: 1, confirm: false }]);
  expect(dirty).toHaveBeenLastCalledWith(false); expect(busy).toHaveBeenLastCalledWith(false);
  await click('edit'); await fill('renamed'); await click('save');
  expect(request.mock.calls.filter(c => c[1]).at(-1)![1]).toMatchObject({ operation: 'update', record_id: 'A', input: { alias: 'renamed' }, expected_revision: '2' });
  vi.mocked(window.confirm).mockReturnValue(false); await click('delete'); expect(request.mock.calls.filter(c => c[1])).toHaveLength(2);
  vi.mocked(window.confirm).mockReturnValue(true); await click('delete');
  expect(request.mock.calls.filter(c => c[1]).at(-1)![1]).toMatchObject({ operation: 'delete', record_id: 'A', input: null, confirm: true, expected_revision: '3' });
  expect(window.confirm).toHaveBeenCalledTimes(2);
});
it('search and paging use encoded, bounded query and never write', async () => {
  records.data.next_cursor = 'page/2&x'; await mount(); await click('next');
  expect(request.mock.calls.at(-1)![0]).toContain('cursor=page%2F2%26x'); expect(get(button('next')).hasAttribute('disabled')).toBe(true);
  await click('previous'); const input = get<HTMLInputElement>(button('search')); input.value = '仓库&别名'; input.dispatchEvent(new Event('input', { bubbles: true }));
  input.form!.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })); await flush();
  expect(request.mock.calls.at(-1)![0]).toContain('search=%E4%BB%93%E5%BA%93%26%E5%88%AB%E5%90%8D');
  expect(request.mock.calls.at(-1)![0]).not.toContain('cursor='); expect(request.mock.calls.every(c => c[1] === undefined)).toBe(true);
});
it.each([new ApiError('stale', 409), new ApiError('lost'), new Error('unknown')])('preserves draft and blocks replay after %s until explicit reread', async failure => {
  await mount(); await click('edit'); await fill('draft'); request.mockRejectedValueOnce(failure); await click('save');
  expect(get<HTMLTextAreaElement>('#resource-field-0').value).toBe('draft'); expect(get(button('save')).hasAttribute('disabled')).toBe(true);
  const calls = request.mock.calls.length; props.revision = '2'; await flush(); await click('save'); expect(request).toHaveBeenCalledTimes(calls);
  meta.revision = '2'; records.revision = '2'; vi.mocked(window.confirm).mockReturnValue(false); await click('reload'); expect(request).toHaveBeenCalledTimes(calls);
  vi.mocked(window.confirm).mockReturnValue(true); await click('reload'); expect(document.querySelector('#resource-field-0')).toBeNull(); await click('edit'); expect(get(button('save')).hasAttribute('disabled')).toBe(false);
});
it('business validation preserves editable draft and hides guest diagnostics', async () => {
  await mount(); await click('create'); await fill('invalid'); request.mockRejectedValueOnce(new ApiError('sensitive guest trace', 422)); await click('save');
  expect(document.body.textContent).not.toContain('sensitive'); expect(get<HTMLTextAreaElement>('#resource-field-0').value).toBe('invalid'); expect(get(button('save')).hasAttribute('disabled')).toBe(false);
});
it('SSE clean changes reread silently; dirty changes preserve schema, draft and conflict', async () => {
  await mount(); const before = request.mock.calls.length; meta.revision = '2'; records.revision = '2'; props.revision = '2'; await flush();
  expect(request.mock.calls.length).toBe(before + 2); expect(busy).not.toHaveBeenCalled();
  await click('edit'); await fill('draft'); const field = get('#resource-field-0');
  meta = { ...management(), plugin_updated_at_ms: 2, revision: '3' }; props.updatedAt = 2; props.revision = '3'; await flush();
  expect(get('#resource-field-0')).toBe(field); expect(get<HTMLTextAreaElement>('#resource-field-0').value).toBe('draft'); expect(document.body.textContent).toContain('数据或插件设置已变化'); expect(request.mock.calls.length).toBe(before + 2);
});
it('missing descriptor hides the section and does not list or mutate', async () => {
  request.mockResolvedValue({ ...management(), descriptor: null }); await mount(); expect(document.querySelector('[aria-label="插件资源"]')).toBeNull(); expect(request).toHaveBeenCalledOnce();
});
it('unknown successful response cannot clear draft or replay', async () => {
  await mount(); await click('edit'); await fill('draft'); request.mockResolvedValueOnce({ unexpected: true }); await click('save');
  expect(document.body.textContent).toContain('写入结果未知'); expect(get<HTMLTextAreaElement>('#resource-field-0').value).toBe('draft'); const count = request.mock.calls.length; await click('save'); expect(request).toHaveBeenCalledTimes(count);
});
it('unmount aborts pending requests and ignores late responses and dirty emissions', async () => {
  let finish!: (value: unknown) => void; request.mockImplementationOnce(() => new Promise(resolve => { finish = resolve; })); await mount();
  const signal = request.mock.calls[0]![2]!; app.unmount(); expect(signal.aborted).toBe(true); const count = dirty.mock.calls.length; finish(management()); await flush(); expect(dirty).toHaveBeenCalledTimes(count); expect(request).toHaveBeenCalledOnce();
});
it('instance switch cancels old write, clears its draft and rejects late results', async () => {
  await mount(); await click('edit'); await fill('old'); let finish!: (value: unknown) => void;
  request.mockImplementationOnce(() => new Promise(resolve => { finish = resolve; })); await click('save'); const signal = request.mock.calls.at(-1)![2]!;
  request.mockResolvedValue({ ...management(), instance_id: 'other/b', descriptor: null }); props.instanceId = 'other/b'; await flush();
  expect(signal.aborted).toBe(true); expect(document.querySelector('#resource-field-0')).toBeNull(); finish(page('2')); await flush(); expect(document.querySelector('[aria-label="插件资源"]')).toBeNull(); expect(busy).toHaveBeenLastCalledWith(false);
});
it('HTTP mock uses Cookie fetch and malformed or lost write responses are never retried', async () => {
  const api = createApi(vi.fn()), fetcher = vi.fn(); vi.stubGlobal('fetch', fetcher);
  fetcher.mockImplementation(async (_path, init: RequestInit) => {
    if (init.method === 'POST') throw new TypeError('connection lost');
    return new Response(JSON.stringify(String(_path).includes('/resource?') ? records : meta));
  });
  await mount(api.request); await click('edit'); await fill('keep'); await click('save');
  expect(fetcher.mock.calls.filter(c => c[1].method === 'POST')).toHaveLength(1); expect(fetcher.mock.calls[0]![1]).toMatchObject({ credentials: 'same-origin', redirect: 'error' });
  expect(document.body.textContent).toContain('不会自动重复写入'); await click('save'); expect(fetcher.mock.calls.filter(c => c[1].method === 'POST')).toHaveLength(1); api.clear();
});

it('empty opaque cursors remain usable and back/forward paging only rejects ancestor cycles', async () => {
  records.data.next_cursor = ''; await mount(); expect(get(button('next')).hasAttribute('disabled')).toBe(false);
  await click('next'); expect(request.mock.calls.at(-1)![0]).toContain('cursor='); expect(get(button('next')).hasAttribute('disabled')).toBe(true);
  await click('previous'); expect(get(button('next')).hasAttribute('disabled')).toBe(false); await click('next'); expect(document.body.textContent).toContain('第 2 页');
});
it('malformed HTTP records and unsafe numbers fail closed without a usable write action', async () => {
  const api = createApi(vi.fn()), fetcher = vi.fn(); vi.stubGlobal('fetch', fetcher);
  fetcher.mockImplementation(async (path: string) => new Response(path.includes('/resource?') ? '{"instance_id":"sample/a","plugin_updated_at_ms":1,"revision":"1","data":{"records":[{"id":"A","values":{"n":9007199254740993}}],"next_cursor":null}}' : JSON.stringify(meta)));
  await mount(api.request); expect(document.body.textContent).toContain('安全整数'); expect(document.querySelector(button('create'))).toBeNull(); expect(fetcher.mock.calls.every(c => c[1].method === 'GET')).toBe(true); api.clear();
});
it('unknown delete emits a dirty guard without a form and SSE cannot clear it', async () => {
  await mount(); request.mockRejectedValueOnce(new ApiError('lost')); await click('delete');
  expect(dirty).toHaveBeenLastCalledWith(true); const count = request.mock.calls.length; props.revision = '2'; await flush(); expect(request).toHaveBeenCalledTimes(count);
  expect(get(button('delete')).hasAttribute('disabled')).toBe(true); expect(document.body.textContent).toContain('不会自动重复写入');
});
it.each(['before', 'after'])('own write SSE arriving %s the response never conflicts with the next draft', async order => {
  await mount(); await click('edit'); await fill('saved'); let finish!: (value: unknown) => void;
  request.mockImplementationOnce(() => new Promise(resolve => { finish = resolve; })); await click('save');
  records.revision = '2'; meta.revision = '2'; if (order === 'before') { props.revision = '2'; await flush(); }
  finish(page('2')); await flush(); await click('edit'); await fill('next draft');
  if (order === 'after') { props.revision = '2'; await flush(); }
  expect(document.body.textContent).not.toContain('数据或插件设置已变化'); expect(get(button('save')).hasAttribute('disabled')).toBe(false); expect(get<HTMLTextAreaElement>('#resource-field-0').value).toBe('next draft');
});

it('a newer resource timestamp than the cached parent is a valid CAS baseline, not a conflict', async () => {
  meta.plugin_updated_at_ms = 2; records.plugin_updated_at_ms = 2;
  request.mockImplementation(async (path, body) => {
    if (body) { meta.revision = '2'; records.revision = '2'; }
    return (path.includes('/resource') ? records : meta) as never;
  });
  await mount(); await click('edit'); await fill('saved'); await click('save');
  expect(request.mock.calls.find(c => c[1])![1]).toMatchObject({ expected_updated_at_ms: 2 });
  await click('edit'); await fill('next'); props.revision = '2'; await flush();
  expect(get(button('save')).hasAttribute('disabled')).toBe(false); expect(document.body.textContent).not.toContain('数据或插件设置已变化');
  props.updatedAt = 2; await flush(); expect(get(button('save')).hasAttribute('disabled')).toBe(false);
});

it('unavailable readonly descriptors show the host reason without trying the blocked guest list', async () => {
  meta.editable = false; request.mockResolvedValue({ ...meta, unavailable_reason: '状态版本尚未加载，需要重启 Bot。' }); await mount();
  expect(document.body.textContent).toContain('状态版本尚未加载，需要重启 Bot'); expect(document.body.textContent).not.toContain('暂无资源记录');
  expect(request).toHaveBeenCalledOnce(); expect(get(button('create')).hasAttribute('disabled')).toBe(true); expect(document.querySelector(button('edit'))).toBeNull();
});
