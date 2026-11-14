import { ApiError, readLogPage } from './api';
import { readLog } from './logPresentation';
import { isObject, type LogEntry } from './types';

export interface TraceTarget { session_id: string; event_id?: string; invocation_id?: string }
export interface TracePage extends TraceTarget {
  entries: LogEntry[]; next_cursor: number; has_more: boolean; partial: true;
  retention_evicted: boolean; oldest_id: number | null; latest_id: number;
  filter_scope: 'all_retained_levels_and_instances';
}
const kinds = new Set(['message.received', 'message.sending', 'message.sent', 'message.failed', 'plugin.invoked', 'plugin.completed', 'plugin.failed', 'plugin.action.completed']);
function validId(id: unknown): id is string {
  // Match backend: a 512-byte capture may truncate UTF-8 to a 509-byte prefix.
  return typeof id === 'string' && id.length > 0 && new TextEncoder().encode(id).length < 509
    && !/[\u0000-\u001f\u007f-\u009f\\]/u.test(id) && !id.includes('[REDACTED]');
}
export function traceTarget(entry: LogEntry, session?: string): TraceTarget | undefined {
  if (!session || !kinds.has(entry.fields.record_kind ?? '')) return;
  if (validId(entry.fields.event_id)) return { session_id: session, event_id: entry.fields.event_id };
  if (validId(entry.fields.invocation_id)) return { session_id: session, invocation_id: entry.fields.invocation_id };
}
export function readTracePage(value: unknown, target: TraceTarget, body: boolean, after?: number): TracePage {
  const invalid = () => new ApiError('链路响应格式异常，请重新读取。');
  if (!isObject(value) || value.session_id !== target.session_id || value.partial !== true
    || value.filter_scope !== 'all_retained_levels_and_instances' || typeof value.retention_evicted !== 'boolean'
    || !Number.isSafeInteger(value.latest_id) || Number(value.latest_id) < 0
    || !(value.oldest_id === null || (Number.isSafeInteger(value.oldest_id) && Number(value.oldest_id) > 0 && Number(value.oldest_id) <= Number(value.latest_id)))
    || !(value.event_id === null || validId(value.event_id)) || !(value.invocation_id === null || validId(value.invocation_id))
    || (target.event_id !== undefined && value.event_id !== target.event_id)
    || (target.invocation_id !== undefined && value.invocation_id !== target.invocation_id)) throw invalid();
  const page = readLogPage({ ...value, dropped: false }, after, target.session_id, body);
  if (page.entries.some(entry => !kinds.has(entry.fields.record_kind ?? '') || entry.id <= (after ?? 0))
    || page.next_cursor > Number(value.latest_id)) throw invalid();
  return { ...target, ...(value.event_id ? { event_id: value.event_id as string } : {}),
    entries: page.entries, next_cursor: page.next_cursor, has_more: page.has_more, partial: true,
    retention_evicted: value.retention_evicted, oldest_id: value.oldest_id as number | null,
    latest_id: value.latest_id as number, filter_scope: value.filter_scope };
}
export function traceReading(entry: LogEntry) {
  const f = entry.fields;
  if (f.record_kind === 'plugin.invoked') return { summary: '插件开始执行', status: '实际调用', detail: f.command_name ? `声明匹配的候选指令：${f.command_name}` : undefined };
  if (f.record_kind === 'message.sent' || f.record_kind === 'message.failed') return {
    summary: '适配器返回发送结果', status: f.status === 'succeeded' ? '适配器已返回' : '适配器报错',
    detail: '插件发送的最终状态以对应调用和命令的动作结果为准。',
  };
  return readLog(entry) ?? { summary: f.record_kind ?? '日志记录', status: f.status ?? '未提供状态' };
}
