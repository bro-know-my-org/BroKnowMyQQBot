import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { createApp, nextTick } from 'vue';
import MarketplacePage from '../src/components/MarketplacePage.vue';
import { createManagementRouter } from '../src/navigation';
import type { Catalogue, CatalogueItem } from '../src/types';
import type { Request } from '../src/api';
import { ApiError } from '../src/api';
const item = (): CatalogueItem => ({ slug: 'ping', plugin_id: 'dev.bkm.ping', name: '<img src=x onerror=alert(1)>', summary: '描述', trust: 'official', repository: 'https://github.com/example/ping', commands: ['ping'], capabilities: ['message.reply', 'storage.private'], latest: { version: '1.0.0', protocol: '>=1.4,<2.0', download: 'https://github.com/example/ping/releases/download/v1.0.0/ping.bkm-plugin', sha256: 'a'.repeat(64) }, selection: 'b'.repeat(64), instance_id: 'dev.bkm.ping/default', installed: null, manageable: true });
let app: ReturnType<typeof createApp>, selected: CatalogueItem, page: Catalogue, fail: boolean;
let pending: (() => void) | undefined;
const posts: Array<{ path: string; body: unknown }> = [];
const busy = vi.fn(), installed = vi.fn();
const request = vi.fn(async (path: string, body?: unknown) => {
  if (fail) throw new ApiError('市场暂不可用', 503);
  if (body) {
    posts.push({ path, body });
    if (pending) await new Promise<void>(resolve => { pending = resolve; });
    return { loaded_version: selected.installed?.loaded_version ?? null, runtime_state: selected.installed?.runtime_state ?? 'not_loaded', instance_id: selected.instance_id, version: selected.latest.version, enabled: false, restart_required: true, missing_permissions: ['storage.private'], config_incomplete: true, updated_at_ms: 2 };
  }
  return path.startsWith('/api/marketplace/details') ? { ...page, plugins: [selected] } : page;
});
const flush = async () => { await new Promise<void>(resolve => setImmediate(resolve)); await nextTick(); };
function get<T extends HTMLElement = HTMLElement>(selector: string): T { const element = document.querySelector<T>(selector); if (!element) throw new Error(`Missing ${selector}`); return element; }
async function click(selector: string) { get<HTMLButtonElement>(selector).click(); await flush(); }
async function check(selector: string) { const el = get<HTMLInputElement>(selector); el.checked = true; el.dispatchEvent(new Event('change', { bubbles: true })); await nextTick(); }
async function mount() {
  const router = createManagementRouter(true); await router.push({ name: 'marketplace' }); await router.isReady();
  const root = document.createElement('div'); document.body.append(root);
  app = createApp(MarketplacePage, { request: request as Request, onBusy: busy, onInstalled: installed }).use(router); app.mount(root); await flush();
}
beforeEach(() => { selected = item(); page = { source: 'https://example.com/index.json', total: 1, offset: 0, plugins: [selected] }; fail = false; pending = undefined; posts.length = 0; vi.clearAllMocks(); vi.spyOn(window, 'scrollTo').mockImplementation(() => {}); vi.spyOn(window, 'confirm').mockReturnValue(true); });
afterEach(() => { app?.unmount(); document.body.replaceChildren(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

it('真实目录、详情、固定哈希与权限可读；默认不授权且需单独确认', async () => {
  await mount();
  expect(document.body.textContent).toContain('https://example.com/index.json');
  expect(document.querySelector('img')).toBeNull();
  expect(posts).toHaveLength(0);
  expect(document.querySelector('[aria-label="市场分页"]')).toBeNull();
  await click('.market-list button');
  expect(get('[aria-label="市场插件列表"]').style.display).toBe('none');
  expect(get('form').style.display).toBe('none');
  expect(get<HTMLDetailsElement>('.market-package').open).toBe(false);
  expect(get('.market-commands').textContent).toContain('ping');
  expect(document.body.textContent).toContain('a'.repeat(64));
  expect(document.activeElement).toBe(get('#market-detail-title'));
  expect(get<HTMLButtonElement>('.market-details .primary').disabled).toBe(true);
  expect([...document.querySelectorAll<HTMLInputElement>('.market-details input')].every(input => !input.checked)).toBe(true);
  await check('.market-permission input[value="message.reply"]');
  expect(posts).toHaveLength(0); expect(window.confirm).not.toHaveBeenCalled();
  await check('.market-confirm input'); await click('.market-details .primary');
  expect(posts).toEqual([{ path: '/api/marketplace/install', body: { selector: 'ping', selection: 'b'.repeat(64), version: '1.0.0', sha256: 'a'.repeat(64), expected_updated_at_ms: null, grants: ['message.reply'], confirm_unsigned: true } }]);
  expect(document.body.textContent).toContain('RESTART REQUIRED');
  expect(document.body.textContent).toContain('当前保持禁用');
  expect(document.body.textContent).toContain('配置尚未完成');
  expect(installed).toHaveBeenCalledOnce();
  await click('.market-details .primary'); expect(posts).toHaveLength(1);
});
it('更新保留旧授权展示，携带安装版本检查且不默认选新增权限', async () => {
  selected.installed = { version: '0.9.0', saved_version: '0.9.0', loaded_version: '0.8.0', restart_required: true, enabled: false, updated_at_ms: 7, granted_permissions: ['message.reply'], runtime_state: 'ready' };
  await mount(); await click('.market-list button');
  expect(document.body.textContent).toContain('已授权，更新时保留');
  expect(document.querySelector('input[value="message.reply"]')).toBeNull();
  expect(get<HTMLInputElement>('input[value="storage.private"]').checked).toBe(false);
  await check('.market-confirm input'); await click('.market-details .primary');
  expect(posts[0]).toMatchObject({ path: '/api/marketplace/update', body: { expected_updated_at_ms: 7, grants: [] } });
  expect(get('.market-install-state').textContent).toContain('已保存版本：1.0.0');
  expect(get('.market-install-state').textContent).toContain('实际加载版本：0.8.0');
  expect(get('.market-install-state').textContent).toContain('需要重启 Bot');
});
it('错误与空状态可重试，搜索词编码，不生成目录', async () => {
  fail = true; await mount(); expect(document.querySelector('[role="alert"]')?.textContent).toContain('市场暂不可用');
  fail = false; page = { ...page, total: 0, plugins: [] }; await click('[role="alert"] button');
  expect(document.body.textContent).toContain('没有匹配的插件');
  const input = get<HTMLInputElement>('#market-search'); input.value = 'a & b'; input.dispatchEvent(new Event('input', { bubbles: true }));
  get('form').dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })); await flush();
  expect(request.mock.calls.at(-1)?.[0]).toContain('q=a%20%26%20b'); expect(posts).toHaveLength(0);
});
it('索引或记录过期后要求重新打开详情，不自动重试安装', async () => {
  await mount(); await click('.market-list button'); await check('.market-confirm input');
  fail = true; await click('.market-details .primary');
  fail = false; await click('.market-details .primary'); expect(posts).toHaveLength(0);
  expect(document.body.textContent).toContain('重新打开详情');
});
it('文件管理条目不允许授权或安装', async () => {
  selected.manageable = false; await mount(); await click('.market-list button');
  expect(get<HTMLFieldSetElement>('fieldset').disabled).toBe(true);
  expect(document.body.textContent).toContain('声明式文件'); expect(posts).toHaveLength(0);
});
it('安装中报告全局忙碌状态并阻止重复提交', async () => {
  await mount(); await click('.market-list button'); await check('.market-confirm input');
  pending = () => {}; await click('.market-details .primary');
  expect(busy).toHaveBeenLastCalledWith(true); expect(get<HTMLFieldSetElement>('fieldset').disabled).toBe(true);
  await click('.market-details .section-heading button'); expect(document.querySelector('.market-details')).not.toBeNull();
  await click('.market-details .primary'); expect(posts).toHaveLength(1); expect(window.confirm).toHaveBeenCalledOnce();
  pending!(); await flush(); expect(busy).toHaveBeenLastCalledWith(false);
});
it('离开页面后忽略晚到响应', async () => {
  let resolve!: (value: Catalogue) => void;
  request.mockImplementationOnce(() => new Promise(r => { resolve = r; }));
  await mount(); app.unmount(); resolve(page); await flush(); expect(document.querySelector('.market-list')).toBeNull();
});

