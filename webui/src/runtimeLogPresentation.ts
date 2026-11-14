import type { LogEntry } from './types';

type Detail = 'version' | 'listen' | 'adapter_count' | 'delay_ms' | 'code';
type Template = readonly [summary: string, status: string, detail?: Detail];
interface Vocabulary { source: string; messages: Record<string, Template> }

// Exact tracing targets and constant messages from the named producer modules.
// Do not reuse these translations for dynamic text or arbitrary namespaces.
const vocabulary: Record<string, Vocabulary> = {
  'bkmqb::bootstrap': { source: '启动配置', messages: {
    'loaded local secrets environment file': ['已加载本地密钥环境文件', '已载入'],
    'starting BroKnowMyQQBot adapters': ['正在启动机器人适配器', '启动中', 'version'],
    'QQ credentials, Gateway endpoint, and plugins are available': ['QQ 凭据、网关和插件预检通过', '预检通过'],
    'BroKnowMyQQBot stopped': ['机器人已停止', '已停止'],
    'optional Browser Runtime is not installed': ['可选浏览器运行时未安装，浏览器能力不可用', '未安装'],
    'optional Browser Runtime is unavailable': ['可选浏览器运行时不可用', '不可用'],
    'optional Browser Runtime discovery task failed': ['可选浏览器运行时检测失败', '检测失败'],
    'optional Browser Runtime discovery timed out': ['可选浏览器运行时检测超时', '检测超时'],
    'timed-out Browser Runtime discovery task failed to join': ['浏览器运行时检测任务收尾失败', '收尾失败'],
    'plugin shutdown failed after plugin loading failure': ['插件加载失败后，关闭插件也失败', '关闭失败'],
    'plugin shutdown failed after Gateway preflight failure': ['QQ 网关预检失败后，关闭插件也失败', '关闭失败'],
    'plugin shutdown failed after runtime failure': ['机器人运行失败后，关闭插件也失败', '关闭失败'],
    'management service also failed during runtime shutdown': ['机器人关闭期间管理服务也发生故障', '服务异常'],
  } },
  'bkmqb::browser': { source: '浏览器运行时', messages: {
    'using private Browser Runtime fallback directory': ['浏览器运行时改用私有备用目录', '目录回退'],
  } },
  'bkmqb::plugins': { source: '插件加载', messages: {
    'WASM plugin is disabled': ['WASM 插件已禁用', '已禁用'],
    'loaded local WASM plugin': ['已加载本地 WASM 插件', '已加载'],
  } },
  'plugin_host::runtime': { source: '插件宿主', messages: {
    'registered plugin': ['已注册插件', '已注册'],
    'scheduled plugin invocation failed': ['定时插件调用失败', '调用失败'],
    'failed to retire scheduled worker': ['定时插件任务收尾失败', '收尾失败'],
  } },
  'bkmqb::management': { source: '管理服务', messages: {
    'management server listening': ['管理服务正在监听', '监听中', 'listen'],
    'configuration validation failed': ['配置校验未通过', '校验失败'],
    'configuration validation worker failed': ['配置校验任务异常退出', '任务异常'],
    'failed to start configuration validation worker': ['配置校验任务启动失败', '启动失败'],
    'management validation jobs exceeded the shutdown grace period': ['管理服务关闭时配置校验任务超时', '关闭超时'],
    'management validation supervisor also failed': ['配置校验监督任务也发生故障', '任务异常'],
  } },
  'bot_core::runtime': { source: '机器人运行时', messages: {
    'bot runtime started': ['机器人运行时已启动', '已启动', 'adapter_count'],
    'bot runtime is shutting down': ['正在关闭机器人运行时', '关闭中'],
    'adapter stopped': ['适配器已停止', '已停止'],
    'adapter stopped with an error': ['适配器因错误停止', '异常停止'],
    'dropping event from unknown adapter': ['已丢弃未知适配器的事件', '已丢弃'],
    'event handler failed': ['事件处理失败', '处理失败'],
    'event handler timed out': ['事件处理超时', '处理超时'],
    'adapter event drain exceeded the runtime shutdown deadline': ['关闭时适配器事件排空超时', '关闭超时'],
    'adapter shutdown timed out; aborting remaining tasks': ['适配器关闭超时，正在中止剩余任务', '关闭超时'],
    'event dispatch shutdown timed out; aborting remaining tasks': ['事件分发关闭超时，正在中止剩余任务', '关闭超时'],
  } },
  'adapter_qqbot::websocket': { source: 'QQ 网关', messages: {
    'connecting to QQ Gateway': ['正在连接 QQ 网关', '连接中'],
    'QQ Gateway connected': ['QQ 网关已连接', '已连接'],
    'QQ Gateway session resumed': ['QQ 网关会话已恢复', '已恢复'],
    'QQ Gateway requested reconnect': ['QQ 网关要求重新连接', '重连中'],
    'QQ Gateway connection stopped': ['QQ 网关连接中断', '连接中断'],
    'retrying QQ Gateway connection after backoff': ['等待退避后重试 QQ 网关连接', '等待重试', 'delay_ms'],
    'QQ Gateway closed WebSocket': ['QQ 网关已关闭连接', '已断开', 'code'],
    'sent QQ Gateway Resume': ['已发送 QQ 会话恢复请求', '等待恢复'],
    'sent QQ Gateway Identify': ['已发送 QQ 登录请求', '等待认证'],
    'ignoring unsupported QQ Gateway opcode': ['已忽略不支持的 QQ 网关操作码', '已忽略'],
    'dropping malformed QQ Gateway dispatch': ['已丢弃格式错误的 QQ 网关事件', '已丢弃'],
    'received QQ message event': ['收到 QQ 消息事件', '已收到'],
    'received unmapped QQ Gateway event': ['收到尚未映射的 QQ 网关事件', '未映射'],
    'QQ message event details': ['QQ 消息事件详情', '事件详情'],
    'unmapped QQ Gateway event details': ['未映射 QQ 网关事件详情', '事件详情'],
    'QQ message action succeeded': ['QQ 消息操作已执行', '已执行'],
    'QQ message action failed': ['QQ 消息操作失败', '执行失败'],
    'QQ platform action succeeded': ['QQ 平台操作已执行', '已执行'],
    'QQ platform action failed': ['QQ 平台操作失败', '执行失败'],
    'QQ message action result details': ['QQ 消息操作结果详情', '结果详情'],
  } },
  'adapter_qqbot::webhook': { source: 'QQ 回调', messages: {
    'QQ Webhook callback server is listening': ['QQ 回调服务正在监听', '监听中', 'listen'],
    'rejected QQ Webhook callback': ['QQ 回调认证未通过', '已拒绝'],
    'rejected malformed QQ Webhook dispatch': ['已拒绝格式错误的 QQ 回调事件', '已拒绝'],
    'acknowledging duplicate QQ Webhook event': ['已确认重复 QQ 回调事件', '重复事件'],
    'accepted QQ Webhook event': ['已接收 QQ 回调事件', '已接收'],
  } },
  'adapter_onebot11': { source: 'OneBot 11', messages: {
    'OneBot 11 reverse WebSocket listening': ['OneBot 11 反向连接服务正在监听', '监听中', 'listen'],
    'OneBot 11 reverse WebSocket connected': ['OneBot 11 反向连接已建立', '已连接'],
    'OneBot 11 reverse WebSocket disconnected': ['OneBot 11 反向连接已断开', '已断开'],
    'OneBot 11 handshake failed': ['OneBot 11 连接握手失败', '握手失败'],
    'OneBot 11 connection ended with an error': ['OneBot 11 连接因错误结束', '连接异常'],
    'rejected excess OneBot 11 handshake': ['OneBot 11 握手数超限，已拒绝新连接', '已拒绝'],
    'rejected additional OneBot 11 connection': ['OneBot 11 已有活动连接，已拒绝额外连接', '已拒绝'],
    'ignored binary OneBot frame': ['已忽略 OneBot 二进制帧', '已忽略'],
    'ignored malformed OneBot action response': ['已忽略格式错误的 OneBot 动作响应', '响应无效'],
    'ignored OneBot response with invalid echo': ['已忽略关联标识无效的 OneBot 响应', '响应无效'],
    'ignored unknown or duplicate OneBot echo': ['已忽略未知或重复的 OneBot 响应', '已忽略'],
    'ignored invalid OneBot event': ['已忽略无效 OneBot 事件', '事件无效'],
    'OneBot event forwarder failed while draining': ['OneBot 事件转发任务在排空时失败', '转发失败'],
  } },
};

