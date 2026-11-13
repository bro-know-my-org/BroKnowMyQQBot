export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
export type Config = Record<string, Json>;
export interface PluginView {
  instance_id: string; plugin_id: string; name: string; description: string; version: string;
  source: string; trust_level: string; signature_status: string; enabled: boolean;
  runtime_state: 'ready' | 'stopped' | 'disabled' | 'not_loaded' | 'draining';
  editable: boolean; config: Config; schema: Config | null;
  requested_permissions: string[]; granted_permissions: string[];
  updated_at_ms: number; pending: boolean; error: string | null;
}
export type PluginAction = 'config' | 'enabled' | 'apply';
export interface Mutation { action: PluginAction; config?: Config; enabled?: boolean }
export interface Commands { prefix: string; revision: string; saved_prefix?: string; pending?: boolean }
export interface Session { mode: 'token' | 'none'; authenticated: boolean }
export interface LogEntry { id: number; timestamp: string; level: string; target: string; message: string; instance_id: string | null; fields: Record<string, string> }
export interface LogPage { session_id: string; entries: LogEntry[]; next_cursor: number; dropped: boolean; has_more: boolean }
export const runtimeNames: Record<PluginView['runtime_state'], string> = {
  ready: '运行中', stopped: '已停止', disabled: '已禁用', not_loaded: '未加载', draining: '正在停止接收任务',
};
export const json = (value: unknown) => JSON.stringify(value, null, 2);
export const isObject = (v: unknown): v is Record<string, unknown> => v !== null && typeof v === 'object' && !Array.isArray(v);
