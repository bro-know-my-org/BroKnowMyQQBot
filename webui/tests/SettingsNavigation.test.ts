import { afterEach, expect, it, vi } from 'vitest';
import { createApp, nextTick } from 'vue';
import App from '../src/App.vue';
import { createManagementRouter } from '../src/navigation';
import { systemSettingsFixture } from './systemSettingsFixture';
import { runtimeFixture } from './runtimeFixture';

let app: ReturnType<typeof createApp> | undefined;
const flush = async () => { await new Promise<void>(resolve => setImmediate(resolve)); await nextTick(); };
const get = <T extends HTMLElement = HTMLElement>(selector: string) => document.querySelector<T>(selector)!;
const click = async (selector: string) => { get<HTMLButtonElement>(selector).click(); await flush(); };
const edit = async (selector: string, value: string) => { get<HTMLInputElement>(selector).value = value; get(selector).dispatchEvent(new Event('input', { bubbles: true })); await nextTick(); };
afterEach(() => { app?.unmount(); app = undefined; document.body.replaceChildren(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

it('连接草稿与忙碌状态保护路由和卸载，动态错误进入通知中心', async () => {
  const confirm = vi.spyOn(window, 'confirm').mockReturnValue(false);
  let finish!: (response: Response) => void;
  const fetcher = vi.fn(async (path: string, init: RequestInit) => {
    if (path === '/api/settings/system' && init.body) return new Promise<Response>(resolve => { finish = resolve; });
    const data = path === '/api/session' ? { mode: 'none', authenticated: true }
      : path === '/api/plugins' ? { plugins: [] } : path === '/api/runtime' ? runtimeFixture()
      : path === '/api/settings/commands' ? { prefix: '/', revision: 'a'.repeat(64), pending: false }
      : systemSettingsFixture();
    return new Response(JSON.stringify(data));
  });
  vi.stubGlobal('fetch', fetcher);
  const router = createManagementRouter(true); await router.push('/adapters'); await router.isReady();
  const root = document.createElement('div'); document.body.append(root); app = createApp(App).use(router); app.mount(root); await flush();
  await click('#edit-connections'); await edit('#connections-webhook-path', '/draft');
  await router.push('/settings'); await flush(); expect(router.currentRoute.value.name).toBe('adapters'); expect(confirm).toHaveBeenCalledTimes(1);
  const unload = new Event('beforeunload', { cancelable: true }); window.dispatchEvent(unload); expect(unload.defaultPrevented).toBe(true);
  get('#connections-form').dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })); await nextTick();
  confirm.mockReturnValue(true); await router.push('/settings'); await flush(); expect(router.currentRoute.value.name).toBe('adapters');
  expect(document.body.textContent).toContain('操作进行中');
  finish(new Response(JSON.stringify({ error: '测试校验错误' }), { status: 422 })); await flush();
  expect(get<HTMLInputElement>('#connections-webhook-path').value).toBe('/draft');
  expect(document.querySelector('.system-settings [role="alert"]')).toBeNull();
  expect(document.body.textContent).toContain('测试校验错误');
  await router.push('/settings'); await flush(); expect(router.currentRoute.value.name).toBe('settings');
  expect(get('#command-prefix')).not.toBeNull();
  const cleanUnload = new Event('beforeunload', { cancelable: true }); window.dispatchEvent(cleanUnload); expect(cleanUnload.defaultPrevented).toBe(false);
});

it('命令与全局草稿独立保护，前缀仍立即生效，共享版本冲突不覆盖全局编辑', async () => {
  vi.spyOn(window, 'confirm').mockReturnValue(false);
  const server = systemSettingsFixture(); let prefix = '/';
  let finishPrefix!: () => void;
  vi.stubGlobal('fetch', vi.fn(async (path: string, init: RequestInit) => {
    const body = init.body ? JSON.parse(String(init.body)) : undefined;
    if (path === '/api/session') return new Response(JSON.stringify({ mode: 'none', authenticated: true }));
    if (path === '/api/plugins') return new Response(JSON.stringify({ plugins: [] }));
    if (body && body.expected_revision !== server.revision) return new Response('{"error":"版本冲突"}', { status: 409 });
    if (path === '/api/settings/commands') {
      if (body) await new Promise<void>(resolve => { finishPrefix = () => { prefix = body.prefix; server.revision = 'b'.repeat(64); resolve(); }; });
      return new Response(JSON.stringify({ prefix, revision: server.revision, saved_prefix: prefix, pending: false }));
    }
    return new Response(JSON.stringify(server));
  }));
  const router = createManagementRouter(true); await router.push('/settings'); await router.isReady();
  const root = document.createElement('div'); document.body.append(root); app = createApp(App).use(router); app.mount(root); await flush();
  await click('#edit-global'); await edit('#global-runtime-event_concurrency', '7'); await edit('#command-prefix', '!!');
  get('.command-settings form').dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })); await nextTick();
  expect(get<HTMLInputElement>('#global-runtime-event_concurrency').matches(':disabled')).toBe(true);
  expect(get<HTMLButtonElement>('#save-global').disabled).toBe(true);
  finishPrefix(); await flush(); expect(document.body.textContent).toContain('前缀已保存并立即生效');
  expect(get<HTMLInputElement>('#global-runtime-event_concurrency').value).toBe('7');
  const unload = new Event('beforeunload', { cancelable: true }); window.dispatchEvent(unload); expect(unload.defaultPrevented).toBe(true);
  await router.push('/adapters'); await flush(); expect(router.currentRoute.value.name).toBe('settings');
  get('#global-form').dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })); await flush();
  expect(get<HTMLButtonElement>('#save-global').disabled).toBe(true);
  expect(get<HTMLTextAreaElement>('#copy-global').value).toContain('"event_concurrency":7');
  expect(get<HTMLInputElement>('#command-prefix').value).toBe('!!'); expect(prefix).toBe('!!');
  expect(server.global.runtime.event_concurrency).toBe(32);
});