it('详情关闭后键盘焦点回到原目录按钮', async () => {
  await mount(); const trigger = get<HTMLButtonElement>('.market-list button'); trigger.focus();
  await click('.market-list button'); await click('.market-details .section-heading button');
  expect(document.activeElement).toBe(trigger);
});
it('翻页沿用已提交搜索，编辑框的新查询仅在提交时从第一页开始', async () => {
  page = { ...page, total: 25, plugins: Array.from({ length: 20 }, (_, i) => ({ ...item(), plugin_id: `dev.bkm.ping${i}` })) };
  await mount(); const input = get<HTMLInputElement>('#market-search'); input.value = 'draft'; input.dispatchEvent(new Event('input', { bubbles: true }));
  await click('nav[aria-label="市场分页"] button:last-child');
  expect(request.mock.calls.at(-1)?.[0]).toBe('/api/marketplace?q=&offset=20&limit=20');
  get('form').dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })); await flush();
  expect(request.mock.calls.at(-1)?.[0]).toBe('/api/marketplace?q=draft&offset=0&limit=20');
});
it('详情返回保留搜索草稿、已提交查询、分页、原按钮和滚动位置，不重复请求目录', async () => {
  page = { ...page, total: 25, plugins: Array.from({ length: 20 }, (_, i) => ({ ...item(), plugin_id: `dev.bkm.ping${i}` })) };
  await mount();
  const input = get<HTMLInputElement>('#market-search'); input.value = 'draft'; input.dispatchEvent(new Event('input', { bubbles: true }));
  page = { ...page, plugins: [selected] }; await click('nav[aria-label="市场分页"] button:last-child');
  vi.stubGlobal('scrollY', 480); const trigger = get<HTMLButtonElement>('.market-list button'); trigger.focus();
  await click('.market-list button'); const reads = request.mock.calls.length;
  await click('.market-details .section-heading button');
  expect(document.querySelector('.market-details')).toBeNull(); expect(get('[aria-label="市场插件列表"]').style.display).toBe('');
  expect(get<HTMLInputElement>('#market-search').value).toBe('draft'); expect(get('[aria-label="市场分页"]').textContent).toContain('21–21 / 25');
  expect(get('.market-results').textContent).toContain('当前结果：全部'); expect(request.mock.calls).toHaveLength(reads);
  expect(document.activeElement).toBe(trigger); expect(window.scrollTo).toHaveBeenLastCalledWith({ top: 480, behavior: 'instant' });
});
it('详情请求失败后保持原列表可操作，不展开空详情', async () => {
  await mount(); fail = true; await click('.market-list button');
  expect(document.querySelector('.market-details')).toBeNull(); expect(get('[aria-label="市场插件列表"]').style.display).toBe('');
  expect(get<HTMLButtonElement>('.market-list button').disabled).toBe(false); expect(get('[role="alert"]').textContent).toContain('市场暂不可用');
});
it('已保存版本与实例运行状态分别说明，同版本不重复更新并保留重启提示', async () => {
  selected.installed = { version: '1.0.0', saved_version: '1.0.0', loaded_version: '0.9.0', restart_required: true, enabled: false, updated_at_ms: 7, granted_permissions: [], runtime_state: 'ready' };
  await mount(); await click('.market-list button');
  expect(get('.market-install-state').textContent).toContain('已保存版本：1.0.0');
  expect(get('.market-install-state').textContent).toContain('已保存为禁用'); expect(get('.market-install-state').textContent).toContain('实例运行状态：运行中');
  expect(get('.market-install-state').textContent).toContain('需重启加载');
  await check('.market-permission input[value="message.reply"]'); await check('.market-confirm input'); await click('.market-details .primary');
  expect(get<HTMLFieldSetElement>('fieldset').disabled).toBe(true);
  expect(get('.market-details .primary').textContent).toContain('已保存');
  expect(posts).toHaveLength(0); expect(window.confirm).not.toHaveBeenCalled();
});

