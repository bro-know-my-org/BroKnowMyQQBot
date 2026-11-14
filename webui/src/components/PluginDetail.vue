<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { RouterLink } from "vue-router";
import SchemaEditor from "./SchemaEditor.vue";
import LogsPage from "./LogsPage.vue";
import PluginVersions from './PluginVersions.vue';
import FailedDeliveries from './FailedDeliveries.vue';
import PluginPermissions from './PluginPermissions.vue';
import type { Request } from "../api";
import { pluginRoute } from "../navigation";
import { json, runtimeNames } from "../types";
import type { Mutation, PluginView, SubscribeLogs } from "../types";
const props = defineProps<{ plugin: PluginView; request: Request; subscribeLogs: SubscribeLogs; tab: "overview" | "config" | "logs"; busy: boolean; conflict: boolean }>();
const emit = defineEmits<{ mutate: [change: Mutation]; dirty: [value: boolean]; error: [message: string]; busy: [value: boolean]; updated: [plugin: PluginView]; uncertain: [] }>();
const editor = ref<InstanceType<typeof SchemaEditor>>(), configDirty = ref(false), permissionsDirty = ref(false);
const dirty = computed(() => configDirty.value || permissionsDirty.value);
watch(dirty, value => emit("dirty", value), { flush: "sync" });
watch(() => props.plugin, () => { configDirty.value = false; emit("dirty", dirty.value); });
watch(() => props.tab, () => { configDirty.value = false; });
function save() { if (props.busy || !props.plugin.editable || props.conflict || !dirty.value) return; try { const config = editor.value!.read(); emit("mutate", { action: "config", config }); } catch (e) { emit("error", (e as Error).message); } }
function discard() { if (window.confirm("有未保存更改，是否放弃这些更改？")) editor.value?.reset(); }
function operate(action: 'uninstall' | 'recover') {
  if (props.busy || !props.plugin.editable || props.conflict || dirty.value) return;
  const target = `“${props.plugin.name || props.plugin.plugin_id}”（实例 ${props.plugin.instance_id}）`;
  const question = action === 'uninstall'
    ? `确认卸载${target}？将停止当前实例并移除安装记录，保留私有数据。`
    : `确认恢复${target}？保留私有数据，停止并退休当前实例，准备失败投递供下次重启重试。需要重启 Bot；重启后可能产生消息，本次不会立即重放。`;
  if (window.confirm(question)) emit('mutate', { action, confirm: true });
}
</script>
<template>
  <article class="detail" aria-label="插件详情">
    <div class="detail-header"><div class="detail-title"><RouterLink :to="{ name: 'plugins' }" class="text-link">← 已安装插件</RouterLink><h1 tabindex="-1">{{ plugin.name || plugin.plugin_id }}</h1><p class="muted">{{ plugin.description || '暂无描述' }}</p><div class="badges"><span class="status-marker"><span class="status-shape" :class="plugin.runtime_state === 'ready' ? 'is-ready' : 'is-offline'" aria-hidden="true" />{{ runtimeNames[plugin.runtime_state] }}</span><PluginVersions :status="plugin" /><span v-if="plugin.pending" class="pending-marker">设置待应用</span></div></div><div class="actions detail-controls"><button id="toggle" type="button" :disabled="busy || !plugin.editable || conflict || dirty" @click="$emit('mutate', { action: 'enabled', enabled: !plugin.enabled })">{{ plugin.enabled ? '保存为停用' : '保存为启用' }}</button><button id="apply" type="button" :disabled="busy || !plugin.editable || conflict || dirty || !plugin.pending || (plugin.restart_required && plugin.enabled)" @click="$emit('mutate', { action: 'apply' })">立即应用</button></div></div>
    <nav class="tabs" aria-label="插件详情页面"><RouterLink v-for="(label, key) in { overview: '总览', config: '配置', logs: '日志' }" :key="key" :to="pluginRoute(plugin.instance_id, key)" class="tab-link" :class="{ active: tab === key }" :aria-current="tab === key ? 'page' : undefined">{{ label }}</RouterLink></nav>
    <div class="detail-body">
      <p v-if="plugin.error" class="notice error" role="alert">错误：{{ plugin.error }}</p>
      <template v-if="tab === 'overview'">
        <div class="status-grid"><div class="status-cell"><span class="muted">已保存设置</span><strong>{{ plugin.enabled ? "已启用" : "已停用" }}</strong></div><div class="status-cell"><span class="muted">运行状态</span><strong>{{ runtimeNames[plugin.runtime_state] }}</strong></div></div>
        <p v-if="plugin.restart_required" class="notice">已保存状态需要重启 Bot 后才能加载；实际加载版本和运行状态以上方快照为准。</p>
        <p class="hint">{{ plugin.pending ? "已保存的设置尚未应用到运行时。" : "没有待应用的设置更改。" }}</p>
        <section class="detail-section"><h2>插件信息</h2><dl><dt>插件 ID</dt><dd>{{ plugin.plugin_id }}</dd><dt>实例 ID</dt><dd>{{ plugin.instance_id }}</dd><dt>来源</dt><dd>{{ plugin.source }}</dd><dt>信任级别</dt><dd>{{ plugin.trust_level }}</dd><dt>签名</dt><dd>{{ plugin.signature_status === "unsigned" ? "未签名" : plugin.signature_status === "n/a" ? "不适用" : plugin.signature_status || "未知" }}</dd><dt>更新时间</dt><dd>{{ plugin.updated_at_ms > 0 ? new Date(plugin.updated_at_ms).toLocaleString("zh-CN") : "未提供" }}</dd></dl></section>
        <PluginPermissions :plugin="plugin" :request="request" :busy="busy || conflict" @dirty="permissionsDirty = $event" @busy="$emit('busy', $event)" @updated="$emit('updated', $event)" @uncertain="$emit('uncertain')" />
        <FailedDeliveries :request="request" :instance-id="plugin.instance_id" :revision="plugin.updated_at_ms" :busy="busy" />
        <section v-if="plugin.editable" class="detail-section"><h2>实例运维</h2><p class="hint">卸载默认保留私有数据。恢复只准备下次重启重试，不会立即重放投递。</p><div class="actions"><button id="recover-plugin" type="button" :disabled="busy || conflict || dirty" @click="operate('recover')">显式恢复并等待重启</button><button id="uninstall-plugin" type="button" :disabled="busy || conflict || dirty" @click="operate('uninstall')">卸载插件（保留数据）</button></div></section>
        <p v-if="!plugin.editable" class="notice">此插件由声明式文件或随包配置管理。配置页仅供查看。</p>
      </template>
      <section v-else-if="tab === 'config'" aria-label="插件配置">
        <h2>插件配置</h2><p class="muted">先保存设置，再应用到运行中的插件。已保存状态：{{ plugin.enabled ? '已启用' : '已停用' }}。</p>
        <p v-if="!plugin.editable" class="notice">此插件由声明式文件或随包配置管理。请修改对应配置文件；此处仅供查看。</p>
        <form @submit.prevent="save"><SchemaEditor ref="editor" :config="plugin.config" :schema="plugin.schema" :disabled="busy || !plugin.editable" @dirty="configDirty = $event" />
          <p class="hint" aria-live="polite">{{ conflict ? "状态已变化，请刷新后重新编辑。当前编辑仍保留，可先复制。" : dirty ? "有未保存更改。请先保存，再启停或立即应用。" : plugin.pending ? "已保存，等待应用到运行时。" : "当前没有未保存更改。" }}</p>
          <div class="actions"><button id="save" class="primary" :disabled="busy || !plugin.editable || conflict || !dirty">保存配置</button><button id="discard" type="button" :disabled="busy || !dirty" @click="discard">放弃编辑</button></div>
        </form>
        <details><summary>查看已保存配置</summary><pre>{{ json(plugin.config) }}</pre></details><details v-if="plugin.schema"><summary>查看配置 Schema</summary><pre>{{ json(plugin.schema) }}</pre></details>
      </section>
      <LogsPage v-else :request="request" :subscribe-logs="subscribeLogs" :instance-id="plugin.instance_id" />
    </div>
  </article>
</template>
