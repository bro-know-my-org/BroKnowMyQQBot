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
