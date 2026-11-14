import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { createProcessControl, type ProcessSession } from '../src/lifecycle';

const OLD = '11111111-1111-4111-8111-111111111111', NEW = '22222222-2222-4222-8222-222222222222';
const response = (value: unknown, status = 200) => new Response(JSON.stringify(value), { status });
const session = (boot_id = OLD, authenticated = true) => ({ boot_id, authenticated, mode: 'token' });
const runtime = (phase = 'running') => ({ boot_id: NEW, health: { phase, status: 'degraded', online_adapters: 0 } });
let control: ReturnType<typeof createProcessControl>;
let ready = vi.fn<(session: ProcessSession) => void>(), lock = vi.fn<(value: boolean) => void>(), authNeeded = vi.fn<() => void>();
const tick = async (ms = 0) => { await vi.advanceTimersByTimeAsync(ms); };
beforeEach(() => {
  vi.useFakeTimers(); ready = vi.fn<(session: ProcessSession) => void>(); lock = vi.fn<(value: boolean) => void>(); authNeeded = vi.fn<() => void>();
  control = createProcessControl({ ready, lock, authNeeded });
});
afterEach(async () => { control.dispose(); await tick(); vi.useRealTimers(); vi.unstubAllGlobals(); });

it('lost 202 is reconciled through old boot, connection loss and a new degraded backend without repeating POST', async () => {
  let probes = 0;
  const fetcher = vi.fn(async (path: string, init: RequestInit) => {
    expect(init.credentials).toBe('same-origin'); expect(init.mode).toBe('same-origin'); expect(init.redirect).toBe('error');
    if (init.method === 'POST') { expect(JSON.parse(String(init.body))).toEqual({ expected_boot_id: OLD }); throw new TypeError('response lost'); }
    if (path === '/api/runtime') return response(runtime());
    probes++;
    if (probes === 2) throw new TypeError('down');
    return response(session(probes < 3 ? OLD : NEW));
  });
  vi.stubGlobal('fetch', fetcher);
  void control.start('restart', OLD, false); await tick(); expect(ready).not.toHaveBeenCalled();
  await tick(250); expect(ready).not.toHaveBeenCalled(); await tick(500);
  expect(ready).toHaveBeenCalledExactlyOnceWith(session(NEW));
  expect(fetcher.mock.calls.filter(([, init]) => init.method === 'POST')).toHaveLength(1);
  expect(control.state.value.phase).toBe('idle'); expect(lock).toHaveBeenLastCalledWith(false);
});

it('old-process 200 never completes restart; timeout retry performs only safe reads', async () => {
  let boot = OLD;
  const fetcher = vi.fn(async (path: string, init: RequestInit) => response(init.method === 'POST' ? { boot_id: OLD, action: 'restart' } : path === '/api/session' ? session(boot) : runtime(), init.method === 'POST' ? 202 : 200));
  vi.stubGlobal('fetch', fetcher);
  void control.start('restart', OLD, false); await tick(90_001);
  expect(control.state.value.phase).toBe('timeout'); expect(ready).not.toHaveBeenCalled();
  boot = NEW; await control.reconnect();
  expect(ready).toHaveBeenCalledExactlyOnceWith(session(NEW));
  expect(fetcher.mock.calls.filter(([, init]) => init.method === 'POST')).toHaveLength(1);
});

it('new process must be running, but platform health may still be degraded', async () => {
  let phase = 'starting';
  vi.stubGlobal('fetch', vi.fn(async (path: string, init: RequestInit) => response(init.method === 'POST' ? { boot_id: OLD, action: 'restart' } : path === '/api/session' ? session(NEW) : runtime(phase), init.method === 'POST' ? 202 : 200)));
  void control.start('restart', OLD, false); await tick(); expect(ready).not.toHaveBeenCalled();
  phase = 'running'; await tick(250); expect(ready).toHaveBeenCalledOnce();
});

it('changed-token session requests login honestly after a verified new boot', async () => {
  const fetcher = vi.fn(async (_: string, init: RequestInit) => response(init.method === 'POST' ? { boot_id: OLD, action: 'restart' } : session(NEW, false), init.method === 'POST' ? 202 : 200));
  vi.stubGlobal('fetch', fetcher); await control.start('restart', OLD, false);
  expect(authNeeded).toHaveBeenCalledOnce(); expect(ready).not.toHaveBeenCalled(); expect(fetcher).toHaveBeenCalledTimes(2);
  expect(lock).toHaveBeenLastCalledWith(false); expect(control.state.value.phase).toBe('idle');
  await control.start('restart', NEW, false); await tick(90_001); expect(fetcher).toHaveBeenCalledTimes(2);
  control = createProcessControl({ ready, lock, authNeeded });
  expect(control.state.value.phase).toBe('idle'); expect(fetcher).toHaveBeenCalledTimes(2);
});

it('busy or dirty guards and repeated requests cannot submit control writes', async () => {
  const fetcher = vi.fn(async (_: string, init: RequestInit) => response(init.method === 'POST' ? { boot_id: OLD, action: 'restart' } : session(), init.method === 'POST' ? 202 : 200));
  vi.stubGlobal('fetch', fetcher);
  await control.start('stop', OLD, true); await control.start('restart', OLD, true); expect(fetcher).not.toHaveBeenCalled();
  void control.start('restart', OLD, false); await tick(); await control.start('stop', OLD, false); await control.start('restart', OLD, false);
  expect(fetcher.mock.calls.filter(([, init]) => init.method === 'POST')).toHaveLength(1);
});

