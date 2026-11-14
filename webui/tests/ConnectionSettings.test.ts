import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { createApp, nextTick } from 'vue';
import ConnectionSettings from '../src/components/ConnectionSettings.vue';
import AdaptersPage from '../src/components/AdaptersPage.vue';
import { ApiError } from '../src/api';
import { connectionGroups, readSettings } from '../src/systemSettings';
import { systemSettingsFixture } from './systemSettingsFixture';
import { runtimeFixture } from './runtimeFixture';

let app: ReturnType<typeof createApp> | undefined;
let server = systemSettingsFixture();
const request = vi.fn(), dirty = vi.fn(), busy = vi.fn();
const flush = async () => { await new Promise<void>(resolve => setImmediate(resolve)); await nextTick(); };
const element = <T extends HTMLElement = HTMLElement>(selector: string) => document.querySelector<T>(selector)!;
const click = async (selector: string) => { element<HTMLButtonElement>(selector).click(); await flush(); };
const input = async (selector: string, value: string | boolean) => {
  const el = element<HTMLInputElement>(selector);
  if (typeof value === 'boolean') el.checked = value; else el.value = value;
  el.dispatchEvent(new Event(typeof value === 'boolean' ? 'change' : 'input', { bubbles: true }));
  el.dispatchEvent(new Event('change', { bubbles: true })); await nextTick();
};
const submit = async () => { element('#connections-form').dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })); await flush(); };
function mount(runtime = false) {
  const root = document.createElement('div'); document.body.append(root);
  app = createApp(runtime ? AdaptersPage : ConnectionSettings, { request, onDirty: dirty, onBusy: busy }); app.mount(root);
}
beforeEach(() => {
  server = systemSettingsFixture(); request.mockReset(); dirty.mockReset(); busy.mockReset();
  request.mockImplementation(async (path: string, body?: { settings: typeof server.connections }) => {
    if (path === '/api/runtime') return runtimeFixture();
    if (body) { server.connections = JSON.parse(JSON.stringify(body.settings)); server.revision = 'b'.repeat(64); }
    return structuredClone(server);
  });
  vi.spyOn(window, 'confirm').mockReturnValue(false);
});
afterEach(() => { app?.unmount(); app = undefined; document.body.replaceChildren(); vi.restoreAllMocks(); });

it('按需读取，未知选择保留，连接保存不改当前运行面板或正文开关', async () => {
  server.connections.qq.environment = 'future_environment';
  mount(true); await flush(); const runtime = element('.runtime-values');
  expect(request.mock.calls.filter(([path]) => path === '/api/settings/system')).toHaveLength(0);
  await click('#edit-connections');
  expect(element<HTMLSelectElement>('#connections-qq-environment').value).toBe('future_environment');
  expect(document.body.textContent).toContain('（现有值）');
  expect(document.body.textContent).toContain('未检查凭据是否存在');
  await input('#connections-onebot11-enabled', true); await input('#connections-webhook-listen', '127.0.0.1:8181');
  expect(dirty).toHaveBeenLastCalledWith(true); await submit();
  const post = request.mock.calls.find(([, body]) => body)?.[1];
  expect(post.scope).toBe('connections'); expect(post.expected_revision).toBe('a'.repeat(64));
  expect(post.settings.qq.environment).toBe('future_environment');
  expect(post.settings.qq.webhook.listen).toBe('127.0.0.1:8181');
  expect(post.settings.onebot11.enabled).toBe(true); expect(post.settings).not.toHaveProperty('global');
  expect(dirty).toHaveBeenLastCalledWith(false); expect(element('.runtime-values')).toBe(runtime);
  expect(document.body.textContent).toContain('请通过现有运行方式重启');
});

it('409 保留可复制草稿并阻止重复保存，刷新必须确认放弃', async () => {
  mount(); await click('#edit-connections'); await input('#connections-webhook-path', '/new-callback');
  request.mockRejectedValueOnce(new ApiError('版本冲突', 409)); await submit();
  expect(element<HTMLInputElement>('#connections-webhook-path').value).toBe('/new-callback');
  expect(element<HTMLTextAreaElement>('#copy-connections').value).toContain('/new-callback');
  expect(element<HTMLButtonElement>('#save-connections').disabled).toBe(true);
  const count = request.mock.calls.length; await submit(); await click('#refresh-connections'); expect(request).toHaveBeenCalledTimes(count);
  vi.mocked(window.confirm).mockReturnValue(true); await click('#refresh-connections');
  expect(element<HTMLInputElement>('#connections-webhook-path').value).toBe('/callbacks/qq');
  expect(dirty).toHaveBeenLastCalledWith(false);
});

