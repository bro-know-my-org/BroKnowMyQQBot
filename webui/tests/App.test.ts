import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { createApp, nextTick } from 'vue';
import type { RouteLocationRaw } from 'vue-router';
import App from '../src/App.vue';
import { createManagementRouter, pluginRoute } from '../src/navigation';
import type { Commands, Config, PluginView } from '../src/types';

const fixture = (overrides: Partial<PluginView> = {}): PluginView => ({
  instance_id: 'sample', plugin_id: 'dev.bkm.sample', name: '<img src=x onerror=alert(1)>', description: '测试插件', version: '1.0', source: 'local', trust_level: 'local-wasm', signature_status: 'unsigned',
  editable: true, enabled: true, runtime_state: 'ready', updated_at_ms: 1, pending: false, error: null, requested_permissions: ['message.reply'], granted_permissions: [],
  config: { count: 1, greeting: 'hello', enabled: false, mode: 1, unknown: { keep: ['unchanged'] } },
  schema: { type: 'object', required: ['count'], properties: { count: { type: 'integer', title: '数量' }, greeting: { type: 'string' }, enabled: { type: 'boolean' }, mode: { enum: [1, 2] }, absent: { type: 'string', default: 'example' } } }, ...overrides,
});
let app: ReturnType<typeof createApp>, plugins: PluginView[], commands: Commands, fail: { path: string; method: string; status: number } | null;
let router: ReturnType<typeof createManagementRouter>;
let sessionAuthenticated: boolean, sessionMode: 'token' | 'none', unsafePluginResponse: boolean;
const requests: Array<{ path: string; method: string; body: Record<string, unknown> | undefined; init: RequestInit }> = [];
const fetcher = vi.fn(async (input: RequestInfo | URL, init: RequestInit = {}) => {
  const path = String(input), method = init.method ?? 'GET', body = init.body ? JSON.parse(String(init.body)) as Record<string, unknown> : undefined;
  requests.push({ path, method, body, init });
  if (fail?.path === path && fail.method === method) { const status = fail.status; fail = null; if (status === 401) sessionAuthenticated = false; return new Response(JSON.stringify({ error: '服务拒绝此次更改' }), { status }); }
  if (path.startsWith('/api/session')) {
    if (path.endsWith('/login')) sessionAuthenticated = true;
    if (path.endsWith('/logout')) sessionAuthenticated = false;
    return new Response(JSON.stringify({ mode: sessionMode, authenticated: sessionMode === 'none' || sessionAuthenticated }));
  }
  if (!sessionAuthenticated && sessionMode !== 'none') return new Response('{}', { status: 401 });
  if (path === '/api/plugins' && unsafePluginResponse) { unsafePluginResponse = false; return new Response(JSON.stringify({ plugins }).replace('"count":1', '"count":9007199254740993')); }
  if (path === '/api/settings/commands') {
    if (body) { expect(body.expected_revision).toBe(commands.revision); commands = { prefix: String(body.prefix), revision: String(Number(commands.revision) + 1) }; }
    return new Response(JSON.stringify(commands));
  }
  if (path.startsWith('/api/marketplace')) return new Response(JSON.stringify({ source: 'https://bkmqb-plugins.github.io/marketplace/index.json', total: 0, offset: 0, plugins: [] }));
  if (!body) return new Response(JSON.stringify({ plugins }));
  const p = plugins.find(p => p.instance_id === body.instance_id)!; expect(body.expected_updated_at_ms).toBe(p.updated_at_ms);
  if (path.endsWith('/config')) p.config = body.config as Config;
  if (path.endsWith('/enabled')) p.enabled = body.enabled as boolean;
  p.pending = !path.endsWith('/apply'); p.updated_at_ms++; if (!p.pending) p.runtime_state = p.enabled ? 'ready' : 'stopped';
  return new Response(JSON.stringify({ plugin: p }));
});
// Mount the real application and dispatch native DOM events; only HTTP is mocked.
function get<T extends HTMLElement = HTMLElement>(selector: string): T {
  const element = document.querySelector<T>(selector); if (!element) throw new Error(`Missing element: ${selector}`); return element;
}
const flush = async () => { await new Promise<void>(resolve => setImmediate(resolve)); await nextTick(); };
async function input(selector: string, value: string | boolean) {
  const element = get<HTMLInputElement>(selector);
  if (typeof value === 'boolean') element.checked = value; else element.value = value;
  for (const type of ['input', 'change']) element.dispatchEvent(new Event(type, { bubbles: true })); await nextTick();
}
const click = async (selector: string) => { get<HTMLButtonElement>(selector).click(); await flush(); };
const submit = async (selector: string) => { get(selector).dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })); await flush(); };
const disabled = (selector: string) => get<HTMLButtonElement>(selector).disabled;
const value = (selector: string) => get<HTMLInputElement>(selector).value;
const text = () => document.body.textContent;
const posts = () => requests.filter(r => r.method === 'POST' && !r.path.startsWith('/api/session'));
const go = async (target: RouteLocationRaw) => { await router.push(target); await flush(); };
async function mount(target: RouteLocationRaw = pluginRoute(plugins[0]?.instance_id ?? 'sample', 'config')) {
  router = createManagementRouter(true); await router.push(target); await router.isReady();
  const root = document.createElement('div'); document.body.append(root); app = createApp(App).use(router); app.mount(root);
  await flush();
}
async function login(target: RouteLocationRaw = pluginRoute(plugins[0]?.instance_id ?? 'sample', 'config')) {
  await mount(target);
  await input('#token', 'test-token'); await submit('#login-form');
}
beforeEach(() => {
  plugins = [fixture()]; commands = { prefix: '/', revision: '1' }; fail = null; requests.length = 0;
  sessionAuthenticated = false; sessionMode = 'token'; unsafePluginResponse = false;
  vi.stubGlobal('fetch', fetcher); vi.spyOn(window, 'confirm').mockReturnValue(true);
  vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => { throw new Error('credentials must stay in memory'); });
});
afterEach(() => { app?.unmount(); document.body.replaceChildren(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

describe('浏览器管理会话', () => {
  it('重新挂载时通过 Cookie 恢复登录和目标路由，不重复提交 Token', async () => {
    await login(); app.unmount(); document.body.replaceChildren(); requests.length = 0;
    await mount(pluginRoute('sample', 'config'));
    expect(value('#field-0')).toBe('1'); expect(document.querySelector('#token')).toBeNull();
    expect(requests.map(r => r.path)).toEqual(['/api/session', '/api/plugins']);
    expect(requests.every(r => r.init.credentials === 'same-origin' && !r.body?.token)).toBe(true);
    expect(Storage.prototype.setItem).not.toHaveBeenCalled();
  });
  it('显式退出撤销会话，刷新后仍显示登录表单', async () => {
    await login(); await click('#logout'); expect(sessionAuthenticated).toBe(false);
    expect(value('#token')).toBe(''); expect(requests.at(-1)!.path).toBe('/api/session/logout');
    app.unmount(); document.body.replaceChildren(); requests.length = 0; await mount();
    expect(document.querySelector('#login-form')).not.toBeNull(); expect(requests.map(r => r.path)).toEqual(['/api/session']);
  });
  it('免认证模式自动进入并显示当前模式', async () => {
    sessionMode = 'none'; await mount({ name: 'overview' });
    expect(text()).toContain('本地免认证'); expect(document.querySelector('#token')).toBeNull(); expect(document.querySelector('#logout')).toBeNull();
    expect(requests.map(r => r.path)).toEqual(['/api/session', '/api/plugins']);
  });
  it('浏览器往返缓存恢复时重新核对会话，失效时不会显示旧管理数据', async () => {
    await login(); window.dispatchEvent(new PageTransitionEvent('pagehide', { persisted: true })); await flush();
    expect(document.querySelector('[aria-label="插件详情"]')).toBeNull();
    window.dispatchEvent(new PageTransitionEvent('pageshow', { persisted: true })); await flush();
    expect(value('#field-0')).toBe('1');
    window.dispatchEvent(new PageTransitionEvent('pagehide', { persisted: true })); sessionAuthenticated = false;
    window.dispatchEvent(new PageTransitionEvent('pageshow', { persisted: true })); await flush();
    expect(document.querySelector('#login-form')).not.toBeNull(); expect(document.querySelector('[aria-label="插件详情"]')).toBeNull();
  });
  it('登录成功但插件读取失败时保留会话，可直接重试加载', async () => {
    fail = { path: '/api/plugins', method: 'GET', status: 500 }; await login();
    expect(sessionAuthenticated).toBe(true); expect(document.querySelector('#token')).toBeNull(); expect(text()).toContain('服务拒绝此次更改');
    expect(text()).toContain('插件状态暂不可用'); expect(text()).not.toContain('未找到插件实例');
    await click('#retry-plugins'); expect(value('#field-0')).toBe('1'); expect(document.querySelector('#retry-plugins')).toBeNull();
    expect(requests.filter(r => r.path === '/api/session/login')).toHaveLength(1);
  });
});

describe('插件管理的实际组件交互', () => {
  it('整数 Schema 的小数边界不会让有效整数被原生 step 校验阻止', async () => {
    plugins = [fixture({ config: { count: 1 }, schema: { type: 'object', properties: { count: { type: 'integer', minimum: 0.5, maximum: 2.5 } } } })];
    await login(); await input('#field-0', '2'); expect(get<HTMLInputElement>('#field-0').checkValidity()).toBe(true);
    await click('#save'); expect(plugins[0]!.config).toEqual({ count: 2 });
    await input('#field-0', '1.5'); await click('#save'); expect(posts()).toHaveLength(1); expect(text()).toContain('有效的整数');
  });
  it('多行字符串和默认值在编辑保存时保留换行', async () => {
    plugins = [fixture({ config: { message: '第一行\n第二行' }, schema: { type: 'object', properties: { message: { type: 'string' }, template: { type: 'string', default: '标题\n正文' } } } })];
    await login(); expect(get('#field-0').tagName).toBe('TEXTAREA'); expect(value('#field-0')).toBe('第一行\n第二行');
    await input('#field-0', '第一行\n修改后的第二行'); await input('input[aria-label="设置 template"]', true); await click('#save');
    expect(plugins[0]!.config).toEqual({ message: '第一行\n修改后的第二行', template: '标题\n正文' });
  });
  it('取消可选字段、保持缺省、保留未知配置，并保持数字/布尔/枚举类型', async () => {
    await login(); expect(document.querySelector('img')).toBeNull(); expect(text()).toContain('<img src=x onerror=alert(1)>');
    await input('input[aria-label="设置 greeting"]', false); await input('#field-0', '7'); await input('#field-3', '1');
    expect(disabled('#apply')).toBe(true); await click('#save');
    const config = posts()[0]!.body!.config as Config;
    expect(config).toEqual({ count: 7, enabled: false, mode: 2, unknown: { keep: ['unchanged'] } });
    expect(config).not.toHaveProperty('greeting'); expect(config).not.toHaveProperty('absent');
    expect(requests.every(r => r.init.mode === 'same-origin' && r.init.credentials === 'same-origin' && !(r.init.headers as Record<string, string>).Authorization)).toBe(true);
    expect(requests.filter(r => r.body?.token).map(r => r.path)).toEqual(['/api/session/login']);
    expect(Storage.prototype.setItem).not.toHaveBeenCalled();
  });
  it('保存配置和启停不改变运行态，应用携带保存后的版本', async () => {
    await login(); await input('#field-0', '3'); await click('#save');
    expect(posts().map(r => r.path)).toEqual(['/api/plugins/config']); expect(plugins[0]!.pending).toBe(true);
    await click('#apply'); expect(posts()[1]!.body!.expected_updated_at_ms).toBe(2);
    await click('#toggle'); expect(plugins[0]!.enabled).toBe(false); expect(plugins[0]!.runtime_state).toBe('ready'); expect(text()).toContain('已停用');
    await click('#apply'); expect(plugins[0]!.runtime_state).toBe('stopped');
  });
  it('409 保留插件草稿并锁定写操作，取消刷新或切换不会丢弃草稿', async () => {
    plugins.push(fixture({ instance_id: 'other', name: '其他插件' })); await login(); await input('#field-0', '8');
    fail = { path: '/api/plugins/config', method: 'POST', status: 409 }; await click('#save');
    expect(value('#field-0')).toBe('8'); expect(disabled('#save')).toBe(true); expect(text()).toContain('刷新');
    await go({ ...pluginRoute('sample', 'config'), query: { view: 'compact' }, hash: '#field' }); expect(disabled('#save')).toBe(true);
    vi.mocked(window.confirm).mockReturnValue(false); await click('#refresh'); await go(pluginRoute('other', 'config')); expect(value('#field-0')).toBe('8');
    vi.mocked(window.confirm).mockReturnValue(true); await click('#refresh'); expect(value('#field-0')).toBe('1');
  });
  it('跨页面保护插件草稿；前缀 409 保留前缀编辑', async () => {
    await login(); await input('#field-0', '9'); vi.mocked(window.confirm).mockReturnValue(false);
    await go({ name: 'settings' }); expect(value('#field-0')).toBe('9'); expect(router.currentRoute.value.name).toBe('plugin-config');
    await click('#save'); await go({ name: 'settings' });
    await input('#command-prefix', ''); expect(disabled('#save-prefix')).toBe(true); expect(posts()).toHaveLength(1);
    await input('#command-prefix', '！！'); await submit('.settings-form');
    expect(commands).toEqual({ prefix: '！！', revision: '2' }); expect(posts()[1]!.body).toEqual({ prefix: '！！', expected_revision: '1' });
    expect(plugins[0]!.config.count).toBe(9);
    fail = { path: '/api/settings/commands', method: 'POST', status: 409 }; await input('#command-prefix', '!!'); await submit('.settings-form');
    expect(value('#command-prefix')).toBe('!!'); expect(disabled('#save-prefix')).toBe(true);
    vi.mocked(window.confirm).mockReturnValue(true); await click('#refresh-prefix'); expect(value('#command-prefix')).toBe('！！');
  });
  it('401 清空登录凭据与插件详情，重新登录只使用新 Token', async () => {
    await login(); fail = { path: '/api/plugins', method: 'GET', status: 401 }; await click('#refresh');
    expect(document.querySelector('[aria-label="插件详情"]')).toBeNull(); expect(value('#token')).toBe(''); expect(text()).toContain('登录已失效');
    await input('#token', 'new-token'); await submit('#login-form');
    expect(requests.filter(r => r.path === '/api/session/login').at(-1)!.body).toEqual({ token: 'new-token' });
    expect((requests.at(-1)!.init.headers as Record<string, string>).Authorization).toBeUndefined(); expect(Storage.prototype.setItem).not.toHaveBeenCalled();
  });
  it('复杂 Schema 用 JSON 编辑并拒绝非对象；文件插件不可编辑', async () => {
    plugins = [fixture({ schema: { allOf: [{ type: 'object' }] } }), fixture({ instance_id: 'file', editable: false, name: '文件插件' })]; await login();
    await input('#config-json', '[]'); await click('#save'); expect(posts()).toHaveLength(0); expect(text()).toContain('JSON 对象');
    await input('#config-json', '{"items":[{"name":"示例"}]}'); await click('#save'); expect(plugins[0]!.config).toEqual({ items: [{ name: '示例' }] });
    await go(pluginRoute('file', 'config')); expect(disabled('#save')).toBe(true); expect(get<HTMLFieldSetElement>('fieldset').disabled).toBe(true); expect(text()).toContain('由声明式文件');
  });
  it('搜索空状态、服务错误和空插件列表有清晰反馈', async () => {
    await login({ name: 'plugins' }); await input('#search', '不存在'); expect(text()).toContain('没有匹配的插件');
    fail = { path: '/api/plugins', method: 'GET', status: 500 }; await click('#refresh'); expect(text()).toContain('服务拒绝此次更改');
    plugins = []; await click('#refresh'); expect(text()).toContain('暂无插件');
  });
  it.each(['9007199254740993', '-9007199254740993', '9007199254740992', '-9007199254740992'])('拒绝表单中的不安全整数 %s', async number => {
    await login(); await input('#field-0', number); await click('#save'); expect(posts()).toHaveLength(0); expect(text()).toContain('安全整数范围'); expect(value('#field-0')).toBe(number);
  });
  it.each(['9007199254740991', '-9007199254740991'])('保留安全整数边界 %s', async number => {
    await login(); await input('#field-0', number); await click('#save'); expect((posts()[0]!.body!.config as Config).count).toBe(Number(number));
  });
  it('整对象 JSON 中的嵌套大整数被拒绝，原始编辑保留', async () => {
    plugins = [fixture({ schema: null })]; await login(); const raw = '{"nested":[{"big":9007199254740993}]}';
    await input('#config-json', raw); await click('#save'); expect(posts()).toHaveLength(0); expect(text()).toContain('安全整数范围'); expect(value('#config-json')).toBe(raw);
    await input('#config-json', '{"safe":9007199254740991,"exact":"9007199254740993"}'); await click('#save'); expect(plugins[0]!.config).toEqual({ safe: Number.MAX_SAFE_INTEGER, exact: '9007199254740993' });
  });
  it('服务端返回大整数时拒绝读取，避免把舍入后的配置放入编辑器', async () => {
    unsafePluginResponse = true;
    await login(); expect(text()).toContain('安全整数范围'); expect(document.querySelector('[aria-label="插件详情"]')).toBeNull(); expect(document.querySelector('#retry-plugins')).not.toBeNull();
  });
  it('前缀待生效时分别显示已保存值和运行值', async () => {
    commands = { prefix: '/', revision: '1', saved_prefix: '!!', pending: true }; await login({ name: 'settings' });
    expect(value('#command-prefix')).toBe('!!'); expect(text()).toContain('当前运行前缀：/'); expect(text()).toContain('待生效'); expect(disabled('#save-prefix')).toBe(false);
    await submit('.settings-form'); expect(posts()[0]!.body).toEqual({ prefix: '!!', expected_revision: '1' });
    expect(commands.prefix).toBe('!!'); expect(disabled('#save-prefix')).toBe(true);
  });
  it('概览统计仅来自真实插件快照，筛选可以定位待应用实例', async () => {
    plugins = [fixture(), fixture({ instance_id: 'pending', name: '待处理', pending: true, runtime_state: 'stopped', error: '初始化失败' })];
    await login({ name: 'overview' });
    expect(Array.from(document.querySelectorAll('.stat-value')).map(el => el.textContent)).toEqual(['2', '1', '1', '1']);
    expect(text()).toContain('不代表平台连接状态');
    await click('.nav-item[aria-label="已安装"]'); await input('#plugin-filter', 'pending');
    expect(document.querySelectorAll('.plugin-item')).toHaveLength(1); expect(get('.plugin-item').textContent).toContain('待处理');
  });
  it('含斜杠和特殊字符的实例可通过深链接准确打开，失效实例不回退到其他插件', async () => {
    const id = 'dev.bkm.example/default #?%中文'; plugins = [fixture({ instance_id: id })]; await login();
    expect(router.currentRoute.value.params.instanceId).toBe(id); expect(value('#field-0')).toBe('1');
    expect(router.resolve(pluginRoute(id, 'config')).href).toContain('%2F');
    await go(pluginRoute('missing', 'config')); expect(text()).toContain('未找到插件实例'); expect(document.querySelector('#save')).toBeNull();
  });
  it('标签、侧栏和浏览器后退均保护草稿，确认后才放弃', async () => {
    await login({ name: 'plugins' }); await go(pluginRoute('sample', 'config')); await input('#field-0', '8');
    vi.mocked(window.confirm).mockReturnValue(false);
    await click('.tab-link:first-child'); expect(value('#field-0')).toBe('8');
    await click('.nav-item[aria-label="概览"]'); expect(value('#field-0')).toBe('8');
    router.back(); await flush(); await flush(); expect(value('#field-0')).toBe('8');
    vi.mocked(window.confirm).mockReturnValue(true); await go({ name: 'plugins' });
    await go(pluginRoute('sample', 'config')); expect(value('#field-0')).toBe('1');
  });
  it('前缀草稿在取消导航后仍可保存，保存过程中拒绝路由切换', async () => {
    await login({ name: 'settings' }); await input('#command-prefix', '!!');
    vi.mocked(window.confirm).mockReturnValue(false); await go({ name: 'plugins' }); expect(value('#command-prefix')).toBe('!!');
    let finish!: (value: Response) => void;
    fetcher.mockImplementationOnce(() => new Promise<Response>(resolve => { finish = resolve; }));
    get('.settings-form').dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })); await nextTick();
    await go({ name: 'overview' }); expect(router.currentRoute.value.name).toBe('settings'); expect(text()).toContain('操作进行中');
    finish(new Response(JSON.stringify({ prefix: '!!', revision: '2' }))); await flush();
    await go({ name: 'overview' }); expect(router.currentRoute.value.name).toBe('overview');
  });
  it('同页查询参数和锚点变化保留草稿保护，409 在离开重进后仍需刷新', async () => {
    await login(); await input('#field-0', '7'); get('#field-0').focus(); await go({ ...pluginRoute('sample', 'config'), query: { view: 'compact' }, hash: '#field' });
    expect(document.activeElement).toBe(get('#field-0'));
    vi.mocked(window.confirm).mockReturnValue(false); await go({ name: 'overview' }); expect(value('#field-0')).toBe('7');
    fail = { path: '/api/plugins/config', method: 'POST', status: 409 }; await click('#save');
    vi.mocked(window.confirm).mockReturnValue(true); await go({ name: 'plugins' }); await go(pluginRoute('sample', 'config'));
    await input('#field-0', '3'); expect(disabled('#save')).toBe(true); await click('#refresh'); expect(disabled('#toggle')).toBe(false);
    await go({ name: 'settings' }); await input('#command-prefix', '!!');
    get('#command-prefix').focus(); await go({ name: 'settings', query: { view: 'advanced' }, hash: '#prefix' }); expect(document.activeElement).toBe(get('#command-prefix'));
    vi.mocked(window.confirm).mockReturnValue(false); await go({ name: 'overview' }); expect(value('#command-prefix')).toBe('!!');
  });
  it.each(['adapters', 'not-found'])('未接入页面 %s 明示边界且不发起不存在的接口请求', async name => {
    await login(name === 'not-found' ? '/unknown-page' : { name });
    expect(text()).toContain(name === 'not-found' ? '未找到页面' : '尚未接入'); expect(requests.filter(r => !r.path.startsWith('/api/session')).map(r => r.path)).toEqual(['/api/plugins']);
  });
});

