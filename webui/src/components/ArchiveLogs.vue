<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref, shallowRef, useId, watch } from 'vue';
import { ApiError, type Request } from '../api';
import { readArchiveInventory, readArchivePage, type ArchiveFile, type ArchivePage } from '../archiveLogs';
import type { PluginView } from '../types';
import TerminalOutput from './TerminalOutput.vue';
const props = defineProps<{ request: Request; plugins?: PluginView[] }>();
const uid = useId(), files = shallowRef<ArchiveFile[]>([]), selected = ref(''), enabled = ref(true), truncated = ref(false);
const busy = ref(false), loading = ref(false), error = ref(''), changed = ref(false), data = shallowRef<ArchivePage>();
const previous = ref<(string | undefined)[]>([]), current = ref<string>();
let alive = true, generation = 0;
async function refreshFiles() {
  const epoch = ++generation; busy.value = true; loading.value = false; error.value = ''; changed.value = false;
  data.value = undefined; selected.value = ''; previous.value = []; current.value = undefined;
  try {
    const inventory = readArchiveInventory(await props.request<unknown>('/api/logs/archives'));
    if (!alive || epoch !== generation) return;
    files.value = inventory.files; enabled.value = inventory.enabled; truncated.value = inventory.truncated;
  } catch (cause) { if (alive && epoch === generation) { files.value = []; error.value = (cause as Error).message; } }
  finally { if (alive && epoch === generation) busy.value = false; }
}
async function load(cursor?: string, direction: 'first' | 'next' | 'previous' = 'first') {
  const selector = selected.value;
  if (!selector || changed.value) return;
  const epoch = ++generation; loading.value = true; error.value = '';
  const params = new URLSearchParams({ selector, limit: '200' }); if (cursor) params.set('cursor', cursor);
  try {
    const raw = await props.request<unknown>(`/api/logs/archives/page?${params}`);
    if (!alive || epoch !== generation) return;
    const page = readArchivePage(raw, selector);
    if (page.next_cursor !== null && page.next_cursor === cursor) throw new Error('历史日志游标未前进，请刷新文件列表。');
    if (direction === 'next') previous.value.push(current.value);
    if (direction === 'previous') previous.value.pop();
    current.value = cursor; data.value = page;
  } catch (cause) {
    if (alive && epoch === generation) {
      changed.value = cause instanceof ApiError && cause.status === 409;
      error.value = changed.value ? '归档已变化或轮转，请刷新文件列表后重试。' : (cause as Error).message;
    }
  } finally { if (alive && epoch === generation) loading.value = false; }
}
function choose() { generation++; loading.value = false; data.value = undefined; previous.value = []; current.value = undefined; changed.value = false; error.value = ''; void load(); }
function next() { if (data.value?.next_cursor && previous.value.length < 511) void load(data.value.next_cursor, 'next'); }
function back() { if (previous.value.length) void load(previous.value.at(-1), 'previous'); }
watch(() => props.request, refreshFiles);
onMounted(refreshFiles);
onBeforeUnmount(() => { alive = false; generation++; });
</script>
<template>
  <section class="archive-logs" aria-label="历史运行日志">
    <div class="section-heading"><h2>历史运行日志</h2><button type="button" :disabled="busy || loading" @click="refreshFiles">{{ busy ? '读取文件列表…' : '刷新文件列表' }}</button></div>
    <p class="hint">仅查询已保存的运行日志，不含消息正文档案。查看历史时停止实时接收；原始模式仍只展示安全归一化字段，源 JSON 不开放。</p>
    <p v-if="error" class="notice error" role="alert">{{ error }}</p>
    <p v-if="!busy && !enabled" class="empty">文件日志未启用，历史运行日志不可用。</p>
    <p v-else-if="!busy && !files.length && !error" class="empty">暂无运行日志文件。</p>
    <p v-if="truncated" class="hint" role="status">文件清单已达到枚举或数量上限，当前仅显示部分文件。</p>
    <div v-if="files.length" class="archive-file-control"><label :for="`${uid}-archive`">运行日志文件（按文件名倒序）</label><select :id="`${uid}-archive`" v-model="selected" :disabled="busy || loading" @change="choose"><option value="">请选择文件与日期</option><option v-for="file in files" :key="file.selector" :value="file.selector" :disabled="!file.available">{{ file.month }} / {{ file.name }} · {{ !file.available ? '超出读取上限' : file.compressed ? '压缩归档' : '未压缩，可能仍在写入' }}</option></select></div>
    <p v-if="loading" class="hint" role="status">正在读取历史页…</p>
    <template v-if="data">
      <div class="actions archive-pagination" role="group" aria-label="历史日志分页"><button type="button" :disabled="loading || changed || !previous.length" @click="back">上一页</button><span>第 {{ previous.length + 1 }} 页</span><button type="button" :disabled="loading || changed || !data.next_cursor || previous.length >= 511" @click="next">下一页</button></div>
      <p class="hint">本页显示 {{ data.entries.length }} / 200 条 · 扫描 {{ data.scanned }} 行 · 跳过 {{ data.skipped }} 行（格式无效、过长或无法安全归一化）。{{ !data.next_cursor && !data.limited ? '已到文件末尾。' : '' }}</p>
      <p v-if="data.limited" class="notice" role="status">已达到累计扫描字节或行数上限，后续内容未读取。</p>
      <TerminalOutput :entries="data.entries" :session-id="`${selected}-${current ?? 'first'}`" :plugins="plugins" historical empty-message="本页没有可安全展示的运行记录，可继续翻页或选择其他文件。" />
    </template>
    <p v-else-if="selected && !loading && !error" class="empty">尚未读取到历史页，请重新选择文件。</p>
  </section>
</template>
<style scoped>
.archive-file-control { min-width: 0; margin: 1rem 0; }
.archive-file-control select { width: 100%; max-width: 100%; }
.archive-pagination { margin: 1rem 0; }
</style>
