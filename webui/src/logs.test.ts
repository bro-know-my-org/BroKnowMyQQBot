import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { createApp, defineComponent, h, nextTick, reactive } from 'vue';
import LogsPage from './components/LogsPage.vue';
import type { Request } from './api';
import type { LogEntry, LogPage, LogQuery, LogStreamHandlers, PluginView, SubscribeLogs } from './types';

const entry = (id = 1, overrides: Partial<LogEntry> = {}): LogEntry => ({ id, timestamp: '2026-11-13T10:00:00Z', level: 'INFO', target: 'runtime', message: `日志 ${id}`, instance_id: null, fields: {}, ...overrides });
const page = (entries: LogEntry[] = [], overrides: Partial<LogPage> = {}): LogPage => ({ session_id: 'first', entries, next_cursor: entries.at(-1)?.id ?? 0, dropped: false, has_more: false, ...overrides });
let app: ReturnType<typeof createApp> | undefined;
const request = vi.fn<(path: string) => Promise<unknown>>();
const streams: Array<{ query: LogQuery; handlers: LogStreamHandlers; close: ReturnType<typeof vi.fn> }> = [];
const subscribeLogs = vi.fn<SubscribeLogs>((query, handlers) => {
  const close = vi.fn(); streams.push({ query, handlers, close }); handlers.onState('connecting'); return close;
});
const latest = () => streams.at(-1)!;
const flush = async () => { for (let i = 0; i < 6; i++) await Promise.resolve(); await vi.advanceTimersByTimeAsync(20); await nextTick(); };
const get = <T extends HTMLElement = HTMLElement>(selector: string) => document.querySelector<T>(selector)!;
const button = (label: string) => [...document.querySelectorAll('button')].find(element => element.textContent === label)!;
async function mount(options: { instanceId?: string; plugins?: PluginView[] } = {}) {
  const props = reactive(options), host = document.createElement('div'); document.body.append(host);
  app = createApp(defineComponent(() => () => h(LogsPage, { request: request as Request, subscribeLogs, ...props }))); app.mount(host);
  await flush(); return props;
}
async function emit(value: LogPage) { latest().handlers.onPage(value); await flush(); }
async function select(index: number, value: string) {
  const control = document.querySelectorAll('select')[index]!; control.value = value;
  control.dispatchEvent(new Event('change', { bubbles: true })); await flush();
}
async function click(label: string) { button(label).click(); await flush(); }
beforeEach(() => { vi.useFakeTimers(); streams.length = 0; subscribeLogs.mockClear(); request.mockReset(); request.mockResolvedValue(page()); });
afterEach(() => { app?.unmount(); app = undefined; document.body.replaceChildren(); vi.useRealTimers(); });

