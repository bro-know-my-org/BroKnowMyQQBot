import { ApiError } from './api';
import { assertSafeNumbers } from './json';
import { isObject, type Config } from './types';

export type ResourceOperation = 'list' | 'create' | 'update' | 'delete';
export interface ManagementResource {
  id: string; title: string; description?: string; schema: Config | boolean;
  columns: { key: string; title: string }[]; operations: ResourceOperation[];
}
export interface ManagementView {
  instance_id: string; plugin_updated_at_ms: number; revision: string;
  descriptor: { schema_version: 1; resources: ManagementResource[] } | null;
  editable: boolean; unavailable_reason: string | null;
}
export interface ResourceRecord { id: string; values: Config }
export interface ResourcePage {
  instance_id: string; plugin_updated_at_ms: number; revision: string;
  data: { records: ResourceRecord[]; next_cursor: string | null };
}
const invalid = () => new ApiError('插件资源响应格式异常，请重新读取确认。');
export const boundedText = (value: unknown, max: number): value is string => typeof value === 'string' && new TextEncoder().encode(value).length <= max;
export const dataRevision = (v: unknown): v is string => typeof v === 'string' && /^(0|[1-9]\d{0,19})(?![\s\S])/.test(v) && BigInt(v) <= 18446744073709551615n;
const identifier = (v: unknown): v is string => typeof v === 'string' && /^[a-zA-Z0-9_-]{1,64}(?![\s\S])/.test(v);
const label = (v: unknown): v is string => boundedText(v, 256);
function boundedJson(value: unknown, max: number) {
  let encoded: string | undefined;
  try { encoded = JSON.stringify(value); } catch { throw invalid(); }
  if (!boundedText(encoded, max)) throw invalid();
  assertSafeNumbers(value);
}
function envelope(raw: unknown, instance: string): asserts raw is Record<string, unknown> {
  boundedJson(raw, 1024 * 1024);
  if (!isObject(raw) || raw.instance_id !== instance || !dataRevision(raw.revision)
    || !Number.isSafeInteger(raw.plugin_updated_at_ms) || Number(raw.plugin_updated_at_ms) < 0) throw invalid();
}
function schemaReferences(schema: Record<string, unknown>) {
  type Scope = { root: Record<string, unknown>; anchors: Map<string, unknown>; visited: Set<unknown> };
  const scopes = new Map<unknown, Scope>();
  function scopeFor(root: Record<string, unknown>): Scope {
    if (!scopes.has(root)) scopes.set(root, { root, anchors: new Map(), visited: new Set() });
    return scopes.get(root)!;
  }
  const stack: Array<{ value: unknown; scope: Scope }> = [{ value: schema, scope: scopeFor(schema) }];
  const references: Array<{ anchor: string; scope: Scope }> = [];
  while (stack.length || references.length) {
    if (!stack.length) {
      const { anchor, scope } = references.shift()!;
      if (!scope.anchors.has(anchor)) throw invalid();
      stack.push({ value: scope.anchors.get(anchor), scope });
    }
    const { value: item, scope: parentScope } = stack.pop()!;
    let scope = parentScope;
    if (isObject(item) && typeof item.$id === 'string' && item.$id && !item.$id.startsWith('#')) scope = scopeFor(item);
    if (scope.visited.has(item)) continue;
    scope.visited.add(item);
    if (Array.isArray(item)) { stack.push(...item.map(value => ({ value, scope }))); continue; }
    if (!isObject(item)) continue;
    for (const key of ['$anchor', '$dynamicAnchor']) if (typeof item[key] === 'string') scope.anchors.set(item[key], item);
    for (const key of ['$ref', '$dynamicRef', '$recursiveRef']) {
      if (!Object.hasOwn(item, key)) continue;
      const reference = item[key];
      if (typeof reference !== 'string' || !reference.startsWith('#')) throw invalid();
      let fragment: string;
      try { fragment = decodeURIComponent(reference.slice(1)); } catch { throw invalid(); }
      if (!fragment) stack.push({ value: scope.root, scope });
      else if (fragment.startsWith('/')) {
        let target: unknown = scope.root, targetScope = scope;
        for (const part of fragment.slice(1).split('/')) {
          if (/~(?![01])/.test(part)) throw invalid();
          const name = part.replace(/~1/g, '/').replace(/~0/g, '~');
          if ((!isObject(target) && !Array.isArray(target)) || !Object.hasOwn(target, name)) throw invalid();
          target = (target as Record<string, unknown>)[name];
          if (isObject(target) && typeof target.$id === 'string' && target.$id && !target.$id.startsWith('#')) targetScope = scopeFor(target);
        }
        if (!isObject(target) && typeof target !== 'boolean') throw invalid();
        stack.push({ value: target, scope: targetScope });
      } else references.push({ anchor: fragment, scope });
    }
    for (const key of ['$defs', 'definitions', 'properties', 'patternProperties', 'dependentSchemas', 'dependencies']) {
      if (isObject(item[key])) stack.push(...Object.values(item[key]).map(value => ({ value, scope })));
    }
    for (const key of ['additionalProperties', 'unevaluatedProperties', 'propertyNames', 'contains', 'additionalItems', 'unevaluatedItems', 'not', 'if', 'then', 'else', 'contentSchema', 'items', 'allOf', 'anyOf', 'oneOf', 'prefixItems']) {
      if (item[key] !== undefined) stack.push({ value: item[key], scope });
    }
  }
}
export function readManagement(raw: unknown, instance: string): ManagementView {
  envelope(raw, instance);
  if (typeof raw.editable !== 'boolean' || !(raw.unavailable_reason === null || boundedText(raw.unavailable_reason, 1024))) throw invalid();
  if (raw.descriptor !== null) {
    const descriptor = raw.descriptor;
    boundedJson(descriptor, 64 * 1024);
    if (!isObject(descriptor) || descriptor.schema_version !== 1 || !Array.isArray(descriptor.resources) || descriptor.resources.length > 16) throw invalid();
    const ids = new Set<string>();
    for (const r of descriptor.resources) {
      if (!isObject(r) || !identifier(r.id) || ids.has(r.id) || !label(r.title)
        || !(r.description === undefined || boundedText(r.description, 256)) || !(isObject(r.schema) || typeof r.schema === 'boolean')
        || !Array.isArray(r.columns) || r.columns.length > 16 || !Array.isArray(r.operations)
        || !r.operations.length || r.operations.length > 4 || new Set(r.operations).size !== r.operations.length
        || r.operations.some(op => !['list', 'create', 'update', 'delete'].includes(op))) throw invalid();
      ids.add(r.id); if (isObject(r.schema)) schemaReferences(r.schema);
      const keys = new Set<string>();
      for (const c of r.columns) {
        if (!isObject(c) || !identifier(c.key) || keys.has(c.key) || !label(c.title)) throw invalid();
        keys.add(c.key);
      }
    }
  }
  return raw as unknown as ManagementView;
}
export function readResourcePage(raw: unknown, instance: string, limit = 100): ResourcePage {
  envelope(raw, instance);
  if (!Number.isInteger(limit) || limit < 1 || limit > 100 || !isObject(raw.data) || !Array.isArray(raw.data.records)
    || raw.data.records.length > limit || !(raw.data.next_cursor === null || boundedText(raw.data.next_cursor, 512))) throw invalid();
  const ids = new Set<string>();
  for (const record of raw.data.records) {
    if (!isObject(record) || !boundedText(record.id, 256) || !record.id || ids.has(record.id) || !isObject(record.values)) throw invalid();
    ids.add(record.id);
  }
  return raw as unknown as ResourcePage;
}
