import { afterEach, expect, it, vi } from 'vitest';
import { createApp, nextTick } from 'vue';
import App from '../src/App.vue';
import { createManagementRouter } from '../src/navigation';
import { bundledFixture } from './bundledFixture';
import { runtimeFixture } from './runtimeFixture';

let app: ReturnType<typeof createApp> | undefined;
const flush = async () => { await new Promise<void>(resolve => setImmediate(resolve)); await nextTick(); };
const get = <T extends HTMLElement = HTMLElement>(selector: string) => document.querySelector<T>(selector)!;
const click = async (selector: string) => { get<HTMLButtonElement>(selector).click(); await flush(); };
const edit = async () => { get<HTMLTextAreaElement>('#field-0').value = '草稿'; get('#field-0').dispatchEvent(new Event('input', { bubbles: true })); await nextTick(); };
const boot_id = '11111111-1111-4111-8111-111111111111';
async function mount(fetcher: (path: string, init: RequestInit) => Promise<Response>, path = '/plugins/bundled?instance_id=dev.bkm.ping%2Fdefault') {
  vi.stubGlobal('fetch', vi.fn(fetcher));
  const router = createManagementRouter(true); await router.push(path); await router.isReady();
  const root = document.createElement('div'); document.body.append(root); app = createApp(App).use(router); app.mount(root); await flush(); await flush(); return router;
}
const response = (data: unknown, status = 200) => new Response(JSON.stringify(data), { status });
afterEach(() => { app?.unmount(); app = undefined; document.body.replaceChildren(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

it('内置目录路由优先于实例详情；草稿保护 query、后退、导航、退出和进程操作', async () => {
  const confirm = vi.spyOn(window, 'confirm').mockReturnValue(false), server = bundledFixture();
  const router = await mount(async path => response(path === '/api/session' ? { mode: 'token', authenticated: true, boot_id }
    : path === '/api/plugins/bundled' ? server : path === '/api/runtime' ? runtimeFixture() : { plugins: [] }));
  expect(router.currentRoute.value.name).toBe('bundled-plugins'); expect(get('#bundled-config')).not.toBeNull();
  await edit(); expect(get<HTMLButtonElement>('#restart-bot').disabled).toBe(true); expect(get<HTMLButtonElement>('#stop-bot').disabled).toBe(true);
  const unload = new Event('beforeunload', { cancelable: true }); window.dispatchEvent(unload); expect(unload.defaultPrevented).toBe(true);
  await router.push('/plugins/bundled?instance_id=dev.bkm.admin%2Fdefault'); await flush(); expect(router.currentRoute.value.query.instance_id).toBe('dev.bkm.ping/default');
  await router.push('/plugins'); await flush(); expect(router.currentRoute.value.name).toBe('bundled-plugins');
  await click('#logout'); expect(document.querySelector('#login-form')).toBeNull();
  // Unrelated query changes keep the same editor and draft without prompting.
  const count = confirm.mock.calls.length; await router.push({ query: { ...router.currentRoute.value.query, view: 'compact' } }); await flush();
  expect(confirm).toHaveBeenCalledTimes(count); expect(get<HTMLTextAreaElement>('#field-0').value).toBe('草稿');
  confirm.mockReturnValue(true); await router.push('/plugins/bundled?instance_id=dev.bkm.admin%2Fdefault'); await flush();
  expect(get<HTMLTextAreaElement>('#field-0').value).toBe('原值'); expect(get<HTMLButtonElement>('#restart-bot').disabled).toBe(false);
  await edit(); confirm.mockReturnValue(false); router.back(); await flush(); await flush();
  expect(router.currentRoute.value.query.instance_id).toBe('dev.bkm.admin/default'); expect(get<HTMLTextAreaElement>('#field-0').value).toBe('草稿');
  confirm.mockReturnValue(true);
  await router.push('/plugins'); await flush(); expect(document.querySelector('a[href$="/plugins/bundled"]')).not.toBeNull();
  expect(document.querySelector('.sidebar-nav a[href$="/plugins/bundled"]')).toBeNull();
});

it('保存 busy 阻止路由与退出，401 清除会话并中止页面请求，错误进入通知中心', async () => {
  vi.spyOn(window, 'confirm').mockReturnValue(true); let finish!: (value: Response) => void;
  const server = bundledFixture(), signals: AbortSignal[] = [];
  const router = await mount(async (path, init) => {
    if (path === '/api/plugins/bundled' && init.body) { signals.push(init.signal as AbortSignal); return new Promise(resolve => { finish = resolve; }); }
    return response(path === '/api/session' ? { mode: 'token', authenticated: true, boot_id } : path === '/api/plugins/bundled' ? server : { plugins: [] });
  });
  await edit(); get('#bundled-config').dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })); await flush();
  await router.push('/plugins'); await flush(); expect(router.currentRoute.value.name).toBe('bundled-plugins');
  expect(get<HTMLButtonElement>('#logout').disabled).toBe(true); expect(get<HTMLButtonElement>('#restart-bot').disabled).toBe(true);
  finish(response({ error: 'owners 校验错误' }, 422)); await flush(); expect(document.body.textContent).toContain('owners 校验错误');
  expect(document.querySelector('[aria-label="内置插件详情"] [role="alert"]')).toBeNull(); expect(get<HTMLTextAreaElement>('#field-0').value).toBe('草稿');
  get('#bundled-config').dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })); await flush();
  finish(response({}, 401)); await flush(); expect(document.querySelector('#login-form')).not.toBeNull();
  expect(document.querySelector('#bundled-config')).toBeNull(); expect(signals.at(-1)!.aborted).toBe(true);
  expect(document.body.textContent).not.toContain('设置已保存');
});

it('retrying an installed-plugin list failure does not clear the bundled editor draft guard', async () => {
  vi.spyOn(window, 'confirm').mockReturnValue(false); let attempts = 0;
  const router = await mount(async path => path === '/api/plugins'
    ? response({ plugins: [] }, attempts++ === 0 ? 503 : 200)
    : response(path === '/api/session' ? { mode: 'token', authenticated: true, boot_id }
      : path === '/api/plugins/bundled' ? bundledFixture() : runtimeFixture()));
  await edit(); expect(get('#retry-plugins')).not.toBeNull();
  await click('#retry-plugins'); await flush();
  expect(document.querySelector('#retry-plugins')).toBeNull();
  expect(get<HTMLTextAreaElement>('#field-0').value).toBe('草稿'); expect(get<HTMLButtonElement>('#restart-bot').disabled).toBe(true);
  const unload = new Event('beforeunload', { cancelable: true }); window.dispatchEvent(unload); expect(unload.defaultPrevented).toBe(true);
  await router.push('/plugins'); await flush(); expect(router.currentRoute.value.name).toBe('bundled-plugins');
  expect(window.confirm).toHaveBeenCalled();
});
