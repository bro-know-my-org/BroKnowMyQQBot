<script setup lang="ts">
import type { Group, Values } from '../systemSettings';
defineProps<{ group: Group; values: Values; prefix: string; active?: boolean }>();
</script>
<template>
  <fieldset class="system-group"><legend>{{ group.title }}</legend><p v-if="group.hint" class="hint">{{ group.hint }}</p>
    <div class="system-fields"><div v-for="field in group.fields" :key="field.key" :class="{ 'system-checkbox': field.type === 'boolean' }">
      <label :for="`${prefix}-${group.key}-${field.key}`">{{ field.label }}</label>
      <input v-if="field.type === 'boolean'" :id="`${prefix}-${group.key}-${field.key}`" v-model="values[field.key]" type="checkbox" :aria-describedby="field.hint ? `${prefix}-${group.key}-${field.key}-hint` : undefined">
      <select v-else-if="field.options" :id="`${prefix}-${group.key}-${field.key}`" v-model="values[field.key]">
        <option v-if="!field.options.includes(String(values[field.key]))" :value="values[field.key]">{{ values[field.key] }}（现有值）</option>
        <option v-for="option in field.options" :key="option" :value="option">{{ option }}</option>
      </select>
      <input v-else-if="field.type === 'number'" :id="`${prefix}-${group.key}-${field.key}`" v-model.number="values[field.key]" type="number" required step="1" :min="active === false ? undefined : field.min" :max="active === false ? undefined : field.max">
      <input v-else :id="`${prefix}-${group.key}-${field.key}`" v-model="values[field.key]" required maxlength="1024" autocomplete="off" spellcheck="false">
      <p v-if="field.hint" :id="`${prefix}-${group.key}-${field.key}-hint`" class="hint">{{ field.hint }}</p>
    </div></div>
  </fieldset>
</template>
