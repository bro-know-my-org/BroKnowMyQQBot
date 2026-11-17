import { expect, it } from 'vitest';
import { readManagement, readResourcePage } from '../src/pluginManagement';
import { readManagementSnapshot } from '../src/status';
import { runtimeFixture } from './runtimeFixture';
export const resource = () => ({ id: 'repositories', title: '仓库别名', description: '管理仓库', schema: { type: 'object', properties: { alias: { type: 'string', title: '别名' } }, required: ['alias'], additionalProperties: false }, columns: [{ key: 'alias', title: '别名' }], operations: ['list', 'create', 'update', 'delete'] });
export const management = () => ({ instance_id: 'sample/a', plugin_updated_at_ms: 1, revision: '1', descriptor: { schema_version: 1, resources: [resource()] }, editable: true, unavailable_reason: null });
export const page = (revision = '1') => ({ instance_id: 'sample/a', plugin_updated_at_ms: 1, revision, data: { records: [{ id: 'A', values: { alias: 'A' } }], next_cursor: null } });
it('accepts the precise v1 descriptor, legacy null and decimal u64 without number conversion', () => {
  expect(readManagement(management(), 'sample/a').descriptor!.resources[0]!.id).toBe('repositories');
  expect(readManagement({ ...management(), descriptor: null }, 'sample/a').descriptor).toBeNull();
  expect(readResourcePage(page('18446744073709551615'), 'sample/a').revision).toBe('18446744073709551615');
  expect(readResourcePage({ ...page(), data: { records: [], next_cursor: null } }, 'sample/a').data.records).toEqual([]);
});
it.each([{}, { ...management(), instance_id: 'other' }, { ...management(), editable: 1 }, { ...management(), plugin_updated_at_ms: 9007199254740992 }, { ...management(), revision: '01' }, { ...management(), revision: '18446744073709551616' }, { ...management(), descriptor: {} }])('rejects malformed envelopes %j', raw => {
  expect(() => readManagement(raw, 'sample/a')).toThrow();
});
it.each([
  { id: 'path/escape' }, { title: '汉'.repeat(86) }, { description: 'a'.repeat(257) },
  { operations: ['list', 'unknown'] }, { operations: ['list', 'list'] },
  { schema: { $ref: 'https://outside/schema' } }, { schema: { properties: { nested: { $dynamicRef: '/tmp/schema' } } } },
  { columns: [{ key: 'alias', title: 'A' }, { key: 'alias', title: 'B' }] }, { columns: Array(17).fill({ key: 'a', title: 'A' }) },
  { schema: { default: { unsafe: 9007199254740992 } } },
])('bounds and validates resource declarations %j', patch => {
  expect(() => readManagement({ ...management(), descriptor: { schema_version: 1, resources: [{ ...resource(), ...patch }] } }, 'sample/a')).toThrow();
});
it('rejects oversized and duplicate descriptors and allows local schema references', () => {
  const base = management();
  expect(() => readManagement({ ...base, descriptor: { schema_version: 1, resources: Array(17).fill(resource()) } }, 'sample/a')).toThrow();
  expect(() => readManagement({ ...base, descriptor: { schema_version: 1, resources: [resource(), resource()] } }, 'sample/a')).toThrow();
  base.descriptor.resources[0]!.schema = { ...resource().schema, ...{ $comment: 'x'.repeat(65536) } };
  expect(() => readManagement(base, 'sample/a')).toThrow();
  expect(() => readManagement({ ...management(), descriptor: { schema_version: 1, resources: [{ ...resource(), schema: { $ref: '#/$defs/record', $defs: { record: true } } }] } }, 'sample/a')).not.toThrow();
});
it.each([
  { records: [{ id: 'a', values: [] }] }, { records: [{ id: 'a', values: { unsafe: 9007199254740992 } }] },
  { records: [{ id: 'a', values: {} }, { id: 'a', values: {} }] }, { records: Array(21).fill({ id: 'A', values: {} }) },
  { records: [{ id: 'x'.repeat(257), values: {} }] }, { records: [{ id: '', values: {} }] },
  { records: [] }, { records: [], next_cursor: 1 }, { records: [], next_cursor: 'x'.repeat(513) },
])('rejects unsafe or unbounded record responses %j', data => {
  expect(() => readResourcePage({ ...page(), data }, 'sample/a', 20)).toThrow();
});
it('optional SSE generation map remains backwards compatible and rejects invalid counters', () => {
  const runtime = { ...runtimeFixture(), boot_id: '11111111-1111-4111-8111-111111111111' };
  const snapshot = { boot_id: runtime.boot_id, revision: '1', runtime, plugins: [], plugins_truncated: false, settings_restart_required: null };
  expect(readManagementSnapshot(snapshot).plugin_data_revisions).toBeUndefined();
  expect(readManagementSnapshot({ ...snapshot, plugin_data_revisions: { 'sample/a': '18446744073709551615' } }).plugin_data_revisions).toEqual({ 'sample/a': '18446744073709551615' });
  for (const value of [null, [], { 'sample/a': 1 }, { 'sample/a': '01' }, { 'sample/a': '18446744073709551616' }, Object.fromEntries(Array.from({ length: 257 }, (_, i) => [String(i), '1']))]) {
    expect(() => readManagementSnapshot({ ...snapshot, plugin_data_revisions: value })).toThrow();
  }
});

it.each([true, false, { type: 'object', properties: { '$ref': { type: 'string' } }, default: { '$ref': 'https://literal' }, const: { '$dynamicRef': 'data' } }])('accepts boolean schemas and literal reference-named data %j', schema => {
  expect(() => readManagement({ ...management(), descriptor: { schema_version: 1, resources: [{ ...resource(), schema }] } }, 'sample/a')).not.toThrow();
});

it('rejects newline-tailed tokens and follows local ref targets with cycle protection', () => {
  for (const patch of [{ id: 'repositories\n' }, { columns: [{ key: 'alias\n', title: '别名' }] }, { schema: { $ref: '#/extension', extension: { $ref: 'https://external' } } }]) {
    expect(() => readManagement({ ...management(), descriptor: { schema_version: 1, resources: [{ ...resource(), ...patch }] } }, 'sample/a')).toThrow();
  }
  expect(() => readResourcePage(page('1\n'), 'sample/a')).toThrow();
  expect(() => readManagement({ ...management(), descriptor: { schema_version: 1, resources: [{ ...resource(), schema: { $ref: '#', default: { $ref: 'https://literal' } } }] } }, 'sample/a')).not.toThrow();
});
it('accepts many small values within the response byte budget', () => {
  expect(() => readResourcePage({ ...page(), data: { records: [{ id: 'A', values: { items: Array(65536).fill(0) } }], next_cursor: null } }, 'sample/a')).not.toThrow();
});

it('resolves embedded schema resource pointers and anchors in their own scope', () => {
  const schema = { $defs: { child: { $id: 'child', $defs: { x: true }, $ref: '#/$defs/x', allOf: [{ $anchor: 'a' }, { $ref: '#a' }] } } };
  const response = (schema: unknown) => ({ ...management(), descriptor: { schema_version: 1, resources: [{ ...resource(), schema }] } });
  expect(() => readManagement(response(schema), 'sample/a')).not.toThrow();
  expect(() => readManagement(response({ $defs: { 'a~2b': true }, $ref: '#/$defs/a~2b' }), 'sample/a')).toThrow();
  expect(() => readManagement(response({ $defs: { 'a~2b': true }, $ref: '#/$defs/a~02b' }), 'sample/a')).not.toThrow();
});
