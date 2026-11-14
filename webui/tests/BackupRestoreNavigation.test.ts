import { afterEach, expect, it, vi } from 'vitest';
import { createApp, nextTick } from 'vue';
import App from '../src/App.vue';
import { createManagementRouter } from '../src/navigation';
import { systemSettingsFixture } from './systemSettingsFixture';
import { runtimeFixture } from './runtimeFixture';
import { backupPlan, OLD, NEW, ID } from './backupFixture';
let app: ReturnType<typeof createApp> | undefined;
const response = (data: unknown, status = 200) => new Response(JSON.stringify(data), { status });
const flush = async () => { await new Promise<void>(resolve => setImmediate(resolve)); await nextTick(); };
const get = <T extends HTMLElement = HTMLButtonElement>(selector: string) => document.querySelector<T>(selector)!;
const click = async (selector: string) => { get<HTMLButtonElement>(selector).click(); await flush(); };
async function mount() {
  const router = createManagementRouter(true); await router.push('/settings'); await router.isReady();
  const root = document.createElement('div'); document.body.append(root); app = createApp(App).use(router); app.mount(root); await flush(); await flush(); return router;
}
async function choose() {
  const input = get<HTMLInputElement>('#restore-archive'); Object.defineProperty(input, 'files', { configurable: true, value: [new File(['archive'], 'fixture.bkm-backup')] });
  input.dispatchEvent(new Event('change', { bubbles: true })); await flush();
}
function read(path: string, boot = OLD) {
  if (path === '/api/session') return response({ mode: 'token', authenticated: true, boot_id: boot });
  if (path === '/api/plugins') return response({ plugins: [] });
  if (path === '/api/runtime') return response({ ...runtimeFixture(), boot_id: boot });
  if (path === '/api/backups') return response([]);
  if (path === '/api/settings/commands') return response({ prefix: '/', revision: 'a'.repeat(64), pending: false });
  return response(systemSettingsFixture());
}
afterEach(() => { app?.unmount(); app = undefined; document.body.replaceChildren(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });
it('upload and inspection drafts guard navigation/logout/unload/process controls and settings; cancel permits editing, whose draft blocks inspection', async () => {
  const confirm = vi.spyOn(window, 'confirm').mockReturnValue(false); let finish!: (r: Response) => void;
  const fetcher = vi.fn(async (path: string) => path === '/api/restores/inspect' ? new Promise<Response>(resolve => { finish = resolve; }) : read(path));
  vi.stubGlobal('fetch', fetcher); const router = await mount(); await choose();
  expect(get<HTMLButtonElement>('#restart-bot').disabled).toBe(true); expect(get<HTMLButtonElement>('#stop-bot').disabled).toBe(true);
  expect(get<HTMLButtonElement>('#logout').disabled).toBe(true); await click('#logout'); expect(fetcher.mock.calls.some(([path]) => path === '/api/session/logout')).toBe(false);
  expect(get<HTMLInputElement>('#command-prefix').disabled).toBe(true); expect(get<HTMLButtonElement>('#edit-global').disabled).toBe(true);
  await router.push('/adapters'); await flush(); expect(router.currentRoute.value.name).toBe('settings');
  const unload = new Event('beforeunload', { cancelable: true }); window.dispatchEvent(unload); expect(unload.defaultPrevented).toBe(true);
  finish(response(backupPlan())); await flush(); expect(get<HTMLButtonElement>('#restart-bot').disabled).toBe(true);
  await router.push('/adapters'); expect(confirm).toHaveBeenCalledOnce(); expect(router.currentRoute.value.name).toBe('settings');
  await click('#cancel-restore-plan'); expect(get<HTMLButtonElement>('#restart-bot').disabled).toBe(false);
  const input = get<HTMLInputElement>('#command-prefix'); input.value = '!'; input.dispatchEvent(new Event('input', { bubbles: true })); await nextTick();
  expect(get<HTMLInputElement>('#restore-archive').disabled).toBe(true); expect(get<HTMLButtonElement>('#create-backup').disabled).toBe(true);
  expect(fetcher.mock.calls.filter(([path]) => path.endsWith('/commit'))).toHaveLength(0);
});
it('one final confirmation owns existing restart modal, preserves unknown outcome notification after reconnect, and never resubmits commit', async () => {
  const confirm = vi.spyOn(window, 'confirm').mockReturnValue(true); let boot = OLD, finish!: (r: Response) => void;
  const fetcher = vi.fn(async (path: string, init: RequestInit) => {
    if (path === '/api/restores/inspect') return response(backupPlan());
    if (path.endsWith('/commit')) { expect(JSON.parse(String(init.body))).toEqual({ expected_boot_id: OLD, confirm_sensitive_plugin_data: true, confirm_unsigned_disabled: true }); return new Promise<Response>(resolve => { finish = resolve; }); }
    if (path.endsWith('/outcome')) return response({ error: 'unknown' }, 410);
    return read(path, boot);
  }); vi.stubGlobal('fetch', fetcher); const router = await mount(); await choose(); await click('#confirm-restore');
  expect(confirm).toHaveBeenCalledOnce(); expect(get('[role=dialog]')).not.toBeNull(); expect(document.querySelector('#restore-archive')).toBeNull();
  await router.push('/adapters'); expect(router.currentRoute.value.name).toBe('settings');
  boot = NEW; finish(response({ error: 'lost acceptance' }, 500)); await flush(); await flush(); await flush();
  expect(document.querySelector('[role=dialog]')).toBeNull(); expect(document.body.textContent).toContain('恢复结果尚未确认');
  expect(document.body.textContent).not.toContain('已完成离线恢复'); expect(fetcher.mock.calls.filter(([path]) => path.endsWith('/commit'))).toHaveLength(1);
  expect(get('#restore-archive')).not.toBeNull();
});
it('explicit stale-plan rejection returns through existing modal acknowledgement and permits a new inspection', async () => {
  vi.spyOn(window, 'confirm').mockReturnValue(true);
  const fetcher = vi.fn(async (path: string) => path === '/api/restores/inspect' ? response(backupPlan()) : path.endsWith('/commit') ? response({ error: '重新检查归档' }, 409) : read(path));
  vi.stubGlobal('fetch', fetcher); await mount(); await choose(); await click('#confirm-restore'); expect(document.body.textContent).toContain('重新检查归档');
  get<HTMLButtonElement>('[role=dialog] button').click(); await flush(); await flush(); await choose();
  expect(get('#restore-plan-title')).not.toBeNull(); expect(fetcher.mock.calls.filter(([path]) => path.endsWith('/commit'))).toHaveLength(1);
  expect(fetcher.mock.calls.filter(([path]) => path === `/api/restores/${ID}/outcome`)).toHaveLength(0);
});
