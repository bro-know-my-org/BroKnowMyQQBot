<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue';
import { ApiError } from '../api';
import type { Request } from '../api';
import type { Commands } from '../types';
import { useNoticeSource } from '../notifications';
const props = defineProps<{ request: Request }>();
const emit = defineEmits<{ dirty: [value: boolean]; busy: [value: boolean] }>();
const saved = ref<Commands | null>(null), prefix = ref(''), busy = ref(false), conflict = ref(false), message = ref(''), error = ref(false);
const dirty = computed(() => saved.value !== null && prefix.value !== (saved.value.saved_prefix ?? saved.value.prefix));
const notifications = useNoticeSource('命令前缀', () => message.value, () => busy.value ? 'progress' : error.value ? 'error' : saved.value?.pending ? 'warning' : 'success');
watch(dirty, value => emit('dirty', value)); watch(busy, value => emit('busy', value));
async function update(save: boolean) {
  if (busy.value || (save && (!saved.value || !prefix.value || conflict.value))) return;
  if (!save && dirty.value && !window.confirm('前缀有未保存更改，是否放弃并刷新前缀？')) return;
  busy.value = true; error.value = false; message.value = save ? '正在保存并应用前缀…' : '正在加载前缀…';
  try {
    const result = await props.request<Commands>('/api/settings/commands', save ? { prefix: prefix.value, expected_revision: saved.value!.revision } : undefined);
    if (typeof result.prefix !== 'string' || !result.prefix || typeof result.revision !== 'string') throw new Error('前缀响应格式异常，请刷新确认状态。');
    saved.value = result; prefix.value = result.saved_prefix ?? result.prefix; conflict.value = false; message.value = result.pending ? '已保存的前缀尚未生效，请刷新确认状态。' : save ? '前缀已保存并立即生效。' : '前缀状态已更新。';
  } catch (e) {
    if (e instanceof ApiError && e.status === 409) conflict.value = true;
    error.value = true; message.value = (e as Error).message;
  } finally { busy.value = false; }
}
onMounted(() => update(false));
</script>
<template>
  <section class="card command-settings" aria-labelledby="commands-title">
    <h2 id="commands-title">全局命令前缀</h2><p class="muted">保存后立即生效，用于所有插件的命令与帮助。</p>
    <form class="settings-form" @submit.prevent="update(true)">
      <div><label for="command-prefix">命令前缀</label><input id="command-prefix" v-model="prefix" required autocomplete="off" :disabled="busy || !saved" placeholder="例如 / 或 !!"></div>
      <div class="actions"><button id="save-prefix" class="primary" :disabled="busy || (!dirty && !saved?.pending) || !prefix || conflict">保存并立即应用</button><button id="refresh-prefix" type="button" :disabled="busy" @click="update(false)">刷新前缀</button></div>
    </form>
    <p v-if="message && !notifications" class="hint" :class="{ 'text-error': error }" :role="error ? 'alert' : 'status'">{{ message }}</p>
    <p v-if="saved" class="hint">当前运行前缀：{{ saved.prefix }}{{ saved.pending ? ' · 已保存设置待生效' : '' }}</p>
    <p v-if="conflict" class="hint">前缀编辑仍保留，请刷新前缀后重试。插件编辑不会受影响。</p>
  </section>
</template>
