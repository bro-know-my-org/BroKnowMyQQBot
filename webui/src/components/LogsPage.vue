<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, useId, watch } from 'vue';
import type { Request } from '../api';
import TerminalOutput from './TerminalOutput.vue';
import { isObject } from '../types';
import type { LogEntry, LogPage, PluginView } from '../types';

const props = defineProps<{ request: Request; plugins?: PluginView[]; instanceId?: string }>();
const uid = useId(), levels = ['TRACE', 'DEBUG', 'INFO', 'WARN', 'ERROR'];
const level = ref('INFO'), selected = ref(''), paused = ref(false), busy = ref(false);
const entries = ref<LogEntry[]>([]), error = ref(''), notice = ref('');
const view = ref<'terminal' | 'structured'>('terminal');
const instance = computed(() => props.instanceId ?? selected.value);
let alive = false, generation = 0, pending = false, resetting = false, cursor: number | undefined, session: string | undefined;
let timer: ReturnType<typeof setTimeout> | undefined;
function stopTimer() { clearTimeout(timer); timer = undefined; }
function validInteger(value: unknown): value is number { return Number.isSafeInteger(value) && Number(value) >= 0; }
function text(value: unknown, max: number): value is string { return typeof value === 'string' && value.length <= max; }
function readPage(value: unknown, after: number | undefined): LogPage {
  const invalid = () => new Error('日志响应格式异常，已停止自动刷新，请手动重试。');
  if (!isObject(value) || !text(value.session_id, 128) || !value.session_id || !validInteger(value.next_cursor)
    || typeof value.dropped !== 'boolean' || typeof value.has_more !== 'boolean' || !Array.isArray(value.entries) || value.entries.length > 200) throw invalid();
  const nextCursor = value.next_cursor;
  let previous = after !== undefined && !value.dropped && value.session_id === session ? after : 0;
  const rows = value.entries.map((entry: unknown): LogEntry => {
    if (!isObject(entry) || !validInteger(entry.id) || entry.id <= previous || entry.id > nextCursor
      || !text(entry.timestamp, 64) || !Number.isFinite(Date.parse(entry.timestamp)) || !text(entry.level, 5) || !levels.includes(entry.level)
      || !text(entry.target, 128) || !text(entry.message, 2048) || !(entry.instance_id === null || text(entry.instance_id, 512))
      || !isObject(entry.fields) || Object.keys(entry.fields).length > 16
      || !Object.entries(entry.fields).every(([key, field]) => key.length <= 64 && text(field, 512))) throw invalid();
    previous = entry.id;
    return { id: entry.id, timestamp: entry.timestamp, level: entry.level, target: entry.target, message: entry.message,
      instance_id: entry.instance_id, fields: { ...entry.fields } as Record<string, string> };
  });
  if (value.has_more && (!rows.length || (after !== undefined && value.next_cursor <= after))) throw invalid();
  if (!value.dropped && after !== undefined && value.session_id === session && value.next_cursor < after) throw invalid();
  return { session_id: value.session_id, entries: rows, next_cursor: value.next_cursor, dropped: value.dropped, has_more: value.has_more };
}
function refresh() { stopTimer(); resetting = false; pending = true; void load(); }
async function load() {
  if (!alive || busy.value) return;
  stopTimer(); pending = false; busy.value = true;
  const current = generation, after = cursor;
  let more = false;
  const query = new URLSearchParams({ limit: '200', level: level.value });
  if (after !== undefined) query.set('after', String(after));
  if (instance.value) query.set('instance_id', instance.value);
  try {
    const raw = await props.request<unknown>(`/api/logs?${query}`);
    if (!alive || current !== generation) return;
    const page = readPage(raw, after);
    error.value = '';
    if (session !== undefined && session !== page.session_id) {
      if (resetting) throw new Error('日志会话持续变化，已停止自动刷新，请手动重试。');
      resetting = true;
      entries.value = []; cursor = undefined; session = page.session_id;
      notice.value = '服务日志会话已变化，已重新读取当前进程的最近日志。'; pending = true;
      return;
    }
    resetting = false; session = page.session_id; cursor = page.next_cursor;
    if (page.dropped) notice.value = '日志窗口已更新：部分记录已超出内存保留范围，当前列表可能不连续。';
    const combined = after === undefined ? page.entries : [...entries.value, ...page.entries];
    entries.value = [...new Map(combined.map(entry => [entry.id, entry])).values()].sort((a, b) => a.id - b.id).slice(-1000);
    more = page.has_more;
  } catch (cause) {
    if (alive && current === generation) error.value = cause instanceof Error ? cause.message : '无法读取日志，请手动重试。';
  } finally {
    busy.value = false;
    if (alive && (pending || (current === generation && !paused.value && !error.value))) {
      timer = setTimeout(() => void load(), pending ? 0 : more ? 100 : 2000);
    }
  }
}
function togglePause() {
  paused.value = !paused.value; generation++; stopTimer(); pending = false;
  if (!paused.value) refresh();
}
watch([level, instance, () => props.request], () => {
  generation++; stopTimer(); entries.value = []; cursor = undefined; session = undefined; notice.value = ''; error.value = '';
  if (alive) refresh();
});
onMounted(() => { alive = true; refresh(); });
onBeforeUnmount(() => { alive = false; generation++; pending = false; stopTimer(); });
</script>
<template>
  <section class="logs-page" aria-label="运行日志">
    <div class="section-heading"><h2>运行日志</h2><span class="status-marker"><span class="status-shape" :class="error ? 'is-error' : paused ? 'is-offline' : 'is-ready'" aria-hidden="true" />{{ error ? '刷新已停止' : paused ? '已暂停' : busy ? '正在读取' : '自动刷新中' }}</span></div>
    <p class="hint">当前进程的最近运行日志，最多显示 1000 条。消息正文日志和历史归档不在此处显示。</p>
    <div class="logs-toolbar">
      <div><label :for="`${uid}-level`">最低级别</label><select :id="`${uid}-level`" v-model="level"><option v-for="item in levels" :key="item" :value="item">{{ item }}</option></select></div>
      <div v-if="instanceId === undefined" class="logs-instance"><label :for="`${uid}-instance`">插件实例</label><select :id="`${uid}-instance`" v-model="selected"><option value="">全部运行日志</option><option v-for="plugin in plugins ?? []" :key="plugin.instance_id" :value="plugin.instance_id">{{ plugin.name || plugin.plugin_id }} · {{ plugin.instance_id }}</option></select></div>
      <div class="actions"><button type="button" :aria-pressed="paused" @click="togglePause">{{ paused ? '继续刷新' : '暂停刷新' }}</button><button type="button" :disabled="busy" @click="refresh">{{ busy ? '读取中…' : '手动刷新' }}</button></div>
    </div>
    <p v-if="instanceId !== undefined" class="hint logs-scope">当前实例：<code>{{ instanceId }}</code>。仅包含明确关联到此实例的日志。</p>
    <p v-if="error" class="notice error" role="alert">{{ error }}</p>
    <p v-if="notice" class="notice" role="status">{{ notice }}</p>
    <div class="logs-view-switch" role="group" aria-label="日志显示方式"><button type="button" :aria-pressed="view === 'terminal'" @click="view = 'terminal'">实时输出</button><button type="button" :aria-pressed="view === 'structured'" @click="view = 'structured'">结构化记录</button></div>
    <TerminalOutput v-if="view === 'terminal'" :entries="entries" :session-id="session" :empty-message="busy ? '正在读取日志…' : error ? '尚未读取到日志，可手动刷新重试。' : '当前筛选暂无日志，等待新的输出。'" />
    <p v-if="view === 'structured'" class="hint logs-count">已显示 {{ entries.length }} / 1000 条 · 按时间先后排列</p>
    <ol v-if="view === 'structured' && entries.length" class="log-list" tabindex="0" aria-label="日志条目">
      <li v-for="entry in entries" :key="`${session}-${entry.id}`" class="log-entry">
        <div class="log-meta"><time :datetime="entry.timestamp">{{ entry.timestamp }}</time><strong class="log-level">{{ entry.level }}</strong><span>{{ entry.target }}</span><code v-if="entry.instance_id">{{ entry.instance_id }}</code></div>
        <p class="log-message">{{ entry.message || '（无消息文本）' }}</p>
        <details v-if="Object.keys(entry.fields).length"><summary>结构化字段（{{ Object.keys(entry.fields).length }}）</summary><dl class="log-fields"><template v-for="(value, key) in entry.fields" :key="key"><dt>{{ key }}</dt><dd>{{ value }}</dd></template></dl></details>
      </li>
    </ol>
    <p v-else-if="view === 'structured'" class="empty">{{ busy ? '正在读取日志…' : error ? '尚未读取到日志，可手动刷新重试。' : '当前筛选暂无日志。' }}</p>
  </section>
</template>
