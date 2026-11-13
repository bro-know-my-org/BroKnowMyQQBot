import { afterEach, describe, expect, it, vi } from 'vitest';
import { createApp, nextTick } from 'vue';
import AppShell from '../src/components/AppShell.vue';
import { createManagementRouter } from '../src/navigation';

let app: ReturnType<typeof createApp>;
async function mount() {
  const router = createManagementRouter(true); await router.push('/'); await router.isReady();
  const root = document.createElement('div'); document.body.append(root);
  app = createApp(AppShell, { busy: false }).use(router); app.mount(root); await nextTick();
  return router;
}
const click = async (selector: string) => { document.querySelector<HTMLButtonElement>(selector)!.click(); await nextTick(); await nextTick(); };
afterEach(() => { app?.unmount(); document.body.replaceChildren(); vi.restoreAllMocks(); });
describe('全局导航外壳', () => {
  it('折叠只保存布局偏好，存储不可用时仍可操作', async () => {
    vi.spyOn(Storage.prototype, 'getItem').mockReturnValue('true');
    const save = vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => { throw new Error('storage unavailable'); });
    await mount(); expect(document.querySelector('.is-collapsed')).not.toBeNull();
    await click('#sidebar-toggle'); expect(document.querySelector('.is-collapsed')).toBeNull();
    expect(save).toHaveBeenCalledExactlyOnceWith('bkmqb.sidebar.collapsed', 'false');
    expect(document.querySelector('.nav-item[aria-label="已安装"]')).not.toBeNull();
  });
  it('手机抽屉隔离背景，Escape 恢复焦点，路由变化关闭菜单', async () => {
    const router = await mount(); await click('.mobile-menu-toggle');
    expect(document.querySelector('[role="dialog"][aria-modal="true"]')).not.toBeNull(); expect(document.querySelector('.app-body')!.hasAttribute('inert')).toBe(true);
    expect(document.body.style.overflow).toBe('hidden'); expect(document.activeElement).toBe(document.querySelector('.mobile-close'));
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' })); await nextTick(); await nextTick();
    expect(document.querySelector('.is-mobile-open')).toBeNull(); expect(document.activeElement).toBe(document.querySelector('.mobile-menu-toggle'));
    await click('.mobile-menu-toggle'); await router.push('/settings'); await nextTick();
    expect(document.querySelector('.is-mobile-open')).toBeNull(); expect(document.body.style.overflow).toBe('');
    await click('.mobile-menu-toggle'); await click('.skip-link'); expect(document.activeElement).toBe(document.querySelector('#page-content'));
  });
  it('跨断点后将焦点移到可见导航控件', async () => {
    vi.spyOn(window, 'innerWidth', 'get').mockReturnValue(390); await mount(); await click('.mobile-menu-toggle');
    vi.spyOn(window, 'innerWidth', 'get').mockReturnValue(1440); window.dispatchEvent(new Event('resize')); await nextTick(); await nextTick(); await nextTick();
    expect(document.activeElement).toBe(document.querySelector('.nav-item.active'));
    (document.activeElement as HTMLElement).blur();
    vi.spyOn(window, 'innerWidth', 'get').mockReturnValue(390); window.dispatchEvent(new Event('resize')); await nextTick(); await nextTick(); await nextTick();
    expect(document.activeElement).toBe(document.querySelector('.mobile-menu-toggle'));
  });
});
