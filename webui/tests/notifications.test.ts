import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { createApp, defineComponent, h, nextTick, provide, ref } from 'vue';
import { createNotifications, notificationKey, useNoticeSource, type NotificationCenter } from '../src/notifications';

let center: NotificationCenter, visibility = 'visible';
beforeEach(() => {
  vi.useFakeTimers(); visibility = 'visible';
  vi.spyOn(document, 'hasFocus').mockReturnValue(true);
  vi.spyOn(document, 'visibilityState', 'get').mockImplementation(() => visibility as DocumentVisibilityState);
  center = createNotifications();
});
afterEach(() => { center.dispose(); vi.useRealTimers(); vi.restoreAllMocks(); document.body.replaceChildren(); });
const advance = (ms: number) => vi.advanceTimersByTime(ms);
const active = () => center.state.active.map(entry => entry.message);
const hide = (value: boolean) => { visibility = value ? 'hidden' : 'visible'; document.dispatchEvent(new Event('visibilitychange')); };

describe('通知计时与会话历史', () => {
  it('累计前台5秒后收起，关闭浮层仍能在历史找回', () => {
    center.notify('控制台', '已保存', 'success'); advance(4999); expect(active()).toEqual(['已保存']);
    advance(1); expect(active()).toEqual([]); expect(center.state.history[0]?.message).toBe('已保存');
    expect(vi.getTimerCount()).toBe(0);
  });
  it('窗口失焦与标签隐藏独立暂停，只消耗实际前台时间', () => {
    center.notify('控制台', '完成'); advance(1700); window.dispatchEvent(new Event('blur'));
    advance(60000); hide(true); window.dispatchEvent(new Event('focus')); advance(60000);
    expect(active()).toHaveLength(1); hide(false); advance(3299); expect(active()).toHaveLength(1);
    advance(1); expect(active()).toEqual([]);
  });
  it('首次加载在后台时不启动计时，激活窗口后才开始', () => {
    center.dispose(); vi.mocked(document.hasFocus).mockReturnValue(false); center = createNotifications();
    center.notify('控制台', '后台消息'); advance(60000); expect(active()).toHaveLength(1);
    window.dispatchEvent(new Event('focus')); advance(5000); expect(active()).toEqual([]);
  });
  it('移动菜单遮挡期间，已有通知和新到通知都暂停，恢复后继续剩余时间', () => {
    center.notify('已有', '已有通知'); advance(1000); center.setSuspended(true);
    center.notify('新到', '新到通知'); advance(60000); expect(active()).toHaveLength(2);
    center.setSuspended(false); advance(3999); expect(active()).toHaveLength(2);
    advance(1); expect(active()).toEqual(['新到通知']); advance(1000); expect(active()).toEqual([]);
  });
  it('历史面板遮住浮层时独立暂停，关闭后继续剩余时间', () => {
    center.notify('已有', '已有通知'); advance(1000); center.setOpen(true);
    center.notify('新到', '新到通知'); advance(60000); expect(active()).toHaveLength(2);
    center.setSuspended(true); center.setOpen(false); advance(60000); expect(active()).toHaveLength(2);
    center.setSuspended(false); advance(4000); expect(active()).toEqual(['新到通知']); advance(1000); expect(active()).toEqual([]);
  });
  it('悬停和键盘焦点独立暂停，离开一个仍保持另一个暂停原因', () => {
    const id = center.notify('测试', '提示')!; advance(1000);
    center.setPaused(id, 'pointer', true); center.setPaused(id, 'focus', true); advance(10000);
    center.setPaused(id, 'pointer', false); advance(10000); expect(active()).toHaveLength(1);
    center.setPaused(id, 'focus', false); advance(3999); expect(active()).toHaveLength(1);
    advance(1); expect(active()).toHaveLength(0);
  });
  it('错误与警告保留，进行中反馈被完成反馈替换且不污染历史', () => {
    center.notify('前缀', '保存中', 'progress'); center.notify('日志', '断开', 'error'); center.notify('市场', '需要重启', 'warning');
    advance(60000); expect(active()).toEqual(['保存中', '断开', '需要重启']); expect(center.state.history).toHaveLength(2);
    center.notify('前缀', '已保存', 'success'); expect(active()).toEqual(['断开', '需要重启', '已保存']);
    center.clearSource('前缀', true); expect(active()).toContain('已保存');
    center.dismiss(center.state.active[0]!.id); expect(center.state.history).toHaveLength(3);
  });
  it('最多3条浮层和100条倒序历史，被挤出的记录仍在历史', () => {
    for (let i = 0; i < 104; i++) center.notify(`来源${i}`, `通知${i}`);
    expect(active()).toEqual(['通知101', '通知102', '通知103']);
    expect(center.state.history).toHaveLength(100); expect(center.state.history[99]?.message).toBe('通知4');
    expect(center.unread.value).toBe(100); expect(vi.getTimerCount()).toBe(3);
  });
  it('实际展示的前台浮层立即已读，后台和遮挡不算展示', () => {
    const id = center.notify('测试', '一')!; expect(center.unread.value).toBe(1);
    window.dispatchEvent(new Event('blur')); center.setVisible(id, 'toast', true); expect(center.unread.value).toBe(1);
    hide(true); window.dispatchEvent(new Event('focus')); expect(center.unread.value).toBe(1);
    center.setSuspended(true); hide(false); expect(center.unread.value).toBe(1);
    center.setSuspended(false); expect(center.unread.value).toBe(0);
    const next = center.notify('测试', '二')!; center.setOpen(true); center.setVisible(next, 'toast', true);
    expect(center.unread.value).toBe(1); center.setOpen(false); expect(center.unread.value).toBe(0);
  });
  it('静默通知只记历史且不替换同来源浮层，打开历史只读实际可见的条目', () => {
    const toast = center.notify('测试', '普通通知')!; center.setVisible(toast, 'toast', true);
    const first = center.notify('测试', '静默一', 'info', { silent: true })!;
    const second = center.notify('测试', '静默二', 'warning', { silent: true })!;
    expect(active()).toEqual(['普通通知']); expect(center.unread.value).toBe(2); expect(vi.getTimerCount()).toBe(1);
    center.setOpen(true); expect(center.unread.value).toBe(2);
    center.setVisible(first, 'history', true); expect(center.unread.value).toBe(1);
    center.setVisible(second, 'history', false); expect(center.state.history[0]?.unread).toBe(true);
    center.markAllRead(); expect(center.unread.value).toBe(0);
  });
  it('静默通知遵循历史容量，清理会话后旧可见回调不影响新通知', () => {
    for (let i = 0; i < 104; i++) center.notify('静默', `通知${i}`, 'info', { silent: true });
    expect(active()).toEqual([]); expect(center.state.history).toHaveLength(100); expect(center.unread.value).toBe(100); expect(vi.getTimerCount()).toBe(0);
    const old = center.state.history[0]!.id; center.reset(); center.setVisible(old, 'history', true);
    center.notify('静默', '新通知', 'info', { silent: true }); expect(center.unread.value).toBe(1);
  });
  it('清空历史同时移除对应浮层，重置清除整个会话并取消定时器', () => {
    center.notify('日志', '完成'); center.notify('任务', '进行中', 'progress'); center.setOpen(true);
    center.clearHistory(); expect(active()).toEqual(['进行中']); expect(center.state.history).toEqual([]);
    center.notify('控制台', '就绪'); center.reset(); expect(active()).toEqual([]);
    expect(center.state.history).toEqual([]); expect(center.state.open).toBe(false); expect(vi.getTimerCount()).toBe(0);
  });
  it('dispose 取消计时并移除监听，后续事件与异步通知不能再写入', () => {
    const remove = vi.spyOn(window, 'removeEventListener'), documentRemove = vi.spyOn(document, 'removeEventListener');
    center.notify('测试', '消息'); center.dispose(); window.dispatchEvent(new Event('focus'));
    center.notify('旧请求', '晚到通知'); advance(6000); expect(center.state.history).toEqual([]); expect(vi.getTimerCount()).toBe(0);
    expect(remove).toHaveBeenCalledWith('focus', expect.any(Function)); expect(remove).toHaveBeenCalledWith('blur', expect.any(Function));
    expect(documentRemove).toHaveBeenCalledWith('visibilitychange', expect.any(Function));
  });
  it('页面卸载只清理进行中反馈，已完成消息及历史继续保留', async () => {
    const message = ref('已完成'), progress = ref(false), shown = ref(true);
    const child = defineComponent({ setup() { useNoticeSource('子页', () => message.value, () => progress.value ? 'progress' : 'success'); return () => null; } });
    const app = createApp(defineComponent({ setup() { provide(notificationKey, center); return () => shown.value ? h(child) : null; } }));
    app.mount(document.body.appendChild(document.createElement('div')));
    shown.value = false; await nextTick(); expect(active()).toEqual(['已完成']);
    shown.value = true; progress.value = true; message.value = '读取中'; await nextTick();
    shown.value = false; await nextTick(); expect(active()).toEqual([]); expect(center.state.history).toHaveLength(1);
    app.unmount();
  });
});
