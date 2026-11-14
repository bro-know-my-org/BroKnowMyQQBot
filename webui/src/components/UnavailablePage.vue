<script setup lang="ts">
import { computed } from 'vue';
import { RouterLink } from 'vue-router';
const props = defineProps<{ kind: string }>();
const pages: Record<string, { title: string; section: string; description: string; detail: string }> = {
  adapters: { title: '连接与适配器', section: '系统', description: '管理 Bot 与消息平台的连接。', detail: '平台连接状态与配置尚未接入管理面板。请使用现有配置文件管理连接；插件运行状态不能用于判断平台是否在线。' },
};
const content = computed(() => pages[props.kind] ?? { title: '页面不存在', section: '控制台', description: '这个地址没有对应的页面。', detail: '请通过侧栏导航，或返回概览继续操作。' });
</script>
<template>
  <div class="page-heading"><div class="page-heading-copy"><span class="eyebrow">{{ content.section }}</span><h1 tabindex="-1">{{ content.title }}</h1><p class="muted">{{ content.description }}</p></div></div>
  <section class="panel empty-state"><span class="status-marker"><span class="status-shape is-offline" aria-hidden="true" />{{ kind === 'not-found' ? '未找到页面' : '尚未接入' }}</span><p class="muted">{{ content.detail }}</p><RouterLink :to="{ name: 'overview' }" class="button-link">返回概览</RouterLink></section>
</template>
