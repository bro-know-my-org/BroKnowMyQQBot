import { isObject } from './types';
export const MAX_BACKUP_BYTES = 128 * 1024 * 1024;
export const opaqueId = (v: unknown): v is string => typeof v === 'string' && /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(v);
export interface BackupArtifact { id: string; bytes: number; created_at_ms: number }
export interface RestoreImpact { instance_id: string; plugin_id: string; version: string; declaration_owned: boolean; requested: string[]; retained_grants: string[] }
export interface RestorePlan {
  operation_id: string; boot_id: string; expires_in_seconds: number;
  manifest: { format_version: number; created_at_ms: number; scope: string; exclusions: string[]; installation_count: number; state_rows: number; package_count: number };
  plugins: RestoreImpact[]; affected_instances: string[]; changed_settings: string[]; settings_before: Record<string, unknown>; settings_after: Record<string, unknown>;
}
export const restoreStatuses = ['pending', 'interrupted', 'rejected', 'cancelled', 'rolled_back', 'applied', 'applied_restart_cancelled', 'applied_restart_failed'] as const;
export type RestoreStatus = typeof restoreStatuses[number] | 'unknown';
export interface RestoreResult { operation_id: string; status: RestoreStatus }
const integer = (v: unknown, max: number): v is number => Number.isSafeInteger(v) && Number(v) >= 0 && Number(v) <= max;
const strings = (v: unknown, count: number, size: number): v is string[] => Array.isArray(v) && v.length <= count && v.every(s => typeof s === 'string' && s.length > 0 && s.length <= size) && new Set(v).size === v.length;
const invalid = () => new Error('备份恢复响应格式异常，请重新读取；不会重复提交恢复。');
type SettingSchema = 'string' | 'boolean' | 'number' | { [key: string]: SettingSchema };
const settingSchema: SettingSchema = {
  commands: { prefix: 'string' },
  runtime: Object.fromEntries(['queue_capacity', 'event_concurrency', 'handler_timeout_seconds', 'shutdown_timeout_seconds', 'dedup_capacity'].map(key => [key, 'number' as const])),
  qq: { ...Object.fromEntries(['enabled', 'public_guild_messages', 'private_guild_messages', 'direct_messages', 'extended_events', 'forum_events', 'audio_events', 'open_forum_events', 'audio_live_member_events', 'check_only'].map(key => [key, 'boolean' as const])), environment: 'string', transport: 'string',
    webhook: { path: 'string', timestamp_tolerance_seconds: 'number', max_body_bytes: 'number', max_request_concurrency: 'number', request_timeout_seconds: 'number' } },
  onebot11: { enabled: 'boolean', action_timeout_seconds: 'number', max_message_bytes: 'number', max_pending_actions: 'number' },
  logging: { console: { enabled: 'boolean', language: 'string', ansi: 'boolean', filter: 'string' }, files: { enabled: 'boolean', filter: 'string', zstd_level: 'number', buffer_lines: 'number', runtime_max_file_mb: 'number', runtime_max_total_mb: 'number', messages_max_file_mb: 'number', messages_max_total_mb: 'number' } },
};
function settings(value: unknown): value is Record<string, unknown> {
  let nodes = 256, bytes = 128 * 1024;
  function visit(v: unknown, schema: SettingSchema): boolean {
    if (--nodes < 0) return false;
    if (schema === 'string') { if (typeof v !== 'string') return false; bytes -= v.length * 3; return v.length <= 4096 && bytes >= 0; }
    if (schema === 'boolean') return typeof v === 'boolean';
    if (schema === 'number') return Number.isSafeInteger(v);
    if (!isObject(v)) return false;
    return Object.keys(v).length === Object.keys(schema).length && Object.entries(v).every(([key, child]) => Object.hasOwn(schema, key) && visit(child, schema[key]!));
  }
  return visit(value, settingSchema);
}
export function readArtifacts(value: unknown): BackupArtifact[] {
  if (!Array.isArray(value) || value.length > 4 || !value.every(v => isObject(v) && opaqueId(v.id) && integer(v.bytes, MAX_BACKUP_BYTES) && integer(v.created_at_ms, 8.64e15)) || new Set(value.map(v => v.id)).size !== value.length) throw invalid();
  return value as BackupArtifact[];
}
export function readRestorePlan(value: unknown): RestorePlan {
  if (!isObject(value) || !opaqueId(value.operation_id) || !opaqueId(value.boot_id) || !integer(value.expires_in_seconds, 900) || !value.expires_in_seconds || !isObject(value.manifest)) throw invalid();
  const m = value.manifest;
  if (m.format_version !== 1 || !integer(m.created_at_ms, 8.64e15) || m.scope !== 'bot-settings+plugin-installations+verified-packages+private-kv'
    || !strings(m.exclusions, 16, 64) || !['secrets.env','process-environment','host-paths-and-auth','logs','command-ledger','outbox','deliveries','scheduled-tasks','health'].every(s => (m.exclusions as string[]).includes(s))
    || !integer(m.installation_count, 256) || !integer(m.state_rows, 16384) || !integer(m.package_count, 256)
    || !strings(value.affected_instances, 512, 256) || !strings(value.changed_settings, 128, 192) || !settings(value.settings_before) || !settings(value.settings_after)
    || !Array.isArray(value.plugins) || value.plugins.length > 512) throw invalid();
  const ids = new Set<string>();
  for (const p of value.plugins) {
    if (!isObject(p) || typeof p.instance_id !== 'string' || !value.affected_instances.includes(p.instance_id) || ids.has(p.instance_id)
      || typeof p.plugin_id !== 'string' || !p.plugin_id || p.plugin_id.length > 192 || typeof p.version !== 'string' || !p.version || p.version.length > 64
      || typeof p.declaration_owned !== 'boolean' || !strings(p.requested, 128, 128) || !strings(p.retained_grants, 128, 128) || !p.retained_grants.every(s => (p.requested as string[]).includes(s))) throw invalid();
    ids.add(p.instance_id);
  }
  if (ids.size !== value.affected_instances.length) throw invalid();
  return value as unknown as RestorePlan;
}
export function readRestoreResult(value: unknown, id: string): RestoreResult {
  if (!isObject(value) || value.operation_id !== id || typeof value.status !== 'string' || !restoreStatuses.includes(value.status as typeof restoreStatuses[number])) throw invalid();
  return value as unknown as RestoreResult;
}
export function restoreMessage(status: RestoreStatus): string {
  if (status === 'applied') return '已完成离线恢复并核验新进程；导入的托管插件保持禁用，请检查配置和权限后再启用。';
  if (status === 'applied_restart_failed' || status === 'applied_restart_cancelled') return '数据已恢复，前次自动重启未完成；现已核验新进程，请检查配置与插件状态。';
  if (status === 'rolled_back') return '恢复未完成，已回滚到原配置和数据。请重新检查归档。';
  if (status === 'cancelled' || status === 'rejected') return '恢复已取消或被拒绝，未应用归档。请重新检查。';
  if (status === 'interrupted') return '恢复在应用前中断，未自动重试。请重新检查归档。';
  return '已连接新进程，但恢复结果尚未确认。请核对配置和插件状态；不会自动重新提交恢复。';
}
