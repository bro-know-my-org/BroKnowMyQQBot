<script setup lang="ts">
import { ref, watch } from 'vue';
import SchemaEditor from './SchemaEditor.vue';
import { json, runtimeNames } from '../types';
import type { Mutation, PluginView } from '../types';
const props = defineProps<{ plugin: PluginView; busy: boolean; conflict: boolean }>();
const emit = defineEmits<{ mutate: [change: Mutation]; dirty: [value: boolean]; error: [message: string] }>();
const editor = ref<InstanceType<typeof SchemaEditor>>(), dirty = ref(false);
watch(dirty, value => emit('dirty', value));
watch(() => props.plugin, () => { dirty.value = false; });
function save() { if (props.busy || !props.plugin.editable || props.conflict || !dirty.value) return; try { const config = editor.value!.read(); emit('mutate', { action: 'config', config }); } catch (e) { emit('error', (e as Error).message); } }
function discard() { if (window.confirm('有未保存更改，是否放弃这些更改？')) editor.value?.reset(); }
</script>
<template>
  <article class="card detail" aria-label="插件详情">
    <span class="eyebrow">插件详情</span><h2>{{ plugin.name || plugin.plugin_id }}</h2><p class="muted">{{ plugin.description || '暂无描述' }}</p>
    <div class="badges"><span class="badge">{{ plugin.trust_level }}</span><span class="badge" :class="{ warning: plugin.signature_status === 'unsigned' }">{{ plugin.signature_status || '签名状态未知' }}</span></div>
    <div class="status-grid">
      <div class="status-cell"><span class="muted">已保存设置</span><strong>{{ plugin.enabled ? '已启用' : '已停用' }}</strong></div>
      <div class="status-cell"><span class="muted">运行状态</span><strong>{{ runtimeNames[plugin.runtime_state] }}</strong></div>
    </div>
    <p class="hint">{{ plugin.pending ? '已保存的设置尚未应用到运行时。' : '没有待应用的设置更改。' }}</p><p v-if="plugin.error" class="notice error" role="alert">{{ plugin.error }}</p>
    <dl><dt>插件 ID</dt><dd>{{ plugin.plugin_id }}</dd><dt>实例 ID</dt><dd>{{ plugin.instance_id }}</dd><dt>版本</dt><dd>{{ plugin.version }}</dd><dt>来源</dt><dd>{{ plugin.source }}</dd><dt>更新时间</dt><dd>{{ new Date(plugin.updated_at_ms).toLocaleString('zh-CN') }}</dd></dl>
    <section class="section"><h3>能力授权</h3><div class="permissions">
      <div v-for="(values, title) in { '请求能力': plugin.requested_permissions, '已授予能力': plugin.granted_permissions }" :key="title"><h3>{{ title }}</h3><ul v-if="values.length"><li v-for="value in values" :key="value">{{ value }}</li></ul><p v-else class="muted">无</p></div>
    </div></section>
    <section class="section"><h3>插件配置</h3>
      <p v-if="!plugin.editable" class="notice">此插件由声明式文件或随包配置管理。请修改对应配置文件；此处仅供查看。</p>
      <form @submit.prevent="save">
        <SchemaEditor ref="editor" :config="plugin.config" :schema="plugin.schema" :disabled="busy || !plugin.editable" @dirty="dirty = $event" />
        <p class="hint" aria-live="polite">{{ conflict ? '状态已变化，请刷新后重新编辑。当前编辑仍保留，可先复制。' : dirty ? '有未保存更改。请先保存，再启停或立即应用。' : plugin.pending ? '已保存，等待应用到运行时。' : '当前没有未保存更改。' }}</p>
        <div class="actions">
          <button id="save" class="primary" :disabled="busy || !plugin.editable || conflict || !dirty">保存配置</button>
          <button id="toggle" type="button" :disabled="busy || !plugin.editable || conflict || dirty" @click="$emit('mutate', { action: 'enabled', enabled: !plugin.enabled })">{{ plugin.enabled ? '保存为停用' : '保存为启用' }}</button>
          <button id="apply" type="button" :disabled="busy || !plugin.editable || conflict || dirty || !plugin.pending" @click="$emit('mutate', { action: 'apply' })">立即应用</button>
          <button id="discard" type="button" :disabled="busy || !dirty" @click="discard">放弃编辑</button>
        </div>
      </form>
      <p class="hint">保存时由服务校验。保存配置与启停只更新已保存设置；立即应用会使用这些设置更新运行时。</p>
      <details><summary>查看已保存配置</summary><pre>{{ json(plugin.config) }}</pre></details>
      <details v-if="plugin.schema"><summary>查看配置 Schema</summary><pre>{{ json(plugin.schema) }}</pre></details>
    </section>
  </article>
</template>
