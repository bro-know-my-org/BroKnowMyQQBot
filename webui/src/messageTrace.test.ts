import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { createApp, defineComponent, h, nextTick, reactive } from 'vue';
import MessageTrace from './components/MessageTrace.vue';
import { readTracePage, traceReading, traceTarget, type TraceTarget } from './messageTrace';
import type { Request } from './api';
import type { LogEntry } from './types';
const target: TraceTarget = { session_id: 'session', event_id: 'e' };
const entry = (id = 1, fields: Record<string, string> = { record_kind: 'message.received', event_id: 'e' }): LogEntry => ({
  id, category: 'message', timestamp: '2026-11-14T10:00:00Z', level: 'INFO', target: 'bkm::message_trace', message: 'received', instance_id: null, fields,
});
const page = (entries = [entry()], extra = {}) => ({ ...target, invocation_id: null, entries, next_cursor: entries.at(-1)?.id ?? 0,
  has_more: false, partial: true, retention_evicted: false, oldest_id: 1, latest_id: 300, filter_scope: 'all_retained_levels_and_instances', ...extra });
const flush = async () => { for (let i = 0; i < 6; i++) await Promise.resolve(); await nextTick(); };
const request = vi.fn<Request>();
let app: ReturnType<typeof createApp> | undefined;
let trigger: HTMLButtonElement;
beforeEach(() => {
  request.mockReset(); request.mockResolvedValue(page());
  Object.defineProperty(HTMLDialogElement.prototype, 'showModal', { configurable: true, value: function (this: HTMLDialogElement) { this.setAttribute('open', ''); } });
  Object.defineProperty(HTMLDialogElement.prototype, 'close', { configurable: true, value: function (this: HTMLDialogElement) { this.removeAttribute('open'); } });
});
afterEach(() => { app?.unmount(); app = undefined; document.body.replaceChildren(); vi.restoreAllMocks(); });
async function mount(body = false) {
  const props = reactive({ request: request as Request, target: { ...target }, includeContent: body, open: true });
  const host = document.createElement('div'); trigger = document.createElement('button'); trigger.textContent = 'entry';
  document.body.append(trigger, host); trigger.focus();
  app = createApp(defineComponent(() => () => props.open ? h(MessageTrace, { ...props, onClose: () => { props.open = false; } }) : null));
  app.mount(host); await flush(); return props;
}
const button = (label: string) => [...document.querySelectorAll('dialog button')].find(item => item.textContent === label) as HTMLButtonElement;
it('only explicit safe event/invocation identities open a trace; command IDs alone never join', () => {
  expect(traceTarget(entry(), 's')).toEqual({ session_id: 's', event_id: 'e' });
  expect(traceTarget(entry(), undefined)).toBeUndefined();
  expect(traceTarget(entry(1, { record_kind: 'plugin.completed', invocation_id: 'i' }), 's')).toEqual({ session_id: 's', invocation_id: 'i' });
  for (const id of ['', 'x'.repeat(509), 'x'.repeat(507) + '🦀', '[REDACTED]', 'e\\n']) {
    expect(traceTarget(entry(1, { record_kind: 'message.received', event_id: id }), 's')).toBeUndefined();
  }
  expect(traceTarget(entry(1, { record_kind: 'plugin.completed', command_id: 'reply' }), 's')).toBeUndefined();
});
it('validates process identity, partial scope, page bound and monotonic cursor', () => {
  expect(readTracePage(page(), target, false).entries).toHaveLength(1);
  for (const extra of [{ session_id: 'other' }, { partial: false }, { filter_scope: 'complete' }, { event_id: 'other' }, { entries: Array.from({ length: 201 }, (_, i) => entry(i + 1)) }, { latest_id: 0 }, { next_cursor: -1 }]) {
    expect(() => readTracePage(page(undefined, extra), target, false)).toThrow();
  }
  expect(() => readTracePage(page(), target, false, 1)).toThrow();
});
it('default trace is metadata-only, makes one read and explains global incomplete scope', async () => {
  request.mockResolvedValue(page([{ ...entry(), content: 'must-hide' }], { retention_evicted: true, has_more: true })); await mount();
  const url = new URL(request.mock.calls[0]![0], 'http://localhost');
  expect(url.pathname).toBe('/api/logs/trace'); expect(url.searchParams.get('include_content')).toBe('false');
  expect(url.searchParams.has('instance_id')).toBe(false); expect(request).toHaveBeenCalledOnce();
  expect(document.body.textContent).toContain('跨全部实例'); expect(document.body.textContent).toContain('发生淘汰');
  expect(document.body.textContent).toContain('已分页'); expect(document.body.textContent).not.toContain('must-hide');
  expect(document.querySelectorAll('.trace-timeline li')).toHaveLength(1);
});
it('distinguishes candidate, actual invocation, no output and canonical unknown result', async () => {
  const invoked = entry(1, { record_kind: 'plugin.invoked', invocation_id: 'i', command_name: 'ping', plugin_id: 'dev.bkm.ping' });
  const none = entry(2, { record_kind: 'plugin.completed', status: 'no_output', invocation_id: 'other', command_name: 'ping' });
  const result = entry(3, { record_kind: 'plugin.action.completed', status: 'unknown', invocation_id: 'i', command_id: 'reply', action_type: 'message.reply' });
  request.mockResolvedValue(page([invoked, none, result])); await mount();
  expect(document.body.textContent).toContain('实际调用'); expect(document.body.textContent).toContain('声明匹配的候选指令：ping');
  expect(document.body.textContent).toContain('插件未产生输出'); expect(document.body.textContent).toContain('结果未知');
  expect(document.body.textContent).toContain('命令：reply');
  expect(traceReading(entry(1, { record_kind: 'message.failed', status: 'failed' })).status).toBe('适配器报错');
});
it('body stays text; disabling body clears retained content and aborts old responses', async () => {
  request.mockResolvedValue(page([{ ...entry(), content: '<img src=x>retained' }])); const props = await mount(true);
  expect(document.querySelector('.trace-body')?.textContent).toBe('<img src=x>retained'); expect(document.querySelector('img')).toBeNull();
  let resolve!: (value: unknown) => void;
  request.mockImplementationOnce(() => new Promise(finish => { resolve = finish as (value: unknown) => void; }));
  button('重新读取').click(); await flush(); const signal = request.mock.calls[1]![2]!;
  props.includeContent = false; await flush(); expect(signal.aborted).toBe(true);
  resolve(page([{ ...entry(), content: 'late-body' }])); await flush();
  expect(document.body.textContent).not.toContain('retained'); expect(document.body.textContent).not.toContain('late-body'); expect(document.querySelector('.trace-body')).toBeNull();
});
it('Escape cancels transport, ignores late result and restores trigger focus', async () => {
  let resolve!: (value: unknown) => void;
  request.mockImplementationOnce(() => new Promise(finish => { resolve = finish as (value: unknown) => void; })); await mount();
  expect(document.activeElement).toBe(document.querySelector('dialog')); const signal = request.mock.calls[0]![2]!;
  document.querySelector('dialog')!.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true })); await flush();
  expect(signal.aborted).toBe(true); expect(document.querySelector('dialog')).toBeNull(); expect(document.activeElement).toBe(trigger);
  resolve(page([{ ...entry(), content: 'late-result' }])); await flush(); expect(document.body.textContent).not.toContain('late-result');
});
it('native cancel closes the dialog and Tab stays within modal controls', async () => {
  const props = await mount(), dialog = document.querySelector('dialog')!;
  dialog.dispatchEvent(new KeyboardEvent('keydown', { key: 'Tab', bubbles: true, cancelable: true })); expect(document.activeElement).toBe(button('关闭链路'));
  button('关闭链路').dispatchEvent(new KeyboardEvent('keydown', { key: 'Tab', shiftKey: true, bubbles: true, cancelable: true })); expect(document.activeElement?.tagName).toBe('SUMMARY');
  dialog.dispatchEvent(new Event('cancel', { cancelable: true })); await flush(); expect(props.open).toBe(false);
});
it('session changes invalidate pending work; next page replaces bounded DOM', async () => {
  request.mockResolvedValueOnce(page([entry()], { has_more: true })); const props = await mount();
  request.mockResolvedValueOnce(page([entry(2)])); button('下一页（最多 200 条）').click(); await flush();
  expect(request.mock.calls[1]![0]).toContain('after=1'); expect(document.querySelectorAll('.trace-timeline li')).toHaveLength(1);
  let resolve!: (value: unknown) => void;
  request.mockImplementationOnce(() => new Promise(finish => { resolve = finish as (value: unknown) => void; }));
  button('重新读取').click(); await flush(); const signal = request.mock.calls[2]![2]!;
  request.mockResolvedValueOnce(page([], { session_id: 'new-session', next_cursor: 0 })); props.target.session_id = 'new-session'; await flush();
  expect(signal.aborted).toBe(true); resolve(page()); await flush(); expect(document.body.textContent).toContain('new-session'); expect(document.body.textContent).toContain('没有可取回');
});
it('shows explicit errors and empty state without an automatic retry loop', async () => {
  request.mockRejectedValueOnce(new Error('日志进程会话已变化，无法取回旧链路')); await mount();
  expect(document.querySelector('[role="alert"]')?.textContent).toContain('无法取回旧链路'); expect(request).toHaveBeenCalledOnce();
  request.mockResolvedValueOnce(page([])); button('重新读取').click(); await flush(); expect(document.body.textContent).toContain('没有可取回'); expect(document.querySelector('[role="alert"]')).toBeNull();
});
