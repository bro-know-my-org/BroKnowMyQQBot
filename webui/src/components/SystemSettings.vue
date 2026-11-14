<script setup lang="ts">
import type { Request } from '../api';
import { globalGroups } from '../globalSettings';
import SettingsEditor from './SettingsEditor.vue';
import SettingsFields from './SettingsFields.vue';
defineProps<{ request: Request; locked?: boolean }>();
const emit = defineEmits<{ dirty: [value: boolean]; busy: [value: boolean] }>();
</script>
<template>
  <SettingsEditor scope="global" title="运行与日志设置" :groups="globalGroups" :request="request" :locked="locked" @dirty="emit('dirty', $event)" @busy="emit('busy', $event)">
    <template #default="{ values }">
      <p class="hint">这是文件中保存的值。BKMQB_QUEUE_CAPACITY、BKMQB_EVENT_CONCURRENCY、RUST_LOG、BKMQB_LOG_LANGUAGE、BKMQB_LOG_MESSAGE_CONTENT 等环境变量仍可在启动时覆盖它们。</p>
      <p class="hint">消息正文默认不采集；修改其他选项不会开启正文。此处不修改管理认证、Token、管理监听器或日志目录。</p>
      <SettingsFields v-for="group in globalGroups" :key="group.key" :group="group" :values="values[group.key]!" prefix="global" />
    </template>
  </SettingsEditor>
</template>