it('市场路由读取真实接口并展示空市场状态', async () => {
  await login({ name: 'marketplace' });
  expect(text()).toContain('没有匹配的插件');
  expect(requests.some(r => r.path.startsWith('/api/marketplace?'))).toBe(true);
  expect(posts()).toHaveLength(0);
});

it('通知按钮可回看跨页历史，退出登录后清空上一会话的通知', async () => {
  vi.spyOn(document, 'hasFocus').mockReturnValue(true);
  await login(); await input('#field-0', '6'); await click('#save');
  expect(document.querySelector('#page-content > .notice')).toBeNull();
  await go({ name: 'overview' }); expect(get('.notification-toast').textContent).toContain('已保存；点击“立即应用”后更新运行时。');
  await click('[aria-controls="notification-history"]');
  expect(get('#notification-history').textContent).toContain('已保存；点击“立即应用”后更新运行时。');
  await click('.notification-history-actions button');
  expect(get('[aria-controls="notification-history"]').getAttribute('aria-label')).toContain('0 条未读');
  await click('#logout'); expect(document.querySelector('#notification-history')).toBeNull();
  await click('[aria-controls="notification-history"]');
  expect(get('#notification-history').textContent).not.toContain('已保存；点击“立即应用”后更新运行时。');
});

it('前缀操作完成进入通知历史，页面保留当前运行值与编辑状态', async () => {
  await login({ name: 'settings' }); await input('#command-prefix', '!!'); await submit('.settings-form');
  expect(get('.command-settings').textContent).not.toContain('前缀已保存并立即生效。');
  expect(get('.command-settings').textContent).toContain('当前运行前缀：!!');
  await click('[aria-controls="notification-history"]');
  expect(get('#notification-history').textContent).toContain('前缀已保存并立即生效。');
});

