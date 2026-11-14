import { isObject } from './types';

export type SettingsScope = 'connections' | 'global';
export type Values = Record<string, string | number | boolean>;
export type Field = { key: string; label: string; type: 'boolean' | 'string' | 'number'; options?: string[]; max?: number; min?: number; hint?: string };
export type Group = { key: string; title: string; fields: Field[]; hint?: string };
export type SettingsSnapshot = { revision: string; values: Record<string, Values> };
export const flag = (key: string, label: string, hint?: string): Field => ({ key, label, type: 'boolean', hint });
export const number = (key: string, label: string, max: number, min = 1): Field => ({ key, label, type: 'number', min, max });
export const text = (key: string, label: string, options?: string[]): Field => ({ key, label, type: 'string', options });

export const connectionGroups: Group[] = [
  { key: 'qq', title: 'QQ 官方 · 已保存设置', hint: '群 @ 与 C2C 事件按既有默认订阅；特殊事件需要平台权限。', fields: [
    flag('enabled', '重启后启用 QQ'), text('environment', 'QQ 环境', ['production', 'sandbox']), text('transport', '事件接入方式', ['websocket', 'webhook']),
    flag('public_guild_messages', '公域频道 @ 消息'), flag('private_guild_messages', '私域频道全部消息（需权限）'), flag('direct_messages', '频道私信（不支持沙箱）'),
    flag('extended_events', '扩展事件（成员、互动等）'), flag('forum_events', '私域论坛事件（需权限）'), flag('audio_events', '音频事件（需权限）'),
    flag('open_forum_events', '开放论坛事件（实验）'), flag('audio_live_member_events', '音视频成员事件（实验）'), flag('check_only', '仅预检，不连接（不可同时启用 OneBot）'),
  ] },
  { key: 'webhook', title: 'QQ Webhook · 本地网络', hint: '选择 webhook 后使用；公网 HTTPS 与 TLS 由反向代理管理。这里只填写本地监听地址和不含查询参数的回调路径。', fields: [
    text('listen', 'Webhook 监听 IP:端口'), text('path', 'Webhook 回调路径'), number('timestamp_tolerance_seconds', '验签时间容差（秒）', 3600),
    number('max_body_bytes', '请求体上限（字节）', 16777216), number('max_request_concurrency', '请求并发上限', 1024), number('request_timeout_seconds', '请求超时（秒）', 60),
  ] },
  { key: 'onebot11', title: 'OneBot 11 · 反向 WebSocket', fields: [
    flag('enabled', '重启后启用 OneBot 11'), text('listen', 'OneBot 监听 IP:端口'), flag('allow_insecure_remote', '允许远程明文连接', '非回环监听需要显式开启，请通过可信网络或 TLS 代理接入。'),
    number('action_timeout_seconds', '动作超时（秒）', 300), number('max_message_bytes', '消息上限（字节）', 16777216), number('max_pending_actions', '等待动作上限', 4096),
  ] },
];

export function readSettings(value: unknown, scope: SettingsScope, groups: Group[]): SettingsSnapshot {
  const fail = () => { throw new Error('设置响应格式异常，请重新读取已保存设置。'); };
  if (!isObject(value) || typeof value.revision !== 'string' || !/^[a-f0-9]{64}$/.test(value.revision)
    || value.apply !== 'restart_required' || value.credential_presence !== 'unknown') return fail();
  const input = value[scope];
  if (!isObject(input)) return fail();
  const values: Record<string, Values> = {};
  for (const group of groups) {
    let parent: unknown = input;
    if (group.key === 'webhook') parent = input.qq;
    else if (['console', 'files'].includes(group.key)) parent = input.logging;
    if (!isObject(parent) || !isObject(parent[group.key])) return fail();
    const raw = parent[group.key] as Record<string, unknown>;
    if (Object.keys(raw).some(key => !group.fields.some(field => field.key === key) && !(group.key === 'qq' && key === 'webhook'))) return fail();
    const fields: Values = {};
    for (const field of group.fields) {
      const v = raw[field.key];
      if (typeof v !== field.type || (typeof v === 'number' && !Number.isSafeInteger(v)) || (typeof v === 'string' && v.length > 1024)) return fail();
      fields[field.key] = v as Values[string];
    }
    values[group.key] = fields;
  }
  return { revision: value.revision, values };
}
export function settingsPayload(snapshot: SettingsSnapshot, scope: SettingsScope) {
  const v = snapshot.values;
  return scope === 'connections' ? { qq: { ...v.qq, webhook: v.webhook }, onebot11: v.onebot11 }
    : { runtime: v.runtime, logging: { console: v.console, files: v.files } };
}
