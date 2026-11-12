<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, shallowRef } from 'vue';
import CommandSettings from './components/CommandSettings.vue';
import PluginList from './components/PluginList.vue';
import PluginDetail from './components/PluginDetail.vue';
import { ApiError, createApi } from './api';
import type { Mutation, PluginView } from './types';
const authenticated = ref(false), tokenInput = ref(''), plugins = shallowRef<PluginView[]>([]), selected = ref<string>();
const busy = ref(false), pluginDirty = ref(false), settingsDirty = ref(false), settingsBusy = ref(false), conflict = ref(false), message = ref(''), error = ref(false);
const plugin = computed(() => plugins.value.find(p => p.instance_id === selected.value));
const api = createApi(() => logout('登录已失效，请重新输入管理 Token。'));
let epoch = 0;
function report(text: string, failed = true) { message.value = text; error.value = failed; }
function logout(text = '') {
  api.clear(); epoch++; authenticated.value = false; tokenInput.value = ''; plugins.value = []; selected.value = undefined;
  pluginDirty.value = false; settingsDirty.value = false; settingsBusy.value = false; conflict.value = false; report(text, !!text);
}
const mayDiscard = (dirty: boolean) => !dirty || window.confirm('有未保存更改，是否放弃这些更改？');
async function perform(task: () => Promise<void>, loading: string) {
  if (busy.value) return;
  const current = epoch; busy.value = true; report(loading, false);
  try { await task(); } catch (e) { if (current === epoch) report((e as Error).message); }
  finally { busy.value = false; }
}
async function refreshPlugins() {
  const data = await api.request<{ plugins: PluginView[] }>('/api/plugins');
  if (!Array.isArray(data.plugins)) throw new Error('插件列表格式异常，请重试。');
  plugins.value = data.plugins; if (!plugin.value) selected.value = plugins.value[0]?.instance_id;
  pluginDirty.value = false; conflict.value = false; authenticated.value = true;
  report(plugins.value.length ? '插件状态已更新。' : '暂无插件，可先通过现有安装方式添加插件。', false);
}
function login() {
  if (busy.value) return;
  const value = tokenInput.value.trim(); tokenInput.value = ''; if (!value) { report('请输入管理 Token。'); return; }
  api.setToken(value);
  void perform(async () => { try { await refreshPlugins(); } catch (e) { api.clear(); throw e; } }, '正在验证并加载插件…');
}
function select(id: string) { if (id !== selected.value && mayDiscard(pluginDirty.value)) { selected.value = id; pluginDirty.value = false; conflict.value = false; report('', false); } }
function mutate(change: Mutation) {
  const p = plugin.value; if (!p?.editable || conflict.value || (pluginDirty.value && change.action !== 'config')) return;
  void perform(async () => {
    try {
      const { action, ...values } = change;
      const data = await api.request<{ plugin: PluginView }>(`/api/plugins/${action}`, { instance_id: p.instance_id, expected_updated_at_ms: p.updated_at_ms, ...values });
      if (!data.plugin || data.plugin.instance_id !== p.instance_id) throw new Error('更新响应格式异常，请刷新确认状态。');
      plugins.value = plugins.value.map(item => item.instance_id === p.instance_id ? data.plugin : item); pluginDirty.value = false;
      report(data.plugin.error ? `操作已完成，但插件报告错误：${data.plugin.error}` : action === 'apply' ? (data.plugin.pending ? '仍有待应用设置，请检查运行状态。' : '已应用，请查看最新运行状态。') : '已保存；点击“立即应用”后更新运行时。', !!data.plugin.error);
    } catch (e) { if (e instanceof ApiError && e.status === 409) conflict.value = true; throw e; }
  }, change.action === 'apply' ? '正在应用已保存设置…' : '正在保存设置…');
}
function beforeUnload(event: BeforeUnloadEvent) { if (pluginDirty.value || settingsDirty.value || busy.value || settingsBusy.value) { event.preventDefault(); event.returnValue = ''; } }
const pageHide = () => logout();
onMounted(() => { window.addEventListener('beforeunload', beforeUnload); window.addEventListener('pagehide', pageHide); });
onBeforeUnmount(() => { api.clear(); window.removeEventListener('beforeunload', beforeUnload); window.removeEventListener('pagehide', pageHide); });
</script>
<template>
  <header class="topbar"><a class="brand" href="/" aria-label="BroKnowMyQQBot 首页"><span class="brand-mark" aria-hidden="true">B</span>BroKnowMyQQBot</a><span class="top-caption">插件管理</span><button v-if="authenticated" id="logout" class="quiet" type="button" @click="mayDiscard(pluginDirty || settingsDirty) && logout()">退出登录</button></header>
  <main>
    <p v-if="message" class="notice" :class="{ error }" :role="error ? 'alert' : 'status'" aria-live="polite">{{ message }}</p>
    <section v-if="!authenticated" class="login card" aria-labelledby="login-title">
      <span class="eyebrow">管理控制台</span><h1 id="login-title">让每个插件，各就其位。</h1><p class="muted">登录后查看插件状态，管理配置与启停。</p>
      <form id="login-form" @submit.prevent="login"><label for="token">管理 Token</label><input id="token" v-model="tokenInput" type="password" autocomplete="off" autocapitalize="off" spellcheck="false" required placeholder="输入管理 Token" :disabled="busy"><button class="primary" :disabled="busy">进入控制台</button></form>
      <p class="hint">Token 仅在本次页面会话中使用，退出或刷新后需重新登录。</p>
    </section>
    <section v-else aria-label="插件控制台">
      <CommandSettings :request="api.request" @dirty="settingsDirty = $event" @busy="settingsBusy = $event" />
      <div class="page-heading"><div><span class="eyebrow">工作空间</span><h1>插件</h1><p class="muted">保存设置，再将更改应用到运行中的插件。</p></div><button id="refresh" type="button" :disabled="busy" @click="mayDiscard(pluginDirty) && perform(refreshPlugins, '正在刷新插件状态…')">刷新插件状态</button></div>
      <div class="workspace"><PluginList :plugins="plugins" :selected="selected" :busy="busy" @select="select" />
        <PluginDetail v-if="plugin" :key="plugin.instance_id" :plugin="plugin" :busy="busy" :conflict="conflict" @mutate="mutate" @dirty="pluginDirty = $event" @error="report" />
        <article v-else class="card detail"><p class="empty">选择一个插件，查看配置与运行状态。</p></article>
      </div>
    </section>
  </main>
  <footer>BroKnowMyQQBot <span>·</span> 让能力井然有序</footer>
</template>
