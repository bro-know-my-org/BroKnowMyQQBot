<script setup lang="ts">
import type { Request } from '../api';
import { connectionGroups, type Values } from '../systemSettings';
import SettingsEditor from './SettingsEditor.vue';
import SettingsFields from './SettingsFields.vue';
defineProps<{ request: Request; locked?: boolean; bootId?: string }>();
const emit = defineEmits<{ dirty: [value: boolean]; busy: [value: boolean]; saved: [bootId: string] }>();
function active(group: string, values: Record<string, Values>) {
  if (group === 'webhook') return values.qq?.enabled === true && values.qq?.transport === 'webhook';
  if (group === 'onebot11') return values.onebot11?.enabled === true;
  return values.qq?.enabled === true;
}
</script>
<template>
  <SettingsEditor scope="connections" title="连接配置" :groups="connectionGroups" :request="request" :locked="locked" :boot-id="bootId" @saved="emit('saved', $event)" @dirty="emit('dirty', $event)" @busy="emit('busy', $event)">
    <template #default="{ values }">
      <p class="hint">凭据由环境变量或本地 secrets.env 管理；可用 BKMQB_SECRETS_FILE 指定文件。此页未检查凭据是否存在，也不读取或写入凭据。</p>
      <p class="hint">QQ：BKMQB_QQ_OFFICIAL_APP_ID / BKMQB_QQ_OFFICIAL_APP_SECRET；OneBot：BKMQB_ONEBOT_ACCESS_TOKEN。监听地址不包含 Token，Webhook 路径不包含查询参数。</p>
      <SettingsFields v-for="group in connectionGroups" :key="group.key" :group="group" :values="values[group.key]!" :active="active(group.key, values)" prefix="connections" />
    </template>
  </SettingsEditor>
</template>
