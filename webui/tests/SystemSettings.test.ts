import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { createApp, nextTick } from 'vue';
import SystemSettings from '../src/components/SystemSettings.vue';
import { ApiError } from '../src/api';
import { systemSettingsFixture } from './systemSettingsFixture';

let app: ReturnType<typeof createApp> | undefined;
let server = systemSettingsFixture();
const request = vi.fn(), dirty = vi.fn(), busy = vi.fn(), saved = vi.fn();
const flush = async () => { await new Promise<void>(resolve => setImmediate(resolve)); await nextTick(); };
const get = <T extends HTMLElement = HTMLElement>(selector: string) => document.querySelector<T>(selector)!;
const click = async (selector: string) => { get<HTMLButtonElement>(selector).click(); await flush(); };
const input = async (selector: string, value: string | boolean) => {
  const el = get<HTMLInputElement>(selector);
  if (typeof value === 'boolean') el.checked = value; else el.value = value;
  el.dispatchEvent(new Event(typeof value === 'boolean' ? 'change' : 'input', { bubbles: true })); await nextTick();
};
const submit = async () => { get('#global-form').dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })); await flush(); };
function mount() { const root = document.createElement('div'); document.body.append(root); app = createApp(SystemSettings, { request, onDirty: dirty, onBusy: busy, onSaved: saved, bootId: '11111111-1111-4111-8111-111111111111' }); app.mount(root); }
beforeEach(() => {
  server = systemSettingsFixture(); request.mockReset(); dirty.mockReset(); busy.mockReset(); saved.mockReset();
  request.mockImplementation(async (_path, body) => {
    if (body) { server.global = JSON.parse(JSON.stringify(body.settings)); server.revision = 'b'.repeat(64); }
    return structuredClone(server);
  });
  vi.spyOn(window, 'confirm').mockReturnValue(false);
});
afterEach(() => { app?.unmount(); app = undefined; document.body.replaceChildren(); vi.restoreAllMocks(); });

it('普通运行和保留容量保存不会开启正文，不发送路径或连接设置', async () => {
  mount(); await click('#edit-global');
  expect(get<HTMLInputElement>('#global-console-message_content').checked).toBe(false);
  expect(get<HTMLInputElement>('#global-files-message_content').checked).toBe(false);
  await input('#global-runtime-event_concurrency', '7'); await input('#global-files-runtime_max_total_mb', '1024'); await input('#global-console-filter', 'info,bot_core=debug');
  await submit();
  const body = request.mock.calls.find(([, body]) => body)![1];
  expect(saved).toHaveBeenCalledExactlyOnceWith('11111111-1111-4111-8111-111111111111');
  expect(body.scope).toBe('global'); expect(body.settings.runtime.event_concurrency).toBe(7);
  expect(body.settings.logging.files.runtime_max_total_mb).toBe(1024);
  expect(body.settings.logging.console.message_content).toBe(false); expect(body.settings.logging.files.message_content).toBe(false);
  expect(body.settings.logging.files).not.toHaveProperty('directory'); expect(body.settings).not.toHaveProperty('connections');
  expect(dirty).toHaveBeenLastCalledWith(false); expect(document.body.textContent).toContain('请通过现有运行方式重启');
});

it('正文按 sink 显式开启，既有正文开关在其他编辑中保持', async () => {
  server.global.logging.console.message_content = true;
  mount(); await click('#edit-global'); await input('#global-runtime-handler_timeout_seconds', '20'); await submit();
  expect(server.global.logging.console.message_content).toBe(true); expect(server.global.logging.files.message_content).toBe(false);
  await input('#global-files-message_content', true); await submit();
  expect(server.global.logging.files.message_content).toBe(true);
});

it('全局冲突和无效过滤器保留可复制草稿；未确认不重复写入', async () => {
  mount(); await click('#edit-global'); await input('#global-files-filter', 'warn,bot_core=trace');
  request.mockRejectedValueOnce(new ApiError('过滤器无效', 422)); await submit();
  expect(get<HTMLTextAreaElement>('#copy-global').value).toContain('warn,bot_core=trace'); expect(get<HTMLButtonElement>('#save-global').disabled).toBe(false);
  request.mockRejectedValueOnce(new ApiError('冲突', 409)); await submit();
  expect(saved).not.toHaveBeenCalled(); expect(get<HTMLButtonElement>('#save-global').disabled).toBe(true);
  await submit(); expect(request.mock.calls.filter(([, body]) => body)).toHaveLength(2);
});