function detailText(key: Detail | undefined, fields: LogEntry['fields']): string | undefined {
  if (!key) return undefined;
  const value = fields[key];
  if (!value) return undefined;
  if (key === 'version') {
    return value.length <= 128 && /^bkmqb [\dA-Za-z.+-]+ \(git (?:[\da-f]+|unknown), built [\d-]+\)$/u.test(value) ? value : undefined;
  }
  if (key === 'listen') {
    if (!/^(?:[\d.]+|\[[\da-f:]+\]):\d{1,5}$/iu.test(value) || Number(value.slice(value.lastIndexOf(':') + 1)) > 65535) return undefined;
    try { new URL(`http://${value}`); return `地址 ${value}`; } catch { return undefined; }
  }
  if (!/^\d+$/u.test(value) || !Number.isSafeInteger(Number(value))) return undefined;
  if (key === 'adapter_count') return `适配器 ${value} 个`;
  if (key === 'delay_ms') return `等待 ${value} 毫秒`;
  return Number(value) <= 65535 ? `关闭码 ${value}` : undefined;
}

export function readRuntimeLog(entry: LogEntry): { summary: string; status: string; source: string; detail?: string } | undefined {
  // Even unknown record kinds must retain their own semantics and raw fallback.
  if (entry.fields.record_kind !== undefined || !Object.hasOwn(vocabulary, entry.target)) return undefined;
  const module = vocabulary[entry.target]!;
  if (!Object.hasOwn(module.messages, entry.message)) return undefined;
  const [summary, status, detail] = module.messages[entry.message]!;
  return { summary, status, source: module.source, detail: detailText(detail, entry.fields) };
}
