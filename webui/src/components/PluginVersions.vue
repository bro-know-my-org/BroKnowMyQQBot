<script setup lang="ts">
import type { VersionStatus } from '../types';
defineProps<{ status: VersionStatus }>();
</script>
<template>
  <span class="row-meta">已保存版本：{{ status.saved_version ?? '未提供' }}</span>
  <span class="row-meta">实际加载版本：{{ status.loaded_version ?? (status.runtime_state === 'not_loaded' ? '尚未加载' : '未知') }}</span>
  <span v-if="status.restart_required" class="pending-marker">需要重启 Bot</span>
</template>
