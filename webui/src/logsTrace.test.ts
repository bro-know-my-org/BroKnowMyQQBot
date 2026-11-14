import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { createApp, nextTick } from 'vue';
import LogsPage from './components/LogsPage.vue';
import type { Request } from './api';
import type { LogEntry, LogStreamHandlers, SubscribeLogs } from './types';
const row = (id: number, fields: Record<string, string> = { record_kind: 'plugin.completed', event_id: 'e', status: 'no_output' }): LogEntry => ({
  id, category: 'runtime', timestamp: '2026-11-14T10:00:00Z', level: 'INFO', target: 'plugin_host::runtime', message: 'completed', instance_id: 'ping-a', fields,
});
let app: ReturnType<typeof createApp> | undefined, handlers: LogStreamHandlers;
const request = vi.fn<Request>(), close = vi.fn();
const subscribe = vi.fn<SubscribeLogs>((_, next) => { handlers = next; return close; });
const flush = async () => { for (let i = 0; i < 5; i++) await Promise.resolve(); await vi.advanceTimersByTimeAsync(20); await nextTick(); };
const button = (label: string) => [...document.querySelectorAll('button')].find(item => item.textContent === label) as HTMLButtonElement;
function push(session = 's', entries = [row(1)]) {
  handlers.onPage({ session_id: session, entries, next_cursor: entries.at(-1)?.id ?? 0, dropped: false, has_more: false });
}
async function mount() {
  const host = document.createElement('div'); host.id = 'page-content'; host.tabIndex = -1; document.body.append(host);
  app = createApp(LogsPage, { request, subscribeLogs: subscribe, instanceId: 'ping-a' }); app.mount(host); await flush();
  push(); await flush();
}
beforeEach(() => {
  vi.useFakeTimers(); request.mockReset(); subscribe.mockClear(); close.mockClear();
  request.mockImplementation(async () => ({ session_id: 's', event_id: 'e', invocation_id: null, entries: [row(1)], next_cursor: 1,
    has_more: false, partial: true, retention_evicted: false, oldest_id: 1, latest_id: 1, filter_scope: 'all_retained_levels_and_instances' }) as never);
  Object.defineProperty(HTMLDialogElement.prototype, 'showModal', { configurable: true, value: function (this: HTMLDialogElement) { this.setAttribute('open', ''); } });
  Object.defineProperty(HTMLDialogElement.prototype, 'close', { configurable: true, value: function (this: HTMLDialogElement) { this.removeAttribute('open'); } });
});
afterEach(() => { app?.unmount(); app = undefined; document.body.replaceChildren(); vi.useRealTimers(); });
it('terminal and raw views link by exact ID, reuse one stream and restore focus without replacing rows', async () => {
  await mount(); const line = document.querySelector('.terminal-line');
  const trigger = document.querySelector<HTMLButtonElement>('.terminal-trace')!;
  expect(trigger.type).toBe('button'); expect(trigger.getAttribute('aria-label')).toContain('日志 1'); trigger.focus(); trigger.click(); await flush();
  expect(document.querySelector('dialog')).not.toBeNull(); expect(subscribe).toHaveBeenCalledOnce();
  const params = new URL(request.mock.calls[0]![0], 'http://localhost').searchParams;
  expect(Object.fromEntries(params)).toMatchObject({ event_id: 'e', session_id: 's', include_content: 'false' }); expect(params.has('instance_id')).toBe(false);
  button('关闭链路').click(); await flush(); expect(document.activeElement).toBe(trigger); expect(document.querySelector('.terminal-line')).toBe(line);
  button('原始日志：关').click(); await flush(); trigger.click(); await flush();
  expect(document.querySelector('dialog')).not.toBeNull(); expect(subscribe).toHaveBeenCalledOnce(); expect(request).toHaveBeenCalledTimes(2);
});
it('structured rows support invocation-only traces; bare command and redacted IDs have no link', async () => {
  await mount(); push('s', [row(2, { record_kind: 'plugin.completed', invocation_id: 'i', status: 'no_output' }),
    row(3, { record_kind: 'plugin.completed', command_id: 'reply' }), row(4, { record_kind: 'plugin.completed', event_id: '[REDACTED]' })]); await flush();
  button('结构化记录').click(); await flush(); expect(document.querySelectorAll('.structured-trace')).toHaveLength(2);
  const triggers = document.querySelectorAll<HTMLButtonElement>('.structured-trace'); triggers[1]!.focus(); triggers[1]!.click(); await flush();
  const params = new URL(request.mock.calls[0]![0], 'http://localhost').searchParams;
  expect(params.get('invocation_id')).toBe('i'); expect(params.has('event_id')).toBe(false);
  document.querySelector('dialog')!.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true })); await flush(); expect(document.activeElement).toBe(triggers[1]);
});
it('process change closes immediately, cancels requests and ignores late trace bodies', async () => {
  let resolve!: (value: unknown) => void;
  request.mockImplementationOnce(() => new Promise(finish => { resolve = finish as (value: unknown) => void; }));
  await mount(); document.querySelector<HTMLButtonElement>('.terminal-trace')!.click(); await flush(); const signal = request.mock.calls[0]![2]!;
  push('new-process', [row(1)]); await nextTick(); expect(document.querySelector('dialog')).toBeNull(); expect(signal.aborted).toBe(true);
  resolve({ content: 'stale-body' }); await flush(); expect(document.body.textContent).not.toContain('stale-body'); expect(subscribe).toHaveBeenCalledOnce();
});
it('changing category/body closes trace and route unmount aborts outstanding read', async () => {
  await mount(); document.querySelector<HTMLButtonElement>('.terminal-trace')!.click(); await flush();
  const category = document.querySelectorAll('select')[1]!; category.value = 'all'; category.dispatchEvent(new Event('change', { bubbles: true })); await flush();
  expect(document.querySelector('dialog')).toBeNull(); const body = document.querySelector<HTMLInputElement>('input[type="checkbox"]')!;
  body.click(); await flush(); push(); await flush(); document.querySelector<HTMLButtonElement>('.terminal-trace')!.click(); await flush();
  expect(request.mock.calls.at(-1)![0]).toContain('include_content=true'); body.click(); await flush(); expect(document.querySelector('dialog')).toBeNull();
  push(); await flush(); request.mockImplementationOnce(() => new Promise(() => {})); document.querySelector<HTMLButtonElement>('.terminal-trace')!.click(); await flush();
  const signal = request.mock.calls.at(-1)![2]!; app!.unmount(); app = undefined; expect(signal.aborted).toBe(true); expect(close).toHaveBeenCalled();
});
