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
    return { instance_id: selected.instance_id, version: selected.latest.version, enabled: false, restart_required: true, missing_permissions: ['storage.private'], config_incomplete: true, updated_at_ms: 2 };
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
beforeEach(() => { selected = item(); page = { source: 'https://example.com/index.json', total: 1, offset: 0, plugins: [selected] }; fail = false; pending = undefined; posts.length = 0; vi.clearAllMocks(); });
afterEach(() => { app?.unmount(); document.body.replaceChildren(); });

it('真实目录、详情、固定哈希与权限可读；默认不授权且需单独确认', async () => {
  await mount();
  expect(document.body.textContent).toContain('https://example.com/index.json');
  expect(document.querySelector('img')).toBeNull();
  expect(posts).toHaveLength(0);
  await click('.market-list button');
  expect(document.body.textContent).toContain('a'.repeat(64));
  expect(document.activeElement).toBe(get('#market-detail-title'));
  expect(get<HTMLButtonElement>('.market-details .primary').disabled).toBe(true);
  expect([...document.querySelectorAll<HTMLInputElement>('.market-details input')].every(input => !input.checked)).toBe(true);
  await check('.market-permission input[value="message.reply"]');
  expect(posts).toHaveLength(0);
  await check('.market-confirm input'); await click('.market-details .primary');
  expect(posts).toEqual([{ path: '/api/marketplace/install', body: { selector: 'ping', selection: 'b'.repeat(64), version: '1.0.0', sha256: 'a'.repeat(64), expected_updated_at_ms: null, grants: ['message.reply'], confirm_unsigned: true } }]);
  expect(document.body.textContent).toContain('RESTART REQUIRED');
  expect(document.body.textContent).toContain('当前保持禁用');
  expect(document.body.textContent).toContain('配置尚未完成');
  expect(installed).toHaveBeenCalledOnce();
  await click('.market-details .primary'); expect(posts).toHaveLength(1);
});
it('更新保留旧授权展示，携带安装版本检查且不默认选新增权限', async () => {
  selected.installed = { version: '0.9.0', enabled: false, updated_at_ms: 7, granted_permissions: ['message.reply'], runtime_state: 'ready' };
  await mount(); await click('.market-list button');
  expect(document.body.textContent).toContain('已授权，更新时保留');
  expect(document.querySelector('input[value="message.reply"]')).toBeNull();
  expect(get<HTMLInputElement>('input[value="storage.private"]').checked).toBe(false);
  await check('.market-confirm input'); await click('.market-details .primary');
  expect(posts[0]).toMatchObject({ path: '/api/marketplace/update', body: { expected_updated_at_ms: 7, grants: [] } });
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
  await click('.market-details .primary'); expect(posts).toHaveLength(1);
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
