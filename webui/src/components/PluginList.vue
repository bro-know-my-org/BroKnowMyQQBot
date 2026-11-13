<script setup lang="ts">
import { computed, ref } from 'vue';
import { RouterLink } from 'vue-router';
import { pluginRoute } from '../navigation';
import { runtimeNames } from '../types';
import type { PluginView } from '../types';
const props = defineProps<{ plugins: PluginView[]; busy: boolean }>();
const search = ref(''), filter = ref('all');
const visible = computed(() => props.plugins.filter(p => [p.name, p.plugin_id, p.instance_id, p.source].join(' ').toLocaleLowerCase().includes(search.value.trim().toLocaleLowerCase()) && (filter.value === 'all' || (filter.value === 'ready' ? p.runtime_state === 'ready' : filter.value === 'pending' ? p.pending : !!p.error))));
</script>
<template>
  <section aria-label="插件列表">
    <div class="plugin-toolbar"><div class="filter-field"><label for="search">查找插件</label><input id="search" v-model="search" type="search" placeholder="名称、ID 或来源" autocomplete="off" :disabled="busy"></div><div><label for="plugin-filter">状态</label><select id="plugin-filter" v-model="filter" :disabled="busy"><option value="all">全部状态</option><option value="ready">运行中</option><option value="pending">待应用</option><option value="error">报告错误</option></select></div></div>
    <p class="hint plugin-count" aria-live="polite">{{ visible.length }} / {{ plugins.length }} 个实例</p>
    <div class="plugin-list">
      <RouterLink v-for="(p, index) in visible" :key="p.instance_id" :to="pluginRoute(p.instance_id)" class="plugin-item" :aria-disabled="busy || undefined"><span class="plugin-index" aria-hidden="true">{{ String(index + 1).padStart(2, '0') }}</span><div class="plugin-identity"><strong>{{ p.name || p.plugin_id }}</strong><small>{{ p.instance_id }}</small><p class="plugin-summary">{{ p.description || '暂无描述' }}</p></div><div class="plugin-end"><span class="status-marker"><span class="status-shape" :class="p.error ? 'is-error' : p.runtime_state === 'ready' ? 'is-ready' : 'is-offline'" aria-hidden="true" />{{ p.error ? '报告错误' : runtimeNames[p.runtime_state] }}</span><span v-if="p.pending" class="pending-marker">设置待应用</span><span class="row-meta">v{{ p.version }}</span></div><span class="row-end" aria-hidden="true">↗</span></RouterLink>
      <p v-if="!visible.length" class="empty">{{ plugins.length ? '没有匹配的插件，试试其他关键词。' : '暂无插件' }}</p>
    </div>
  </section>
</template>
