import { assertSafeNumbers, parseSafeJson } from './json';
import { isObject } from './types';
import type { LogEntry, LogPage, Session, SubscribeLogs } from './types';
type Path = '/api/plugins' | `/api/plugins/${'config' | 'enabled' | 'apply'}` | '/api/settings/commands' | '/api/session' | `/api/session/${'login' | 'logout'}` | '/api/logs' | `/api/logs?${string}`;
export class ApiError extends Error {
  constructor(message: string, public readonly status = 0) { super(message); }
}
export type Request = <T>(path: Path, body?: unknown) => Promise<T>;
const levels = ['TRACE', 'DEBUG', 'INFO', 'WARN', 'ERROR'];
const integer = (value: unknown): value is number => Number.isSafeInteger(value) && Number(value) >= 0;
const text = (value: unknown, max: number): value is string => typeof value === 'string' && value.length <= max;
export function readLogPage(value: unknown, after?: number, session?: string, includeContent = false): LogPage {
  const invalid = () => new ApiError('日志响应格式异常，已停止接收，请手动重试。');
  if (!isObject(value) || !text(value.session_id, 128) || !value.session_id || !integer(value.next_cursor)
    || typeof value.dropped !== 'boolean' || typeof value.has_more !== 'boolean' || !Array.isArray(value.entries) || value.entries.length > 200) throw invalid();
  const nextCursor = value.next_cursor;
  let previous = 0;
  const rows = value.entries.map((entry: unknown): LogEntry => {
    if (!isObject(entry) || !integer(entry.id) || entry.id <= previous || entry.id > nextCursor
      || !text(entry.timestamp, 64) || !Number.isFinite(Date.parse(entry.timestamp)) || !text(entry.level, 5) || !levels.includes(entry.level)
      || !text(entry.target, 128) || !text(entry.message, 2048) || !(entry.instance_id === null || text(entry.instance_id, 512))
      || (entry.category !== 'runtime' && entry.category !== 'message')
      || (entry.content !== undefined && (entry.category !== 'message' || !text(entry.content, 2048)))
      || !isObject(entry.fields) || Object.keys(entry.fields).length > 16
      || !Object.entries(entry.fields).every(([key, field]) => key.length <= 64 && text(field, 512))) throw invalid();
    previous = entry.id;
    return { id: entry.id, category: entry.category, ...(includeContent && entry.content !== undefined ? { content: entry.content as string } : {}),
      timestamp: entry.timestamp, level: entry.level, target: entry.target, message: entry.message,
      instance_id: entry.instance_id, fields: { ...entry.fields } as Record<string, string> };
  });
  const sameSession = value.session_id === session;
  if (value.has_more && (!rows.length || (sameSession && after !== undefined && nextCursor <= after))) throw invalid();
  if (!value.dropped && sameSession && after !== undefined && nextCursor < after) throw invalid();
  return { session_id: value.session_id, entries: rows, next_cursor: nextCursor, dropped: value.dropped, has_more: value.has_more };
}
export function createApi(onUnauthorized: () => void) {
  let generation = 0;
  const active = new Set<AbortController>();
  const streams = new Set<() => void>();
  const clear = () => { generation++; streams.forEach(close => close()); active.forEach(c => c.abort()); active.clear(); };
  const unauthorized = () => { clear(); onUnauthorized(); };
  const performRequest = async <T>(path: Path, body?: unknown, valid = () => true, owner?: AbortSignal): Promise<T> => {
    const current = generation, controller = new AbortController(); active.add(controller);
    const abort = () => controller.abort();
    owner?.addEventListener('abort', abort, { once: true });
    if (owner?.aborted) controller.abort();
    const timer = setTimeout(() => controller.abort(), 30000);
    try {
      assertSafeNumbers(body);
      const response = await fetch(path, {
        method: body === undefined ? 'GET' : 'POST', mode: 'same-origin', credentials: 'same-origin', cache: 'no-store', redirect: 'error', signal: controller.signal,
        headers: body === undefined ? {} : { 'Content-Type': 'application/json' },
        ...(body === undefined ? {} : { body: JSON.stringify(body) }),
      });
      if (current !== generation || !valid()) throw new ApiError('会话已结束。');
      if (response.status === 401) { unauthorized(); throw new ApiError('登录已失效，请重新输入管理 Token。', 401); }
      if (response.status === 409) throw new ApiError('其他操作已更新设置。请先复制需要保留的编辑，再刷新后重试。', 409);
      let data: unknown;
      try { data = parseSafeJson(await response.text()); }
      catch (e) { throw new ApiError(e instanceof SyntaxError ? `响应无法读取（${response.status}），请刷新确认状态。` : (e as Error).message); }
      if (current !== generation || !valid()) throw new ApiError('会话已结束。');
      if (!response.ok) throw new ApiError(data && typeof data === 'object' && 'error' in data && typeof data.error === 'string' ? data.error : `请求失败（${response.status}），请刷新确认状态。`, response.status);
      return data as T;
    } catch (error) {
      if (error instanceof ApiError) throw error;
      if (controller.signal.aborted) throw new ApiError('请求已取消或超时，请刷新确认状态。');
      if (error instanceof TypeError) throw new ApiError('无法连接管理服务，请检查连接并刷新确认状态。');
      throw error;
    } finally { clearTimeout(timer); active.delete(controller); owner?.removeEventListener('abort', abort); }
  };
  const request: Request = (path, body) => performRequest(path, body);
  const subscribeLogs: SubscribeLogs = (query, handlers) => {
    const limit = query.limit ?? 200;
    if (!levels.includes(query.level) || !integer(limit) || limit < 1 || limit > 200
      || !['runtime', 'message', 'all'].includes(query.category ?? 'runtime')
      || (query.include_content !== undefined && typeof query.include_content !== 'boolean')
      || (query.after === undefined) !== (query.session_id === undefined)
      || (query.after !== undefined && !integer(query.after))
      || (query.session_id !== undefined && (!text(query.session_id, 128) || !query.session_id))) throw new ApiError('日志订阅参数无效。');
    const params = new URLSearchParams({ level: query.level, limit: String(limit), category: query.category ?? 'runtime', include_content: String(query.include_content ?? false) });
    if (query.instance_id) params.set('instance_id', query.instance_id);
    if (query.after !== undefined) { params.set('after', String(query.after)); params.set('session_id', query.session_id!); }
    const source = new EventSource(`/api/logs/stream?${params}`);
    const current = generation;
    let probe: AbortController | undefined;
    let closed = false, checking = false, after = query.after, session = query.session_id;
    const live = () => !closed && current === generation;
    const close = () => { if (closed) return; closed = true; source.close(); probe?.abort(); streams.delete(close); };
    const fail = () => { close(); handlers.onError(new ApiError('日志响应格式异常，已停止接收，请手动重试。')); };
    streams.add(close); handlers.onState('connecting');
    source.onopen = () => { if (live()) handlers.onState('open'); };
    source.addEventListener('logs', event => {
      if (!live()) return;
      let page: LogPage;
      try { page = readLogPage(parseSafeJson((event as MessageEvent).data), after, session, query.include_content); }
      catch { fail(); return; }
      after = page.next_cursor; session = page.session_id;
      handlers.onPage(page);
    });
    source.addEventListener('auth-required', () => { if (live()) unauthorized(); });
    source.onerror = () => {
      if (!live()) return;
      handlers.onState(source.readyState === EventSource.CLOSED ? 'closed' : 'reconnecting');
      if (checking) return;
      checking = true;
      probe = new AbortController();
      // EventSource hides HTTP status, so check the Cookie session after a failed handshake.
      void performRequest<Session>('/api/session', undefined, live, probe.signal).then(status => {
        if (live() && status && ['token', 'none'].includes(status.mode) && status.authenticated === false) unauthorized();
      }).catch(() => { /* Network failures retain EventSource's native reconnect and existing output. */ })
        .finally(() => {
          checking = false;
          if (live() && source.readyState === EventSource.CLOSED) {
            close(); handlers.onError(new ApiError('日志连接已停止，请手动重试。'));
          }
        });
    };
    return close;
  };
  return { request, subscribeLogs, clear };
}
