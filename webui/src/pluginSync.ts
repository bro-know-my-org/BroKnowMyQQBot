import type { PluginView } from './types';
import type { PluginStatus } from './status';
const summaryKeys = ['plugin_id', 'name', 'version', 'loaded_version', 'enabled', 'runtime_state', 'pending', 'restart_required', 'updated_at_ms'] as const;
export function mergeStatus(current: PluginView[], snapshot: { plugins: readonly PluginStatus[]; plugins_truncated: boolean }, protectedIds: Set<string>, resetRevision = false) {
  const summaries = new Map(snapshot.plugins.map(p => [p.instance_id, p]));
  const conflicts: string[] = [], removed: string[] = [], savedChangedIds: string[] = [];
  let reconcile = snapshot.plugins.some(p => !current.some(old => old.instance_id === p.instance_id));
  let changed = false;
  const plugins = current.map(old => {
    const next = summaries.get(old.instance_id);
    if (!next) {
      if (!snapshot.plugins_truncated) {
        if (protectedIds.has(old.instance_id)) { conflicts.push(old.instance_id); removed.push(old.instance_id); }
        else reconcile = true;
      }
      return old;
    }
    // A cached sample created before an accepted write cannot roll its saved state back.
    if (!resetRevision && next.updated_at_ms < old.updated_at_ms) return old;
    if (next.runtime_state !== old.runtime_state) reconcile = true;
    const savedChanged = next.updated_at_ms !== old.updated_at_ms || next.version !== old.version;
    if (savedChanged) { savedChangedIds.push(old.instance_id); reconcile = true; if (protectedIds.has(old.instance_id)) conflicts.push(old.instance_id); }
    if (!summaryKeys.some(key => old[key] !== next[key])) return old;
    changed = true;
    return { ...old, ...next, saved_version: old.saved_version === null && next.updated_at_ms === 0 ? null : next.version };
  });
  return { plugins: changed ? plugins : current, conflicts, removed, savedChangedIds, reconcile };
}
export function mergePluginRead(current: PluginView[], incoming: PluginView[], protectedIds: Set<string>, resetRevision = false) {
  const oldById = new Map(current.map(p => [p.instance_id, p]));
  const result = incoming.map(next => {
    const old = oldById.get(next.instance_id);
    if (!old) return next;
    if (!resetRevision && next.updated_at_ms < old.updated_at_ms) return old;
    // Keep editor inputs stable for status-only rereads; protected drafts keep their schema too.
    const sameConfig = protectedIds.has(old.instance_id) || JSON.stringify(old.config) === JSON.stringify(next.config);
    const sameSchema = protectedIds.has(old.instance_id) || JSON.stringify(old.schema) === JSON.stringify(next.schema);
    return { ...next, config: sameConfig && sameSchema ? old.config : next.config, schema: sameSchema ? old.schema : next.schema };
  });
  for (const old of current) if (protectedIds.has(old.instance_id) && !incoming.some(p => p.instance_id === old.instance_id)) result.push(old);
  return result;
}
