import { afterEach, expect, it, vi } from 'vitest';
import { createApp, defineComponent, h, nextTick, ref } from 'vue';
import TerminalOutput from './components/TerminalOutput.vue';
import type { LogEntry } from './types';
let app: ReturnType<typeof createApp>;
const flush = async () => { await nextTick(); await nextTick(); };
const entry = (id: number): LogEntry => ({ id, category: 'runtime', timestamp: '2026-11-13T10:00:00.123Z', level: 'INFO', target: 'runtime', message: `line ${id}`, instance_id: null, fields: {} });
afterEach(() => { app?.unmount(); document.body.replaceChildren(); vi.restoreAllMocks(); });
it('新增日志时跟随，向上阅读不抢位置，可以恢复跟随，也可以主动关闭跟随', async () => {
  const entries = ref<LogEntry[]>([]), host = document.createElement('div'); document.body.append(host);
  app = createApp(defineComponent(() => () => h(TerminalOutput, { entries: entries.value, emptyMessage: '等待输出' }))); app.mount(host);
  const viewport = document.querySelector<HTMLElement>('.terminal-viewport')!, follow = document.querySelector<HTMLButtonElement>('.terminal-toolbar button')!;
  let position = 0, viewportHeight = 60, lineHeight = 20;
  // Model real scroll bounds: a 60px viewport containing 20px lines.
  Object.defineProperties(viewport, {
    clientHeight: { get: () => viewportHeight },
    scrollHeight: { get: () => entries.value.length * lineHeight },
    scrollTop: { get: () => position, set: (value: number) => { position = Math.max(0, Math.min(value, viewport.scrollHeight - viewport.clientHeight)); } },
  });
  entries.value = Array.from({ length: 10 }, (_, i) => entry(i + 1)); await flush();
  expect(position + viewport.clientHeight).toBe(viewport.scrollHeight); expect(follow.getAttribute('aria-pressed')).toBe('true');
  viewport.scrollTop = 20; viewport.dispatchEvent(new Event('scroll'));
  entries.value = [...entries.value, entry(11), entry(12)]; await flush();
  expect(position).toBe(20); expect(follow.getAttribute('aria-pressed')).toBe('false');
  expect(document.querySelector('.terminal-footer')?.textContent).toContain('正在查看较早记录');
  follow.click(); await flush(); expect(position + viewport.clientHeight).toBe(viewport.scrollHeight);
  entries.value = [...entries.value, entry(13)]; await flush(); expect(position + viewport.clientHeight).toBe(viewport.scrollHeight);
  viewportHeight = 40; window.dispatchEvent(new Event('resize')); await flush();
  expect(position + viewport.clientHeight).toBe(viewport.scrollHeight); expect(follow.getAttribute('aria-pressed')).toBe('true');
  follow.click(); await flush(); const stoppedAt = position;
  lineHeight = 40; window.dispatchEvent(new Event('resize')); await flush(); expect(position).toBe(stoppedAt);
  entries.value = [...entries.value, entry(14)]; await flush(); expect(position).toBe(stoppedAt); expect(follow.getAttribute('aria-pressed')).toBe('false');
});
it('终端保持文本转义并显示实例、时间与同一行字段', async () => {
  const host = document.createElement('div'); document.body.append(host);
  app = createApp(TerminalOutput, { entries: [{ ...entry(1), message: '<img src=x>', instance_id: 'plugin/a', fields: { error: '<script>bad</script>', detail: 'two words' } }], emptyMessage: '等待输出' }); app.mount(host);
  const line = document.querySelector('.terminal-line')!;
  expect(line.textContent).toContain('INFO  runtime [plugin/a]: <img src=x>');
  expect(line.textContent).toContain('error=<script>bad</script> detail="two words"');
  expect(line.querySelector('time')?.getAttribute('datetime')).toBe('2026-11-13T10:00:00.123Z');
  expect(document.querySelector('script,img')).toBeNull();
});