it('展开移动导航时不消耗被遮住的通知倒计时', async () => {
  vi.spyOn(document, 'hasFocus').mockReturnValue(true);
  vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout', 'performance'] });
  try {
    await login({ name: 'overview' }); vi.advanceTimersByTime(1000); await nextTick();
    await click('[aria-label="打开导航菜单"]');
    vi.advanceTimersByTime(60000); await nextTick(); expect(document.querySelector('.notification-toast')).not.toBeNull();
    await click('.mobile-close'); vi.advanceTimersByTime(3999); await nextTick();
    expect(document.querySelector('.notification-toast')).not.toBeNull();
    vi.advanceTimersByTime(1); await nextTick(); expect(document.querySelector('.notification-toast')).toBeNull();
  } finally { app?.unmount(); vi.useRealTimers(); }
});

it('商店保存过程中阻止导航、退出和刷新丢失操作状态', async () => {
  let complete!: () => void;
  const item = { slug: 'ping', plugin_id: 'dev.bkm.ping', name: 'Ping', summary: '', trust: 'community', repository: 'https://github.com/example/ping', commands: ['ping'], capabilities: [], latest: { version: '1.0.0', protocol: '>=1.4,<2.0', download: 'https://github.com/example/ping/releases/download/v1.0.0/ping.bkm-plugin', sha256: 'a'.repeat(64) }, selection: 'b'.repeat(64), instance_id: 'dev.bkm.ping/default', installed: null, manageable: true };
  vi.stubGlobal('fetch', async (input: RequestInfo | URL, init: RequestInit = {}) => {
    const path = String(input);
    if (path === '/api/marketplace/install') {
      await new Promise<void>(resolve => { complete = resolve; });
      return new Response(JSON.stringify({ instance_id: item.instance_id, version: '1.0.0', enabled: true, restart_required: true, missing_permissions: [], config_incomplete: false, updated_at_ms: 1 }));
    }
    if (path.startsWith('/api/marketplace')) return new Response(JSON.stringify({ source: 'https://example.com/index.json', total: 1, offset: 0, plugins: [item] }));
    return fetcher(input, init);
  });
  await login({ name: 'marketplace' }); await click('.market-list button');
  await input('.market-confirm input', true); await click('.market-details .primary');
  await go({ name: 'overview' }); expect(router.currentRoute.value.name).toBe('marketplace');
  expect(disabled('#logout')).toBe(true);
  const event = new Event('beforeunload', { cancelable: true }); window.dispatchEvent(event); expect(event.defaultPrevented).toBe(true);
  complete(); await flush(); await go({ name: 'overview' }); expect(router.currentRoute.value.name).toBe('overview');
});

