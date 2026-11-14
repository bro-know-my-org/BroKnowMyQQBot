<script setup lang="ts">
import type { Request } from '../api';
import RuntimePanel from './RuntimePanel.vue';
import ConnectionSettings from './ConnectionSettings.vue';
defineProps<{ request: Request; locked?: boolean }>();
const emit = defineEmits<{ dirty: [value: boolean]; busy: [value: boolean] }>();
</script>
<template>
  <div class="page-heading"><div class="page-heading-copy"><span class="eyebrow">系统 / CONNECTIONS</span><h1 tabindex="-1">连接与适配器</h1><p class="muted">查看本次运行实际装配的适配器与连接状态。</p></div></div>
  <RuntimePanel :request="request" connections />
  <p class="hint">上方状态只读，展示当前进程状态。配置文件的后续修改需重启后才会反映到这里；未在线表示连接尚不可用。</p>
  <ConnectionSettings :request="request" :locked="locked" @dirty="emit('dirty', $event)" @busy="emit('busy', $event)" />
</template>
