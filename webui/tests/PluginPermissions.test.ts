import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { createApp, h, nextTick, reactive } from 'vue';
import PluginPermissions from '../src/components/PluginPermissions.vue';
import type { Request } from '../src/api';
import { ApiError, createApi } from '../src/api';
import type { PermissionView, PluginView } from '../src/types';

let app: ReturnType<typeof createApp>;
const plugin = (): PluginView => ({ instance_id: 'sample/a', plugin_id: 'dev.bkm.sample', name: 'Sample', description: '', version: '1.0.0', saved_version: '1.0.0', loaded_version: '1.0.0', restart_required: false, source: 'local', trust_level: 'local-wasm', signature_status: 'unsigned', editable: true, enabled: true, runtime_state: 'ready', updated_at_ms: 1, pending: false, error: null, requested_permissions: ['untrusted.metadata'], granted_permissions: ['message.reply'], config: {}, schema: null });
const verified = (): PermissionView => ({ instance_id: 'sample/a', expected_updated_at_ms: 1, version: '1.0.0', sha256: 'a'.repeat(64), requested_permissions: ['message.reply', 'storage.private'], granted_permissions: ['message.reply'] });
const props = reactive({ plugin: plugin(), busy: false });
const request = vi.fn(), dirty = vi.fn(), busy = vi.fn(), updated = vi.fn(), uncertain = vi.fn();
const flush = async () => { await new Promise<void>(resolve => setImmediate(resolve)); await nextTick(); };
const get = <T extends HTMLElement = HTMLButtonElement>(selector: string) => document.querySelector<T>(selector)!;
async function click(selector: string) { get<HTMLButtonElement>(selector).click(); await flush(); }
async function toggle(permission: string) { const input = get<HTMLInputElement>(`input[value="${permission}"]`); input.click(); await nextTick(); }
async function mount() {
  const root = document.createElement('div'); document.body.append(root);
  app = createApp({ render: () => h(PluginPermissions, { ...props, request: request as Request, onDirty: dirty, onBusy: busy, onUpdated: updated, onUncertain: uncertain }) }); app.mount(root); await flush();
}
beforeEach(() => { vi.clearAllMocks(); props.plugin = plugin(); props.busy = false; request.mockResolvedValue(verified()); vi.spyOn(window, 'confirm').mockReturnValue(true); });
afterEach(() => { app?.unmount(); document.body.replaceChildren(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

it('只从校验接口生成选择，新增默认未选，取消二次确认保留逐项决定且零保存', async () => {
  await mount(); expect(request).not.toHaveBeenCalled(); await click('#edit-permissions');
  expect(request.mock.calls[0]![0]).toBe('/api/plugins/permissions?instance_id=sample%2Fa');
  expect(document.querySelector('input[value="untrusted.metadata"]')).toBeNull();
  expect(get<HTMLInputElement>('input[value="message.reply"]').checked).toBe(true);
  expect(get<HTMLInputElement>('input[value="storage.private"]').checked).toBe(false);
  await toggle('storage.private'); expect(dirty).toHaveBeenLastCalledWith(true);
  vi.mocked(window.confirm).mockReturnValue(false); await click('#save-permissions');
  expect(window.confirm).toHaveBeenCalledOnce(); expect(request).toHaveBeenCalledOnce();
  expect(get<HTMLInputElement>('input[value="storage.private"]').checked).toBe(true);
});
it('确认撤权携带CAS，忙碌期间不能重复提交，保存后报告实际禁用与重启状态', async () => {
  await mount(); await click('#edit-permissions'); await toggle('message.reply');
  let finish!: (value: unknown) => void;
  request.mockImplementationOnce(() => new Promise(resolve => { finish = resolve; }));
  await click('#save-permissions'); expect(busy).toHaveBeenLastCalledWith(true);
  expect(get<HTMLFieldSetElement>('fieldset').disabled).toBe(true);
  await click('#save-permissions'); expect(request).toHaveBeenCalledTimes(2);
  expect(request.mock.calls[1]!.slice(0, 2)).toMatchObject(['/api/plugins/permissions', { instance_id: 'sample/a', expected_updated_at_ms: 1, grants: [], confirm: true }]);
  expect(window.confirm).toHaveBeenCalledOnce(); expect(vi.mocked(window.confirm).mock.calls[0]![0]).toContain('撤销：message.reply');
  finish({ plugin: { ...plugin(), granted_permissions: [], enabled: false, runtime_state: 'disabled', restart_required: true, updated_at_ms: 2 } }); await flush();
  expect(updated.mock.calls[0]![0]).toMatchObject({ runtime_state: 'disabled', restart_required: true });
  expect(dirty).toHaveBeenLastCalledWith(false); expect(busy).toHaveBeenLastCalledWith(false);
});
it.each([new ApiError('记录过期', 409), new ApiError('连接丢失'), new Error('读取异常')])('未知结果保留选择并阻止重放，显式重读才恢复：%s', async error => {
  await mount(); await click('#edit-permissions'); await toggle('storage.private');
  request.mockRejectedValueOnce(error); await click('#save-permissions');
  expect(uncertain).toHaveBeenCalledOnce(); expect(get('[role="alert"]').textContent).toContain('不会自动重复保存');
  expect(get<HTMLInputElement>('input[value="storage.private"]').checked).toBe(true);
  await click('#save-permissions'); expect(request).toHaveBeenCalledTimes(2);
  await click('#edit-permissions'); expect(request).toHaveBeenCalledTimes(3);
  expect(get<HTMLInputElement>('input[value="storage.private"]').checked).toBe(false);
});
it('只读来源不提供编辑；拒绝越界响应而不生成无界选择', async () => {
  props.plugin = { ...plugin(), editable: false }; await mount();
  expect(document.querySelector('#edit-permissions')).toBeNull(); expect(request).not.toHaveBeenCalled();
  props.plugin = plugin(); await nextTick(); request.mockResolvedValueOnce({ ...verified(), requested_permissions: Array.from({ length: 129 }, (_, i) => String(i)) });
  await click('#edit-permissions'); expect(document.querySelector('fieldset')).toBeNull(); expect(get('[role="alert"]').textContent).toContain('格式异常');
});
it('二进制API复用Cookie和401清理，安全整数、取消信号与响应结构都受检查', async () => {
  const logout = vi.fn(), api = createApi(logout), fetcher = vi.fn(); vi.stubGlobal('fetch', fetcher);
  const file = new File(['package'], 'sample.bkm-plugin');
  fetcher.mockResolvedValueOnce(new Response('{}', { status: 401 }));
  await expect(api.upload(file)).rejects.toMatchObject({ status: 401 }); expect(logout).toHaveBeenCalledOnce();
  expect(fetcher.mock.calls[0]![1]).toMatchObject({ body: file, mode: 'same-origin', credentials: 'same-origin', redirect: 'error', headers: { 'Content-Type': 'application/octet-stream' } });
  fetcher.mockResolvedValueOnce(new Response('{"expires_in_seconds":9007199254740993}'));
  await expect(api.upload(file)).rejects.toThrow('安全整数');
  fetcher.mockResolvedValueOnce(new Response('{}')); await expect(api.upload(file)).rejects.toThrow('格式异常');
  const controller = new AbortController(); controller.abort();
  await expect(api.upload(file, controller.signal)).rejects.toThrow();
  await expect(api.upload(new File(['x'], 'wrong.zip'))).rejects.toThrow('32 MiB');
  api.clear();
});
