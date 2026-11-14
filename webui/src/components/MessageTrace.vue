<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, shallowRef, useId, watch } from 'vue';
import type { Request } from '../api';
import type { PluginView } from '../types';
import { logSource } from '../logPresentation';
import { readTracePage, traceReading, type TracePage, type TraceTarget } from '../messageTrace';
const props = defineProps<{ request: Request; target: TraceTarget; includeContent: boolean; plugins?: PluginView[] }>();
const emit = defineEmits<{ close: [] }>();
const uid = useId(), dialog = ref<HTMLDialogElement>();
const page = shallowRef<TracePage>(), busy = ref(false), error = ref('');
const labels = computed(() => new Map((props.plugins ?? []).map(plugin => [plugin.instance_id, plugin])));
let generation = 0, controller: AbortController | undefined, opener: HTMLElement | null = null;
function cancel() { generation++; controller?.abort(); controller = undefined; busy.value = false; }
async function load(after?: number) {
  cancel(); controller = new AbortController(); const signal = controller.signal, current = generation;
  const target = { ...props.target }, body = props.includeContent;
  busy.value = true; error.value = '';
  const params = new URLSearchParams({ session_id: target.session_id, include_content: String(body), limit: '200' });
  if (target.event_id !== undefined) params.set('event_id', target.event_id);
  else if (target.invocation_id !== undefined) params.set('invocation_id', target.invocation_id);
  if (after !== undefined) params.set('after', String(after));
  try {
    const raw = await props.request<unknown>(`/api/logs/trace?${params}`, undefined, signal);
    if (current !== generation || signal.aborted) return;
    page.value = readTracePage(raw, target, body, after);
  } catch (cause) {
    if (current === generation && !signal.aborted) error.value = cause instanceof Error ? cause.message : '链路无法读取，请重试。';
  } finally { if (current === generation) busy.value = false; }
}
function keydown(event: KeyboardEvent) {
  if (event.key === 'Escape') { event.preventDefault(); emit('close'); return; }
  if (event.key !== 'Tab') return;
  const controls = Array.from(dialog.value?.querySelectorAll<HTMLElement>('button:not(:disabled), summary, [tabindex="0"]') ?? []);
  const first = controls[0], last = controls.at(-1);
  if (event.shiftKey && (document.activeElement === first || document.activeElement === dialog.value)) { event.preventDefault(); last?.focus(); }
  else if (!event.shiftKey && (document.activeElement === last || document.activeElement === dialog.value)) { event.preventDefault(); first?.focus(); }
}
watch([() => props.target.session_id, () => props.target.event_id, () => props.target.invocation_id, () => props.includeContent, () => props.request], () => {
  // Clear retained body synchronously, even if a cancelled transport resolves later.
  cancel(); page.value = undefined; void load();
}, { flush: 'sync' });
onMounted(() => {
  opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  dialog.value?.showModal(); dialog.value?.focus(); void load();
});
onBeforeUnmount(() => {
  cancel(); page.value = undefined; dialog.value?.close();
  if (opener?.isConnected && !opener.closest('[inert]')) opener.focus();
  else document.getElementById('page-content')?.focus();
});
</script>
<template>
  <Teleport to="body">
    <dialog ref="dialog" class="message-trace" :aria-labelledby="`${uid}-title`" :aria-describedby="`${uid}-scope`" tabindex="-1" @cancel.prevent="emit('close')" @keydown="keydown">
      <div class="section-heading"><h2 :id="`${uid}-title`">消息处理链路</h2><button type="button" @click="emit('close')">关闭链路</button></div>
      <p>事件：<code>{{ page?.event_id ?? target.event_id ?? '尚无保留的事件关联' }}</code></p>
      <p v-if="target.invocation_id">调用：<code>{{ target.invocation_id }}</code></p>
      <p class="hint">进程日志会话：<code>{{ target.session_id }}</code></p>
      <p :id="`${uid}-scope`" class="notice">不完整窗口：跨全部实例及已保留级别查询，不受日志页筛选限制。缺失步骤可能被配置过滤、尚未发生或已淘汰；不代表完整历史，也不推断处理或发送成功。</p>
      <p v-if="page?.retention_evicted" class="notice">日志窗口已发生淘汰，较早的链路步骤可能无法取回。</p>
      <p class="hint">{{ includeContent ? '仅显示已保留的安全正文；未采集、被清除或已淘汰的正文无法补取。此面板不会启动采集。' : '正文已关闭。此面板不会启动正文采集；原始字段与正文开关独立。' }}</p>
      <div class="actions"><button type="button" :disabled="busy" @click="load()">重新读取</button><button v-if="page?.has_more" type="button" :disabled="busy" @click="load(page.next_cursor)">下一页（最多 200 条）</button></div>
      <p v-if="busy" role="status">正在读取链路…</p><p v-if="error" class="notice error" role="alert">{{ error }}</p>
      <p v-if="page && !page.entries.length" class="empty">当前保留窗口内没有可取回的关联记录。</p>
      <p v-if="page?.has_more" class="hint">结果已分页，当前仅显示其中一页；下一页继续按记录顺序读取。</p>
      <ol v-if="page?.entries.length" class="trace-timeline" aria-label="链路步骤">
        <li v-for="entry in page.entries" :key="`${target.session_id}-${entry.id}`">
          <template v-for="reading in [traceReading(entry)]" :key="entry.id">
            <div class="log-meta"><time :datetime="entry.timestamp">{{ entry.timestamp }}</time><strong>{{ reading.status }}</strong><span v-if="entry.fields.elapsed_ms">耗时 {{ entry.fields.elapsed_ms }} ms</span></div>
            <h3>{{ reading.summary }}</h3><p>{{ logSource(entry, labels) }}</p><p v-if="reading.detail">{{ reading.detail }}</p>
          </template>
          <p v-if="entry.fields.invocation_id" class="hint">调用：<code>{{ entry.fields.invocation_id }}</code><template v-if="entry.fields.command_id"> · 命令：<code>{{ entry.fields.command_id }}</code></template></p>
          <p v-if="includeContent && entry.category === 'message'" class="trace-body">{{ entry.content ?? '正文未采集或已清除' }}</p>
          <details><summary>原始安全字段</summary><dl class="log-fields"><template v-for="(value, key) in entry.fields" :key="key"><dt>{{ key }}</dt><dd>{{ value }}</dd></template></dl></details>
        </li>
      </ol>
    </dialog>
  </Teleport>
</template>
<style scoped>
.message-trace { position: fixed; inset: 0 0 0 auto; margin: 0; width: min(100%, 760px); max-width: 100%; height: 100dvh; max-height: 100dvh; padding: 24px; border: 0; border-left: 2px solid var(--e-ink-text); background: var(--e-ink-paper); color: var(--e-ink-text); overflow: auto; overflow-wrap: anywhere; }
.message-trace::backdrop { background: rgb(32 33 30 / 35%); }
.message-trace .section-heading { align-items: center; flex-wrap: wrap; gap: 8px; }
.message-trace p { margin-block: 12px; }
.trace-timeline { list-style: none; margin: 24px 0 0; padding: 0 0 0 16px; border-left: 2px solid var(--e-ink-rule); }
.trace-timeline li { padding: 0 0 20px 16px; border-bottom: 1px solid var(--e-ink-rule); margin-bottom: 20px; }
.trace-timeline h3 { margin-top: 8px; }
.trace-timeline summary { min-height: 44px; cursor: pointer; }
.trace-body { white-space: pre-wrap; font-family: var(--font-mono); }
@media (max-width: 480px) { .message-trace { padding: 16px; } .trace-timeline { padding-left: 8px; } .trace-timeline li { padding-left: 8px; } }
</style>
