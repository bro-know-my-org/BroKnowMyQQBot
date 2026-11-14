<script setup lang="ts">
import { computed } from 'vue';
import { RouterLink } from 'vue-router';
import type { Request } from '../api';
import RuntimePanel from './RuntimePanel.vue';
import { pluginRoute } from '../navigation';
import { runtimeNames } from '../types';
import type { PluginView } from '../types';
const props = defineProps<{ plugins: PluginView[]; refreshedAt: string; busy: boolean; request: Request }>();
defineEmits<{ refresh: [] }>();
const running = computed(() => props.plugins.filter(p => p.runtime_state === 'ready').length);
const pending = computed(() => props.plugins.filter(p => p.pending));
const errors = computed(() => props.plugins.filter(p => p.error || p.runtime_state === 'disabled'));
const attention = computed(() => props.plugins.filter(p => p.pending || p.error || p.runtime_state === 'disabled'));
</script>
<template>
  <div class="page-heading"><div class="page-heading-copy"><span class="eyebrow">运行 / OVERVIEW</span><h1 tabindex="-1">概览</h1><p class="muted">查看 Bot 健康、连接、事件与插件待办。</p></div><button id="refresh" type="button" :disabled="busy" @click="$emit('refresh')">刷新插件状态</button></div>
  <RuntimePanel :request="request" />
  <p v-if="!refreshedAt" class="hint">插件状态尚未读取，请等待加载或重试；运行快照独立更新。</p>
  <template v-else>
  <section class="overview-stats" aria-label="插件统计">
    <div v-for="stat in [{ label: '插件实例', value: plugins.length, note: '当前可管理的插件实例' }, { label: '运行中', value: running, note: '正在接收并处理事件' }, { label: '待应用', value: pending.length, note: '保存后尚未更新运行时' }, { label: '报告错误', value: errors.length, note: '需要查看插件详情' }]" :key="stat.label" class="stat"><span class="stat-label">{{ stat.label }}</span><strong class="stat-value">{{ stat.value }}</strong><span class="stat-note">{{ stat.note }}</span></div>
  </section>
  <p class="hint overview-lead">状态快照 · {{ refreshedAt || '尚未刷新' }}。仅统计插件实例，不代表平台连接状态。</p>
  <div class="overview-grid">
    <section class="panel" aria-labelledby="attention-title"><div class="panel-heading"><h2 id="attention-title">需要关注</h2><span class="plugin-count">{{ attention.length }}</span></div><div v-if="attention.length" class="reading-rows"><RouterLink v-for="p in attention" :key="p.instance_id" :to="pluginRoute(p.instance_id, p.pending && !p.error && p.runtime_state !== 'disabled' ? 'config' : 'overview')" class="reading-row"><div><strong class="row-title">{{ p.name || p.plugin_id }}</strong><span class="row-meta">{{ p.error || p.runtime_state === 'disabled' ? '报告错误' : '设置待应用' }}</span></div><span class="row-end" aria-hidden="true">↗</span></RouterLink></div><div v-else class="empty-state"><span class="status-marker"><span class="status-shape is-ready" aria-hidden="true" />暂无待办</span><p class="muted">当前没有待应用的设置或插件错误。</p></div></section>
    <section class="panel" aria-labelledby="instances-title"><div class="panel-heading"><h2 id="instances-title">插件运行</h2><RouterLink :to="{ name: 'plugins' }" class="text-link">查看全部 →</RouterLink></div><div class="reading-rows"><RouterLink v-for="p in plugins.slice(0, 5)" :key="p.instance_id" :to="pluginRoute(p.instance_id)" class="reading-row"><div><strong class="row-title">{{ p.name || p.plugin_id }}</strong><span class="row-meta">{{ p.version }}</span></div><span class="status-marker"><span class="status-shape" :class="p.runtime_state === 'ready' ? 'is-ready' : 'is-offline'" aria-hidden="true" />{{ runtimeNames[p.runtime_state] }}</span></RouterLink><p v-if="!plugins.length" class="empty-state muted">暂无插件实例。</p></div></section>
  </div>
  </template>
</template>