it.each([
  ['0.4.0', '0.4.0', '0.2.6', '市场版本较旧'],
  ['0.4.0', '0.4.0', '0.4.0', '已安装'],
  ['0.4.0+saved', '0.4.0+loaded', '0.4.0+market', '已安装'],
  ['0.2.6', '0.4.0', '0.3.0', '低于实际加载版本'],
  ['0.2.6', '0.4.0', '0.2.6', '低于实际加载版本'],
  ['0.4.0', null, 'v0.5.0', '无法比较'],
  ['broken', null, '0.5.0', '无法比较'],
  ['0.4.0', 'broken', '0.5.0', '无法比较'],
])('DOM 更新入口拒绝 saved=%s loaded=%s candidate=%s', async (saved, loaded, candidate, reason) => {
  selected.latest.version = candidate!;
  selected.installed = { version: saved!, saved_version: saved!, loaded_version: loaded, restart_required: saved !== loaded,
    enabled: true, updated_at_ms: 7, granted_permissions: [], runtime_state: 'ready' };
  await mount();
  expect(get('.market-list').textContent).toContain(reason);
  await click('.market-list button');
  expect(get('.market-version-policy').textContent).toContain(reason);
  expect(get<HTMLFieldSetElement>('fieldset').disabled).toBe(true);
  expect(get('.market-details .primary').textContent).not.toContain('确认更新');
  // Bypass native disabled behavior to exercise the real click handler's own guard.
  await check('.market-confirm input');
  get<HTMLFieldSetElement>('fieldset').disabled = false;
  get<HTMLButtonElement>('.market-details .primary').disabled = false;
  await click('.market-details .primary');
  expect(posts).toHaveLength(0); expect(window.confirm).not.toHaveBeenCalled();
  expect(busy).not.toHaveBeenCalledWith(true); expect(installed).not.toHaveBeenCalled();
});