it('缓冲淘汰上方记录时保持正在阅读的行，当前行被淘汰时回到最早保留记录', async () => {
  const sessionId = ref('first');
  const entries = ref(Array.from({ length: 10 }, (_, i) => entry(i + 1))), host = document.createElement('div'); document.body.append(host);
  app = createApp(defineComponent(() => () => h(TerminalOutput, { entries: entries.value, sessionId: sessionId.value, emptyMessage: '等待输出' }))); app.mount(host);
  const viewport = document.querySelector<HTMLElement>('.terminal-viewport')!;
  let position = 0;
  Object.defineProperties(viewport, {
    clientHeight: { get: () => 60 }, scrollHeight: { get: () => entries.value.length * 20 },
    scrollTop: { get: () => position, set: (value: number) => { position = Math.max(0, Math.min(value, viewport.scrollHeight - 60)); } },
  });
  vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockImplementation(function (this: HTMLElement) {
    const index = this.classList.contains('terminal-line') ? Array.from(this.parentElement!.children).indexOf(this) : 0;
    const top = this.classList.contains('terminal-line') ? index * 20 - position : 0;
    return { top, bottom: top + (this === viewport ? 60 : 20) } as DOMRect;
  });
  window.dispatchEvent(new Event('resize')); await flush();
  viewport.scrollTop = 100; viewport.dispatchEvent(new Event('scroll')); await flush();
  entries.value = Array.from({ length: 10 }, (_, i) => entry(i + 3)); await flush();
  expect(document.querySelector('[data-entry-id="6"]')?.getBoundingClientRect().top).toBe(0);
  expect(position).toBe(60);
  entries.value = Array.from({ length: 10 }, (_, i) => entry(i + 20)); await flush();
  expect(position).toBe(0); expect(document.querySelector('.terminal-line')?.textContent).toContain('line 20');
  sessionId.value = 'second'; entries.value = []; await flush();
  viewport.scrollTop = 0; viewport.dispatchEvent(new Event('scroll')); await flush();
  expect(document.querySelector('.terminal-toolbar button')?.getAttribute('aria-pressed')).toBe('false');
  entries.value = Array.from({ length: 10 }, (_, i) => entry(i + 30)); await flush();
  expect(position).toBe(0);
  expect(document.querySelector('.terminal-toolbar button')?.getAttribute('aria-pressed')).toBe('false');
});

it('追加输出保留已有行节点和选中的文本区域，不重写旧行', async () => {
  const entries = ref([entry(1)]), host = document.createElement('div'); document.body.append(host);
  app = createApp(defineComponent(() => () => h(TerminalOutput, { entries: entries.value, emptyMessage: '等待输出' }))); app.mount(host);
  const first = document.querySelector('.terminal-line')!, text = first.querySelector('.terminal-message')!.firstChild!;
  const changes: MutationRecord[] = [], observer = new MutationObserver(records => changes.push(...records));
  observer.observe(first, { childList: true, characterData: true, subtree: true });
  try {
    entries.value = [...entries.value, entry(2), entry(3)]; await flush();
    expect(document.querySelector('.terminal-line')).toBe(first);
    expect(first.querySelector('.terminal-message')!.firstChild).toBe(text);
    expect(changes).toHaveLength(0);
  } finally { observer.disconnect(); }
});

it('默认长行横向滚动，换行开关保留行节点和跟随状态', async () => {
  const host = document.createElement('div'); document.body.append(host);
  app = createApp(TerminalOutput, { entries: [{ ...entry(1), message: '很长的日志 '.repeat(200) }], emptyMessage: '等待输出' }); app.mount(host);
  const viewport = document.querySelector<HTMLElement>('.terminal-viewport')!, line = document.querySelector('.terminal-line');
  const controls = document.querySelectorAll<HTMLButtonElement>('.terminal-controls button'), wrap = controls[1]!;
  expect(viewport.classList.contains('is-wrapping')).toBe(false); expect(wrap.getAttribute('aria-pressed')).toBe('false');
  viewport.scrollLeft = 240; viewport.dispatchEvent(new Event('scroll')); await flush();
  expect(controls[0]!.getAttribute('aria-pressed')).toBe('true');
  wrap.click(); await flush(); expect(viewport.classList.contains('is-wrapping')).toBe(true);
  expect(wrap.textContent).toBe('自动换行：开'); expect(document.querySelector('.terminal-line')).toBe(line);
  wrap.click(); await flush(); expect(viewport.classList.contains('is-wrapping')).toBe(false);
  expect(controls[0]!.getAttribute('aria-pressed')).toBe('true');
});

