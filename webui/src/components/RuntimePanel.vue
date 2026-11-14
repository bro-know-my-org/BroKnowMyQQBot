<script setup lang="ts">
import { computed } from 'vue';
import type { Request } from '../api';
import { failureNames, phaseNames, useRuntime } from '../runtime';
import type { AdapterStatus } from '../runtime';
const props = defineProps<{ request: Request; connections?: boolean }>();
const { data, loading, stale, error, updatedAt, refresh } = useRuntime(props.request);
const state = computed(() => {
  if (!data.value) return error.value ? '状态不可用' : '正在读取状态';
  const { health, snapshot } = data.value;
  return snapshot.phase === 'running' && health.status === 'degraded' ? '运行降级' : phaseNames[snapshot.phase];
});
function adapterState(adapter: AdapterStatus) {
  const phase = data.value!.snapshot.phase;
  if (phase !== 'running') return phaseNames[phase];
  return adapter.online ? '在线' : '已配置 · 未在线';
}
</script>
<template>
  <section class="runtime-panel panel" aria-label="Bot 运行状态">
    <div class="panel-heading"><h2>{{ connections ? '当前连接' : 'Bot 运行' }}</h2><button type="button" :disabled="loading" @click="refresh">{{ error ? '重试运行状态' : '刷新运行状态' }}</button></div>
    <p class="status-marker"><span class="status-shape" :class="data?.health.status === 'healthy' && !stale ? 'is-ready' : 'is-offline'" aria-hidden="true" /><strong>{{ state }}</strong><span v-if="stale">{{ data ? '快照已过期' : '暂无快照' }}</span></p>
    <p v-if="error" role="status" class="runtime-error">{{ error }}{{ data ? ' 保留上次成功读取的数据。' : '' }}</p>
    <template v-if="data">
      <dl class="runtime-values">
        <div><dt>在线 / 已配置</dt><dd>{{ data.health.online_adapters }} / {{ data.health.configured_adapters }}</dd></div>
        <div><dt>事件已接收</dt><dd>{{ data.snapshot.events_received }}</dd></div>
        <div><dt>事件已完成</dt><dd>{{ data.snapshot.events_completed }}</dd></div>
        <div><dt>队列积压 / 容量</dt><dd>{{ data.snapshot.queue_depth }} / {{ data.snapshot.queue_capacity }}</dd></div>
        <div><dt>正在处理</dt><dd>{{ data.snapshot.active_events }}</dd></div>
        <div><dt>重复 / 拒绝</dt><dd>{{ data.snapshot.duplicate_events }} / {{ data.snapshot.rejected_events }}</dd></div>
      </dl>
      <div v-if="connections" class="adapter-list">
        <article v-for="(adapter, id) in data.snapshot.adapters" :key="id" class="adapter-row">
          <h3>{{ id }}</h3><p class="status-marker"><span class="status-shape" :class="adapter.online && data.snapshot.phase === 'running' ? 'is-ready' : 'is-offline'" aria-hidden="true" />{{ adapterState(adapter) }}</p>
          <dl class="runtime-values"><div><dt>接收 / 拒绝事件</dt><dd>{{ adapter.events_received }} / {{ adapter.events_rejected }}</dd></div><div><dt>累计故障</dt><dd>{{ adapter.failures }}</dd></div></dl>
          <p v-if="adapter.last_failure" class="hint">最近记录的故障：{{ failureNames[adapter.last_failure] }}。{{ adapter.online ? '当前已在线；历史故障计数保留。' : '请结合运行日志检查连接与平台配置。' }}</p>
        </article>
        <p v-if="!Object.keys(data.snapshot.adapters).length" class="muted">运行时尚未报告适配器，当前没有可确认的在线连接。</p>
      </div>
      <p class="hint">运行快照 · {{ updatedAt }} · 前台约每 5 秒更新。计数来自当前进程；队列深度为采样值。</p>
    </template>
  </section>
</template>
