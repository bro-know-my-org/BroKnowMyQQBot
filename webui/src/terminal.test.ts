import { afterEach, expect, it, vi } from 'vitest';
import { createApp, defineComponent, h, nextTick, ref } from 'vue';
import TerminalOutput from './components/TerminalOutput.vue';
import type { LogEntry } from './types';
let app: ReturnType<typeof createApp>;
const flush = async () => { await nextTick(); await nextTick(); };
const entry = (id: number): LogEntry => ({ id, timestamp: '2026-11-13T10:00:00.123Z', level: 'INFO', target: 'runtime', message: `line ${id}`, instance_id: null, fields: {} });
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