it('发送结果只展示状态，不把结果记录误标为正文未采集', async () => {
  const host = document.createElement('div'); document.body.append(host);
  app = createApp(TerminalOutput, { entries: [
    { ...entry(1), category: 'message', fields: { record_kind: 'message.sending' }, content: 'pong' },
    { ...entry(2), category: 'message', fields: { record_kind: 'message.sent', status: 'succeeded' } },
    { ...entry(3), category: 'message', fields: { record_kind: 'message.failed', status: 'failed' } },
  ], emptyMessage: '等待输出', includeContent: true }); app.mount(host);
  expect(document.querySelectorAll('.terminal-body')).toHaveLength(1);
  expect(host.textContent).toContain('正文=pong');
  expect(host.textContent).not.toContain('正文未采集');
});

it("中文阅读与原始日志切换保留节点，已采集正文仍按独立开关显示", async () => {
  const includeContent = ref(false), host = document.createElement("div"); document.body.append(host);
  const entries: LogEntry[] = [{ ...entry(1), category: "message", target: "bkm::message_trace", message: "received message", fields: { record_kind: "message.received", message_scope: "private", event_id: "event-1", adapter_id: "qq-official" }, content: "<img src=x>" }];
  app = createApp(defineComponent(() => () => h(TerminalOutput, { entries, includeContent: includeContent.value, emptyMessage: "等待输出" }))); app.mount(host);
  const line = document.querySelector(".terminal-line"), raw = [...document.querySelectorAll("button")].find(button => button.textContent === "原始日志：关")!;
  expect(line?.textContent).toContain("[已收到] qq-official: 收到一条私聊消息");
  expect(line?.textContent).not.toContain("event_id"); expect(line?.textContent).not.toContain("<img");
  raw.click(); await flush();
  expect(document.querySelector(".terminal-line")).toBe(line); expect(raw.getAttribute("aria-pressed")).toBe("true");
  expect(line?.textContent).toContain("INFO  bkm::message_trace: [方向未标注] received message");
  expect(line?.textContent).toContain("event_id=event-1"); expect(line?.textContent).not.toContain("<img");
  includeContent.value = true; await flush(); expect(line?.textContent).toContain("正文=<img src=x>");
  raw.click(); await flush(); expect(line?.textContent).toContain("收到一条私聊消息"); expect(line?.textContent).toContain("正文=<img src=x>");
  expect(document.querySelector("img")).toBeNull();
});
it("阅读层中的插件展示名称和指令保持转义，追加记录不重写已有摘要", async () => {
  const host = document.createElement("div"); document.body.append(host);
  const entries = ref<LogEntry[]>([{ ...entry(1), instance_id: "plugin/default", fields: { record_kind: "plugin.completed", status: "output_committed", plugin_id: "dev.bkm.plugin", command_name: "<script>bad</script>", command_count: "1" } }]);
  app = createApp(defineComponent(() => () => h(TerminalOutput, { entries: entries.value, plugins: [{ instance_id: "plugin/default", plugin_id: "dev.bkm.plugin", name: "<img src=x>" } as import("./types").PluginView], emptyMessage: "等待输出" }))); app.mount(host);
  const row = document.querySelector(".terminal-line"), text = row?.querySelector(".terminal-message")?.firstChild;
  expect(row?.textContent).toContain("<img src=x> · plugin/default: 插件产生了输出");
  expect(row?.textContent).toContain("候选指令：<script>bad</script>"); expect(document.querySelector("script,img")).toBeNull();
  entries.value = [...entries.value, entry(2)]; await flush();
  expect(document.querySelector(".terminal-line")).toBe(row); expect(row?.querySelector(".terminal-message")?.firstChild).toBe(text);
});

