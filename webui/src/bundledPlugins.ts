import { assertSafeNumbers } from './json';
import { isObject, runtimeNames, type Config, type PluginView } from './types';

export interface BundledPlugin {
  key: string; instance_id: string; plugin_id: string; name: string; description: string; version: string;
  diagnostic: boolean; enabled: boolean; config: Config; schema: Config | null; requested_permissions: string[];
  loaded: boolean; runtime_state: PluginView['runtime_state']; pending: boolean; restart_required: boolean;
}
export interface BundledCatalogue { revision: string; plugins: BundledPlugin[] }
const jsonValue = (v: unknown): boolean => v === null || ['boolean', 'string', 'number'].includes(typeof v)
  || (Array.isArray(v) ? v.every(jsonValue) : isObject(v) && Object.values(v).every(jsonValue));
const config = (v: unknown) => isObject(v) && jsonValue(v);
export function readBundledCatalogue(value: unknown): BundledCatalogue {
  assertSafeNumbers(value);
  const invalid = () => new Error('内置插件目录格式异常，请重新读取确认状态。');
  if (!isObject(value) || typeof value.revision !== 'string' || !/^[a-f0-9]{64}$/.test(value.revision)
    || !Array.isArray(value.plugins)) throw invalid();
  const keys = new Set<string>(), instances = new Set<string>();
  const plugins = value.plugins.map((p: unknown) => {
    if (!isObject(p) || !['key', 'instance_id', 'plugin_id', 'name', 'version'].every(k => typeof p[k] === 'string' && !!p[k])
      || typeof p.description !== 'string' || !['diagnostic', 'enabled', 'loaded', 'pending', 'restart_required'].every(k => typeof p[k] === 'boolean')
      || typeof p.runtime_state !== 'string' || !Object.hasOwn(runtimeNames, p.runtime_state)
      || !config(p.config) || !(p.schema === null || config(p.schema))
      || !Array.isArray(p.requested_permissions) || !p.requested_permissions.every(v => typeof v === 'string' && !!v)
      || keys.has(p.key as string) || instances.has(p.instance_id as string)) throw invalid();
    keys.add(p.key as string); instances.add(p.instance_id as string);
    return p as unknown as BundledPlugin;
  });
  return { revision: value.revision, plugins };
}
