import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { createApp, nextTick } from 'vue';
import LogsPage from '../src/components/LogsPage.vue';
import { ApiError, type Request } from '../src/api';
import { readArchiveInventory, readArchivePage } from '../src/archiveLogs';
import type { LogStreamHandlers, SubscribeLogs } from '../src/types';
let app: ReturnType<typeof createApp>;
const selector = 'a'.repeat(64), other = 'b'.repeat(64);
const file = (id = selector) => ({ selector: id, name: '20261114T000000.000+0800-p1-test-s000.jsonl.zst', month: '2026-11', bytes: 123, compressed: true, available: true });
const row = (id = 1) => ({ id, timestamp: '2026-11-14T00:00:00Z', level: 'INFO', target: 'bot_core::runtime', category: 'runtime', instance_id: null, message: 'bot runtime started', fields: { adapter_count: '1' } });
const page = (id = 1, next_cursor: string | null = null) => ({ selector, entries: [row(id)], next_cursor, skipped: 0, scanned: 1, limited: false });
const request = vi.fn<(path: string) => Promise<unknown>>();
const close = vi.fn(), streams: LogStreamHandlers[] = [];
const subscribe = vi.fn<SubscribeLogs>((_query, handlers) => { streams.push(handlers); return close; });
const flush = async () => { await new Promise<void>(resolve => setImmediate(resolve)); await nextTick(); };
const button = (label: string) => [...document.querySelectorAll('button')].find(b => b.textContent?.trim() === label)!;
async function click(label: string) { button(label).click(); await flush(); }
async function choose(value = selector) {
  const control = document.querySelector<HTMLSelectElement>('.archive-file-control select')!;
  control.value = value; control.dispatchEvent(new Event('change', { bubbles: true })); await flush();
}
async function mount() { app = createApp(LogsPage, { request: request as Request, subscribeLogs: subscribe }); app.mount(document.body.appendChild(document.createElement('div'))); await flush(); }
beforeEach(() => {
  request.mockReset(); close.mockClear(); subscribe.mockClear(); streams.length = 0;
  request.mockImplementation(async path => path === '/api/logs/archives' ? { enabled: true, files: [file(), file(other)], truncated: false } : page());
});
afterEach(() => { app?.unmount(); document.body.replaceChildren(); });
it('历史切换关闭SSE，回到实时从最近窗口重新订阅，旧响应不能更新界面', async () => {
  await mount(); expect(subscribe).toHaveBeenCalledOnce(); const stale = streams[0]!;
  await click('历史运行日志'); expect(close).toHaveBeenCalledOnce(); expect(request).toHaveBeenCalledWith('/api/logs/archives');
  stale.onError(new Error('旧流错误')); await flush(); expect(document.body.textContent).not.toContain('旧流错误');
  await choose(); expect(document.querySelectorAll('.terminal-line')).toHaveLength(1); expect(subscribe).toHaveBeenCalledOnce();
  expect(document.querySelector('.terminal-toolbar')?.textContent).toContain('ARCHIVED RUNTIME');
  expect(document.querySelector('.terminal-viewport')?.classList.contains('is-wrapping')).toBe(false);
  expect(document.body.textContent).toContain('机器人运行时已启动');
  await click('原始日志：关'); expect(document.body.textContent).toContain('bot runtime started'); expect(request).toHaveBeenCalledTimes(2);
  await click('实时日志'); expect(subscribe).toHaveBeenCalledTimes(2); expect(subscribe.mock.calls[1]![0].after).toBeUndefined();
  expect(subscribe.mock.calls[1]![0].include_content).toBe(false);
});
it('按页替换且保留旧页直至新页读取成功，支持上一页与末页', async () => {
  await mount(); await click('历史运行日志'); request.mockResolvedValueOnce(page(1,'cursor2')); await choose();
  let resolve!: (value: unknown) => void;
  request.mockImplementationOnce(() => new Promise(done => { resolve = done; })); await click('下一页');
  expect(document.querySelectorAll('.terminal-line')).toHaveLength(1); expect(document.querySelector('[data-entry-id="1"]')).not.toBeNull();
  resolve(page(201)); await flush(); expect(document.querySelector('[data-entry-id="1"]')).toBeNull();
  expect(document.querySelector('[data-entry-id="201"]')).not.toBeNull(); expect(button('下一页').disabled).toBe(true);
  expect(document.body.textContent).toContain('第 2 页'); expect(request.mock.calls.at(-1)![0]).toContain('cursor=cursor2');
  request.mockResolvedValueOnce(page(1,'cursor2')); await click('上一页'); expect(document.body.textContent).toContain('第 1 页');
});
it('切回实时并重开历史后迟到HTTP不能注入旧文件数据', async () => {
  await mount(); await click('历史运行日志'); let resolve!: (value: unknown) => void;
  request.mockImplementationOnce(() => new Promise(done => { resolve = done; })); await choose(); await click('实时日志');
  await click('历史运行日志'); resolve(page(77)); await flush(); expect(document.querySelector('.terminal-line')).toBeNull();
});
it('轮转错误保留已读页但停止翻页，刷新文件清单后才能重选', async () => {
  await mount(); await click('历史运行日志'); request.mockResolvedValueOnce(page(1,'cursor2')); await choose();
  request.mockRejectedValueOnce(new ApiError('generic conflict',409)); await click('下一页');
  expect(document.querySelector('[role="alert"]')?.textContent).toContain('归档已变化或轮转'); expect(button('下一页').disabled).toBe(true);
  await click('刷新文件列表'); expect(document.querySelector('.terminal-line')).toBeNull(); expect(document.querySelector('[role="alert"]')).toBeNull();
});
it('未启用、空清单、截断、不可读取文件、跳过行及扫描上限都有明确状态', async () => {
  await mount(); request.mockResolvedValueOnce({ enabled: false, files: [], truncated: false }); await click('历史运行日志');
  expect(document.body.textContent).toContain('文件日志未启用');
  request.mockResolvedValueOnce({ enabled: true, files: [], truncated: false }); await click('刷新文件列表'); expect(document.body.textContent).toContain('暂无运行日志文件');
  request.mockResolvedValueOnce({ enabled: true, files: [file(), { ...file(other), available: false }], truncated: true }); await click('刷新文件列表');
  expect(document.body.textContent).toContain('当前仅显示部分文件'); expect(document.querySelectorAll('option')[2]?.disabled).toBe(true);
  request.mockResolvedValueOnce({ selector, entries: [], next_cursor: null, skipped: 12, scanned: 12, limited: true }); await choose();
  expect(document.body.textContent).toContain('跳过 12 行'); expect(document.body.textContent).toContain('后续内容未读取'); expect(button('下一页').disabled).toBe(true);
});
it('拒绝超限清单、跨文件页、消息正文以及重复游标，DOM保持有界', async () => {
  expect(() => readArchiveInventory({ enabled: true, files: Array(129).fill(file()), truncated: true })).toThrow();
  expect(() => readArchivePage({ ...page(), selector: other }, selector)).toThrow();
  expect(() => readArchivePage({ ...page(), entries: [{ ...row(), category: 'message', content: 'private' }] }, selector)).toThrow();
  expect(() => readArchivePage({ ...page(), entries: Array.from({length:201},(_,i)=>row(i+1)), scanned: 201 }, selector)).toThrow();
  await mount(); await click('历史运行日志');
  request.mockResolvedValueOnce({ ...page(), entries: Array.from({length:200},(_,i)=>row(i+1)), scanned:200, next_cursor:'same' }); await choose();
  expect(document.querySelectorAll('.terminal-line')).toHaveLength(200);
  request.mockResolvedValueOnce({ ...page(), next_cursor:'same' }); await click('下一页'); expect(document.querySelector('[role="alert"]')?.textContent).toContain('游标未前进');
  expect(document.querySelectorAll('.terminal-line')).toHaveLength(200);
});
