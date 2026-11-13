import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { createApp, defineComponent, h, nextTick, reactive } from 'vue';
import LogsPage from './components/LogsPage.vue';
import type { Request } from './api';
import type { LogEntry, LogPage, PluginView } from './types';

const entry = (id = 1, overrides: Partial<LogEntry> = {}): LogEntry => ({ id, timestamp: '2026-11-13T10:00:00Z', level: 'INFO', target: 'runtime', message: `日志 ${id}`, instance_id: null, fields: {}, ...overrides });
const page = (entries: LogEntry[] = [], overrides: Partial<LogPage> = {}): LogPage => ({ session_id: 'first', entries, next_cursor: entries.at(-1)?.id ?? 0, dropped: false, has_more: false, ...overrides });
let app: ReturnType<typeof createApp> | undefined;
const request = vi.fn<(path: string) => Promise<unknown>>();
const flush = async () => { for (let i = 0; i < 6; i++) await Promise.resolve(); await nextTick(); };
const get = <T extends HTMLElement = HTMLElement>(selector: string) => document.querySelector<T>(selector)!;
const button = (label: string) => [...document.querySelectorAll('button')].find(element => element.textContent === label)!;
async function mount(options: { instanceId?: string; plugins?: PluginView[] } = {}) {
  const props = reactive(options);
  const host = document.createElement('div'); document.body.append(host);
  app = createApp(defineComponent(() => () => h(LogsPage, { request: request as Request, ...props }))); app.mount(host);
  await flush(); return props;
}
async function select(index: number, value: string) {
  const control = document.querySelectorAll('select')[index]!; control.value = value;
  control.dispatchEvent(new Event('change', { bubbles: true })); await flush();
}
async function click(label: string) { button(label).click(); await flush(); }
const lastQuery = () => new URL(request.mock.calls.at(-1)![0], 'http://localhost').searchParams;
beforeEach(() => { vi.useFakeTimers(); request.mockReset(); request.mockResolvedValue(page()); });
afterEach(() => { app?.unmount(); app = undefined; document.body.replaceChildren(); vi.useRealTimers(); });

