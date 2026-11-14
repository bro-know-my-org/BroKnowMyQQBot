<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue';
import type { Request, RestoreUpload, RestoreDiscard } from '../api';
import { readArtifacts, readRestorePlan, type BackupArtifact, type RestorePlan } from '../backupRestore';
import { useNoticeSource } from '../notifications';
const props = defineProps<{ request: Request; upload: RestoreUpload; discard: RestoreDiscard; bootId: string; locked?: boolean }>();
const emit = defineEmits<{ dirty: [value: boolean]; busy: [value: boolean]; restore: [id: string] }>();
const artifacts = ref<BackupArtifact[]>([]), plan = ref<RestorePlan>(), busy = ref(false), uploading = ref(false), stale = ref(false), message = ref(''), failed = ref(false);
const fileInput = ref<HTMLInputElement>();
let disposed = false, owner = new AbortController(), expiry: ReturnType<typeof setTimeout> | undefined, expiresAt = 0;
const blocked = computed(() => busy.value || props.locked);
const ready = computed(() => plan.value && !stale.value && plan.value.boot_id === props.bootId);
const notices = useNoticeSource('备份与恢复', () => message.value, () => { if (busy.value) return 'progress'; if (failed.value) return 'error'; return 'success'; });
watch(busy, value => emit('busy', value), { flush: 'sync' });
watch(() => !!plan.value, value => emit('dirty', value), { flush: 'sync' });
onBeforeUnmount(() => { disposed = true; owner.abort(); clearTimeout(expiry); });
function feedback(text: string, error = false) { message.value = text; failed.value = error; }
function begin(text: string) { owner = new AbortController(); busy.value = true; feedback(text); return owner.signal; }
async function list(signal: AbortSignal) { const items = readArtifacts(await props.request('/api/backups', undefined, signal)); if (!disposed) artifacts.value = items; }
async function refresh() {
  if (blocked.value) return; const signal = begin('正在读取备份列表…');
  try { await list(signal); if (!disposed) feedback('已读取服务器上的临时备份；最多保留 4 份，24 小时后过期。'); }
  catch (error) { if (!disposed) feedback((error as Error).message, true); }
  finally { if (!disposed) busy.value = false; }
}
async function create() {
  if (blocked.value || !window.confirm('确认创建逻辑备份？包含非凭据 Bot 设置、已安装插件包/配置与私有数据，可能包含业务敏感内容。请妥善保管下载的归档。不包含凭据、环境、日志或执行历史/队列。')) return;
  const signal = begin('正在创建一致的逻辑备份…');
  try {
    const artifact = readArtifacts([await props.request('/api/backups', { confirm_sensitive_plugin_data: true }, signal)])[0]!;
    if (!disposed) { artifacts.value = [artifact, ...artifacts.value.filter(a => a.id !== artifact.id)].slice(0, 4); feedback('备份已创建，请下载并妥善保管敏感归档。'); }
  } catch (error) {
    if (!disposed) { feedback((error as Error).message, true); try { await list(signal); } catch { /* Keep existing list and never replay creation. */ } }
  } finally { if (!disposed) busy.value = false; }
}
function download(event: MouseEvent) {
  if (blocked.value || !window.confirm('确认下载敏感逻辑归档？插件配置和私有数据可能包含业务敏感内容；不包含系统凭据和执行历史。')) event.preventDefault();
}
async function choose(event: Event) {
  const input = event.target as HTMLInputElement, file = input.files?.[0]; input.value = '';
  if (!file || blocked.value || plan.value) return;
  const signal = begin('正在上传并完整检查归档；不会修改配置或插件数据…'); uploading.value = true;
  try {
    const inspected = readRestorePlan(await props.upload(file, signal));
    if (!disposed && !signal.aborted) { plan.value = inspected; stale.value = false; expiresAt = performance.now() + inspected.expires_in_seconds * 1000; clearTimeout(expiry); expiry = setTimeout(() => { stale.value = true; }, inspected.expires_in_seconds * 1000); feedback('检查完成。请核对影响范围；尚未恢复任何数据。'); }
  } catch (error) { if (!disposed) feedback(signal.aborted ? '已取消上传检查。' : (error as Error).message, !signal.aborted); }
  finally { if (!disposed) { uploading.value = false; busy.value = false; if (signal.aborted) { feedback('已取消上传检查。'); await nextTick(); if (!disposed) fileInput.value?.focus(); } } }
}
async function cancelPlan() {
  if (blocked.value || !plan.value) return; const id = plan.value.operation_id; const signal = begin('正在取消检查计划…');
  try { await props.discard(id, signal); if (!disposed) feedback('已取消恢复计划，未应用归档。'); }
  catch { if (!disposed) feedback('已关闭本页计划；服务器检查票据会自动过期。未提交恢复。', true); }
  finally { if (!disposed) { plan.value = undefined; clearTimeout(expiry); busy.value = false; await nextTick(); if (!disposed) fileInput.value?.focus(); } }
}
function restore() {
  if (blocked.value || !ready.value || !plan.value) return;
  if (performance.now() >= expiresAt) { stale.value = true; return; }
  if (!window.confirm(`确认离线恢复 ${plan.value.affected_instances.length} 个实例的数据和 ${plan.value.changed_settings.length} 项设置？将优雅停止、保留本地恢复前快照并重启。托管插件包均按未签名处理并保持禁用，不增加授权；内置/文件声明配置不被覆盖。当前宿主认证、监听、路径与凭据保持不变。历史消息、动作账本、投递和调度队列不从归档导入，也不会重放。`)) return;
  const id = plan.value.operation_id; plan.value = undefined; clearTimeout(expiry); emit('restore', id);
}
onMounted(refresh);
</script>
<template>
  <section class="card system-settings backup-restore" aria-labelledby="backup-restore-title">
    <h2 id="backup-restore-title">备份与恢复</h2>
    <p>逻辑备份范围：文件中保存的非凭据 Bot 设置、已安装插件的配置与固定哈希包，以及可验证身份的已安装/声明实例私有数据。插件业务数据可能敏感，请妥善保管归档。</p>
    <p class="hint">不包含 secrets.env、进程环境、宿主认证与文件路径、日志、动作账本、投递、调度队列或健康记录。恢复保留当前宿主设置与声明配置，不自动重放历史任务，也不会开启正文采集。孤立私有数据无法验证身份时会拒绝导出。</p>
    <div class="actions"><button id="create-backup" type="button" :disabled="blocked" @click="create">创建逻辑备份</button><button id="refresh-backups" type="button" :disabled="blocked" @click="refresh">读取备份列表</button></div>
    <ul v-if="artifacts.length" class="backup-artifacts"><li v-for="artifact in artifacts" :key="artifact.id"><span>{{ new Date(artifact.created_at_ms).toLocaleString() }} · {{ (artifact.bytes / 1024 / 1024).toFixed(2) }} MiB</span><a class="button-link" :href="`/api/backups/${artifact.id}`" :download="`bkmqb-${artifact.id}.bkm-backup`" :aria-disabled="blocked || undefined" @click="download">下载敏感归档</a></li></ul>
    <p v-else class="muted">暂无已读取的备份。服务器最多保留 4 份、24 小时；请下载后自行保管。</p>
    <label for="restore-archive">检查恢复归档（.bkm-backup，最多 128 MiB）</label><input id="restore-archive" ref="fileInput" type="file" accept=".bkm-backup" :disabled="blocked || !!plan" @change="choose">
    <button v-if="uploading" id="cancel-restore-upload" type="button" @click="owner.abort()">取消上传检查</button>
    <section v-if="plan" aria-labelledby="restore-plan-title">
      <h3 id="restore-plan-title">恢复检查计划 · 尚未应用</h3><p>格式 {{ plan.manifest.format_version }} · 来源时间 {{ new Date(plan.manifest.created_at_ms).toLocaleString() }} · {{ plan.manifest.package_count }} 个包 · {{ plan.manifest.state_rows }} 条私有数据</p>
      <p>需要重启。检查票据最长 15 分钟有效；配置、声明或安装记录改变后必须重新检查。恢复前自动保留的私有回滚归档留在服务器。</p>
      <ul><li v-for="plugin in plan.plugins" :key="plugin.instance_id"><strong>{{ plugin.instance_id }}</strong> · {{ plugin.plugin_id }} {{ plugin.version }}<p>{{ plugin.declaration_owned ? '内置/文件声明：仅恢复私有数据，当前声明配置与授权不变。' : '未签名托管包：恢复后保持禁用，检查配置与权限后再手动启用。' }}</p><p>包请求能力：{{ plugin.requested.join('、') || '无' }}；当前可保留授权：{{ plugin.retained_grants.join('、') || '无' }}。本次不会新增授权。</p></li></ul>
      <p>将修改的设置：{{ plan.changed_settings.join('、') || '无' }}</p><details><summary>核对设置变更前后</summary><h4>当前文件设置</h4><pre>{{ JSON.stringify(plan.settings_before, null, 2) }}</pre><h4>恢复后的文件设置</h4><pre>{{ JSON.stringify(plan.settings_after, null, 2) }}</pre></details>
      <p v-if="!ready" role="alert">检查已过期或进程已变化，请取消计划后重新上传检查。</p>
      <div class="actions"><button id="confirm-restore" class="primary" type="button" :disabled="blocked || !ready" @click="restore">确认离线恢复并重启</button><button id="cancel-restore-plan" type="button" :disabled="blocked" @click="cancelPlan">取消计划</button></div>
    </section>
    <p v-if="message && !notices" class="hint" :role="failed ? 'alert' : 'status'">{{ message }}</p>
  </section>
</template>
<style scoped>
.backup-restore { overflow-wrap: anywhere; }
.backup-restore pre { max-width: 100%; overflow: auto; white-space: pre-wrap; }
.backup-artifacts { padding: 0; list-style: none; }
.backup-artifacts li { display: flex; align-items: center; justify-content: space-between; flex-wrap: wrap; gap: 12px; margin-block: 12px; }
.backup-restore input[type=file] { min-height: 44px; max-width: 100%; }
.backup-restore h3 { margin-top: 24px; }
</style>
