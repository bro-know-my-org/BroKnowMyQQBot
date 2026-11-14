import type { LogEntry, PluginView } from './types';
import { readRuntimeLog } from './runtimeLogPresentation';

interface Reading { summary: string; status: string; detail?: string; source?: string }
const scopes: Record<string, string> = { private: '私聊', group: '群聊', channel: '频道' };
const actionStatuses: Record<string, string> = {
  succeeded: '已执行', failed: '执行失败', denied: '权限拒绝', timed_out: '执行超时',
  unknown: '结果未知', cancelled: '已取消',
};
const actionNames: Record<string, string> = {
  'message.reply': '回复消息', 'message.send': '发送消息', 'media.reply': '回复媒体',
  'media.send': '发送媒体', 'http.request': 'HTTP 请求', 'schedule.create': '创建定时任务',
  'schedule.cancel': '取消定时任务', 'browser.run': '浏览器任务',
};

function known(table: Record<string, string>, key: string | undefined): string | undefined {
  return key !== undefined && Object.hasOwn(table, key) ? table[key] : undefined;
}

// Record kinds take precedence; legacy runtime events use exact producer templates.
export function readLog(entry: LogEntry): Reading | undefined {
  const f = entry.fields, kind = f.record_kind;
  const scope = known(scopes, f.message_scope) ?? '';
  switch (kind) {
    case 'message.received': return { summary: `收到一条${scope}消息`, status: '已收到' };
    case 'message.sending': return { summary: `正在发送${scope}消息`, status: '发送中' };
    case 'message.sent': return { summary: `${scope}消息已发送`, status: '已发送' };
    case 'message.failed': return { summary: `${scope}消息发送失败`, status: '发送失败' };
    case 'plugin.invoked': return {
      summary: '插件开始执行', status: f.command_name ? '指令候选' : '调用中',
      detail: f.command_name ? `声明匹配：${f.command_name}` : undefined,
    };
    case 'plugin.completed': {
      if (f.status === 'no_output') return { summary: '插件未产生输出', status: '无输出', detail: f.command_name ? `候选指令：${f.command_name}` : undefined };
      if (f.status !== 'output_committed') return undefined;
      const counts = ([['command_count', '动作'], ['state_op_count', '状态写入'], ['diagnostic_count', '诊断']] as const)
        .flatMap(([key, label]) => /^\d+$/u.test(f[key] ?? '') && Number(f[key]) > 0 ? [`${label} ${f[key]}`] : []);
      return {
        summary: '插件产生了输出',
        status: '输出已提交', detail: [f.command_name ? `候选指令：${f.command_name}` : '', ...counts].filter(Boolean).join(' · '),
      };
    }
    case 'plugin.failed': return { summary: '插件执行失败', status: '执行失败', detail: f.error_class };
    case 'plugin.action.completed': {
      const status = known(actionStatuses, f.status);
      if (!status) return undefined;
      return { summary: '插件动作执行结果', status, detail: known(actionNames, f.action_type) ?? f.action_type };
    }
    default: return kind === undefined ? readRuntimeLog(entry) : undefined;
  }
}

export function logSource(entry: LogEntry, plugins: ReadonlyMap<string, PluginView>): string {
  const instance = entry.instance_id, plugin = instance ? plugins.get(instance) : undefined;
  const label = plugin && (!entry.fields.plugin_id || entry.fields.plugin_id === plugin.plugin_id)
    ? plugin.name || plugin.plugin_id : entry.fields.plugin_id;
  const runtime = readRuntimeLog(entry);
  if (runtime) return [runtime.source, label || instance, entry.fields.adapter_id].filter(Boolean).join(' · ');
  return [label, instance, entry.fields.adapter_id].filter(Boolean).join(' · ') || entry.target;
}

export function messageDirection(entry: LogEntry): string {
  if (entry.fields.direction === 'inbound') return '入站';
  if (entry.fields.direction === 'outbound') return '出站';
  return entry.fields.direction || '方向未标注';
}

export function isMessageResult(entry: LogEntry): boolean {
  return entry.fields.record_kind === 'message.sent' || entry.fields.record_kind === 'message.failed';
}