describe('持续日志的实际 DOM 交互', () => {
  it('突发一万条只保留一个1000条待渲染窗口，一帧批量提交', async () => {
    await mount(); await emit(page([entry(1)]));
    const content = get('.terminal-lines'), changes: MutationRecord[] = [];
    const observer = new MutationObserver(records => changes.push(...records)); observer.observe(content, { childList: true });
    for (let start = 2; start <= 10000; start += 200) {
      latest().handlers.onPage(page(Array.from({ length: Math.min(200, 10001 - start) }, (_, i) => entry(start + i))));
    }
    await nextTick(); expect(content.children).toHaveLength(1); expect(changes).toHaveLength(0);
    await flush(); expect(content.children).toHaveLength(1000); expect(get('.terminal-message').textContent).toBe('日志 9001');
    expect(content.lastElementChild?.textContent).toContain('日志 10000'); observer.disconnect();
    await click('暂停接收'); await click('继续接收'); expect(latest().query.after).toBe(10000);
  });
  it('暂停快照跨进程时无游标重取最近窗口，进程连续变化时明确失败', async () => {
    await mount(); await emit(page([entry(10)])); await click('暂停接收');
    request.mockResolvedValueOnce(page([entry(11)], { session_id: 'second' }))
      .mockResolvedValueOnce(page([entry(1), entry(11)], { session_id: 'second' }));
    await click('读取最新日志'); expect(request).toHaveBeenCalledTimes(2);
    expect(request.mock.calls[1]![0]).not.toContain('after='); expect(get('.terminal-message').textContent).toBe('日志 1');
    expect(document.body.textContent).toContain('日志会话已变化');
    request.mockResolvedValueOnce(page([entry(12)], { session_id: 'third' }))
      .mockResolvedValueOnce(page([entry(13)], { session_id: 'fourth' }));
    await click('读取最新日志'); expect(request).toHaveBeenCalledTimes(4);
    expect(document.body.textContent).toContain('日志会话持续变化'); expect(get('.terminal-message').textContent).toBe('日志 1');
  });
  it('按真实文本渲染，固定实例锁定筛选，两种视图共享同一订阅和暂停状态', async () => {
    await mount({ instanceId: 'sample / #中文' });
    await emit(page([entry(1, { message: '<img src=x onerror=alert(1)>', instance_id: 'sample / #中文', fields: { detail: '<script>bad</script>' } })]));
    expect(document.querySelectorAll('select')).toHaveLength(1);
    expect(latest().query).toEqual({ level: 'INFO', limit: 200, instance_id: 'sample / #中文' });
    expect(document.body.textContent).toContain('<img src=x onerror=alert(1)>');
    expect(document.body.textContent).toContain('<script>bad</script>'); expect(document.querySelector('img, script')).toBeNull();
    await click('暂停接收'); await click('结构化记录');
    expect(document.querySelector('.terminal-viewport')).toBeNull();
    expect(document.querySelector('.log-list details')?.textContent).toContain('<script>bad</script>');
    await click('实时输出'); expect(button('继续接收').getAttribute('aria-pressed')).toBe('true');
    expect(subscribeLogs).toHaveBeenCalledTimes(1); expect(request).not.toHaveBeenCalled();
  });
  it('无2秒轮询，空控制页不修改终端节点或文字，游标仍用于续接', async () => {
    await mount(); await emit(page([entry(1)]));
    const row = get('.terminal-line'), viewport = get('.terminal-viewport'), text = document.body.textContent;
    const mutations: MutationRecord[] = [], observer = new MutationObserver(records => mutations.push(...records));
    observer.observe(get('.terminal-panel'), { childList: true, characterData: true, subtree: true });
    for (let cursor = 2; cursor <= 5; cursor++) await emit(page([], { next_cursor: cursor }));
    await vi.advanceTimersByTimeAsync(20000);
    expect(get('.terminal-line')).toBe(row); expect(get('.terminal-viewport')).toBe(viewport);
    expect(document.body.textContent).toBe(text); expect(mutations).toEqual([]); observer.disconnect();
    expect(subscribeLogs).toHaveBeenCalledTimes(1); expect(request).not.toHaveBeenCalled();
    await click('重新连接'); expect(latest().query).toMatchObject({ after: 5, session_id: 'first' });
  });
  it('追加、重放去重、保留旧行和最多1000条，两种视图数量一致', async () => {
    await mount(); await emit(page([entry(1)])); const row = get('.terminal-line');
    await emit(page([entry(1, { message: '重复条目不得覆盖旧对象' }), entry(2)]));
    expect(get('.terminal-line')).toBe(row); expect(get('.terminal-message').textContent).toBe('日志 1');
    expect(document.querySelectorAll('.terminal-line')).toHaveLength(2);
    for (let start = 3; start <= 1003; start += 200) {
      const count = Math.min(200, 1003 - start + 1);
      await emit(page(Array.from({ length: count }, (_, index) => entry(start + index)), { has_more: start < 1003 }));
    }
    expect(document.querySelectorAll('.terminal-line')).toHaveLength(1000);
    expect(get('.terminal-message').textContent).toBe('日志 4');
    await click('结构化记录'); expect(document.querySelectorAll('.log-entry')).toHaveLength(1000);
    expect(subscribeLogs).toHaveBeenCalledTimes(1); expect(request).not.toHaveBeenCalled();
  });
  it('暂停关闭流，手动更新只GET一次，继续使用新的游标和会话', async () => {
    await mount(); await emit(page([entry(1)])); const old = latest(); await click('暂停接收');
    expect(old.close).toHaveBeenCalledTimes(1);
    old.handlers.onPage(page([entry(2)])); await flush(); expect(document.querySelectorAll('.terminal-line')).toHaveLength(1);
    request.mockResolvedValueOnce(page([entry(3)])); await click('读取最新日志');
    const query = new URL(request.mock.calls[0]![0], 'http://localhost').searchParams;
    expect(query.get('after')).toBe('1'); expect(query.get('limit')).toBe('200');
    expect(button('继续接收')).toBeDefined(); await vi.advanceTimersByTimeAsync(10000);
    expect(request).toHaveBeenCalledTimes(1); expect(subscribeLogs).toHaveBeenCalledTimes(1);
    await click('继续接收'); expect(latest().query).toMatchObject({ after: 3, session_id: 'first' });
    expect(document.querySelectorAll('.terminal-line')).toHaveLength(2);
  });
  it('筛选切换与实例props更新关闭旧流并丢弃迟到回调，暂停中筛选不恢复采集', async () => {
    const props = await mount({ instanceId: 'first' }); await emit(page([entry(1)])); const old = latest();
    await select(0, 'WARN'); expect(old.close).toHaveBeenCalledTimes(1);
    old.handlers.onPage(page([entry(2)])); old.handlers.onError(new Error('旧错误')); old.handlers.onState('reconnecting'); await flush();
    expect(document.querySelectorAll('.terminal-line')).toHaveLength(0); expect(document.querySelector('[role="alert"]')).toBeNull();
    expect(latest().query).toEqual({ level: 'WARN', limit: 200, instance_id: 'first' });
    props.instanceId = 'second'; await flush(); expect(latest().query.instance_id).toBe('second');
    await click('暂停接收'); const count = streams.length; await select(0, 'ERROR'); expect(streams).toHaveLength(count);
    await click('读取最新日志'); expect(request.mock.calls[0]![0]).toContain('level=ERROR'); expect(streams).toHaveLength(count);
  });
  it('全局实例筛选重开流并移除旧游标', async () => {
    await mount({ plugins: [{ instance_id: 'other', name: '其他', plugin_id: 'example' } as PluginView] });
    await emit(page([entry(1)])); await select(1, 'other');
    expect(streams[0]!.close).toHaveBeenCalledTimes(1); expect(latest().query).toEqual({ level: 'INFO', limit: 200, instance_id: 'other' });
  });
  it('断开和原生重连保持输出与按钮文字，空重连页不替换记录', async () => {
    await mount(); await emit(page([entry(1)])); const row = get('.terminal-line');
    latest().handlers.onState('reconnecting'); await flush();
    expect(document.body.textContent).toContain('连接中断，重连中'); expect(get('.terminal-line')).toBe(row);
    expect(button('重新连接').disabled).toBe(false); expect(button('暂停接收')).toBeDefined();
    latest().handlers.onState('open'); await emit(page([], { next_cursor: 2 }));
    expect(get('.terminal-line')).toBe(row); expect(document.body.textContent).toContain('实时接收中');
    expect(subscribeLogs).toHaveBeenCalledTimes(1); expect(request).not.toHaveBeenCalled();
  });
  it('服务重启替换窗口，窗口丢失及重启提示持续保留', async () => {
    await mount(); await emit(page([entry(10)]));
    await emit(page([entry(1)], { session_id: 'second', dropped: true }));
    expect(document.body.textContent).toContain('日志会话已变化'); expect(get('.terminal-message').textContent).toBe('日志 1');
    await emit(page([], { session_id: 'second', next_cursor: 2 })); expect(document.body.textContent).toContain('日志会话已变化');
    await emit(page([entry(9)], { session_id: 'second', dropped: true }));
    expect(document.body.textContent).toContain('可能不连续'); await click('重新连接');
    expect(latest().query).toMatchObject({ after: 9, session_id: 'second' });
  });
  it('损坏消息关闭流并明确手动重试，保留已有记录', async () => {
    await mount(); await emit(page([entry(1)])); const old = latest();
    old.handlers.onError(new Error('日志响应格式异常，已停止接收，请手动重试。')); await flush();
    expect(old.close).toHaveBeenCalledTimes(1); expect(document.body.textContent).toContain('日志响应格式异常');
    expect(get('.terminal-message').textContent).toBe('日志 1'); await vi.advanceTimersByTimeAsync(10000);
    expect(subscribeLogs).toHaveBeenCalledTimes(1); await click('重新连接'); expect(streams).toHaveLength(2);
    await emit(page([entry(2)])); expect(document.querySelector('[role="alert"]')).toBeNull();
  });
  it('暂停快照失败或格式损坏可重试，迟到HTTP不污染新订阅，卸载释放流', async () => {
    await mount(); await emit(page([entry(1)])); await click('暂停接收');
    request.mockResolvedValueOnce(page([entry(3), entry(2)], { next_cursor: 3 })); await click('读取最新日志');
    expect(document.body.textContent).toContain('日志响应格式异常');
    request.mockRejectedValueOnce(new Error('服务不可用')); await click('读取最新日志'); expect(document.body.textContent).toContain('服务不可用');
    let resolve!: (value: unknown) => void;
    request.mockImplementationOnce(() => new Promise(finish => { resolve = finish; })); await click('读取最新日志');
    await click('继续接收'); resolve(page([entry(8)])); await flush();
    expect(document.body.textContent).not.toContain('日志 8'); const old = latest(); app!.unmount(); app = undefined;
    expect(old.close).toHaveBeenCalledTimes(1); old.handlers.onPage(page([entry(9)])); await flush();
    await vi.advanceTimersByTimeAsync(10000); expect(request).toHaveBeenCalledTimes(3);
  });
});
