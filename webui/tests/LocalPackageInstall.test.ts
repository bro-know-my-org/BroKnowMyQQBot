import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { createApp, h, nextTick, reactive } from 'vue';
import { createManagementRouter } from '../src/navigation';
import LocalPackageInstall from '../src/components/LocalPackageInstall.vue';
import type { LocalInspection, PluginView } from '../src/types';
import type { Request, Upload } from '../src/api';
const inspection = (): LocalInspection => ({ selection: '11111111-1111-4111-8111-111111111111', expires_in_seconds: 300, plugin_id: 'dev.sample', name: 'Sample', description: 'Local plugin', version: '2.0', sha256: 'a'.repeat(64), signature_status: 'unsigned', commands: ['sample'], requested_permissions: ['message.reply', 'storage.private'], default_instance_id: 'dev.sample/default' });
const saved = () => ({ instance_id: 'dev.sample/default', version: '2.0', updated_at_ms: 2, enabled: false, restart_required: true, config_incomplete: true, missing_permissions: ['storage.private'], loaded_version: null, runtime_state: 'not_loaded' });
const installed = () => ({ instance_id: 'dev.sample/default', plugin_id: 'dev.sample', editable: true, version: '1.0', updated_at_ms: 1, granted_permissions: ['message.reply'] }) as PluginView;
const props = reactive({ plugins: [] as PluginView[], locked: false });
const request = vi.fn(), upload = vi.fn(), dirty = vi.fn(), busy = vi.fn(), complete = vi.fn(), uncertain = vi.fn();
let app: ReturnType<typeof createApp>;
const flush = async () => { await new Promise<void>(resolve => setImmediate(resolve)); await nextTick(); };
const get = <T extends HTMLElement = HTMLButtonElement>(s: string) => document.querySelector<T>(s)!;
const click = async (s: string) => { get<HTMLButtonElement>(s).click(); await flush(); };
async function choose(name = 'sample.bkm-plugin') {
  const input = get<HTMLInputElement>('#local-package-file'); Object.defineProperty(input, 'files', { configurable: true, value: [new File(['fixture'], name)] });
  input.dispatchEvent(new Event('change', { bubbles: true })); await flush();
}
async function mount() {
  const root = document.createElement('div'); document.body.append(root);
  const router = createManagementRouter(true); await router.push({ name: 'plugins' }); await router.isReady();
  app = createApp({ render: () => h(LocalPackageInstall, { ...props, request: request as Request, upload: upload as Upload, onDirty: dirty, onBusy: busy, onInstalled: complete, onUncertain: uncertain }) }).use(router); app.mount(root); await flush();
}
beforeEach(() => { vi.clearAllMocks(); props.plugins = []; props.locked = false; request.mockReset().mockResolvedValue(saved()); upload.mockReset().mockResolvedValue(inspection()); vi.spyOn(window, 'confirm').mockReturnValue(true); });
afterEach(() => { app?.unmount(); document.body.replaceChildren(); vi.restoreAllMocks(); });
it('真实文件选择仅检查，逐项默认未选，独立未签名同意和取消确认保留表单且零安装', async () => {
  await mount(); await choose(); expect(upload).toHaveBeenCalledOnce(); expect(request).not.toHaveBeenCalled(); expect(dirty).toHaveBeenLastCalledWith(true);
  expect(document.body.textContent).toContain('a'.repeat(64)); expect(document.body.textContent).toContain('sample');
  expect(get<HTMLInputElement>('input[value="message.reply"]').checked).toBe(false);
  expect(get<HTMLButtonElement>('#commit-local-package').disabled).toBe(true);
  await click('input[value="message.reply"]'); await click('#local-unsigned'); vi.mocked(window.confirm).mockReturnValue(false);
  await click('#commit-local-package'); expect(window.confirm).toHaveBeenCalledOnce(); expect(request).not.toHaveBeenCalled();
  expect(get<HTMLInputElement>('#local-unsigned').checked).toBe(true); expect(get<HTMLInputElement>('input[value="message.reply"]').checked).toBe(true);
});
it('确认新建提交固定票据和不存在CAS，忙碌防重入，禁用结果提供配置入口', async () => {
  await mount(); await choose(); await click('input[value="message.reply"]'); await click('#local-unsigned');
  let resolve!: (value: unknown) => void; request.mockImplementationOnce(() => new Promise(r => { resolve = r; }));
  await click('#commit-local-package'); await click('#commit-local-package'); expect(request).toHaveBeenCalledOnce(); expect(busy).toHaveBeenLastCalledWith(true);
  expect(request.mock.calls[0]!.slice(0, 2)).toEqual(['/api/plugins/local/commit', { selection: inspection().selection, sha256: 'a'.repeat(64), plugin_id: 'dev.sample', version: '2.0', instance_id: 'dev.sample/default', expected_updated_at_ms: null, grants: ['message.reply'], confirm_unsigned: true, confirm: true }]);
  resolve(saved()); await flush(); expect(complete).toHaveBeenCalledOnce(); expect(dirty).toHaveBeenLastCalledWith(false); expect(busy).toHaveBeenLastCalledWith(false);
  expect(document.body.textContent).toContain('实例保持禁用'); expect(document.body.textContent).toContain('必要配置尚未完成'); expect(get<HTMLAnchorElement>('a').href).toContain('/config');
});
it('更新保留现有授权，只提交新增决定和固定修订，列表改变后阻止提交', async () => {
  props.plugins = [installed()]; await mount(); await choose();
  expect(get<HTMLInputElement>('input[type="checkbox"]').checked).toBe(true); expect(get<HTMLInputElement>('input[type="checkbox"]').disabled).toBe(true);
  await click('input[value="storage.private"]'); await click('#local-unsigned'); await click('#commit-local-package');
  expect(request.mock.calls[0]![1]).toMatchObject({ expected_updated_at_ms: 1, grants: ['storage.private'] }); expect(vi.mocked(window.confirm).mock.calls[0]![0]).toContain('更新');
  await choose(); props.plugins = [{ ...installed(), updated_at_ms: 3 }]; await nextTick();
  expect(document.body.textContent).toContain('此选择不可再提交'); expect(get<HTMLButtonElement>('#commit-local-package').disabled).toBe(true);
});
it.each([new Error('网络丢失'), new Error('选择已过期'), new Error('修订冲突')])('不确定结果保留决定，触发读取且绝不自动重放：%s', async failure => {
  await mount(); await choose(); await click('#local-unsigned'); request.mockRejectedValueOnce(failure); await click('#commit-local-package');
  expect(uncertain).toHaveBeenCalledOnce(); expect(get<HTMLInputElement>('#local-unsigned').checked).toBe(true); expect(document.body.textContent).toContain('不会自动重复安装');
  await click('#commit-local-package'); expect(request).toHaveBeenCalledOnce(); expect(dirty).toHaveBeenLastCalledWith(true);
});
it('换文件立即清除旧票据，同文件可重选，检查期间锁定文件避免取消后滞留票据', async () => {
  await mount(); await choose(); upload.mockRejectedValueOnce(new Error('损坏包')); await choose('bad.bkm-plugin');
  expect(request.mock.calls[0]!.slice(0, 2)).toEqual(['/api/plugins/local/discard', { selection: inspection().selection }]);
  expect(document.querySelector('#commit-local-package')).toBeNull(); expect(document.body.textContent).toContain('损坏包');
  let resolve!: (value: LocalInspection) => void; upload.mockImplementationOnce((_file: File, signal: AbortSignal) => new Promise((r, reject) => { resolve = r; signal.addEventListener('abort', () => reject(new Error('aborted'))); })); await choose();
  const signal = upload.mock.calls[2]![1] as AbortSignal; expect(get<HTMLInputElement>('#local-package-file').disabled).toBe(true);
  await choose('invalid.zip'); expect(signal.aborted).toBe(false); expect(upload).toHaveBeenCalledTimes(3);
  resolve(inspection()); await flush(); await choose('invalid.zip'); expect(document.querySelector('#commit-local-package')).toBeNull(); expect(document.body.textContent).toContain('32 MiB');
  expect(get<HTMLInputElement>('#local-package-file').value).toBe('');
});
it('不同身份或只读目标、非法ID不可提交，改为新实例后需重新逐项同意', async () => {
  props.plugins = [{ ...installed(), editable: false }]; await mount(); await choose(); expect(document.body.textContent).toContain('只读声明');
  const input = get<HTMLInputElement>('#local-instance'); input.value = '../escape'; input.dispatchEvent(new Event('input', { bubbles: true })); await nextTick();
  expect(document.body.textContent).toContain('请输入合法实例');
  input.value = 'dev.sample/new'; input.dispatchEvent(new Event('input', { bubbles: true })); await nextTick();
  expect(get<HTMLInputElement>('input[value="message.reply"]').checked).toBe(false); expect(get<HTMLInputElement>('#local-unsigned').checked).toBe(false);
});