it('校验失败保留草稿，忙碌时字段和刷新不可操作', async () => {
  mount(); await click('#edit-connections'); await input('#connections-onebot11-listen', '127.0.0.1:6789');
  let reject!: (e: Error) => void;
  request.mockImplementationOnce(() => new Promise((_, fail) => { reject = fail; }));
  element('#connections-form').dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })); await nextTick();
  expect(busy).toHaveBeenLastCalledWith(true); expect(element('#connections-onebot11-listen').matches(':disabled')).toBe(true);
  expect(element<HTMLButtonElement>('#refresh-connections').disabled).toBe(true);
  reject(new ApiError('设置无效', 422)); await flush();
  expect(element<HTMLInputElement>('#connections-onebot11-listen').value).toBe('127.0.0.1:6789');
  expect(element<HTMLButtonElement>('#save-connections').disabled).toBe(false); expect(busy).toHaveBeenLastCalledWith(false);
});

it('结果未知时重新读取，确认已保存后解除草稿，不重复写入', async () => {
  mount(); await click('#edit-connections'); await input('#connections-webhook-path', '/saved-after-timeout');
  request.mockImplementationOnce(async (_path, body) => {
    server.connections = JSON.parse(JSON.stringify(body.settings)); server.revision = 'c'.repeat(64); throw new ApiError('timeout');
  });
  await submit();
  expect(request.mock.calls.filter(([, body]) => body)).toHaveLength(1);
  expect(request.mock.calls.filter(([, body]) => !body)).toHaveLength(2);
  expect(dirty).toHaveBeenLastCalledWith(false); expect(document.body.textContent).toContain('重新读取已确认保存');
});

it('重新读取仍不确认时保留草稿和写入阻断；卸载取消请求', async () => {
  mount(); await click('#edit-connections'); await input('#connections-webhook-path', '/uncertain');
  request.mockRejectedValueOnce(new ApiError('offline')); await submit();
  expect(element<HTMLTextAreaElement>('#copy-connections').value).toContain('/uncertain');
  expect(element<HTMLButtonElement>('#save-connections').disabled).toBe(true);
  expect(request.mock.calls.filter(([, body]) => body)).toHaveLength(1);
  const signal = request.mock.calls[0]![2] as AbortSignal; app!.unmount(); app = undefined; expect(signal.aborted).toBe(true);
});

it('初始错误有重试且不会提供空默认表单；异常结构拒绝读取', async () => {
  mount(); request.mockRejectedValueOnce(new ApiError('不可用', 503)); await click('#edit-connections');
  expect(element('#connections-qq-enabled')).toBeNull(); expect(document.body.textContent).toContain('设置暂不可用');
  await click('#refresh-connections'); expect(element('#connections-qq-enabled')).not.toBeNull();
  const malformed = systemSettingsFixture(); Object.assign(malformed.connections.qq, { app_secret: 'never-display' });
  expect(() => readSettings(malformed, 'connections', connectionGroups)).toThrow('设置响应格式异常');
});

it('原生保存保留未启用适配器的零值；启用后再施加数值约束', async () => {
  server.connections.qq.webhook.max_body_bytes = 0;
  server.connections.onebot11.max_pending_actions = 0;
  mount(); await click('#edit-connections'); await input('#connections-qq-environment', 'sandbox');
  expect(element<HTMLFormElement>('#connections-form').checkValidity()).toBe(true);
  await click('#save-connections');
  expect(request.mock.calls.filter(([, body]) => body)).toHaveLength(1);
  expect(server.connections.onebot11.max_pending_actions).toBe(0);
  expect(server.connections.qq.webhook.max_body_bytes).toBe(0);
  await input('#connections-onebot11-enabled', true);
  expect(element<HTMLFormElement>('#connections-form').checkValidity()).toBe(false);
  await click('#save-connections'); expect(request.mock.calls.filter(([, body]) => body)).toHaveLength(1);
});
