import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { expect, it } from 'vitest';
import { logSource, readLog } from './logPresentation';
import type { LogEntry, PluginView } from './types';

const entry = (target: string, message: string, fields: LogEntry['fields'] = {}, level = 'INFO'): LogEntry => ({
  id: 1, timestamp: '2026-11-14T00:00:00Z', category: 'runtime', level,
  target, message, instance_id: null, fields,
});
// Targets are Rust module paths (the application binary is named bkmqb).
const producers: [string, string, [string, string, string][]][] = [
  ['bkmqb::bootstrap', 'src/bootstrap.rs', [
    ['loaded local secrets environment file', '已载入', '已加载本地密钥环境文件'],
    ['starting BroKnowMyQQBot adapters', '启动中', '正在启动机器人适配器'],
    ['optional Browser Runtime is not installed', '未安装', '可选浏览器运行时未安装，浏览器能力不可用'],
    ['optional Browser Runtime is unavailable', '不可用', '可选浏览器运行时不可用'],
    ['optional Browser Runtime discovery task failed', '检测失败', '可选浏览器运行时检测失败'],
    ['optional Browser Runtime discovery timed out', '检测超时', '可选浏览器运行时检测超时'],
    ['QQ credentials, Gateway endpoint, and plugins are available', '预检通过', 'QQ 凭据、网关和插件预检通过'],
    ['BroKnowMyQQBot stopped', '已停止', '机器人已停止'],
    ['plugin shutdown failed after runtime failure', '关闭失败', '机器人运行失败后，关闭插件也失败'],
  ]],
  ['bkmqb::browser', 'src/browser.rs', [
    ['using private Browser Runtime fallback directory', '目录回退', '浏览器运行时改用私有备用目录'],
  ]],
  ['plugin_host::runtime', 'crates/plugin-host/src/runtime/mod.rs', [
    ['registered plugin', '已注册', '已注册插件'],
  ]],
  ['bkmqb::plugins', 'src/plugins.rs', [
    ['loaded local WASM plugin', '已加载', '已加载本地 WASM 插件'],
    ['WASM plugin is disabled', '已禁用', 'WASM 插件已禁用'],
  ]],
  ['bkmqb::management', 'src/management.rs', [
    ['management server listening', '监听中', '管理服务正在监听'],
    ['configuration validation failed', '校验失败', '配置校验未通过'],
  ]],
  ['bot_core::runtime', 'crates/bot-core/src/runtime.rs', [
    ['bot runtime started', '已启动', '机器人运行时已启动'],
    ['bot runtime is shutting down', '关闭中', '正在关闭机器人运行时'],
    ['event handler failed', '处理失败', '事件处理失败'],
    ['event handler timed out', '处理超时', '事件处理超时'],
    ['adapter stopped with an error', '异常停止', '适配器因错误停止'],
  ]],
  ['adapter_qqbot::websocket', 'crates/adapter-qqbot/src/websocket.rs', [
    ['connecting to QQ Gateway', '连接中', '正在连接 QQ 网关'],
    ['QQ Gateway connected', '已连接', 'QQ 网关已连接'],
    ['QQ Gateway session resumed', '已恢复', 'QQ 网关会话已恢复'],
    ['QQ Gateway requested reconnect', '重连中', 'QQ 网关要求重新连接'],
    ['QQ Gateway connection stopped', '连接中断', 'QQ 网关连接中断'],
    ['retrying QQ Gateway connection after backoff', '等待重试', '等待退避后重试 QQ 网关连接'],
    ['QQ Gateway closed WebSocket', '已断开', 'QQ 网关已关闭连接'],
    ['sent QQ Gateway Resume', '等待恢复', '已发送 QQ 会话恢复请求'],
    ['sent QQ Gateway Identify', '等待认证', '已发送 QQ 登录请求'],
    ['received QQ message event', '已收到', '收到 QQ 消息事件'],
    ['received unmapped QQ Gateway event', '未映射', '收到尚未映射的 QQ 网关事件'],
    ['QQ message action succeeded', '已执行', 'QQ 消息操作已执行'],
    ['QQ message action failed', '执行失败', 'QQ 消息操作失败'],
    ['QQ platform action succeeded', '已执行', 'QQ 平台操作已执行'],
    ['QQ platform action failed', '执行失败', 'QQ 平台操作失败'],
  ]],
  ['adapter_qqbot::webhook', 'crates/adapter-qqbot/src/webhook.rs', [
    ['QQ Webhook callback server is listening', '监听中', 'QQ 回调服务正在监听'],
    ['accepted QQ Webhook event', '已接收', '已接收 QQ 回调事件'],
    ['rejected QQ Webhook callback', '已拒绝', 'QQ 回调认证未通过'],
  ]],
  ['adapter_onebot11', 'crates/adapter-onebot11/src/lib.rs', [
    ['OneBot 11 reverse WebSocket listening', '监听中', 'OneBot 11 反向连接服务正在监听'],
    ['OneBot 11 reverse WebSocket connected', '已连接', 'OneBot 11 反向连接已建立'],
    ['OneBot 11 reverse WebSocket disconnected', '已断开', 'OneBot 11 反向连接已断开'],
    ['OneBot 11 handshake failed', '握手失败', 'OneBot 11 连接握手失败'],
    ['ignored malformed OneBot action response', '响应无效', '已忽略格式错误的 OneBot 动作响应'],
    ['ignored invalid OneBot event', '事件无效', '已忽略无效 OneBot 事件'],
  ]],
];

