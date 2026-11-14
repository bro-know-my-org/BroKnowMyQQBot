import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { createApp, h, nextTick, reactive } from 'vue';
import FailedDeliveries from '../src/components/FailedDeliveries.vue';
import type { Request } from '../src/api';

const row = { attempts: 3, error_class: 'guest_trap', updated_at_ms: 1000 };
let app: ReturnType<typeof createApp>;
const props = reactive({ instanceId: 'plugin/a #?', revision: 1, busy: false });
const request = vi.fn();
const flush = async () => { await new Promise<void>(resolve => setImmediate(resolve)); await nextTick(); };
const get = (id: string) => document.querySelector<HTMLButtonElement>(id)!;
async function mount() {
  const root = document.createElement('div'); document.body.append(root);
  app = createApp({ render: () => h(FailedDeliveries, { ...props, request: request as Request }) }); app.mount(root); await flush();
}
beforeEach(() => { vi.clearAllMocks(); props.instanceId = 'plugin/a #?'; props.revision = 1; props.busy = false; });
afterEach(() => { app?.unmount(); document.body.replaceChildren(); });

it('只显示安全分类、尝试和时间，用有界游标分页且回到最新时重置', async () => {
  request.mockResolvedValueOnce({ entries: Array.from({ length: 20 }, () => ({ ...row, payload: 'secret body', last_error: 'secret token' })), next_before: 42 });
  request.mockResolvedValueOnce({ entries: [{ ...row, error_class: 'unknown' }], next_before: null });
  request.mockResolvedValueOnce({ entries: [], next_before: null });
  await mount();
  expect(request.mock.calls[0]![0]).toBe('/api/plugins/failures?instance_id=plugin%2Fa+%23%3F&limit=20');
  expect(document.querySelectorAll('.failure-list li')).toHaveLength(20);
  expect(document.body.textContent).toContain('尝试 3 次');
  expect(document.body.textContent).not.toContain('secret');
  get('#next-failures').focus(); get('#next-failures').click(); await flush();
  expect(request.mock.calls[1]![0]).toContain('before=42');
  expect(document.querySelectorAll('.failure-list li')).toHaveLength(1);
  expect(document.body.textContent).toContain('未分类');
  expect(document.body.textContent).toContain('第 2 页');
  expect(document.activeElement).toBe(get('#refresh-failures'));
  get('#refresh-failures').click(); await flush();
  expect(request.mock.calls[2]![0]).not.toContain('before=');
  expect(document.body.textContent).toContain('第 1 页');
  expect(document.body.textContent).toContain('当前没有死信记录');
});

it.each([
  { entries: Array.from({ length: 21 }, () => row), next_before: null },
  { entries: [row], next_before: 10 },
  { entries: [{ ...row, error_class: 'secret diagnostic' }], next_before: null },
  { entries: [{ ...row, attempts: -1 }], next_before: null },
])('拒绝越界或未知分类响应，不直接显示服务端诊断', async page => {
  request.mockResolvedValue(page); await mount();
  expect(document.querySelector('[role="alert"]')?.textContent).toContain('格式异常');
  expect(document.body.textContent).not.toContain('secret');
  expect(document.querySelectorAll('.failure-list li')).toHaveLength(0);
});

it.each([false, true])('延迟末页响应期间原按钮失焦时恢复焦点，并尊重其他焦点选择：%s', async moveFocus => {
  request.mockResolvedValueOnce({ entries: Array.from({ length: 20 }, () => row), next_before: 42 });
  let finish!: (value: unknown) => void;
  request.mockImplementationOnce(() => new Promise(resolve => { finish = resolve; }));
  await mount();
  const button = get('#next-failures'); button.focus(); button.click(); await nextTick();
  expect(button.disabled).toBe(true);
  // Model the native blur caused by disabling a button; jsdom does not do this itself.
  button.blur();
  const other = document.createElement('button'); document.body.append(other);
  if (moveFocus) other.focus();
  finish({ entries: [], next_before: null }); await flush();
  expect(document.activeElement).toBe(moveFocus ? other : get('#refresh-failures'));
});

it('卸载后忽略晚到的分页结果', async () => {
  let finish!: (value: unknown) => void;
  request.mockImplementationOnce(() => new Promise(resolve => { finish = resolve; }));
  await mount(); app.unmount();
  finish({ entries: [row], next_before: null }); await flush();
  expect(document.querySelector('.failure-list')).toBeNull();
});

it.each(['revision', 'instanceId'] as const)('恢复或切换实例后清除旧记录和游标，即使新读取失败：%s', async field => {
  request.mockResolvedValueOnce({ entries: Array.from({ length: 20 }, () => row), next_before: 42 });
  let fail!: (reason: Error) => void;
  request.mockImplementationOnce(() => new Promise((_, reject) => { fail = reject; }));
  await mount(); expect(document.querySelectorAll('.failure-list li')).toHaveLength(20);
  if (field === 'revision') props.revision++; else props.instanceId = 'other-plugin';
  await nextTick();
  expect(document.querySelector('.failure-list')).toBeNull();
  expect(document.querySelector('#next-failures')).toBeNull();
  fail(new Error('读取失败')); await flush();
  expect(document.querySelector('[role="alert"]')?.textContent).toContain('读取失败');
  expect(document.querySelector('.failure-list')).toBeNull();
  expect(document.querySelector('#next-failures')).toBeNull();
});