it.each([
  ['0.9.0', '0.9.0', '0.10.0'],
  ['0.4.0-rc.1', '0.4.0-rc.1', '0.4.0'],
  ['0.2.6', '0.4.0', '0.4.0'],
  ['0.2.6', '0.4.0', '0.5.0'],
])('DOM 更新入口允许 saved=%s loaded=%s candidate=%s', async (saved, loaded, candidate) => {
  selected.latest.version = candidate;
  selected.installed = { version: saved, saved_version: saved, loaded_version: loaded, restart_required: saved !== loaded,
    enabled: false, updated_at_ms: 7, granted_permissions: ['message.reply'], runtime_state: 'ready' };
  await mount(); await click('.market-list button');
  expect(get<HTMLFieldSetElement>('fieldset').disabled).toBe(false);
  await check('input[value="storage.private"]'); await check('.market-confirm input'); await click('.market-details .primary');
  expect(posts).toEqual([{ path: '/api/marketplace/update', body: { selector: 'ping', selection: 'b'.repeat(64), version: candidate,
    sha256: 'a'.repeat(64), expected_updated_at_ms: 7, grants: ['storage.private'], confirm_unsigned: true } }]);
  expect(get('.market-install-state').textContent).toContain(`已保存版本：${candidate}`);
  expect(get('.market-install-state').textContent).toContain(`实际加载版本：${loaded}`);
  expect(get('.market-install-state').textContent).toContain('需要重启 Bot');
});

it('提交函数重新比较当前版本，不依赖此前渲染的可更新状态', async () => {
  selected.installed = { version: '0.4.0', saved_version: '0.4.0', loaded_version: '0.4.0', restart_required: false,
    enabled: true, updated_at_ms: 7, granted_permissions: [], runtime_state: 'ready' };
  await mount(); await click('.market-list button'); await check('.market-confirm input');
  expect(get<HTMLButtonElement>('.market-details .primary').disabled).toBe(false);
  // The catalogue is a shallow snapshot: mutate it without a render to detect a stale eligibility cache.
  selected.latest.version = '0.2.6';
  await click('.market-details .primary');
  expect(posts).toHaveLength(0); expect(window.confirm).not.toHaveBeenCalled();
  expect(get('[role="alert"]').textContent).toContain('不降级');
});

it.each([false, true])('取消最终%s安装/更新确认不写入且保留逐项授权与未签名选择', async update => {
  if (update) selected.installed = { version: '0.9.0', saved_version: '0.9.0', loaded_version: '0.8.0', restart_required: true, enabled: true, updated_at_ms: 7, granted_permissions: ['message.reply'], runtime_state: 'ready' };
  await mount(); await click('.market-list button');
  await check('input[value="storage.private"]'); await check('.market-confirm input');
  vi.mocked(window.confirm).mockReturnValue(false);
  await click('.market-details .primary');
  expect(posts).toHaveLength(0); expect(window.confirm).toHaveBeenCalledOnce();
  expect(window.confirm).toHaveBeenCalledWith(expect.stringContaining(`${update ? '更新' : '安装'}未签名插件`));
  expect(window.confirm).toHaveBeenCalledWith(expect.stringContaining('1.0.0'));
  expect(window.confirm).toHaveBeenCalledWith(expect.stringContaining('新增授权：storage.private'));
  expect(window.confirm).toHaveBeenCalledWith(expect.stringContaining(`保留授权：${update ? 'message.reply' : '无'}`));
  expect(window.confirm).toHaveBeenCalledWith(expect.stringContaining('重启 Bot'));
  expect(get<HTMLInputElement>('input[value="storage.private"]').checked).toBe(true);
  expect(get<HTMLInputElement>('.market-confirm input').checked).toBe(true);
  expect(get<HTMLFieldSetElement>('fieldset').disabled).toBe(false);
  expect(busy).toHaveBeenLastCalledWith(false); expect(installed).not.toHaveBeenCalled();
  expect(document.body.textContent).not.toContain('本次选择已结束');
  vi.mocked(window.confirm).mockReturnValue(true);
  await click('.market-details .primary');
  expect(posts).toEqual([{ path: `/api/marketplace/${update ? 'update' : 'install'}`, body: {
    selector: 'ping', selection: 'b'.repeat(64), version: '1.0.0', sha256: 'a'.repeat(64),
    expected_updated_at_ms: update ? 7 : null, grants: ['storage.private'], confirm_unsigned: true,
  } }]);
});

it('确认框显示期间的重入点击不会产生第二个框或重复写入', async () => {
  await mount(); await click('.market-list button'); await check('.market-confirm input');
  vi.mocked(window.confirm).mockImplementation(() => { get<HTMLButtonElement>('.market-details .primary').click(); return true; });
  await click('.market-details .primary');
  expect(window.confirm).toHaveBeenCalledOnce(); expect(posts).toHaveLength(1);
});
