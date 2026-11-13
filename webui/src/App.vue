<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, shallowRef, watch } from 'vue';
import { RouterLink, useRoute, useRouter } from 'vue-router';
import AppShell from './components/AppShell.vue';
import OverviewPage from './components/OverviewPage.vue';
import UnavailablePage from './components/UnavailablePage.vue';
import CommandSettings from './components/CommandSettings.vue';
import PluginList from './components/PluginList.vue';
import PluginDetail from './components/PluginDetail.vue';
import { ApiError, createApi } from './api';
import type { Mutation, PluginView } from './types';
const route = useRoute(), router = useRouter();
const authenticated = ref(false), tokenInput = ref(''), plugins = shallowRef<PluginView[]>([]), refreshedAt = ref('');
const busy = ref(false), pluginDirty = ref(false), settingsDirty = ref(false), settingsBusy = ref(false), conflicts = ref(new Set<string>()), message = ref(''), error = ref(false);
const plugin = computed(() => plugins.value.find(p => p.instance_id === route.params.instanceId));
const conflict = computed(() => !!plugin.value && conflicts.value.has(plugin.value.instance_id));
const detailTab = computed(() => route.name === 'plugin-config' ? 'config' : route.name === 'plugin-logs' ? 'logs' : 'overview');
const api = createApi(() => logout('登录已失效，请重新输入管理 Token。'));
let epoch = 0;
function report(text: string, failed = true) { message.value = text; error.value = failed; }
function logout(text = '') {
  api.clear(); epoch++; authenticated.value = false; tokenInput.value = ''; plugins.value = []; refreshedAt.value = '';
  pluginDirty.value = false; settingsDirty.value = false; settingsBusy.value = false; conflicts.value.clear(); report(text, !!text);
}
const mayDiscard = (dirty: boolean) => !dirty || window.confirm('有未保存更改，是否放弃这些更改？');
const removeGuard = router.beforeEach((to, from) => {
  if (!authenticated.value) return true;
  if (busy.value || settingsBusy.value) { report('操作进行中，请等待完成后再切换页面。'); return false; }
  if (to.name === from.name && to.params.instanceId === from.params.instanceId) return true;
  return mayDiscard(pluginDirty.value || settingsDirty.value);
});
watch([() => route.name, () => route.params.instanceId], () => { pluginDirty.value = false; settingsDirty.value = false; report('', false); });
async function perform(task: () => Promise<void>, loading: string) {
  if (busy.value) return;
  const current = epoch; busy.value = true; report(loading, false);
  try { await task(); } catch (e) { if (current === epoch) report((e as Error).message); }
  finally { busy.value = false; }
}
async function refreshPlugins() {
  const data = await api.request<{ plugins: PluginView[] }>('/api/plugins');
  if (!Array.isArray(data.plugins)) throw new Error('插件列表格式异常，请重试。');
  plugins.value = data.plugins; refreshedAt.value = new Date().toLocaleTimeString('zh-CN', { hour12: false });
  pluginDirty.value = false; conflicts.value.clear(); authenticated.value = true;
  report(plugins.value.length ? '插件状态已更新。' : '暂无插件，可先通过现有安装方式添加插件。', false);
}
function login() {
  if (busy.value) return;
  const value = tokenInput.value.trim(); tokenInput.value = ''; if (!value) { report('请输入管理 Token。'); return; }
  api.setToken(value);
  void perform(async () => { try { await refreshPlugins(); } catch (e) { api.clear(); throw e; } }, '正在验证并加载插件…');
}
function refresh() { if (mayDiscard(pluginDirty.value)) void perform(refreshPlugins, '正在刷新插件状态…'); }
function mutate(change: Mutation) {
  const p = plugin.value; if (!p?.editable || conflict.value || (pluginDirty.value && change.action !== 'config')) return;
  void perform(async () => {
    try {
      const { action, ...values } = change;
      const data = await api.request<{ plugin: PluginView }>(`/api/plugins/${action}`, { instance_id: p.instance_id, expected_updated_at_ms: p.updated_at_ms, ...values });
      if (!data.plugin || data.plugin.instance_id !== p.instance_id) throw new Error('更新响应格式异常，请刷新确认状态。');
      plugins.value = plugins.value.map(item => item.instance_id === p.instance_id ? data.plugin : item); pluginDirty.value = false;
      report(data.plugin.error ? `操作已完成，但插件报告错误：${data.plugin.error}` : action === 'apply' ? (data.plugin.pending ? '仍有待应用设置，请检查运行状态。' : '已应用，请查看最新运行状态。') : '已保存；点击“立即应用”后更新运行时。', !!data.plugin.error);
    } catch (e) { if (e instanceof ApiError && e.status === 409) conflicts.value.add(p.instance_id); throw e; }
  }, change.action === 'apply' ? '正在应用已保存设置…' : '正在保存设置…');
}
function beforeUnload(event: BeforeUnloadEvent) { if (pluginDirty.value || settingsDirty.value || busy.value || settingsBusy.value) { event.preventDefault(); event.returnValue = ''; } }
const pageHide = () => logout();
onMounted(() => { window.addEventListener('beforeunload', beforeUnload); window.addEventListener('pagehide', pageHide); });
onBeforeUnmount(() => { removeGuard(); api.clear(); window.removeEventListener('beforeunload', beforeUnload); window.removeEventListener('pagehide', pageHide); });
</script>
<template>
  <div v-if="!authenticated" class="login-shell">
    <header class="login-masthead"><span class="brand-mark" aria-hidden="true">B.</span>BroKnowMyQQBot</header>
    <main class="login card" aria-labelledby="login-title">
      <span class="eyebrow">管理控制台 / CONTROL PANEL</span><h1 id="login-title">登录控制台</h1><p class="muted">查看运行情况，管理插件与配置。</p>
      <p v-if="message" class="notice" :class="{ error }" :role="error ? 'alert' : 'status'">{{ error ? '提示：' : '' }}{{ message }}</p>
      <form id="login-form" @submit.prevent="login"><label for="token">管理 Token</label><input id="token" v-model="tokenInput" type="password" autocomplete="off" autocapitalize="off" spellcheck="false" required placeholder="输入管理 Token" :disabled="busy"><button class="primary" :disabled="busy">进入控制台</button></form>
      <p class="hint">Token 仅在本次页面会话中使用，退出或刷新后需重新登录。</p>
    </main><footer class="login-footer">BKMQB — CONTROL PANEL</footer>
  </div>
  <AppShell v-else :busy="busy || settingsBusy" @logout="mayDiscard(pluginDirty || settingsDirty) && logout()">
    <p v-if="message" class="notice" :class="{ error }" :role="error ? 'alert' : 'status'" aria-live="polite">{{ error ? '提示：' : '' }}{{ message }}</p>
    <OverviewPage v-if="route.name === 'overview'" :plugins="plugins" :refreshed-at="refreshedAt" :busy="busy" @refresh="refresh" />
    <template v-else-if="route.name === 'plugins'"><div class="page-heading"><div class="page-heading-copy"><span class="eyebrow">插件 / INSTALLED</span><h1 tabindex="-1">已安装插件</h1><p class="muted">查看实例状态，进入插件管理配置。</p></div><div class="actions"><button id="refresh" type="button" :disabled="busy" @click="refresh">刷新状态</button><RouterLink :to="{ name: 'marketplace' }" class="button-link">浏览插件市场 ↗</RouterLink></div></div><PluginList :plugins="plugins" :busy="busy" /></template>
    <template v-else-if="route.meta.section === 'plugins'">
      <div class="detail-toolbar"><button id="refresh" type="button" :disabled="busy" @click="refresh">刷新状态</button></div>
      <PluginDetail v-if="plugin" :key="plugin.instance_id" :plugin="plugin" :tab="detailTab" :busy="busy" :conflict="conflict" @mutate="mutate" @dirty="pluginDirty = $event" @error="report" />
      <section v-else class="panel empty-state"><h1 tabindex="-1">未找到插件实例</h1><p>该实例不在当前管理列表中，可能已移除或未加载。</p><RouterLink :to="{ name: 'plugins' }" class="button-link">返回已安装插件</RouterLink></section>
    </template>
    <template v-else-if="route.name === 'settings'"><div class="page-heading"><div class="page-heading-copy"><span class="eyebrow">系统 / SETTINGS</span><h1 tabindex="-1">设置</h1><p class="muted">管理 Bot 的全局行为。</p></div></div><CommandSettings :request="api.request" @dirty="settingsDirty = $event" @busy="settingsBusy = $event" /></template>
    <UnavailablePage v-else :kind="String(route.name)" />
  </AppShell>
</template>
