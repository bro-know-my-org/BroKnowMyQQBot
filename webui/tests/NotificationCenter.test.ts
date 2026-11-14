import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { createApp, nextTick } from 'vue';
import NotificationCenter from '../src/components/NotificationCenter.vue';
import { createNotifications } from '../src/notifications';
let app: ReturnType<typeof createApp>, center: ReturnType<typeof createNotifications>, trigger: HTMLButtonElement;
let intersection: IntersectionObserverCallback;
let observerOptions: IntersectionObserverInit | undefined;
const targets = new Set<Element>(), disconnect = vi.fn(() => targets.clear());
const intersect = (target: Element, shown = true, ratio = shown ? 1 : 0) => intersection([{
  target, isIntersecting: shown, intersectionRatio: ratio, boundingClientRect: target.getBoundingClientRect(),
  intersectionRect: target.getBoundingClientRect(), rootBounds: null, time: performance.now(),
}], {} as IntersectionObserver);
const get = <T extends HTMLElement = HTMLElement>(selector: string) => document.querySelector<T>(selector)!;
beforeEach(() => {
  vi.spyOn(document, 'hasFocus').mockReturnValue(true);
  vi.spyOn(HTMLElement.prototype, 'getClientRects').mockReturnValue([new DOMRect(0, 0, 44, 44)] as unknown as DOMRectList);
  targets.clear(); disconnect.mockClear();
  vi.stubGlobal('IntersectionObserver', class {
    constructor(callback: IntersectionObserverCallback, options?: IntersectionObserverInit) { intersection = callback; observerOptions = options; }
    observe(element: Element) { targets.add(element); }
    unobserve(element: Element) { targets.delete(element); }
    disconnect = disconnect;
  });
  center = createNotifications(); trigger = document.createElement('button'); trigger.textContent = '通知历史'; trigger.setAttribute('aria-controls', 'notification-history');
  const root = document.createElement('div'); document.body.append(trigger, root); app = createApp(NotificationCenter, { center }); app.mount(root);
});
afterEach(() => { app.unmount(); center.dispose(); document.body.replaceChildren(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });
it('打开历史后聚焦关闭按钮，Escape 关闭并回到触发按钮', async () => {
  trigger.focus(); center.notify('设置', '前缀已保存', 'success'); center.setOpen(true); await nextTick(); await nextTick();
  expect(document.activeElement).toBe(get('[aria-label="关闭通知历史"]'));
  expect(get('#notification-history').textContent).toContain('前缀已保存'); intersect(get('.notification-history-list li')); expect(center.unread.value).toBe(0);
  document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' })); await nextTick();
  expect(get('#notification-history')).toBeNull(); expect(document.activeElement).toBe(trigger); expect(center.state.history).toHaveLength(1);
});
it('显式关闭历史可回焦，清空和空状态明确', async () => {
  trigger.focus(); center.notify('插件', '已保存'); center.setOpen(true); await nextTick(); await nextTick();
  const clear = [...document.querySelectorAll<HTMLButtonElement>('.notification-history-actions button')].find(button => button.textContent === '清空历史')!;
  clear.click(); await nextTick(); expect(get('#notification-history').textContent).toContain('暂无历史通知'); expect(center.state.active).toHaveLength(0);
  get<HTMLButtonElement>('[aria-label="关闭通知历史"]').click(); await nextTick(); expect(document.activeElement).toBe(trigger);
});
it('新浮层不抢焦点，关闭后仍可回看，消息使用安全文本', async () => {
  trigger.focus(); center.notify('插件', '<img src=x onerror=alert(1)>', 'error'); await nextTick();
  expect(document.activeElement).toBe(trigger); expect(document.querySelector('img')).toBeNull(); expect(get('[role="alert"]').textContent).toContain('<img');
  get<HTMLButtonElement>('[aria-label="关闭通知"]').click(); await nextTick();
  expect(get('.notification-toast')).toBeNull(); center.setOpen(true); await nextTick(); expect(get('#notification-history').textContent).toContain('<img');
});
it('鼠标和键盘分别暂停，仅焦点离开整条通知后解除焦点暂停', async () => {
  const paused = vi.spyOn(center, 'setPaused'); const id = center.notify('设置', '已保存')!; await nextTick();
  const toast = get('.notification-toast'), close = get<HTMLButtonElement>('[aria-label="关闭通知"]');
  toast.dispatchEvent(new MouseEvent('mouseenter')); expect(paused).toHaveBeenLastCalledWith(id, 'pointer', true);
  close.focus(); expect(paused).toHaveBeenLastCalledWith(id, 'focus', true);
  toast.dispatchEvent(new FocusEvent('focusout', { bubbles: true, relatedTarget: close })); expect(paused).toHaveBeenLastCalledWith(id, 'focus', true);
  toast.dispatchEvent(new MouseEvent('mouseleave')); expect(paused).toHaveBeenLastCalledWith(id, 'pointer', false);
  trigger.focus(); expect(paused).toHaveBeenLastCalledWith(id, 'focus', false);
});
it('会话重置不会把焦点送回旧控件；inert 控件不接收关闭后的焦点', async () => {
  trigger.focus(); center.setOpen(true); await nextTick(); await nextTick(); center.reset(); await nextTick();
  expect(document.activeElement).not.toBe(trigger);
  trigger.focus(); center.setOpen(true); await nextTick(); await nextTick(); trigger.setAttribute('inert', '');
  get<HTMLButtonElement>('[aria-label="关闭通知历史"]').click(); await nextTick(); expect(document.activeElement).not.toBe(trigger);
});
it('已有移动导航模态菜单时历史面板不抢焦点或 Escape', async () => {
  const modal = document.createElement('aside'); modal.setAttribute('aria-modal', 'true'); modal.tabIndex = -1; document.body.append(modal); modal.focus();
  center.setOpen(true); await nextTick(); await nextTick(); expect(document.activeElement).toBe(modal);
  const event = new KeyboardEvent('keydown', { key: 'Escape', cancelable: true }); document.dispatchEvent(event);
  expect(center.state.open).toBe(true); expect(event.defaultPrevented).toBe(false);
});
it('键盘关闭浮层后聚焦下一条通知，最后一条关闭后回到通知按钮', async () => {
  center.notify('前缀', '已保存'); center.notify('插件', '已更新'); await nextTick();
  get<HTMLButtonElement>('[aria-label="关闭通知"]').focus(); get<HTMLButtonElement>('[aria-label="关闭通知"]').click(); await nextTick(); await nextTick();
  expect(document.activeElement).toBe(get('[aria-label="关闭通知"]'));
  get<HTMLButtonElement>('[aria-label="关闭通知"]').click(); await nextTick(); await nextTick(); expect(document.activeElement).toBe(trigger);
  expect(center.state.history).toHaveLength(2);
});
it.each(['replace', 'evict'])('自动移除聚焦通知时恢复键盘位置：%s', async mode => {
  center.notify('原通知', '原通知'); await nextTick(); get<HTMLButtonElement>('[aria-label="关闭通知"]').focus();
  if (mode === 'replace') center.notify('原通知', '新通知');
  else for (let i = 0; i < 3; i++) center.notify(`新来源${i}`, `新通知${i}`);
  await nextTick(); await nextTick(); expect(document.activeElement).toBe(get('[aria-label="关闭通知"]'));
  expect(center.state.history.some(entry => entry.message === '原通知')).toBe(true);
});
it('浮层实际进入视口后立即已读，未展示的静默条目仍未读', async () => {
  center.notify('普通', '普通消息'); center.notify('静默', '静默消息', 'info', { silent: true }); await nextTick();
  expect(center.unread.value).toBe(2); expect(targets.size).toBe(1);
  intersect(get('.notification-toast')); expect(center.unread.value).toBe(1);
  center.setOpen(true); await nextTick(); await nextTick(); expect(center.unread.value).toBe(1);
  intersect(get('.notification-history-list li')); expect(center.unread.value).toBe(0);
});
it('历史未滚动到的条目不自动已读，全部已读按钮仍可显式清除', async () => {
  for (let i = 0; i < 3; i++) center.notify('静默', `通知${i}`, 'info', { silent: true });
  center.setOpen(true); await nextTick(); await nextTick();
  const rows = document.querySelectorAll('.notification-history-list li'); intersect(rows[0]!); intersect(rows[1]!, false);
  expect(center.unread.value).toBe(2);
  get<HTMLButtonElement>('.notification-history-actions button').click(); await nextTick(); expect(center.unread.value).toBe(0);
});
it('销毁或移除条目后停止观察，晚到的可见性回调不再写入', async () => {
  center.notify('普通', '旧通知'); await nextTick(); const old = get('.notification-toast');
  center.notify('普通', '新通知'); await nextTick(); expect(targets.has(old)).toBe(false);
  intersect(old); expect(center.unread.value).toBe(2);
  app.unmount(); expect(disconnect).toHaveBeenCalledOnce(); expect(targets.size).toBe(0);
  intersect(old); expect(center.unread.value).toBe(2);
});
it('条目从仅接触边界到出现实际面积时仍能收到可见更新并标为已读', async () => {
  center.notify('静默', '边缘条目', 'info', { silent: true }); center.setOpen(true); await nextTick(); await nextTick();
  const target = get('.notification-history-list li');
  intersect(target, true, 0);
  expect(center.unread.value).toBe(1);
  const thresholds = Array.isArray(observerOptions?.threshold) ? observerOptions.threshold : [observerOptions?.threshold ?? 0];
  // isIntersecting stays true: the browser only reports this scroll if a threshold index changes.
  const indexAt = (ratio: number) => thresholds.filter(threshold => threshold <= ratio).length;
  if (indexAt(0) !== indexAt(0.005)) intersect(target, true, 0.005);
  expect(center.unread.value).toBe(0);
});