for (const [target, file, fixtures] of producers) {
  const source = readFileSync(resolve(import.meta.dirname, '../..', file), 'utf8');
  it.each(fixtures)(`${target} 精确识别源码模板 %s`, (message, status, summary) => {
    expect(source).toContain(`"${message}"`);
    expect(readLog(entry(target, message))).toMatchObject({ status, summary });
    expect(logSource(entry(target, message), new Map())).not.toContain('::');
    expect(readLog(entry(`${target}::other`, message))).toBeUndefined();
    expect(readLog(entry('third_party', message))).toBeUndefined();
    expect(readLog(entry(target, `${message} extra`))).toBeUndefined();
  });
}

it('record_kind优先，未知或未识别的结构化状态不被英文模板改写', () => {
  const base = entry('adapter_qqbot::websocket', 'QQ Gateway connected');
  expect(readLog({ ...base, fields: { record_kind: 'message.failed' } })?.status).toBe('发送失败');
  const unknown: LogEntry['fields'][] = [{ record_kind: 'new.kind' }, { record_kind: '' }, { record_kind: 'plugin.completed', status: 'future' }];
  for (const fields of unknown) {
    expect(readLog({ ...base, fields })).toBeUndefined();
  }
  for (const target of ['constructor', '__proto__', 'toString']) expect(readLog(entry(target, 'registered plugin'))).toBeUndefined();
  expect(readLog(entry('bkmqb::bootstrap', 'constructor'))).toBeUndefined();
});

it('只展示模板选定的有效详情，隐藏路径、错误原文、身份与内部字段', () => {
  const fields = { path: '/private/secrets.env', error: 'internal path', seq: '41', reason: 'internal detail', event_id: 'private-id', extra: 'hidden' };
  const loaded = readLog(entry('bkmqb::bootstrap', 'loaded local secrets environment file', fields));
  expect(loaded?.detail).toBeUndefined();
  expect(JSON.stringify(loaded)).not.toMatch(/private|internal|hidden/u);
  const version = 'bkmqb 0.1.0 (git 1bb3e0e, built 2026-11-14)';
  expect(readLog(entry('bkmqb::bootstrap', 'starting BroKnowMyQQBot adapters', { ...fields, version }))?.detail).toBe(version);
  expect(readLog(entry('bot_core::runtime', 'bot runtime started', { ...fields, adapter_count: '2' }))?.detail).toBe('适配器 2 个');
  expect(readLog(entry('adapter_qqbot::websocket', 'retrying QQ Gateway connection after backoff', { delay_ms: '1500' }))?.detail).toBe('等待 1500 毫秒');
  expect(readLog(entry('adapter_qqbot::websocket', 'QQ Gateway closed WebSocket', { code: '4014', reason: 'secret' }))?.detail).toBe('关闭码 4014');
  for (const value of ['-1', 'NaN', 'Infinity', '1.5', '9007199254740992', '<img>']) {
    expect(readLog(entry('bot_core::runtime', 'bot runtime started', { adapter_count: value }))?.detail).toBeUndefined();
  }
  expect(readLog(entry('bkmqb::bootstrap', 'starting BroKnowMyQQBot adapters', { version: '/private/path' }))?.detail).toBeUndefined();
});

it.each(['127.0.0.1:9090', '[::1]:8080', '0.0.0.0:3000'])('显示有效监听地址 %s', listen => {
  expect(readLog(entry('bkmqb::management', 'management server listening', { listen }))?.detail).toBe(`地址 ${listen}`);
});
it.each(['http://user:secret@localhost', '/private/path', '127.0.0.1:65536', '[invalid]:9090', '999.1.1.1:80', '127.0.0.1:9090/path'])('无效监听地址不进入摘要 %s', listen => {
  expect(readLog(entry('bkmqb::management', 'management server listening', { listen }))?.detail).toBeUndefined();
});

it('运行插件名称使用PluginView，实例或插件ID不匹配时不套用其他名称', () => {
  const plugins = new Map([['ping/default', { name: '连通测试', plugin_id: 'dev.bkm.ping' } as PluginView]]);
  const registered = { ...entry('plugin_host::runtime', 'registered plugin', { plugin_id: 'dev.bkm.ping' }), instance_id: 'ping/default' };
  expect(logSource(registered, plugins)).toBe('插件宿主 · 连通测试');
  expect(logSource({ ...registered, fields: { plugin_id: 'dev.other' } }, plugins)).toBe('插件宿主 · dev.other');
  expect(logSource({ ...registered, instance_id: null }, plugins)).toBe('插件宿主 · dev.bkm.ping');
  expect(logSource({ ...entry('bkmqb::plugins', 'WASM plugin is disabled'), instance_id: 'ping/default' }, plugins)).toBe('插件加载 · 连通测试');
});