it('stop ends automatic reconnect on disconnect, describes uncertainty, and manual reconnect never starts a process', async () => {
  let reachable = false;
  const fetcher = vi.fn(async (path: string, init: RequestInit) => {
    if (init.method === 'POST') return response({ boot_id: OLD, action: 'stop' }, 202);
    if (!reachable) throw new TypeError('offline');
    return response(path === '/api/session' ? session(NEW) : runtime());
  });
  vi.stubGlobal('fetch', fetcher); await control.start('stop', OLD, false);
  expect(control.state.value.phase).toBe('disconnected'); expect(control.state.value.message).toContain('停止请求已接受');
  expect(control.state.value.message).toContain('无法仅凭断连确认正常退出');
  const calls = fetcher.mock.calls.length; await tick(180_000); expect(fetcher).toHaveBeenCalledTimes(calls);
  reachable = true; await control.reconnect(); expect(ready).toHaveBeenCalledOnce();
  expect(fetcher.mock.calls.filter(([, init]) => init.method === 'POST')).toHaveLength(1);
});

it('lost stop response never claims acceptance and does not keep reconnecting', async () => {
  const fetcher = vi.fn(async () => { throw new TypeError('offline'); });
  vi.stubGlobal('fetch', fetcher); await control.start('stop', OLD, false);
  expect(control.state.value.message).toContain('结果尚未确认'); expect(control.state.value.message).not.toContain('已接受');
  await tick(180_000); expect(fetcher).toHaveBeenCalledTimes(2);
});

it('a supervisor-started process does not resume automatically after stop', async () => {
  const fetcher = vi.fn(async (_: string, init: RequestInit) => response(init.method === 'POST' ? { boot_id: OLD, action: 'stop' } : session(NEW), init.method === 'POST' ? 202 : 200));
  vi.stubGlobal('fetch', fetcher); await control.start('stop', OLD, false);
  expect(control.state.value.phase).toBe('external-start'); expect(ready).not.toHaveBeenCalled();
  await tick(180_000); expect(fetcher).toHaveBeenCalledTimes(2);
});

it('HTTP errors or malformed metadata do not falsely claim the stop disconnected', async () => {
  const fetcher = vi.fn(async (_: string, init: RequestInit) => init.method === 'POST' ? response({ boot_id: OLD, action: 'stop' }, 202) : response({ not: 'a session' }));
  vi.stubGlobal('fetch', fetcher); void control.start('stop', OLD, false); await tick(90_001);
  expect(control.state.value.phase).toBe('timeout'); expect(control.state.value.message).not.toContain('已断开');
});

it('explicit conflict releases the guard, without probing or retrying the write', async () => {
  const fetcher = vi.fn(async () => response({ error: '已有其他关闭操作进行中' }, 409));
  vi.stubGlobal('fetch', fetcher); await control.start('stop', OLD, false);
  expect(control.state.value.phase).toBe('rejected'); expect(lock).toHaveBeenLastCalledWith(false);
  expect(fetcher).toHaveBeenCalledOnce(); control.dismissRejection(); expect(control.state.value.phase).toBe('idle');
});

it.each([408, 500, 503])('uncertain HTTP %s reconciles with reads and cannot repeat the write', async status => {
  const fetcher = vi.fn(async (_: string, init: RequestInit) => response(init.method === 'POST' ? {} : session(), init.method === 'POST' ? status : 200));
  vi.stubGlobal('fetch', fetcher); void control.start('restart', OLD, false); await tick();
  expect(control.state.value.phase).toBe('waiting'); expect(control.state.value.message).toContain('结果尚未确认');
  await control.start('restart', OLD, false); await tick(90_001);
  expect(control.state.value.phase).toBe('timeout');
  expect(fetcher.mock.calls.filter(([, init]) => init.method === 'POST')).toHaveLength(1);
});

it('hanging requests time out and disposal cancels all probes and timers', async () => {
  const signals: AbortSignal[] = [];
  const fetcher = vi.fn((_path: string, init: RequestInit) => { signals.push(init.signal as AbortSignal); return new Promise<Response>(() => {}); });
  vi.stubGlobal('fetch', fetcher); void control.start('restart', OLD, false); await tick(3000);
  expect(signals[0]!.aborted).toBe(true); expect(fetcher).toHaveBeenCalledTimes(2);
  control.dispose(); await tick(180_000); expect(signals[1]!.aborted).toBe(true);
  expect(fetcher).toHaveBeenCalledTimes(2); expect(ready).not.toHaveBeenCalled(); expect(vi.getTimerCount()).toBe(0);
});

it.each(['restart', 'stop'] as const)('same-boot authentication loss during %s disposes and unlocks without replay', async action => {
  const fetcher = vi.fn(async (_: string, init: RequestInit) => response(init.method === 'POST' ? { boot_id: OLD, action } : session(OLD, false), init.method === 'POST' ? 202 : 200));
  vi.stubGlobal('fetch', fetcher); await control.start(action, OLD, false);
  expect(authNeeded).toHaveBeenCalledOnce(); expect(lock).toHaveBeenLastCalledWith(false);
  expect(control.state.value.phase).toBe('idle');
  await control.start(action, OLD, false); await tick(90_001);
  expect(fetcher).toHaveBeenCalledTimes(2); expect(ready).not.toHaveBeenCalled();
});
