<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, ref, shallowRef, watch } from 'vue';
import { RouterLink } from 'vue-router';
import type { Request, Upload } from '../api';
import type { InstallResult, LocalInspection, PluginView } from '../types';
import { MAX_LOCAL_PACKAGE_BYTES, readLocalInstall } from '../pluginPackages';
import { pluginRoute } from '../navigation';
import '../pluginPackages.css';
const props = defineProps<{ request: Request; upload: Upload; plugins: PluginView[]; locked: boolean }>();
const emit = defineEmits<{ dirty: [value: boolean]; busy: [value: boolean]; installed: []; uncertain: [] }>();
const file = shallowRef<File>(), inspection = shallowRef<LocalInspection>(), target = shallowRef<PluginView>();
const instance = ref(''), grants = ref<string[]>([]), unsigned = ref(false), working = ref(false), committing = ref(false), stale = ref(false);
const error = ref(''), result = shallowRef<InstallResult>(), heading = ref<HTMLElement>(), action = ref<HTMLButtonElement>();
const dirty = computed(() => !!file.value && !result.value);
watch(dirty, value => emit('dirty', value), { flush: 'sync' });
const current = computed(() => props.plugins.find(p => p.instance_id === instance.value));
const validId = computed(() => instance.value.length > 0 && instance.value.length <= 192 && /^[A-Za-z0-9._/-]+$/.test(instance.value) && instance.value.split('/').every(p => p && p !== '.' && p !== '..'));
const targetError = computed(() => current.value && (!current.value.editable || current.value.plugin_id !== inspection.value?.plugin_id) ? '此实例属于只读声明或不同插件，请选择其他实例 ID。' : '');
const preserved = computed(() => inspection.value?.requested_permissions.filter(p => target.value?.granted_permissions.includes(p)) ?? []);
watch(() => props.plugins, () => { if (inspection.value && !result.value && current.value?.updated_at_ms !== target.value?.updated_at_ms) stale.value = true; });
let generation = 0, alive = true, expires = 0;
let controller = new AbortController();
function setBusy(value: boolean) { working.value = value; emit('busy', value); }
async function discard(selection?: string) {
  if (!selection) return;
  try { await props.request('/api/plugins/local/discard', { selection }); } catch { /* Expiry also bounds abandoned selections. */ }
}
onBeforeUnmount(() => { alive = false; generation++; controller.abort(); if (!committing.value) void discard(inspection.value?.selection); emit('dirty', false); emit('busy', false); });
function chooseTarget() {
  target.value = current.value ? { ...current.value, granted_permissions: [...current.value.granted_permissions] } : undefined;
  grants.value = []; unsigned.value = false;
}
async function select(event: Event) {
  if (working.value || props.locked) return;
  const input = event.target as HTMLInputElement, selected = input.files?.[0];
  input.value = ''; // Allow explicitly selecting the same file again after an error or expiry.
  const oldSelection = inspection.value?.selection, own = ++generation;
  controller.abort(); controller = new AbortController();
  file.value = selected; inspection.value = undefined; result.value = undefined; instance.value = ''; target.value = undefined;
  grants.value = []; unsigned.value = false; error.value = ''; stale.value = false;
  setBusy(true);
  try {
    await discard(oldSelection);
    if (!alive || own !== generation) return;
    if (!selected) return;
    if (!selected.name.toLowerCase().endsWith('.bkm-plugin') || !selected.size || selected.size > MAX_LOCAL_PACKAGE_BYTES) throw new Error('请选择不超过 32 MiB 的 .bkm-plugin 文件。');
    const checked = await props.upload(selected, controller.signal);
    if (!alive || own !== generation) { void discard(checked.selection); return; }
    inspection.value = checked; expires = Date.now() + checked.expires_in_seconds * 1000;
    instance.value = checked.default_instance_id; chooseTarget();
    await nextTick(); heading.value?.focus();
  } catch (e) { if (alive && own === generation) error.value = (e as Error).message; }
  finally { if (alive && own === generation) setBusy(false); }
}
async function install() {
  const checked = inspection.value;
  if (!checked || props.locked || working.value || stale.value || result.value || !validId.value || targetError.value || !unsigned.value) return;
  if (Date.now() >= expires) { stale.value = true; error.value = '文件检查已过期，请重新选择文件。'; return; }
  const chosen = [...new Set([...preserved.value, ...grants.value])];
  if (!window.confirm(`确认${target.value ? '更新' : '安装'}插件 ${checked.plugin_id} 到实例 ${instance.value}？\n目标版本：${checked.version}\n授予：${chosen.join('、') || '无'}\n这是未签名包；保存后需要重启。保留已有配置、启停状态和私有数据；缺少必要配置或权限时将停用。`)) return;
  setBusy(true); committing.value = true; error.value = '';
  try {
    const saved = readLocalInstall(await props.request('/api/plugins/local/commit', { selection: checked.selection, sha256: checked.sha256, plugin_id: checked.plugin_id, version: checked.version, instance_id: instance.value, expected_updated_at_ms: target.value?.updated_at_ms ?? null, grants: grants.value, confirm_unsigned: true, confirm: true }, controller.signal), instance.value, checked.version);
    if (alive) { result.value = saved; emit('installed'); }
  } catch (e) { if (alive) { stale.value = true; error.value = `${(e as Error).message} 正在重新读取实例状态；不会自动重复安装。重新选择文件前请核对实例。`; emit('uncertain'); } }
  finally { if (alive) { committing.value = false; setBusy(false); await nextTick(); if (document.activeElement === document.body) action.value?.focus(); } }
}
</script>
<template>
  <section class="panel local-package" aria-labelledby="local-package-title">
    <h2 id="local-package-title">安装 / 更新本地插件包</h2>
    <p class="hint">选择 .bkm-plugin 文件，最大 32 MiB。同时最多检查或保留 2 个文件，检查结果 5 分钟内有效。检查文件不会安装或运行插件。</p>
    <label for="local-package-file">本地插件包</label><input id="local-package-file" type="file" accept=".bkm-plugin" :disabled="locked || working" @change="select">
    <p v-if="working" role="status">{{ committing ? '正在提交安装，请等待结果…' : '正在上传并校验文件…' }}</p>
    <template v-if="inspection">
      <h3 ref="heading" tabindex="-1">{{ inspection.name }} · {{ inspection.version }}</h3><p>{{ inspection.description }}</p>
      <dl><dt>插件标识</dt><dd>{{ inspection.plugin_id }}</dd><dt>SHA-256</dt><dd><code>{{ inspection.sha256 }}</code></dd><dt>签名状态</dt><dd>未签名</dd></dl>
      <p>命令：{{ inspection.commands.join('、') || '无' }}</p>
      <fieldset :disabled="locked || working || stale || !!result"><legend>安装目标与授权</legend>
        <label for="local-instance">实例 ID（存在则更新，不存在则新建）</label><input id="local-instance" v-model="instance" maxlength="192" @input="chooseTarget">
        <p class="hint">仅支持字母、数字、点、下划线、短横线及分段斜线；不可含空段、. 或 .. 段。随包与文件声明实例不可修改。</p>
        <p v-if="!validId || targetError" role="alert">{{ targetError || '请输入合法实例 ID。' }}</p>
        <p v-else>{{ target ? `更新 ${target.version} → ${inspection.version}，修订 ${target.updated_at_ms}；保留现有配置与启停状态。` : '新建实例；缺少必要配置或权限时保持禁用。' }}</p>
        <label v-for="permission in inspection.requested_permissions" :key="permission" class="package-choice"><input v-if="preserved.includes(permission)" type="checkbox" checked disabled><input v-else v-model="grants" type="checkbox" :value="permission"><span>{{ permission }}{{ preserved.includes(permission) ? '（保留已有授权）' : '' }}</span></label>
        <p v-if="!inspection.requested_permissions.length">未请求权限。</p>
        <label class="package-choice"><input id="local-unsigned" v-model="unsigned" type="checkbox"><span>我同意安装这个未签名插件包。</span></label>
      </fieldset>
      <p v-if="stale" class="notice">此选择不可再提交。请核对最新实例状态，重新选择文件后再决定。</p>
      <button id="commit-local-package" ref="action" class="primary" type="button" :disabled="locked || working || stale || !!result || !validId || !!targetError || !unsigned" @click="install">{{ target ? '确认更新本地包' : '确认安装本地包' }}</button>
    </template>
    <p v-if="error" role="alert" class="notice error">{{ error }}</p>
    <div v-if="result" role="status"><p>已保存 {{ result.version }}；当前加载版本：{{ result.loaded_version || '未加载' }}。需要重启 Bot。{{ result.enabled ? '' : '实例保持禁用。' }}</p><p v-if="result.config_incomplete">必要配置尚未完成。</p><p v-if="result.missing_permissions.length">缺少必要权限：{{ result.missing_permissions.join('、') }}</p><RouterLink :to="pluginRoute(result.instance_id, 'config')" class="button-link">管理实例配置与权限 ↗</RouterLink></div>
  </section>
</template>
