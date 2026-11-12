import { isObject } from './types';
import type { Config, Json } from './types';
export interface Field {
  key: string; title: string; description: string; required: boolean; type: string;
  choices: Json[] | null; default?: Json; minimum?: number; maximum?: number;
}
// A deliberately small subset. Compositions, references, nested structures and unknown rules use JSON.
export function formFields(schema: Config | null, config: Config): Field[] | null {
  const rootKeys = ['type', 'properties', 'required', 'additionalProperties', 'title', 'description', 'default', '$schema', '$id', '$comment'];
  const fieldKeys = ['type', 'title', 'description', 'default', 'enum', 'minimum', 'maximum', 'minLength', 'maxLength', 'pattern', 'format', 'examples', '$comment'];
  if (!schema || schema.type !== 'object' || !isObject(schema.properties) || Object.keys(schema).some(k => !rootKeys.includes(k)) || isObject(schema.additionalProperties)) return null;
  const required = schema.required ?? [], properties = schema.properties;
  if (!Array.isArray(required) || required.some(k => typeof k !== 'string' || !Object.hasOwn(properties, k))) return null;
  const fields: Field[] = [];
  for (const [key, s] of Object.entries(properties)) {
    if (!isObject(s) || Object.keys(s).some(k => !fieldKeys.includes(k))) return null;
    if (s.enum !== undefined && (!Array.isArray(s.enum) || !s.enum.length || s.enum.some(v => isObject(v) || Array.isArray(v)))) return null;
    if ((s.type !== undefined && (typeof s.type !== 'string' || !['boolean', 'string', 'number', 'integer'].includes(s.type))) || (!s.enum && !s.type)) return null;
    const choices = (s.enum as Json[] | undefined) ?? (s.type === 'boolean' ? [true, false] : null);
    const valid = (v: Json) => {
      if (choices) return choices.includes(v);
      if (s.type === 'integer') return Number.isInteger(v);
      return typeof v === s.type;
    };
    if ((Object.hasOwn(config, key) && !valid(config[key]!)) || (Object.hasOwn(s, 'default') && !valid(s.default as Json))) return null;
    fields.push({ key, title: typeof s.title === 'string' ? s.title : key, description: typeof s.description === 'string' ? s.description : '', required: required.includes(key),
      type: typeof s.type === 'string' ? s.type : '', choices, ...(Object.hasOwn(s, 'default') ? { default: s.default as Json } : {}),
      ...(typeof s.minimum === 'number' ? { minimum: s.minimum } : {}), ...(typeof s.maximum === 'number' ? { maximum: s.maximum } : {}) });
  }
  return fields;
}
