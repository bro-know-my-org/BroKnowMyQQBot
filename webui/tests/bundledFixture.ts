import type { BundledCatalogue } from '../src/bundledPlugins';
export const bundledFixture = (): BundledCatalogue => ({ revision: 'a'.repeat(64), plugins: ['ping', 'help', 'echo', 'counter', 'reminder', 'admin'].map(key => ({
  key, instance_id: `dev.bkm.${key}/default`, plugin_id: `dev.bkm.${key}`, name: key, description: `${key} 的用途`, version: '0.1.0',
  diagnostic: false, enabled: key !== 'admin', config: { greeting: '原值' }, schema: { type: 'object', properties: { greeting: { type: 'string' } } },
  requested_permissions: ['message.reply'], loaded: key !== 'admin', runtime_state: key === 'admin' ? 'not_loaded' : 'ready', pending: false, restart_required: false,
})) });