it("向上阅读并换行时，异步插件名称和原始日志切换均保留可见行锚点", async () => {
  const host = document.createElement("div"); document.body.append(host);
  const entries = Array.from({ length: 10 }, (_, index): LogEntry => ({ ...entry(index + 1), instance_id: "plugin/default", fields: { record_kind: "plugin.completed", status: "no_output" } }));
  const plugins = ref([{ instance_id: "plugin/default", name: "短名" } as import("./types").PluginView]);
  app = createApp(defineComponent(() => () => h(TerminalOutput, { entries, plugins: plugins.value, emptyMessage: "等待输出" }))); app.mount(host);
  const viewport = document.querySelector<HTMLElement>(".terminal-viewport")!;
  let position = 0;
  const height = () => document.querySelector(".terminal-field") ? 60 : document.querySelector(".terminal-source")?.textContent?.startsWith("长") ? 40 : 20;
  Object.defineProperties(viewport, {
    clientHeight: { get: () => 60 }, scrollHeight: { get: () => 10 * height() },
    scrollTop: { get: () => position, set: (value: number) => { position = Math.max(0, Math.min(value, viewport.scrollHeight - 60)); } },
  });
  vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(function (this: HTMLElement) {
    const line = this.classList.contains("terminal-line"), index = line ? Array.from(this.parentElement!.children).indexOf(this) : 0;
    const top = line ? index * height() - position : 0;
    return { top, bottom: top + (line ? height() : 60) } as DOMRect;
  });
  const controls = document.querySelectorAll<HTMLButtonElement>(".terminal-controls button");
  controls[1]!.click(); await flush(); window.dispatchEvent(new Event("resize"));
  viewport.scrollTop = 100; viewport.dispatchEvent(new Event("scroll")); await flush();
  const anchor = document.querySelector("[data-entry-id=\"6\"]")!;
  expect(anchor.getBoundingClientRect().top).toBe(0);
  plugins.value = [{ ...plugins.value[0]!, name: "长名称需要换行" }]; await flush();
  expect(position).toBe(200); expect(anchor.getBoundingClientRect().top).toBe(0);
  controls[2]!.click(); await flush();
  expect(position).toBe(300); expect(anchor.getBoundingClientRect().top).toBe(0);
  expect(controls[0]!.getAttribute("aria-pressed")).toBe("false");
});

it('常见运行日志默认中文摘要，保留级别并完整恢复安全原文，追加保持旧节点', async () => {
  const host = document.createElement('div'); document.body.append(host);
  const entries = ref<LogEntry[]>([
    { ...entry(1), target: 'bkmqb::browser', level: 'WARN', message: 'using private Browser Runtime fallback directory', fields: { path: '/private/browser', reason: '<img src=x>' } },
    { ...entry(2), target: 'bkmqb::bootstrap', message: 'optional Browser Runtime is not installed', fields: {} },
    { ...entry(3), target: 'bkmqb::management', message: 'management server listening', fields: { listen: '127.0.0.1:9090', auth: 'None' } },
    { ...entry(4), target: 'adapter_qqbot::websocket', message: 'QQ message action failed', fields: { adapter_id: 'qq-official', error: '<script>failure</script>' } },
  ]);
  app = createApp(defineComponent(() => () => h(TerminalOutput, { entries: entries.value, emptyMessage: '等待输出' }))); app.mount(host);
  const rows = [...host.querySelectorAll('.terminal-line')], raw = [...host.querySelectorAll('button')].find(button => button.textContent === '原始日志：关')!;
  expect(rows[0]?.textContent).toContain('[目录回退] 浏览器运行时: 浏览器运行时改用私有备用目录 [WARN]');
  expect(rows[0]?.textContent).not.toMatch(/private|reason|img/u);
  expect(rows[1]?.textContent).toContain('可选浏览器运行时未安装，浏览器能力不可用 [INFO]');
  expect(rows[2]?.textContent).toContain('管理服务正在监听 · 地址 127.0.0.1:9090 [INFO]');
  expect(rows[2]?.textContent).not.toContain('auth');
  expect(rows[3]?.textContent).toContain('[执行失败] QQ 网关 · qq-official: QQ 消息操作失败 [INFO]');
  const text = rows[0]?.querySelector('.terminal-message')?.firstChild;
  entries.value = [...entries.value, entry(5)]; await flush();
  expect(host.querySelector('.terminal-line')).toBe(rows[0]);
  expect(rows[0]?.querySelector('.terminal-message')?.firstChild).toBe(text);
  raw.click(); await flush();
  expect(host.querySelector('.terminal-line')).toBe(rows[0]);
  expect(rows[0]?.textContent).toContain('WARN  bkmqb::browser: using private Browser Runtime fallback directory path=/private/browser reason="<img src=x>"');
  expect(rows[2]?.textContent).toContain('INFO  bkmqb::management: management server listening listen=127.0.0.1:9090 auth=None');
  expect(rows[3]?.textContent).toContain('error=<script>failure</script>');
  expect(host.querySelector('img,script')).toBeNull();
  expect(host.querySelector('.terminal-body')).toBeNull();
  raw.click(); await flush();
  expect(rows[0]?.textContent).toContain('浏览器运行时改用私有备用目录 [WARN]');
});