it('安装完成与插件列表重试重叠时仍排队读取安装后的新快照', async () => {
  let finishInstall!: () => void, finishRetry!: () => void, reads = 0;
  const item = { slug: 'ping', plugin_id: 'dev.bkm.market-ping', name: 'Market Ping', summary: '', trust: 'community', repository: 'https://github.com/example/ping', commands: ['ping'], capabilities: [], latest: { version: '1.0.0', protocol: '>=1.4,<2.0', download: 'https://github.com/example/ping/releases/download/v1.0.0/ping.bkm-plugin', sha256: 'a'.repeat(64) }, selection: 'b'.repeat(64), instance_id: 'dev.bkm.market-ping/default', installed: null, manageable: true };
  vi.stubGlobal('fetch', async (input: RequestInfo | URL, init: RequestInit = {}) => {
    const path = String(input);
    if (path === '/api/plugins') {
      reads++; if (reads === 1) return new Response('{}', { status: 503 });
      const snapshot = JSON.stringify({ plugins });
      if (reads === 2) await new Promise<void>(resolve => { finishRetry = resolve; });
      return new Response(snapshot);
    }
    if (path === '/api/marketplace/install') {
      await new Promise<void>(resolve => { finishInstall = resolve; });
      plugins.push(fixture({ instance_id: item.instance_id, plugin_id: item.plugin_id, name: 'Market Ping' }));
      return new Response(JSON.stringify({ instance_id: item.instance_id, version: '1.0.0', enabled: true, restart_required: true, missing_permissions: [], config_incomplete: false, updated_at_ms: 1 }));
    }
    if (path.startsWith('/api/marketplace')) return new Response(JSON.stringify({ source: 'https://example.com/index.json', total: 1, offset: 0, plugins: [item] }));
    return fetcher(input, init);
  });
  await login({ name: 'marketplace' }); await click('.market-list button');
  await input('.market-confirm input', true); await click('.market-details .primary');
  await click('#retry-plugins'); expect(reads).toBe(2);
  finishInstall(); await flush(); expect(reads).toBe(2);
  finishRetry(); await flush(); await flush(); expect(reads).toBe(3);
  await go({ name: 'plugins' }); expect(text()).toContain('Market Ping');
});

