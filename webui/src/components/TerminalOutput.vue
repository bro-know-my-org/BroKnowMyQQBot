<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue';
import type { LogEntry, PluginView } from '../types';
import { isMessageResult, logSource, messageDirection, readLog } from '../logPresentation';
const props = defineProps<{ entries: LogEntry[]; sessionId?: string; emptyMessage: string; includeContent?: boolean; historical?: boolean; plugins?: PluginView[] }>();
const viewport = ref<HTMLElement>(), content = ref<HTMLElement>(), following = ref(true);
const wrapping = ref(false), raw = ref(false);
const pluginLabels = computed(() => new Map((props.plugins ?? []).map(plugin => [plugin.instance_id, plugin])));
let observer: ResizeObserver | undefined;
let geometry = '';
function measure() { const element = viewport.value; return element ? `${element.clientWidth}/${element.clientHeight}/${element.scrollHeight}` : ''; }
function scrollToBottom() {
  if (viewport.value) viewport.value.scrollTop = viewport.value.scrollHeight;
  geometry = measure();
}
function toggleFollow() { following.value = !following.value; if (following.value) scrollToBottom(); }
function onScroll() {
  const element = viewport.value;
  if (!element) return;
  const current = measure();
  // Resizing/reflow and DOM changes can scroll too; retain follow through geometry changes.
  if (following.value && current !== geometry) { scrollToBottom(); return; }
  if (element.scrollHeight - element.scrollTop - element.clientHeight > 16) following.value = false;
  geometry = current;
}
const clock = new Intl.DateTimeFormat('zh-CN', { hour12: false, hour: '2-digit', minute: '2-digit', second: '2-digit' });
function time(timestamp: string) {
  const date = new Date(timestamp);
  return `${clock.format(date)}.${String(date.getMilliseconds()).padStart(3, '0')}`;
}
function field(value: string) { return /\s|["=]/u.test(value) ? JSON.stringify(value) : value; }
watch([() => props.entries, () => props.sessionId, raw, pluginLabels], async ([, newSession], [, oldSession]) => {
  const element = viewport.value;
  if (!element) return;
  const top = element.getBoundingClientRect().top;
  const anchor = !following.value && newSession === oldSession
    ? Array.from(element.querySelectorAll<HTMLElement>('.terminal-line')).find(row => row.getBoundingClientRect().bottom > top)
    : undefined;
  const anchorId = anchor?.dataset.entryId, offset = anchor?.getBoundingClientRect().top;
  await nextTick();
  if (viewport.value !== element) return;
  if (following.value) scrollToBottom();
  else if (newSession !== oldSession) element.scrollTop = 0;
  else if (anchorId !== undefined && offset !== undefined) {
    const retained = Array.from(element.querySelectorAll<HTMLElement>('.terminal-line')).find(row => row.dataset.entryId === anchorId);
    if (retained) element.scrollTop += retained.getBoundingClientRect().top - offset;
    else element.scrollTop = 0;
  }
});
function resized() { if (following.value) scrollToBottom(); else geometry = measure(); }
async function toggleWrap() {
  wrapping.value = !wrapping.value;
  await nextTick();
  resized();
}
onMounted(() => {
  scrollToBottom(); window.addEventListener('resize', resized);
  if (typeof ResizeObserver !== 'undefined') {
    observer = new ResizeObserver(resized);
    if (viewport.value) observer.observe(viewport.value);
    if (content.value) observer.observe(content.value);
  }
});
onBeforeUnmount(() => { observer?.disconnect(); window.removeEventListener('resize', resized); });
</script>
<template>
  <div class="terminal-panel">
    <div class="terminal-toolbar"><span>{{ historical ? 'BKMQB / ARCHIVED RUNTIME' : 'BKMQB / LIVE OUTPUT' }}</span><div class="terminal-controls"><button type="button" :aria-pressed="following" @click="toggleFollow">{{ following ? '正在跟随输出 ↓' : '跟随输出 ↓' }}</button><button type="button" :aria-pressed="wrapping" @click="toggleWrap">自动换行：{{ wrapping ? '开' : '关' }}</button><button type="button" :aria-pressed="raw" @click="raw = !raw">原始日志：{{ raw ? '开' : '关' }}</button></div></div>
    <div ref="viewport" class="terminal-viewport" :class="{ 'is-wrapping': wrapping }" tabindex="0" role="region" :aria-label="historical ? '历史运行日志输出' : '实时日志输出'" @scroll="onScroll">
      <div ref="content" class="terminal-lines">
      <div v-for="entry in entries" v-memo="[entry, sessionId, includeContent, raw, pluginLabels]" :key="`${sessionId}-${entry.id}`" class="terminal-line" :data-entry-id="entry.id"><time :datetime="entry.timestamp" :title="entry.timestamp">{{ time(entry.timestamp) }}</time>{{ ' ' }}<template v-for="reading in [raw ? undefined : readLog(entry)]"><template v-if="reading"><strong class="terminal-status">{{ `[${reading.status}]` }}</strong>{{ ' ' }}<span class="terminal-source">{{ logSource(entry, pluginLabels) }}</span>{{ ': ' }}<span class="terminal-message">{{ reading.summary }}</span><span v-if="reading.detail" class="terminal-detail">{{ ` · ${reading.detail}` }}</span><span v-if="reading.source || entry.level === 'WARN' || entry.level === 'ERROR'" class="terminal-level">{{ ` [${entry.level}]` }}</span></template><template v-else><strong class="terminal-level">{{ entry.level.padEnd(5) }}</strong>{{ ' ' }}<span class="terminal-source">{{ entry.target }}</span>{{ entry.instance_id ? ` [${entry.instance_id}]` : '' }}{{ ': ' }}<span v-if="entry.category === 'message'">{{ `[${messageDirection(entry)}] ` }}</span><span class="terminal-message">{{ entry.message || '（无消息文本）' }}</span><span v-for="(value, key) in entry.fields" :key="key" class="terminal-field">{{ ` ${key}=${field(value)}` }}</span></template></template><span v-if="entry.category === 'message' && includeContent && (!isMessageResult(entry) || entry.content !== undefined)" class="terminal-body">{{ ` 正文=${entry.content ?? '正文未采集'}` }}</span></div>
      <p v-if="!entries.length" class="terminal-empty">{{ emptyMessage }}</p>
      </div>
    </div>
    <div class="terminal-footer"><span>{{ following ? '跟随最新输出' : '正在查看较早记录' }}</span><span>{{ entries.length }} / 1000 行记录 · 本地时间</span></div>
  </div>
</template>
