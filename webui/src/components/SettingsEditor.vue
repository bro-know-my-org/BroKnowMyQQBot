<script setup lang="ts">
import { computed, onBeforeUnmount, ref, watch } from 'vue';
import { ApiError, type Request } from '../api';
import { useNoticeSource } from '../notifications';
import { readSettings, settingsPayload, type Group, type SettingsScope, type SettingsSnapshot } from '../systemSettings';
const props = defineProps<{ request: Request; scope: SettingsScope; title: string; groups: Group[]; locked?: boolean }>();
const emit = defineEmits<{ dirty: [value: boolean]; busy: [value: boolean] }>();
const opened = ref(false), busy = ref(false), blocked = ref(false), failed = ref(false), message = ref('');
const draft = ref<SettingsSnapshot | null>(null), saved = ref('');
const encoded = computed(() => draft.value ? JSON.stringify(settingsPayload(draft.value, props.scope)) : '');
const dirty = computed(() => !!draft.value && encoded.value !== saved.value);
const notices = useNoticeSource(props.title, () => message.value, () => {
  if (busy.value) return 'progress';
  return failed.value ? 'error' : 'success';
});
watch(dirty, value => emit('dirty', value)); watch(busy, value => emit('busy', value));
const controller = new AbortController();
let disposed = false;
onBeforeUnmount(() => { disposed = true; controller.abort(); });
async function read() { return readSettings(await props.request('/api/settings/system', undefined, controller.signal), props.scope, props.groups); }
function accept(view: SettingsSnapshot) { draft.value = view; saved.value = JSON.stringify(settingsPayload(view, props.scope)); blocked.value = false; }
async function refresh() {
  if (busy.value || props.locked || (dirty.value && !window.confirm('有未保存设置，是否放弃编辑并重新读取？'))) return;
  opened.value = true; busy.value = true; failed.value = false; message.value = '正在读取已保存设置…';
  try { const view = await read(); if (!disposed) { accept(view); message.value = '已读取文件设置；普通设置重启后生效，环境覆盖仍优先。'; } }
  catch (error) { if (!disposed) { failed.value = true; message.value = (error as Error).message; } }
  finally { if (!disposed) busy.value = false; }
}
async function save() {
  if (busy.value || props.locked || blocked.value || !dirty.value || !draft.value) return;
  busy.value = true; failed.value = false; message.value = '正在保存，当前运行连接与日志不变…';
  const submitted = encoded.value;
  try {
    const response = readSettings(await props.request('/api/settings/system', { scope: props.scope, expected_revision: draft.value.revision, settings: settingsPayload(draft.value, props.scope) }, controller.signal), props.scope, props.groups);
    if (!disposed) { accept(response); message.value = '已保存；请通过现有运行方式重启 Bot 后生效。'; }
  } catch (error) {
    if (disposed) return;
    failed.value = true; message.value = (error as Error).message;
    const status = error instanceof ApiError ? error.status : 0;
    if (status === 409) blocked.value = true;
    if (status === 0 || status >= 500) {
      blocked.value = true;
      try {
        const current = await read();
        if (!disposed && JSON.stringify(settingsPayload(current, props.scope)) === submitted) {
          accept(current); failed.value = false; message.value = '重新读取已确认保存；请重启 Bot 后生效。';
        }
      } catch { /* Retain edits and block repeat writes until an explicit refresh. */ }
      if (!disposed && blocked.value) message.value = '保存结果未确认，已尝试重新读取，未自动重试写入。请复制编辑后重新读取。';
    }
  } finally { if (!disposed) busy.value = false; }
}
</script>
<template>
  <section class="card system-settings" :aria-labelledby="`${scope}-settings-title`">
    <h2 :id="`${scope}-settings-title`">{{ title }}</h2>
    <p class="muted">普通设置仅保存，重启后生效。当前进程可能受环境变量覆盖，无法从这里判断是否仍有待应用修改。</p>
    <button v-if="!opened" :id="`edit-${scope}`" type="button" :disabled="locked" @click="refresh">编辑已保存设置</button>
    <template v-else>
      <form :id="`${scope}-form`" @submit.prevent="save">
        <fieldset :disabled="busy || locked || !draft"><slot v-if="draft" :values="draft.values" /></fieldset>
        <p v-if="!draft" role="status">{{ busy ? '正在加载已保存设置…' : '设置暂不可用，请重新读取。' }}</p>
        <div class="actions"><button :id="`save-${scope}`" class="primary" :disabled="busy || locked || !dirty || blocked">保存，重启后生效</button><button :id="`refresh-${scope}`" type="button" :disabled="busy || locked" @click="refresh">重新读取已保存设置</button></div>
      </form>
      <p v-if="blocked" class="hint">保存已暂停。编辑仍保留，请复制需要保留的内容后重新读取。</p>
      <details v-if="dirty || blocked" class="settings-copy"><summary>复制当前编辑</summary><label :for="`copy-${scope}`">当前非敏感设置（只读，可复制）</label><textarea :id="`copy-${scope}`" readonly :value="encoded" rows="6" /></details>
    </template>
    <p v-if="message && !notices" class="hint" :role="failed ? 'alert' : 'status'">{{ message }}</p>
  </section>
</template>
