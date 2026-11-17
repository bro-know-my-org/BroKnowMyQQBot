<script setup lang="ts">
import { computed, ref, shallowReactive, shallowRef, watch } from 'vue';
import { formFields } from '../schema';
import { assertSafeNumbers, parseSafeJson } from '../json';
import { isObject, json } from '../types';
import type { Config } from '../types';
const props = defineProps<{ config: Config; schema: Config | null; disabled: boolean; idPrefix?: string }>();
const emit = defineEmits<{ dirty: [value: boolean] }>();
const forceJson = ref(false), raw = ref(''), dirty = ref(false);
const definitions = computed(() => formFields(props.schema, props.config));
const fields = shallowRef<Array<{ definition: NonNullable<ReturnType<typeof formFields>>[number]; included: boolean; value: string }>>([]);
const useJson = computed(() => forceJson.value || definitions.value === null);
function reset() {
  forceJson.value = false; raw.value = json(props.config); dirty.value = false; emit('dirty', false);
  fields.value = (definitions.value ?? []).map(definition => {
    const present = Object.hasOwn(props.config, definition.key);
    const value = present ? props.config[definition.key] : Object.hasOwn(definition, 'default') ? definition.default : definition.type === 'boolean' ? false : '';
    const index = definition.choices?.findIndex(v => v === value);
    return shallowReactive({ definition, included: definition.required || present, value: definition.choices ? (index === -1 ? '' : String(index)) : String(value) });
  });
}
watch(() => props.config, reset, { immediate: true });
function edit() { dirty.value = true; emit('dirty', true); }
function switchJson() { if (!dirty.value || window.confirm('有未保存更改，是否放弃并切换编辑方式？')) { reset(); forceJson.value = true; } }
function read(): Config {
  if (useJson.value) {
    let value: unknown; try { value = parseSafeJson(raw.value); } catch (e) { if (e instanceof SyntaxError) throw new Error('配置 JSON 格式错误，请检查括号、引号和逗号。'); throw e; }
    if (!isObject(value)) throw new Error('配置必须是 JSON 对象。'); return value as Config;
  }
  const result = new Map(Object.entries(props.config));
  for (const { definition: d, included, value } of fields.value) {
    if (!included) { result.delete(d.key); continue; }
    if (d.choices) { if (value === '') throw new Error(`请选择“${d.title}”。`); result.set(d.key, d.choices[Number(value)]!); }
    else if (['integer', 'number'].includes(d.type)) {
      const number = Number(value); if (!value.trim() || !Number.isFinite(number) || (d.type === 'integer' && !Number.isInteger(number))) throw new Error(`“${d.title}”需要有效的${d.type === 'integer' ? '整数' : '数字'}。`);
      assertSafeNumbers(number);
      result.set(d.key, number);
    } else result.set(d.key, value);
  }
  return Object.fromEntries(result);
}
defineExpose({ read, reset });
</script>
<template>
  <fieldset :disabled="disabled" @input="edit" @change="edit">
    <template v-if="useJson">
      <p class="hint">此配置使用整对象 JSON 编辑，适用于复杂对象、数组或组合规则。保存时由服务校验。</p>
      <label :for="`${idPrefix ?? ''}config-json`">配置 JSON</label><textarea :id="`${idPrefix ?? ''}config-json`" v-model="raw" spellcheck="false" />
    </template>
    <template v-else>
      <div v-for="(field, index) in fields" :key="field.definition.key" class="field">
        <div class="field-heading">
          <label :for="`${idPrefix ?? ''}field-${index}`">{{ field.definition.title }}{{ field.definition.required ? '（必填）' : '' }}</label>
          <label v-if="!field.definition.required"><input v-model="field.included" type="checkbox" :aria-label="`设置 ${field.definition.title}`"> 设置此项</label>
        </div>
        <select v-if="field.definition.choices" :id="`${idPrefix ?? ''}field-${index}`" v-model="field.value" :disabled="!field.included">
          <option value="">请选择</option><option v-for="(choice, i) in field.definition.choices" :key="i" :value="String(i)">{{ json(choice) }}</option>
        </select>
        <textarea v-else-if="field.definition.type === 'string'" :id="`${idPrefix ?? ''}field-${index}`" v-model="field.value" :disabled="!field.included" />
        <input v-else :id="`${idPrefix ?? ''}field-${index}`" :value="field.value" type="number" step="any" :min="field.definition.minimum" :max="field.definition.maximum" :disabled="!field.included" @input="field.value = ($event.target as HTMLInputElement).value">
        <p v-if="field.definition.description" class="hint">{{ field.definition.description }}</p>
        <p class="hint">{{ field.definition.key }}<template v-if="Object.hasOwn(field.definition, 'default')"> · 默认值：{{ json(field.definition.default) }}</template>{{ !field.definition.required ? ' · 取消设置时不保存此字段' : '' }}</p>
      </div>
      <p v-if="!fields.length" class="muted">没有可配置字段。</p>
    </template>
  </fieldset>
  <button v-if="!useJson" type="button" class="quiet" :disabled="disabled" @click="switchJson">使用整对象 JSON 编辑</button>
</template>
