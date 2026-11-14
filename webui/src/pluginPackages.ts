import { isObject } from './types';
import type { InstallResult, LocalInspection, PermissionView, PluginView } from './types';

export const MAX_LOCAL_PACKAGE_BYTES = 32 * 1024 * 1024;
const text = (v: unknown, max: number): v is string => typeof v === 'string' && v.length <= max;
const integer = (v: unknown): v is number => Number.isSafeInteger(v) && Number(v) >= 0;
export const permissionsList = (v: unknown): v is string[] => Array.isArray(v) && v.length <= 128 && v.every(p => text(p, 256) && !!p) && new Set(v).size === v.length;
const hash = (v: unknown): v is string => typeof v === 'string' && /^[a-f0-9]{64}$/.test(v);
const invalid = () => new Error('插件管理响应格式异常，请重新读取状态。');
export function readLocalInspection(v: unknown): LocalInspection {
  if (!isObject(v) || !text(v.selection, 36) || !/^[a-f0-9-]{36}$/.test(v.selection)
    || !integer(v.expires_in_seconds) || v.expires_in_seconds > 300 || !v.expires_in_seconds
    || !text(v.plugin_id, 128) || !v.plugin_id || !text(v.name, 256) || !text(v.description, 4096)
    || !text(v.version, 128) || !v.version || !hash(v.sha256) || v.signature_status !== 'unsigned'
    || !Array.isArray(v.commands) || v.commands.length > 256 || !v.commands.every(c => text(c, 128))
    || !permissionsList(v.requested_permissions) || !text(v.default_instance_id, 192) || !v.default_instance_id) throw invalid();
  return v as unknown as LocalInspection;
}
export function readPermissions(v: unknown, id: string): PermissionView {
  if (!isObject(v) || v.instance_id !== id || !integer(v.expected_updated_at_ms) || !text(v.version, 128) || !hash(v.sha256)
    || !permissionsList(v.requested_permissions) || !permissionsList(v.granted_permissions)) throw invalid();
  return v as unknown as PermissionView;
}
export function readPermissionSave(v: unknown, selected: PermissionView, grants: string[]): PluginView {
  if (!isObject(v) || !isObject(v.plugin)) throw invalid();
  const p = v.plugin;
  if (p.instance_id !== selected.instance_id || p.version !== selected.version || !integer(p.updated_at_ms) || p.updated_at_ms <= selected.expected_updated_at_ms
    || p.restart_required !== true || typeof p.enabled !== 'boolean' || !permissionsList(p.granted_permissions)
    || p.granted_permissions.length !== grants.length || !grants.every(g => (p.granted_permissions as string[]).includes(g))
    || !['ready', 'stopped', 'disabled', 'not_loaded', 'draining'].includes(String(p.runtime_state))) throw invalid();
  return p as unknown as PluginView;
}
export function readLocalInstall(v: unknown, id: string, version: string): InstallResult {
  if (!isObject(v) || v.instance_id !== id || v.version !== version || !integer(v.updated_at_ms) || typeof v.enabled !== 'boolean'
    || v.restart_required !== true || typeof v.config_incomplete !== 'boolean' || !permissionsList(v.missing_permissions)
    || !(v.loaded_version === null || text(v.loaded_version, 128))
    || !['ready', 'stopped', 'disabled', 'not_loaded', 'draining'].includes(String(v.runtime_state))) throw invalid();
  return v as unknown as InstallResult;
}
