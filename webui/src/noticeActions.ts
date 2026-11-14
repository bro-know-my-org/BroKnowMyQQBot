import type { RouteLocationRaw } from 'vue-router';
import type { FaultDestination } from './faultNotices';
export type NoticeAction = FaultDestination | { page: 'logs' } | { page: 'plugin-config'; instanceId: string };
const labels = { adapters: '查看连接与适配器', plugins: '查看已安装插件', settings: '查看设置', logs: '查看日志', 'plugin-overview': '查看插件状态', 'plugin-config': '查看插件配置' };
export function readNoticeAction(value: unknown): NoticeAction | undefined {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return;
  const action = value as Record<string, unknown>;
  if (typeof action.page !== 'string' || !Object.hasOwn(labels, action.page)) return;
  if (action.page === 'plugin-overview' || action.page === 'plugin-config') {
    if (Object.keys(action).some(k => !['page', 'instanceId'].includes(k)) || typeof action.instanceId !== 'string'
      || !action.instanceId.length || action.instanceId.length > 512 || /[\u0000-\u001f\u007f]/.test(action.instanceId)) return;
    try { encodeURIComponent(action.instanceId); } catch { return; }
    return { page: action.page, instanceId: action.instanceId };
  }
  if (Object.keys(action).some(k => k !== 'page')) return;
  return { page: action.page } as NoticeAction;
}
export const noticeActionLabel = (action: NoticeAction) => labels[action.page];
export function noticeActionRoute(value: unknown): RouteLocationRaw | undefined {
  const action = readNoticeAction(value);
  if (!action) return;
  return 'instanceId' in action ? { name: action.page, params: { instanceId: action.instanceId } } : { name: action.page };
}
export const readNoticeKey = (value: unknown): string | undefined => typeof value === 'string' && value.length > 0 && value.length <= 768 && !/[\u0000-\u001f\u007f]/.test(value) ? value : undefined;
