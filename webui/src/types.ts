export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
export type Config = Record<string, Json>;
export interface VersionStatus {
  saved_version: string | null; loaded_version: string | null; restart_required: boolean;
  runtime_state: 'ready' | 'stopped' | 'disabled' | 'not_loaded' | 'draining';
}
export interface PluginView extends VersionStatus {
  instance_id: string; plugin_id: string; name: string; description: string; version: string;
  source: string; trust_level: string; signature_status: string; enabled: boolean;
  runtime_state: 'ready' | 'stopped' | 'disabled' | 'not_loaded' | 'draining';
  editable: boolean; config: Config; schema: Config | null;
  requested_permissions: string[]; granted_permissions: string[];
  updated_at_ms: number; pending: boolean; error: string | null;
}
export type PluginAction = 'config' | 'enabled' | 'apply' | 'uninstall' | 'recover';
export interface Mutation { action: PluginAction; config?: Config; enabled?: boolean; confirm?: boolean }
export interface FailedDeliveryPage { entries: { attempts: number; error_class: string; updated_at_ms: number }[]; next_before: number | null }
export interface Commands { prefix: string; revision: string; saved_prefix?: string; pending?: boolean }
export interface Session { mode: 'token' | 'none'; authenticated: boolean; history_scope?: string | null; boot_id?: string }
export type LogCategory = 'runtime' | 'message';
export interface LogEntry { id: number; category: LogCategory; content?: string; timestamp: string; level: string; target: string; message: string; instance_id: string | null; fields: Record<string, string> }
export interface LogPage { session_id: string; entries: LogEntry[]; next_cursor: number; dropped: boolean; has_more: boolean }
export interface LogQuery { level: string; category?: LogCategory | 'all'; include_content?: boolean; instance_id?: string; limit?: number; after?: number; session_id?: string }
export type LogStreamState = 'connecting' | 'open' | 'reconnecting' | 'closed';
export interface LogStreamHandlers { onPage: (page: LogPage) => void; onState: (state: LogStreamState) => void; onError: (error: Error) => void }
export type SubscribeLogs = (query: LogQuery, handlers: LogStreamHandlers) => () => void;
export const runtimeNames: Record<PluginView['runtime_state'], string> = {
  ready: '运行中', stopped: '已停止', disabled: '已禁用', not_loaded: '未加载', draining: '正在停止接收任务',
};
export const json = (value: unknown) => JSON.stringify(value, null, 2);
export const isObject = (v: unknown): v is Record<string, unknown> => v !== null && typeof v === 'object' && !Array.isArray(v);

export interface CatalogueItem {
  slug: string; plugin_id: string; name: string; summary: string; trust: string; repository: string;
  commands: string[]; capabilities: string[];
  latest: { version: string; protocol: string; download: string; sha256: string };
  selection: string; instance_id: string; manageable: boolean;
  installed: (VersionStatus & { version: string; enabled: boolean; updated_at_ms: number; granted_permissions: string[] }) | null;
}
export interface Catalogue { source: string; total: number; offset: number; plugins: CatalogueItem[] }
export interface InstallResult {
  instance_id: string; version: string; enabled: boolean; restart_required: boolean;
  missing_permissions: string[]; config_incomplete: boolean; updated_at_ms: number;
  loaded_version: string | null; runtime_state: PluginView['runtime_state'];
}
