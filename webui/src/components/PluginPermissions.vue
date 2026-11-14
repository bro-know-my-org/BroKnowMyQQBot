<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, ref, shallowRef, watch } from 'vue';
import type { Request } from '../api';
import type { PermissionView, PluginView } from '../types';
import { readPermissions, readPermissionSave } from '../pluginPackages';
import '../pluginPackages.css';
const props = defineProps<{ plugin: PluginView; request: Request; busy: boolean }>();
const emit = defineEmits<{ dirty: [value: boolean]; busy: [value: boolean]; updated: [plugin: PluginView]; uncertain: [] }>();
const verified = shallowRef<PermissionView>(), grants = ref<string[]>([]), working = ref(false), stale = ref(false), error = ref(''), message = ref('');
const heading = ref<HTMLElement>(), saveButton = ref<HTMLButtonElement>();
const dirty = computed(() => !!verified.value && (grants.value.length !== verified.value.granted_permissions.length || grants.value.some(g => !verified.value!.granted_permissions.includes(g))));
watch(dirty, value => emit('dirty', value), { flush: 'sync' });
watch(() => props.plugin.updated_at_ms, () => { if (verified.value && props.plugin.updated_at_ms !== verified.value.expected_updated_at_ms) stale.value = true; });
let alive = true;
const controller = new AbortController();
onBeforeUnmount(() => { alive = false; controller.abort(); emit('dirty', false); emit('busy', false); });
function setBusy(value: boolean) { working.value = value; emit('busy', value); }
async function load() {
  if (props.busy || working.value || !props.plugin.editable) return;
  if (dirty.value && !window.confirm('有未保存授权，是否放弃并重新读取？')) return;
  setBusy(true); error.value = '';
  try {
    const result = readPermissions(await props.request(`/api/plugins/permissions?${new URLSearchParams({ instance_id: props.plugin.instance_id })}`, undefined, controller.signal), props.plugin.instance_id);
    if (alive) { verified.value = result; grants.value = result.granted_permissions.filter(g => result.requested_permissions.includes(g)); stale.value = false; message.value = ''; await nextTick(); heading.value?.focus(); }
  } catch (e) { if (alive) error.value = (e as Error).message; }
  finally { if (alive) setBusy(false); }
}
async function save() {
  const selection = verified.value;
  if (!selection || props.busy || working.value || stale.value || !dirty.value) return;
  const added = grants.value.filter(g => !selection.granted_permissions.includes(g));
  const removed = selection.granted_permissions.filter(g => !grants.value.includes(g));
  if (!window.confirm(`确认调整实例 ${selection.instance_id}（${selection.version}）的权限？\n新增：${added.join('、') || '无'}\n撤销：${removed.join('、') || '无'}\n撤权先停止并退休旧实例；授权保存后需要重启。缺少必要权限将停用，私有数据与配置保留。`)) return;
  const chosen = [...grants.value]; setBusy(true); error.value = '';
  try {
    const plugin = readPermissionSave(await props.request('/api/plugins/permissions', { instance_id: selection.instance_id, expected_updated_at_ms: selection.expected_updated_at_ms, grants: chosen, confirm: true }, controller.signal), selection, chosen);
    if (alive) { verified.value = { ...selection, granted_permissions: chosen, expected_updated_at_ms: plugin.updated_at_ms }; stale.value = false; emit('updated', plugin); message.value = '授权已保存，需要重启 Bot。'; }
  } catch (e) { if (alive) { stale.value = true; error.value = `${(e as Error).message} 请重新读取确认；不会自动重复保存。`; emit('uncertain'); } }
  finally { if (alive) { setBusy(false); await nextTick(); if (document.activeElement === document.body) saveButton.value?.focus(); } }
}
</script>
<template>
  <section class="detail-section plugin-permissions" aria-label="能力授权">
    <h2>能力授权</h2>
    <template v-if="!verified"><div class="permissions"><div v-for="(values, title) in { '请求能力': plugin.requested_permissions, '已授予能力': plugin.granted_permissions }" :key="title"><h3>{{ title }}</h3><ul v-if="values.length"><li v-for="value in values.slice(0, 128)" :key="value">{{ value }}</li></ul><p v-else>无</p></div></div></template>
    <p v-if="!plugin.editable" class="hint">随包或文件声明的授权只读，请修改其声明来源。</p>
    <button v-else id="edit-permissions" type="button" :disabled="busy || working" @click="load">{{ verified ? '重新读取权限' : '调整权限' }}</button>
    <template v-if="verified"><h3 ref="heading" tabindex="-1">已校验 {{ verified.version }} 的声明权限</h3><p class="hint">逐项勾选授予、取消勾选撤销。新增授权需要重启；撤权会先停止旧实例，保存不会自动启用。</p>
      <fieldset :disabled="busy || working || stale"><legend>实例 {{ verified.instance_id }}</legend><label v-for="permission in verified.requested_permissions" :key="permission" class="package-choice"><input v-model="grants" type="checkbox" :value="permission"><span>{{ permission }}</span></label><p v-if="!verified.requested_permissions.length">未请求权限。</p></fieldset>
      <p v-if="stale" class="notice">状态可能已经改变。选择仍保留，请重新读取后再决定。</p>
      <button id="save-permissions" ref="saveButton" type="button" class="primary" :disabled="busy || working || stale || !dirty" @click="save">{{ working ? '正在处理…' : '保存权限调整' }}</button>
    </template>
    <p v-if="error" role="alert" class="notice error">{{ error }}</p><p v-if="message" role="status">{{ message }}</p>
  </section>
</template>
