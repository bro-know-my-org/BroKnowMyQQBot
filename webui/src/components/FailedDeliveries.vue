<script setup lang="ts">
import { nextTick, onBeforeUnmount, ref, shallowRef, watch } from 'vue';
import type { Request } from '../api';
import { isObject } from '../types';
import type { FailedDeliveryPage } from '../types';
const props = defineProps<{ request: Request; instanceId: string; revision: number; busy: boolean }>();
const page = shallowRef<FailedDeliveryPage>(), number = ref(1), loading = ref(false), error = ref('');
const refreshButton = ref<HTMLButtonElement>();
const classes: Record<string, string> = { unknown: '未分类（旧记录或未提供分类）', guest_trap: '插件陷阱', resource_exhausted: '资源或时间超限', transient: '临时故障', invalid_output: '无效输出', host_failure: '宿主存储故障', lifecycle_changed: '生命周期已变化', plugin_error: '插件错误' };
let generation = 0;
let focusObserver: AbortController | undefined;
onBeforeUnmount(() => { generation++; focusObserver?.abort(); });
function readPage(value: unknown, before?: number): FailedDeliveryPage {
  const invalid = () => new Error('失败记录格式异常，请重新读取。');
  if (!isObject(value) || !Array.isArray(value.entries) || value.entries.length > 20
    || !(value.next_before === null || (Number.isSafeInteger(value.next_before) && Number(value.next_before) > 0
      && (before === undefined || Number(value.next_before) < before)))) throw invalid();
  if (value.next_before !== null && value.entries.length !== 20) throw invalid();
  const entries = value.entries.map(entry => {
    if (!isObject(entry) || !Number.isSafeInteger(entry.attempts) || Number(entry.attempts) < 0
      || !Number.isSafeInteger(entry.updated_at_ms) || !Number.isFinite(new Date(Number(entry.updated_at_ms)).getTime())
      || typeof entry.error_class !== 'string' || !Object.hasOwn(classes, entry.error_class)) throw invalid();
    return { attempts: Number(entry.attempts), updated_at_ms: Number(entry.updated_at_ms), error_class: entry.error_class };
  });
  return { entries, next_before: value.next_before as number | null };
}
async function load(before?: number, nextNumber = 1) {
  if (props.busy || loading.value) return;
  const current = ++generation;
  const nextButton = document.activeElement?.id === 'next-failures' ? document.activeElement : undefined;
  let focusMoved = false;
  const observeFocus = (event: FocusEvent) => { if (event.target !== nextButton && event.target !== document.body) focusMoved = true; };
  const observer = new AbortController(); focusObserver?.abort(); focusObserver = observer;
  if (nextButton) document.addEventListener('focusin', observeFocus, { signal: observer.signal });
  loading.value = true; error.value = '';
  try {
    const params = new URLSearchParams({ instance_id: props.instanceId, limit: '20' });
    if (before !== undefined) params.set('before', String(before));
    const result = readPage(await props.request(`/api/plugins/failures?${params}`), before);
    if (current === generation) {
      const restoreFocus = result.next_before === null && nextButton && !focusMoved;
      page.value = result; number.value = nextNumber;
      if (restoreFocus) { loading.value = false; await nextTick(); if (current === generation && !focusMoved) refreshButton.value?.focus(); }
    }
  } catch (e) { if (current === generation) error.value = (e as Error).message; }
  finally { observer.abort(); if (current === generation) loading.value = false; }
}
watch(() => [props.instanceId, props.revision, props.busy] as const, () => {
  generation++; focusObserver?.abort(); loading.value = false;
  if (!props.busy) void load();
}, { immediate: true });
</script>
<template>
  <section class="detail-section" aria-labelledby="failures-title">
    <div class="section-heading"><h2 id="failures-title">失败投递 / 死信</h2><button id="refresh-failures" ref="refreshButton" type="button" :disabled="busy || loading" @click="load()">重新读取首页</button></div>
    <p class="hint">仅展示安全分类、尝试次数和时间，不展示消息正文或原始诊断。</p>
    <p v-if="loading" role="status">正在读取失败记录…</p>
    <p v-if="error" class="notice error" role="alert">{{ error }}</p>
    <template v-if="page">
      <p class="hint" role="status">第 {{ number }} 页 · 本页 {{ page.entries.length }} 条{{ page.next_before !== null ? ' · 还有其他记录' : '' }}</p>
      <ul v-if="page.entries.length" class="failure-list"><li v-for="(entry, index) in page.entries" :key="index"><strong>{{ classes[entry.error_class] }}</strong><span>尝试 {{ entry.attempts }} 次</span><time :datetime="new Date(entry.updated_at_ms).toISOString()">{{ new Date(entry.updated_at_ms).toLocaleString('zh-CN') }}</time></li></ul>
      <p v-else>当前没有死信记录。</p>
      <button v-if="page.next_before !== null" id="next-failures" type="button" :disabled="busy || loading" @click="load(page.next_before!, number + 1)">下一页</button>
    </template>
  </section>
</template>
