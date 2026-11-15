<script setup lang="ts">
import { computed, inject, nextTick, onBeforeUnmount, onMounted, ref, shallowRef, watch } from 'vue';
import { RouterLink, useRoute } from 'vue-router';
import { ApiError, type Request } from '../api';
import { readBundledCatalogue, type BundledCatalogue } from '../bundledPlugins';
import { runtimeNames, type Config } from '../types';
import { useNoticeSource } from '../notifications';
import { managementStatusKey } from '../status';
import SchemaEditor from './SchemaEditor.vue';
const props = defineProps<{ request: Request; locked?: boolean }>();
const emit = defineEmits<{ dirty: [value: boolean]; busy: [value: boolean]; saved: [] }>();
const route = useRoute(), catalogue = shallowRef<BundledCatalogue>(), busy = ref(false), dirty = ref(false), blocked = ref(false);
const shared = inject(managementStatusKey, undefined), conflict = ref(false), feedbackBusy = ref(false);
const writesBlocked = computed(() => blocked.value || conflict.value);
const bundledRevision = computed(() => shared?.snapshot.value?.bundled_revision ?? null);
const message = ref(''), failed = ref(false), base = shallowRef<Config>({}), editorSchema = shallowRef<Config | null>(null), editorEpoch = ref(0);
const editor = ref<InstanceType<typeof SchemaEditor>>(), detailHeading = ref<HTMLElement>();
const selectedId = computed(() => typeof route.query.instance_id === 'string' ? route.query.instance_id : '');
const plugins = computed(() => catalogue.value?.plugins.map(p => {
  const summary = shared?.snapshot.value?.plugins.find(s => s.instance_id === p.instance_id && s.plugin_id === p.plugin_id);
  if (!summary) return p;
  // Runtime updates never replace saved configuration. Pending flags must describe the same declaration.
  const pending = bundledRevision.value === catalogue.value?.revision ? { pending: summary.pending, restart_required: summary.restart_required } : {};
  return { ...p, ...pending, loaded: summary.loaded_version !== null, runtime_state: summary.runtime_state };
}) ?? []);
const plugin = computed(() => plugins.value.find(p => p.instance_id === selectedId.value));
const notices = useNoticeSource('内置插件', () => message.value, () => feedbackBusy.value ? 'progress' : failed.value ? 'error' : 'success');
let live = true, readAbortController: AbortController | undefined, writeAbortController: AbortController | undefined;
let revisionEpoch = 0, syncPending = false;
let retryTimer: ReturnType<typeof setTimeout> | undefined, retryDelay = 1000, autoReadFailed = false;
function clearRetry() { clearTimeout(retryTimer); retryTimer = undefined; retryDelay = 1000; }
watch(dirty, value => emit('dirty', value), { flush: 'sync' });
watch(busy, value => emit('busy', value), { flush: 'sync' });
function resetEditor() { base.value = plugin.value?.config ?? {}; editorSchema.value = plugin.value?.schema ?? null; editorEpoch.value++; dirty.value = false; }
watch(selectedId, async (_id, previous) => {
  resetEditor(); await nextTick(); if (!live) return;
  if (selectedId.value) detailHeading.value?.focus();
  else Array.from(document.querySelectorAll<HTMLElement>('[data-bundled-instance]')).find(el => el.dataset.bundledInstance === previous)?.focus();
});
function syncCatalogue() {
  if (live && syncPending && !retryTimer && !busy.value && !props.locked && !document.hidden) void reload(true, true);
}
watch(bundledRevision, revision => {
  if (!revision) return;
  clearRetry();
  revisionEpoch++;
  if (revision === catalogue.value?.revision) { syncCatalogue(); return; }
  syncPending = true;
  // A successful save may emit its own SSE before its response. Resolve that after the write.
  if (dirty.value && !writeAbortController) conflict.value = true;
  syncCatalogue();
}, { immediate: true, flush: 'sync' });
async function reload(preserve = false, automatic = false) {
  if (busy.value || props.locked) return;
  if (!preserve && dirty.value && !window.confirm('有未保存更改，是否放弃编辑并刷新内置插件？')) return;
  const retrySynchronization = automatic || syncPending;
  if (!automatic) clearRetry();
  busy.value = true;
  if (!automatic) { feedbackBusy.value = true; failed.value = false; message.value = '正在读取内置插件…'; }
  const controller = new AbortController(); readAbortController = controller;
  let observedEpoch = revisionEpoch;
  try {
    while (live && !controller.signal.aborted) {
      if (props.locked) { syncPending = true; return; }
      syncPending = false;
      observedEpoch = revisionEpoch;
      const data = readBundledCatalogue(await props.request('/api/plugins/bundled', undefined, controller.signal));
      if (!live || controller.signal.aborted) return;
      // A change received during this read needs a fresh request; the delayed response is obsolete.
      if (observedEpoch !== revisionEpoch) { syncPending = true; continue; }
      if (preserve && dirty.value && !data.plugins.some(p => p.instance_id === selectedId.value)) throw new Error('最新目录缺少正在编辑的插件，草稿仍保留，请先复制配置。');
      if (automatic) { if (dirty.value && catalogue.value?.revision !== data.revision) conflict.value = true; }
      else { blocked.value = false; conflict.value = false; }
      catalogue.value = data;
      if (!preserve || !dirty.value) resetEditor();
      clearRetry();
      if (automatic && autoReadFailed) { autoReadFailed = false; failed.value = false; message.value = ''; }
      if (!automatic) message.value = preserve && dirty.value ? '已重新读取状态；编辑草稿仍保留，请核对后再保存。' : '内置插件状态已更新。';
      return;
    }
  } catch (e) {
    if (live && !controller.signal.aborted) {
      if (observedEpoch === revisionEpoch) {
        failed.value = true; message.value = (e as Error).message;
        if (retrySynchronization) {
          autoReadFailed = true; syncPending = true;
          retryTimer = setTimeout(() => { retryTimer = undefined; syncCatalogue(); }, retryDelay);
          retryDelay = Math.min(retryDelay * 2, 30000);
        }
      }
      else if (!automatic) message.value = '';
    }
  }
  finally { if (live) { busy.value = false; feedbackBusy.value = false; } if (readAbortController === controller) readAbortController = undefined; syncCatalogue(); }
}
async function save(kind: 'config' | 'enabled') {
  const p = plugin.value;
  if (busy.value || props.locked || writesBlocked.value || !p || !catalogue.value || (kind === 'config' ? !dirty.value : dirty.value)) return;
  let change: { config: Config } | { enabled: boolean };
  try { change = kind === 'config' ? { config: editor.value!.read() } : { enabled: !p.enabled }; }
  catch (e) { failed.value = true; message.value = (e as Error).message; return; }
  const action = kind === 'config' ? '保存配置' : p.enabled ? '保存为停用' : '保存为启用';
  const authority = p.key === 'admin' ? 'admin 的 owners 配置与启停会影响 Bot 的 owner/admin 权限；停用后这些权限来源将不可用。' : '';
  if (!window.confirm(`确认将“${p.name}”（${p.instance_id}）${action}？仅保存设置，下次重启 Bot 后生效；当前运行状态不会立即改变，私有数据保留。${authority}`)) return;
  busy.value = true; feedbackBusy.value = true; failed.value = false; message.value = '正在保存内置插件设置…';
  const controller = new AbortController(); writeAbortController = controller;
  try {
    const data = readBundledCatalogue(await props.request('/api/plugins/bundled', { name: p.key, expected_revision: catalogue.value.revision, confirm: true, ...change }, controller.signal));
    if (!live || controller.signal.aborted) return;
    if (!data.plugins.some(next => next.instance_id === p.instance_id && next.key === p.key)) throw new Error('保存响应缺少目标插件，请重新读取确认状态。');
    catalogue.value = data; resetEditor(); message.value = '设置已保存，下次重启 Bot 后生效；私有数据保留。当前运行状态以目录快照为准。'; emit('saved');
  } catch (e) {
    if (!live || controller.signal.aborted) return;
    // A lost response can already have committed. Only an explicit successful read reopens writes.
    if (!(e instanceof ApiError) || e.status === 0 || e.status === 408 || e.status === 409 || e.status >= 500) blocked.value = true;
    failed.value = true; message.value = (e as Error).message;
  } finally { if (live) { busy.value = false; feedbackBusy.value = false; } if (writeAbortController === controller) writeAbortController = undefined; syncCatalogue(); }
}
onMounted(() => { document.addEventListener('visibilitychange', syncCatalogue); void reload(); });
watch(() => props.locked, locked => { if (!locked) { if (!catalogue.value) void reload(); else syncCatalogue(); } });
onBeforeUnmount(() => { live = false; clearRetry(); document.removeEventListener('visibilitychange', syncCatalogue); readAbortController?.abort(); writeAbortController?.abort(); emit('dirty', false); emit('busy', false); });
</script>
<template>
  <div class="page-heading"><div class="page-heading-copy"><span class="eyebrow">插件 / BUNDLED</span><h1 tabindex="-1">内置插件</h1><p class="muted">查看随 Bot 提供的全部可用插件，包括尚未启用的插件。设置仅在下次重启后生效。</p></div><div class="actions"><RouterLink :to="{ name: 'plugins' }" class="button-link">已安装插件</RouterLink><button id="refresh-bundled" type="button" :disabled="busy || locked" @click="reload(writesBlocked)">{{ writesBlocked ? '重新读取（保留草稿）' : '刷新状态' }}</button></div></div>
  <p v-if="message && !notices" :role="failed ? 'alert' : 'status'" class="hint">{{ message }}</p>
  <p v-if="blocked" role="status" class="notice">保存发生冲突或结果未知，已阻止重复写入。编辑草稿仍保留，请显式重新读取并核对状态。</p>
  <p v-else-if="conflict" role="status" class="notice">已保存设置发生变化，编辑草稿仍保留。请显式重新读取并核对状态后再保存。</p>
  <section v-if="!catalogue" class="panel empty-state"><p>{{ busy ? '正在读取内置插件…' : '尚未读取到目录，请刷新重试。' }}</p></section>
  <template v-else-if="!selectedId">
    <section v-for="p in plugins" :key="p.key" class="card">
      <h2>{{ p.name }}</h2><p>{{ p.description }}</p><p class="hint">{{ p.version }}{{ p.diagnostic ? ' · 诊断插件' : '' }} · 已保存：{{ p.enabled ? '启用' : '停用' }} · 当前：{{ runtimeNames[p.runtime_state] }}{{ p.loaded ? '（本进程已注册）' : '（本进程未注册）' }}</p>
      <p v-if="p.pending || p.restart_required" class="pending-marker">设置待重启生效</p><RouterLink :to="{ name: 'bundled-plugins', query: { instance_id: p.instance_id } }" :data-bundled-instance="p.instance_id" class="button-link">管理 {{ p.name }}</RouterLink>
    </section><p v-if="!catalogue.plugins.length" class="panel empty-state">此构建没有可用内置插件。</p>
  </template>
  <article v-else-if="plugin" class="detail" aria-label="内置插件详情">
    <div class="detail-header"><div class="detail-title"><RouterLink :to="{ name: 'bundled-plugins' }" class="text-link">← 内置插件目录</RouterLink><h2 ref="detailHeading" tabindex="-1">{{ plugin.name }}</h2><p>{{ plugin.description }}</p></div><button id="toggle-bundled" type="button" :disabled="busy || locked || writesBlocked || dirty" @click="save('enabled')">{{ plugin.enabled ? '保存为停用' : '保存为启用' }}</button></div>
    <div class="detail-body">
      <div class="status-grid"><div class="status-cell"><span class="muted">已保存设置</span><strong>{{ plugin.enabled ? '已启用' : '已停用' }}</strong></div><div class="status-cell"><span class="muted">当前进程</span><strong>{{ runtimeNames[plugin.runtime_state] }}</strong><span>{{ plugin.loaded ? '已注册' : '未注册' }}</span></div></div>
      <p v-if="plugin.pending || plugin.restart_required" class="notice">设置待重启生效；保存设置不会加载、停止或重新初始化当前实例。</p>
      <p v-if="plugin.key === 'admin'" class="notice">admin 的 owners 配置与启停影响 Bot 的 owner/admin 权限。启用前必须配置 owners；停用后该权限来源不可用。所有更改在下次重启后生效。</p>
      <section class="detail-section"><h2>插件信息与权限</h2><dl><dt>插件 ID</dt><dd>{{ plugin.plugin_id }}</dd><dt>实例 ID</dt><dd>{{ plugin.instance_id }}</dd><dt>版本</dt><dd>{{ plugin.version }}</dd><dt>类型</dt><dd>{{ plugin.diagnostic ? '诊断插件' : '正式内置插件' }}</dd><dt>请求权限</dt><dd>{{ plugin.requested_permissions.join('、') || '无' }}</dd></dl></section>
      <section class="detail-section"><h2>插件配置</h2><p class="hint">保存配置或启停均需重启 Bot，私有数据会保留。</p>
        <form id="bundled-config" @submit.prevent="save('config')"><SchemaEditor ref="editor" :key="`${selectedId}:${editorEpoch}`" :config="base" :schema="editorSchema" :disabled="busy || locked" @dirty="dirty = $event" /><p class="hint">{{ dirty ? '有未保存更改。' : '当前没有未保存更改。' }}</p><button id="save-bundled" class="primary" :disabled="busy || locked || writesBlocked || !dirty">保存配置（重启生效）</button></form>
      </section>
    </div>
  </article>
  <section v-else class="panel empty-state"><p>目录中没有此内置插件。</p><RouterLink :to="{ name: 'bundled-plugins' }" class="button-link">返回内置插件目录</RouterLink></section>
</template>