describe('运行日志的实际 DOM 交互', () => {
  it('按真实文本渲染消息与字段，固定实例不出现插件选择器', async () => {
    request.mockResolvedValue(page([entry(1, { message: '<img src=x onerror=alert(1)>', instance_id: 'sample / #中文', fields: { detail: '<script>bad</script>' } })]));
    await mount({ instanceId: 'sample / #中文' });
    expect(document.querySelectorAll('select')).toHaveLength(1);
    expect(lastQuery().get('instance_id')).toBe('sample / #中文');
    expect(document.body.textContent).toContain('<img src=x onerror=alert(1)>');
    expect(document.body.textContent).toContain('<script>bad</script>');
    expect(document.querySelector('img, script')).toBeNull();
  });
  it('非重叠轮询从完成时间计时，携带游标，卸载后停止', async () => {
    let resolve!: (value: unknown) => void;
    request.mockImplementationOnce(() => new Promise(finish => { resolve = finish; }));
    await mount(); await vi.advanceTimersByTimeAsync(10000);
    expect(request).toHaveBeenCalledTimes(1);
    resolve(page([entry(1)])); await flush();
    await vi.advanceTimersByTimeAsync(1999); expect(request).toHaveBeenCalledTimes(1);
    request.mockResolvedValue(page([], { next_cursor: 1 }));
    await vi.advanceTimersByTimeAsync(1); expect(request).toHaveBeenCalledTimes(2); expect(lastQuery().get('after')).toBe('1');
    app!.unmount(); app = undefined; await vi.advanceTimersByTimeAsync(10000); expect(request).toHaveBeenCalledTimes(2);
  });
  it('暂停停止定时请求，手动刷新一次，继续后恢复轮询', async () => {
    await mount(); await click('暂停刷新');
    expect(button('继续刷新').getAttribute('aria-pressed')).toBe('true');
    await vi.advanceTimersByTimeAsync(10000); expect(request).toHaveBeenCalledTimes(1);
    await click('手动刷新'); expect(request).toHaveBeenCalledTimes(2);
    await vi.advanceTimersByTimeAsync(10000); expect(request).toHaveBeenCalledTimes(2);
    await click('继续刷新'); expect(request).toHaveBeenCalledTimes(3);
    await vi.advanceTimersByTimeAsync(2000); expect(request).toHaveBeenCalledTimes(4);
  });
  it('筛选清空旧窗口并丢弃过时响应，等待旧请求后才发新请求', async () => {
    let resolve!: (value: unknown) => void;
    request.mockImplementationOnce(() => new Promise(finish => { resolve = finish; }));
    await mount({ plugins: [{ instance_id: 'other', name: '其他', plugin_id: 'example' } as PluginView] });
    await select(0, 'WARN'); await select(1, 'other');
    expect(request).toHaveBeenCalledTimes(1);
    resolve(page([entry(1, { message: '过时结果' })])); await flush();
    expect(document.body.textContent).not.toContain('过时结果');
    await vi.advanceTimersByTimeAsync(0); expect(request).toHaveBeenCalledTimes(2);
    expect(lastQuery().get('level')).toBe('WARN'); expect(lastQuery().get('instance_id')).toBe('other'); expect(lastQuery().has('after')).toBe(false);
  });
  it('实例 props 更新会重新筛选，卸载后丢弃在途结果', async () => {
    const props = await mount({ instanceId: 'first' });
    props.instanceId = 'second'; await flush(); expect(lastQuery().get('instance_id')).toBe('second');
    let resolve!: (value: unknown) => void;
    request.mockImplementationOnce(() => new Promise(finish => { resolve = finish; }));
    await click('手动刷新'); app!.unmount(); app = undefined;
    resolve(page([entry(1)])); await flush(); await vi.advanceTimersByTimeAsync(10000);
    expect(request).toHaveBeenCalledTimes(3);
  });
  it('进程会话变化重新读取最近窗口，保留窗口丢失提示', async () => {
    request.mockResolvedValueOnce(page([entry(10)])); await mount();
    request.mockResolvedValueOnce(page([], { session_id: 'second', next_cursor: 1, dropped: true }));
    request.mockResolvedValueOnce(page([entry(1)], { session_id: 'second' }));
    await click('手动刷新'); expect(document.body.textContent).toContain('日志会话已变化');
    await vi.advanceTimersByTimeAsync(0); expect(lastQuery().has('after')).toBe(false);
    expect(document.body.textContent).toContain('日志 1'); expect(document.body.textContent).not.toContain('日志 10');
    request.mockResolvedValueOnce(page([entry(2)], { session_id: 'second', dropped: true }));
    await click('手动刷新'); expect(document.body.textContent).toContain('可能不连续');
  });
  it('分页及时续取且列表最多保留1000条', async () => {
    request.mockImplementation(async path => {
      const after = Number(new URL(path, 'http://localhost').searchParams.get('after') ?? 0);
      return page(Array.from({ length: 200 }, (_, index) => entry(after + index + 1)), { has_more: after < 1000 });
    });
    await mount(); expect(request).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(99); expect(request).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(401); expect(request).toHaveBeenCalledTimes(6);
    expect(document.querySelectorAll('.log-entry')).toHaveLength(1000);
    expect(get('.log-entry .log-message').textContent).toBe('日志 201');
  });
  it('连续变化的会话停止重试，避免立即轮询循环', async () => {
    request.mockResolvedValueOnce(page([entry(1)])); await mount();
    request.mockResolvedValueOnce(page([], { session_id: 'second' }));
    request.mockResolvedValueOnce(page([], { session_id: 'third' }));
    await click('手动刷新'); await vi.advanceTimersByTimeAsync(0);
    expect(document.body.textContent).toContain('日志会话持续变化');
    await vi.advanceTimersByTimeAsync(10000); expect(request).toHaveBeenCalledTimes(3);
  });
  it.each([
    { session_id: 'bad', entries: [], next_cursor: 0, dropped: false, has_more: true },
    page([entry(2), entry(1)], { next_cursor: 2 }),
    page([entry(1, { fields: { detail: 42 as unknown as string } })]),
    page([entry(Number.MAX_SAFE_INTEGER + 1)]),
  ])('拒绝坏响应并停止轮询，可手动重试', async invalid => {
    request.mockResolvedValueOnce(invalid); await mount();
    expect(document.body.textContent).toContain('日志响应格式异常');
    await vi.advanceTimersByTimeAsync(10000); expect(request).toHaveBeenCalledTimes(1);
    await click('手动刷新'); expect(request).toHaveBeenCalledTimes(2);
    expect(document.body.textContent).not.toContain('日志响应格式异常');
  });
  it('服务失败停止自动请求，暂停会丢弃在途响应', async () => {
    request.mockRejectedValueOnce(new Error('服务不可用')); await mount();
    expect(document.querySelector('[role="alert"]')?.textContent).toContain('服务不可用');
    await vi.advanceTimersByTimeAsync(10000); expect(request).toHaveBeenCalledTimes(1);
    let resolve!: (value: unknown) => void;
    request.mockImplementationOnce(() => new Promise(finish => { resolve = finish; }));
    await click('手动刷新'); await click('暂停刷新'); resolve(page([entry(1)])); await flush();
    expect(document.querySelectorAll('.log-entry')).toHaveLength(0);
    await vi.advanceTimersByTimeAsync(10000); expect(request).toHaveBeenCalledTimes(2);
  });
});