it('写入响应丢失后离开并重进市场仍同步已安装实例，不重复提交写入', async () => {
  let writes = 0;
  const item = { slug: 'ping', plugin_id: 'dev.bkm.market-ping', name: 'Market Ping', summary: '', trust: 'community', repository: 'https://github.com/example/ping', commands: ['ping'], capabilities: [], latest: { version: '1.0.0', protocol: '>=1.4,<2.0', download: 'https://github.com/example/ping/releases/download/v1.0.0/ping.bkm-plugin', sha256: 'a'.repeat(64) }, selection: 'b'.repeat(64), instance_id: 'dev.bkm.market-ping/default', installed: null as unknown, manageable: true };
  vi.stubGlobal('fetch', async (input: RequestInfo | URL, init: RequestInit = {}) => {
    const path = String(input);
    if (path === '/api/marketplace/install') {
      writes++; plugins.push(fixture({ instance_id: item.instance_id, plugin_id: item.plugin_id, name: 'Market Ping' }));
      item.installed = { version: '1.0.0', enabled: true, updated_at_ms: 1, granted_permissions: [], runtime_state: 'not_loaded' };
      throw new TypeError('response lost after commit');
    }
    if (path.startsWith('/api/marketplace')) return new Response(JSON.stringify({ source: 'https://example.com/index.json', total: 1, offset: 0, plugins: [item] }));
    return fetcher(input, init);
  });
  await login({ name: 'marketplace' }); await click('.market-list button');
  await input('.market-confirm input', true); await click('.market-details .primary');
  expect(text()).toContain('无法连接管理服务'); expect(writes).toBe(1);
  await go({ name: 'overview' }); await go({ name: 'marketplace' });
  await click('.market-list button'); await flush();
  await go(pluginRoute(item.instance_id));
  expect(text()).toContain('Market Ping'); expect(text()).not.toContain('未找到插件实例'); expect(writes).toBe(1);
});
