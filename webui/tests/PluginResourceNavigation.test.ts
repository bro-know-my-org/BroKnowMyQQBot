import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { createApp, nextTick } from 'vue';
import App from '../src/App.vue';
import { createManagementRouter, pluginRoute } from '../src/navigation';
import type { PluginView } from '../src/types';
import { runtimeFixture } from './runtimeFixture';
const BOOT = '11111111-1111-4111-8111-111111111111';
class Source extends EventTarget {
  static all: Source[] = []; close = vi.fn();
  constructor(public url: string) { super(); Source.all.push(this); }
}
let app: ReturnType<typeof createApp>, plugin: PluginView, revision: string, sequence: number;
const response = (value: unknown, status = 200) => new Response(JSON.stringify(value), { status });
const schema = { type: 'object', properties: { alias: { type: 'string', title: '别名' } }, required: ['alias'] };
const resource = { id: 'repositories', title: '仓库别名', schema, columns: [{ key: 'alias', title: '别名' }], operations: ['list', 'create', 'update', 'delete'] };
const fetcher = vi.fn(async (path: string, init: RequestInit) => {
  if (path === '/api/session') return response({ mode: 'token', authenticated: true, boot_id: BOOT });
  if (path === '/api/runtime') return response({ ...runtimeFixture(), boot_id: BOOT });
  if (path === '/api/plugins') return response({ plugins: [plugin] });
  if (path === '/api/plugins/config') { plugin = { ...plugin, config: JSON.parse(String(init.body)).config, updated_at_ms: plugin.updated_at_ms + 1 }; return response({ plugin }); }
  if (path.startsWith('/api/plugins/management')) {
    if (init.method === 'POST') revision = String(Number(revision) + 1);
    return response({ instance_id: plugin.instance_id, plugin_updated_at_ms: plugin.updated_at_ms, revision,
      ...(path.includes('/resource') ? { data: { records: [{ id: 'A', values: { alias: 'A' } }], next_cursor: null } }
        : { descriptor: { schema_version: 1, resources: [resource] }, editable: true, unavailable_reason: null }) });
  }
  return response({ entries: [], next_before: null });
});
const flush = async () => { await new Promise<void>(resolve => setImmediate(resolve)); await nextTick(); };
const get = <T extends HTMLElement = HTMLButtonElement>(selector: string) => document.querySelector<T>(selector)!;
const click = async (selector: string) => { get(selector).click(); await flush(); };
const edit = async (selector: string, value: string) => { const input = get<HTMLTextAreaElement>(selector); input.value = value; input.dispatchEvent(new Event('input', { bubbles: true })); await nextTick(); };
function push() {
  Source.all[0]!.dispatchEvent(new MessageEvent('status', { data: JSON.stringify({ boot_id: BOOT, revision: String(++sequence), runtime: { ...runtimeFixture(), boot_id: BOOT }, plugins: [plugin], plugins_truncated: false, settings_restart_required: null, plugin_data_revisions: { [plugin.instance_id]: revision } }) }));
}
async function mount() {
  const router = createManagementRouter(true); await router.push(pluginRoute(plugin.instance_id, 'config')); await router.isReady();
  const root = document.createElement('div'); document.body.append(root); app = createApp(App).use(router); app.mount(root); await flush(); await flush(); return router;
}
beforeEach(() => {
  vi.clearAllMocks(); Source.all = []; revision = '1'; sequence = 0;
  plugin = { instance_id: 'sample/a', plugin_id: 'dev.bkm.sample', name: '示例', description: '', version: '1.0.0', saved_version: '1.0.0', loaded_version: '1.0.0', restart_required: false, source: 'local', trust_level: 'local-wasm', signature_status: 'unsigned', editable: true, enabled: true, runtime_state: 'ready', updated_at_ms: 1, pending: false, error: null, requested_permissions: [], granted_permissions: [], config: { alias: '配置原值' }, schema };
  vi.stubGlobal('fetch', fetcher); vi.stubGlobal('EventSource', Source); vi.spyOn(window, 'confirm').mockReturnValue(false);
});
afterEach(() => { app?.unmount(); document.body.replaceChildren(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });
it('resource draft guards navigation, tab, unload, logout and process controls through the existing root', async () => {
  const router = await mount(); await click('[data-resource="edit"]'); await edit('#resource-field-0', '资源草稿');
  expect(get<HTMLButtonElement>('#save').disabled).toBe(true); expect(get<HTMLButtonElement>('#restart-bot').disabled).toBe(true); expect(get<HTMLButtonElement>('#stop-bot').disabled).toBe(true);
  const unload = new Event('beforeunload', { cancelable: true }); window.dispatchEvent(unload); expect(unload.defaultPrevented).toBe(true);
  await router.push(pluginRoute(plugin.instance_id, 'overview')); await flush(); expect(router.currentRoute.value.name).toBe('plugin-config');
  await router.push('/plugins'); await flush(); expect(router.currentRoute.value.name).toBe('plugin-config'); await click('#logout'); expect(document.querySelector('#login-form')).toBeNull();
  expect(Source.all).toHaveLength(1); expect(Source.all[0]!.url).toBe('/api/status/stream'); expect(fetcher.mock.calls.some(c => c[1].method === 'POST')).toBe(false);
});
it('saving resource preserves independent config draft and its navigation guard', async () => {
  const router = await mount(); await edit('#field-0', '配置草稿'); const configField = get('#field-0');
  await click('[data-resource="edit"]'); await edit('#resource-field-0', '资源草稿'); await click('[data-resource="save"]');
  expect(get('#field-0')).toBe(configField); expect(get<HTMLTextAreaElement>('#field-0').value).toBe('配置草稿'); expect(get<HTMLButtonElement>('#save').disabled).toBe(false);
  await router.push('/plugins'); await flush(); expect(router.currentRoute.value.name).toBe('plugin-config');
  expect(get<HTMLButtonElement>('#restart-bot').disabled).toBe(true); expect(fetcher.mock.calls.filter(c => c[1].method === 'POST')).toHaveLength(1);
});
it('config save preserves resource draft and dirty guard, updated timestamp blocks its stale save', async () => {
  const router = await mount(); await click('[data-resource="edit"]'); await edit('#resource-field-0', '资源草稿'); const resourceField = get('#resource-field-0'); await edit('#field-0', '新配置');
  await click('#save'); expect(get('#resource-field-0')).toBe(resourceField); expect(get<HTMLTextAreaElement>('#resource-field-0').value).toBe('资源草稿');
  expect(get<HTMLButtonElement>('[data-resource="save"]').disabled).toBe(true); expect(get<HTMLButtonElement>('#restart-bot').disabled).toBe(true);
  await router.push('/plugins'); await flush(); expect(router.currentRoute.value.name).toBe('plugin-config');
  expect(document.body.textContent).toContain('数据或插件设置已变化'); expect(fetcher.mock.calls.filter(c => c[1].method === 'POST')).toHaveLength(1);
});
it('shared SSE refreshes clean resources without resetting config draft, then protects dirty resource on generation and updated_at changes', async () => {
  await mount(); await edit('#field-0', '配置草稿'); const configField = get('#field-0');
  push(); await flush(); const before = fetcher.mock.calls.length; revision = '2'; push(); await flush();
  expect(fetcher.mock.calls.length).toBe(before + 2); expect(get('#field-0')).toBe(configField); expect(get<HTMLTextAreaElement>('#field-0').value).toBe('配置草稿');
  await click('[data-resource="edit"]'); await edit('#resource-field-0', '资源草稿'); const resourceField = get('#resource-field-0');
  plugin = { ...plugin, updated_at_ms: 2, config: { alias: '远端配置' } }; revision = '3'; push(); await flush();
  expect(get('#resource-field-0')).toBe(resourceField); expect(get('#field-0')).toBe(configField); expect(get<HTMLTextAreaElement>('#field-0').value).toBe('配置草稿'); expect(get<HTMLTextAreaElement>('#resource-field-0').value).toBe('资源草稿'); expect(Source.all).toHaveLength(1);
  vi.mocked(window.confirm).mockReturnValue(true); await click('[data-resource="reload"]'); expect(get<HTMLTextAreaElement>('#field-0').value).toBe('配置草稿'); expect(get<HTMLButtonElement>('#restart-bot').disabled).toBe(true);
});
it('in-flight resource write blocks other mutations and navigation, session expiry cancels and clears the editor', async () => {
  const router = await mount(); await edit('#field-0', '配置草稿'); await click('[data-resource="edit"]');
  let finish!: (value: Response) => void; fetcher.mockImplementationOnce(() => new Promise(resolve => { finish = resolve; })); await click('[data-resource="save"]');
  expect(get<HTMLButtonElement>('#save').disabled).toBe(true); expect(get<HTMLButtonElement>('#logout').disabled).toBe(true); expect(get<HTMLButtonElement>('#restart-bot').disabled).toBe(true);
  vi.mocked(window.confirm).mockReturnValue(true); await router.push('/plugins'); await flush(); expect(router.currentRoute.value.name).toBe('plugin-config');
  finish(response({}, 401)); await flush(); expect(document.querySelector('#login-form')).not.toBeNull(); expect(document.querySelector('#resource-field-0')).toBeNull(); expect(Source.all[0]!.close).toHaveBeenCalled();
});
