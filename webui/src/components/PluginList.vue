<script setup lang="ts">
import { computed, ref } from 'vue';
import { runtimeNames } from '../types';
import type { PluginView } from '../types';
const props = defineProps<{ plugins: PluginView[]; selected?: string; busy: boolean }>();
defineEmits<{ select: [id: string] }>();
const search = ref('');
const visible = computed(() => props.plugins.filter(p => [p.name, p.plugin_id, p.instance_id, p.source].join(' ').toLocaleLowerCase().includes(search.value.trim().toLocaleLowerCase())));
</script>
<template>
  <aside class="card sidebar" aria-label="插件列表">
    <label for="search">查找插件 <span class="muted">· {{ plugins.length }}</span></label>
    <input id="search" v-model="search" type="search" placeholder="名称、ID 或来源" autocomplete="off" :disabled="busy">
    <div class="plugin-list">
      <button v-for="p in visible" :key="p.instance_id" type="button" class="plugin-item" :aria-current="p.instance_id === selected ? 'true' : 'false'" :disabled="busy" @click="$emit('select', p.instance_id)">
        <strong>{{ p.name || p.plugin_id }}</strong><small>{{ p.instance_id }}</small><small>{{ runtimeNames[p.runtime_state] }}{{ p.pending ? ' · 待应用' : '' }}</small>
      </button>
      <p v-if="!visible.length" class="empty">{{ plugins.length ? '没有匹配的插件，试试其他关键词。' : '暂无插件' }}</p>
    </div>
  </aside>
</template>
