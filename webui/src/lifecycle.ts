import { shallowRef } from 'vue';
import { isObject } from './types';

export type ProcessAction = 'restart' | 'stop';
export interface ProcessSession { boot_id: string; mode: 'token' | 'none'; authenticated: boolean }
type Phase = 'idle' | 'requesting' | 'waiting' | 'timeout' | 'disconnected' | 'external-start' | 'rejected';
export interface ProcessState { phase: Phase; action: ProcessAction; message: string }
interface Options {
  lock: (value: boolean) => void;
  ready: (session: ProcessSession) => void;
  authNeeded: () => void;
}
class ConnectionError extends Error {}
const DEADLINE = 90_000, REQUEST_TIMEOUT = 3_000, INITIAL_BACKOFF = 250, MAX_BACKOFF = 4_000;
const SESSION_PATH = '/api/session', RUNTIME_PATH = '/api/runtime';
const CONTROL_PATHS = { restart: '/api/restart', stop: '/api/stop' } as const;
const bootId = (v: unknown): v is string => typeof v === 'string' && /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(v);
function session(value: unknown): ProcessSession {
  if (!isObject(value) || !bootId(value.boot_id) || typeof value.mode !== 'string' || !['token', 'none'].includes(value.mode) || typeof value.authenticated !== 'boolean') throw new Error('会话响应格式异常');
  return value as unknown as ProcessSession;
}

// Process-control requests bypass the regular API's global 401 teardown. They
// carry only the browser's same-origin HttpOnly cookie and never replay writes.
export function createProcessControl(options: Options) {
  const state = shallowRef<ProcessState>({ phase: 'idle', action: 'restart', message: '' });
  let alive = true, operation: AbortController | undefined, oldBoot = '', accepted = false;
  let deadline = 0;
  const set = (phase: Phase, message: string) => { state.value = { ...state.value, phase, message }; };
  const live = (owner: AbortController) => alive && operation === owner && !owner.signal.aborted;
  async function read(path: string, owner: AbortController, body?: unknown) {
    const controller = new AbortController();
    let timer: ReturnType<typeof setTimeout>;
    let abort = () => {};
    const cancelled = new Promise<never>((_, reject) => {
      abort = () => { controller.abort(); reject(new ConnectionError('连接已取消或超时')); };
      owner.signal.addEventListener('abort', abort, { once: true });
      timer = setTimeout(abort, Math.max(1, Math.min(REQUEST_TIMEOUT, deadline - performance.now())));
    });
    try {
      return await Promise.race([cancelled, (async () => {
        const response = await fetch(path, {
          method: body === undefined ? 'GET' : 'POST', mode: 'same-origin', credentials: 'same-origin',
          cache: 'no-store', redirect: 'error', signal: controller.signal,
          ...(body === undefined ? {} : { headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) }),
        });
        const text = await response.text();
        let value: unknown = null;
        try { value = JSON.parse(text); } catch { /* Invalid JSON is a reachable but unusable response. */ }
        return { response, value };
      })()]);
    } catch (error) { if (error instanceof TypeError) throw new ConnectionError('服务连接失败'); throw error; }
    finally { clearTimeout(timer!); owner.signal.removeEventListener('abort', abort); }
  }
  function pause(ms: number, owner: AbortController) {
    return new Promise<void>(resolve => {
      const done = () => { clearTimeout(timer); owner.signal.removeEventListener('abort', done); resolve(); };
      const timer = setTimeout(done, ms);
      owner.signal.addEventListener('abort', done, { once: true });
      if (owner.signal.aborted) done();
    });
  }
  // Auth loss terminates this controller. The authenticated shell must recreate
  // it after login; no pending write is resumed or replayed across that boundary.
  function requireAuth(owner: AbortController) {
    alive = false; owner.abort(); set('idle', ''); options.lock(false); options.authNeeded();
  }
  async function probe(owner: AbortController, manual = false) {
    let delay = INITIAL_BACKOFF;
    while (live(owner) && performance.now() < deadline) {
      try {
        const result = await read(SESSION_PATH, owner);
        if (!live(owner)) return;
        if (!result.response.ok) throw new Error('服务暂不可用');
        const next = session(result.value);
        if (!next.authenticated) { requireAuth(owner); return; }
        if (state.value.action === 'stop' && !manual) {
          if (next.boot_id !== oldBoot) {
            set('external-start', '服务出现新的启动标识，可能已由外部服务管理器启动。请手动重新连接。'); return;
          }
        } else if (next.boot_id !== oldBoot || manual) {
          const runtime = await read(RUNTIME_PATH, owner);
          if (!live(owner)) return;
          if (runtime.response.status === 401) { requireAuth(owner); return; }
          if (runtime.response.ok && isObject(runtime.value) && runtime.value.boot_id === next.boot_id
            && isObject(runtime.value.health) && runtime.value.health.phase === 'running') {
            set('idle', ''); options.lock(false); options.ready(next); return;
          }
        }
      } catch (error) {
        if (!live(owner)) return;
        if (state.value.action === 'stop' && error instanceof ConnectionError) {
          set('disconnected', accepted ? '停止请求已接受，服务已断开。无法仅凭断连确认正常退出，也可能是网络故障。' : '服务已断开，停止请求结果尚未确认。也可能是网络故障，请核对终端状态。');
          return;
        }
      }
      await pause(Math.min(delay, Math.max(0, deadline - performance.now())), owner);
      delay = Math.min(delay * 2, MAX_BACKOFF);
    }
    if (live(owner)) set('timeout', state.value.action === 'restart' ? '连接等待超时，尚未确认新进程已启动。请核对终端状态，再重试连接。' : '尚未确认服务断开。停止可能仍在处理中，请核对终端状态。');
  }
  async function start(action: ProcessAction, boot: string, blocked: boolean) {
    if (!alive || state.value.phase !== 'idle' || blocked || !bootId(boot)) return;
    operation?.abort(); const owner = new AbortController(); operation = owner;
    oldBoot = boot; accepted = false; deadline = performance.now() + DEADLINE;
    state.value = { phase: 'requesting', action, message: action === 'restart' ? '正在请求优雅重启…' : '正在请求优雅停止…' };
    options.lock(true);
    try {
      const result = await read(CONTROL_PATHS[action], owner, { expected_boot_id: boot });
      if (!live(owner)) return;
      if (result.response.status !== 202) {
        if (result.response.status === 401) { requireAuth(owner); return; }
        set('rejected', isObject(result.value) && typeof result.value.error === 'string' ? result.value.error.slice(0, 512) : '操作未被接受，请刷新状态后确认。');
        options.lock(false); return;
      }
      accepted = isObject(result.value) && result.value.boot_id === boot && result.value.action === action;
    } catch { if (!live(owner)) return; }
    if (!live(owner)) return;
    const progress = action === 'restart' ? '正在重启：等待任务结束并确认新进程…' : '正在停止：等待任务结束和服务断开…';
    set('waiting', accepted ? progress : '请求结果尚未确认，正在核对服务状态；不会重复发送操作。');
    await probe(owner);
  }
  async function reconnect() {
    if (!alive || !['timeout', 'disconnected', 'external-start'].includes(state.value.phase)) return;
    operation?.abort(); const owner = new AbortController(); operation = owner; deadline = performance.now() + DEADLINE;
    set('waiting', '正在重新连接；仅查询状态，不会启动进程或重复发送操作。');
    await probe(owner, state.value.action === 'stop');
  }
  function dismissRejection() { if (state.value.phase === 'rejected') set('idle', ''); }
  function dispose() { alive = false; operation?.abort(); }
  return { state, start, reconnect, dismissRejection, dispose };
}
