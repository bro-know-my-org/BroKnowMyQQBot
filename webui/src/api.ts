import { assertSafeNumbers, parseSafeJson } from './json';
type Path = '/api/plugins' | `/api/plugins/${'config' | 'enabled' | 'apply'}` | '/api/settings/commands';
export class ApiError extends Error {
  constructor(message: string, public readonly status = 0) { super(message); }
}
export type Request = <T>(path: Path, body?: unknown) => Promise<T>;
export function createApi(onUnauthorized: () => void) {
  let token = '', generation = 0;
  const active = new Set<AbortController>();
  const clear = () => { token = ''; generation++; active.forEach(c => c.abort()); active.clear(); };
  const request: Request = async <T>(path: Path, body?: unknown): Promise<T> => {
    const current = generation, controller = new AbortController(); active.add(controller);
    const timer = setTimeout(() => controller.abort(), 30000);
    try {
      assertSafeNumbers(body);
      const response = await fetch(path, {
        method: body === undefined ? 'GET' : 'POST', mode: 'same-origin', credentials: 'omit', cache: 'no-store', redirect: 'error', signal: controller.signal,
        headers: { Authorization: `Bearer ${token}`, ...(body === undefined ? {} : { 'Content-Type': 'application/json' }) },
        ...(body === undefined ? {} : { body: JSON.stringify(body) }),
      });
      if (current !== generation) throw new ApiError('会话已结束。');
      if (response.status === 401) { onUnauthorized(); throw new ApiError('登录已失效，请重新输入管理 Token。', 401); }
      if (response.status === 409) throw new ApiError('其他操作已更新设置。请先复制需要保留的编辑，再刷新后重试。', 409);
      let data: unknown;
      try { data = parseSafeJson(await response.text()); }
      catch (e) { throw new ApiError(e instanceof SyntaxError ? `响应无法读取（${response.status}），请刷新确认状态。` : (e as Error).message); }
      if (current !== generation) throw new ApiError('会话已结束。');
      if (!response.ok) throw new ApiError(data && typeof data === 'object' && 'error' in data && typeof data.error === 'string' ? data.error : `请求失败（${response.status}），请刷新确认状态。`, response.status);
      return data as T;
    } catch (error) {
      if (error instanceof ApiError) throw error;
      if (controller.signal.aborted) throw new ApiError('请求已取消或超时，请刷新确认状态。');
      if (error instanceof TypeError) throw new ApiError('无法连接管理服务，请检查连接并刷新确认状态。');
      throw error;
    } finally { clearTimeout(timer); active.delete(controller); }
  };
  return { request, clear, setToken(value: string) { clear(); token = value; } };
}
