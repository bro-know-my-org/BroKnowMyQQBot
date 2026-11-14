<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, shallowRef, useId, watch } from 'vue';
import { readLogPage } from '../api';
import type { Request } from '../api';
import TerminalOutput from './TerminalOutput.vue';
import { isMessageResult, messageDirection } from '../logPresentation';
import { useNoticeSource } from '../notifications';
import type { LogEntry, LogPage, LogQuery, LogStreamState, PluginView, SubscribeLogs } from '../types';

const props = defineProps<{ request: Request; subscribeLogs: SubscribeLogs; plugins?: PluginView[]; instanceId?: string }>();
const uid = useId(), levels = ['TRACE', 'DEBUG', 'INFO', 'WARN', 'ERROR'];
const level = ref('INFO'), selected = ref(''), paused = ref(false), busy = ref(false);
const category = ref<NonNullable<LogQuery['category']>>('runtime'), capture = ref(false);
const includeContent = computed({ get: () => category.value !== 'runtime' && capture.value, set: value => { capture.value = value; } });
const entries = shallowRef<LogEntry[]>([]), session = ref<string>(), error = ref(''), notice = ref('');
const state = ref<LogStreamState>('connecting'), view = ref<'terminal' | 'structured'>('terminal');
const notifications = useNoticeSource('日志', () => error.value || notice.value, () => error.value ? 'error' : 'info');
const instance = computed(() => props.instanceId ?? selected.value);
const status = computed(() => error.value ? '接收已停止' : paused.value ? '已暂停' : state.value === 'open' ? '实时接收中' : state.value === 'closed' ? '连接已停止，请手动重试' : state.value === 'reconnecting' ? '连接中断，重连中' : '正在连接');
let alive = false, generation = 0, cursor: number | undefined, closeStream: (() => void) | undefined;
let receivedSession: string | undefined, pendingEntries: LogEntry[] | undefined, frame: number | undefined;
function publish() {
  if (frame !== undefined) cancelAnimationFrame(frame);
  frame = undefined;
  if (pendingEntries) { entries.value = pendingEntries; pendingEntries = undefined; }
  session.value = receivedSession;
}
function stop() { generation++; closeStream?.(); closeStream = undefined; busy.value = false; publish(); }
function query(): LogQuery {
  return { level: level.value, category: category.value, include_content: includeContent.value, limit: 200, ...(instance.value ? { instance_id: instance.value } : {}),
    ...(cursor === undefined ? {} : { after: cursor, session_id: receivedSession }) };
}
function accept(page: LogPage) {
  const changed = receivedSession !== undefined && receivedSession !== page.session_id;
  const reset = changed || (cursor !== undefined && page.dropped && page.next_cursor < cursor);
  if (changed) notice.value = '服务日志会话已变化，已重新读取当前进程的最近日志。';
  else if (page.dropped) notice.value = '日志窗口已更新：部分记录已超出内存保留范围，当前列表可能不连续。';
  const rows = page.entries.map(entry => {
    if (includeContent.value || entry.content === undefined) return entry;
    const { content: _content, ...metadata } = entry; return metadata;
  });
  const added = reset || cursor === undefined ? rows : rows.filter(entry => entry.id > cursor!);
  const base = pendingEntries ?? entries.value;
  if (reset) { if (base.length || added.length) pendingEntries = added.slice(-1000); }
  else if (added.length) pendingEntries = [...base, ...added].slice(-1000);
  receivedSession = page.session_id; cursor = page.next_cursor; error.value = '';
  // One bounded pending window, even while animation frames are suspended in a hidden tab.
  if (frame === undefined && (pendingEntries || session.value !== receivedSession)) frame = requestAnimationFrame(publish);
}
function subscribe() {
  stop(); error.value = ''; state.value = 'connecting';
  const current = generation, live = () => alive && current === generation;
  try {
    closeStream = props.subscribeLogs(query(), {
      onPage: page => { if (live()) { accept(page); state.value = 'open'; } },
      onState: value => { if (live()) state.value = value; },
      onError: cause => { if (live()) { stop(); error.value = cause.message; } },
    });
  } catch (cause) { if (live()) { stop(); error.value = cause instanceof Error ? cause.message : '无法连接日志，请手动重试。'; } }
}
async function refresh() {
  if (!alive || busy.value) return;
  if (!paused.value) { subscribe(); return; }
  stop(); error.value = ''; busy.value = true;
  const current = generation, after = cursor, previousSession = receivedSession;
  const body = includeContent.value;
  const params = new URLSearchParams({ level: level.value, limit: '200', category: category.value, include_content: String(body) });
  if (instance.value) params.set('instance_id', instance.value);
  if (after !== undefined) params.set('after', String(after));
  try {
    const raw = await props.request<unknown>(`/api/logs?${params}`);
    if (!alive || current !== generation) return;
    let page = readLogPage(raw, after, previousSession, body);
    if (previousSession !== undefined && previousSession !== page.session_id && after !== undefined) {
      params.delete('after');
      const recent = await props.request<unknown>(`/api/logs?${params}`);
      if (!alive || current !== generation) return;
      const reset = readLogPage(recent, undefined, undefined, body);
      if (reset.session_id !== page.session_id) throw new Error('日志会话持续变化，请稍后手动重试。');
      page = reset;
    }
    accept(page);
  } catch (cause) {
    if (alive && current === generation) error.value = cause instanceof Error ? cause.message : '无法读取日志，请手动重试。';
  } finally { if (current === generation) busy.value = false; }
}
function togglePause() {
  paused.value = !paused.value; stop();
  if (!paused.value) subscribe();
}
watch([level, instance, category, includeContent, () => props.request, () => props.subscribeLogs], () => {
  if (category.value === 'runtime') capture.value = false;
  stop(); if (entries.value.length) entries.value = []; cursor = undefined; session.value = undefined; receivedSession = undefined; notice.value = ''; error.value = '';
  if (alive && !paused.value) subscribe();
});
onMounted(() => { alive = true; subscribe(); });
onBeforeUnmount(() => { alive = false; stop(); });
</script>
<template>
  <section class="logs-page" aria-label="运行日志">
    <div class="section-heading"><h2>运行日志</h2><span class="status-marker"><span class="status-shape" :class="error ? 'is-error' : paused || state === 'closed' ? 'is-offline' : 'is-ready'" aria-hidden="true" />{{ status }}</span></div>
    <p class="hint">查看当前进程最近 1000 条日志。消息正文默认关闭，开启前的正文无法补取。</p>
    <div class="logs-toolbar">
      <div><label :for="`${uid}-level`">最低级别</label><select :id="`${uid}-level`" v-model="level"><option v-for="item in levels" :key="item" :value="item">{{ item }}</option></select></div>
      <div><label :for="`${uid}-category`">日志类型</label><select :id="`${uid}-category`" v-model="category"><option value="runtime">运行</option><option value="message">消息</option><option value="all">全部</option></select></div>
      <div class="logs-capture"><label><input v-model="includeContent" type="checkbox" :disabled="category === 'runtime'" :aria-describedby="category === 'runtime' ? undefined : `${uid}-privacy`">采集并显示正文</label></div>
      <div v-if="instanceId === undefined" class="logs-instance"><label :for="`${uid}-instance`">插件实例</label><select :id="`${uid}-instance`" v-model="selected"><option value="">全部实例与全局日志</option><option v-for="plugin in plugins ?? []" :key="plugin.instance_id" :value="plugin.instance_id">{{ plugin.name || plugin.plugin_id }} · {{ plugin.instance_id }}</option></select></div>
      <div class="actions"><button type="button" :aria-pressed="paused" @click="togglePause">{{ paused ? '继续接收' : '暂停接收' }}</button><button type="button" :disabled="busy" @click="refresh">{{ busy ? '读取中…' : paused ? '读取最新日志' : '重新连接' }}</button></div>
    </div>
    <p v-if="category !== 'runtime'" :id="`${uid}-privacy`" class="hint">正文仅在开启接收时采集，此设置不会保存。暂停期间的正文可能无法补取；切换类型或正文开关会重新读取最近记录。</p>
    <p v-if="instanceId !== undefined" class="hint logs-scope">当前实例：<code>{{ instanceId }}</code>。仅包含明确关联到此实例的日志；入站消息通常属于全局日志。</p>
    <p v-if="error && !notifications" class="notice error" role="alert">{{ error }}</p>
    <p v-if="notice && !notifications" class="notice" role="status">{{ notice }}</p>
    <div class="logs-view-switch" role="group" aria-label="日志显示方式"><button type="button" :aria-pressed="view === 'terminal'" @click="view = 'terminal'">实时输出</button><button type="button" :aria-pressed="view === 'structured'" @click="view = 'structured'">结构化记录</button></div>
    <TerminalOutput v-if="view === 'terminal'" :entries="entries" :plugins="plugins" :session-id="session" :include-content="includeContent" :empty-message="busy ? '正在读取日志…' : error ? '尚未读取到日志，可手动重试。' : '当前筛选暂无日志，等待新的输出。'" />
    <p v-if="view === 'structured'" class="hint logs-count">已显示 {{ entries.length }} / 1000 条 · 按时间先后排列</p>
    <ol v-if="view === 'structured' && entries.length" class="log-list" tabindex="0" aria-label="日志条目">
      <li v-for="entry in entries" :key="`${session}-${entry.id}`" class="log-entry">
        <div class="log-meta"><time :datetime="entry.timestamp">{{ entry.timestamp }}</time><strong class="log-level">{{ entry.level }}</strong><span>{{ entry.target }}</span><code v-if="entry.instance_id">{{ entry.instance_id }}</code></div>
        <p class="log-message">{{ entry.message || '（无消息文本）' }}</p>
        <p v-if="entry.category === 'message'" class="log-message">{{ messageDirection(entry) }}<span v-if="includeContent && (!isMessageResult(entry) || entry.content !== undefined)"> · {{ entry.content ?? '正文未采集' }}</span></p>
        <details v-if="Object.keys(entry.fields).length"><summary>结构化字段（{{ Object.keys(entry.fields).length }}）</summary><dl class="log-fields"><template v-for="(value, key) in entry.fields" :key="key"><dt>{{ key }}</dt><dd>{{ value }}</dd></template></dl></details>
      </li>
    </ol>
    <p v-else-if="view === 'structured'" class="empty">{{ busy ? '正在读取日志…' : error ? '尚未读取到日志，可手动重试。' : '当前筛选暂无日志。' }}</p>
  </section>
</template>
