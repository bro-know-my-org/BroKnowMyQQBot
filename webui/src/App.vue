<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, provide, ref, shallowRef, watch } from 'vue';
import { RouterLink, useRoute, useRouter } from 'vue-router';
import AppShell from './components/AppShell.vue';
import OverviewPage from './components/OverviewPage.vue';
import AdaptersPage from './components/AdaptersPage.vue';
import LogsPage from './components/LogsPage.vue';
import UnavailablePage from './components/UnavailablePage.vue';
import MarketplacePage from './components/MarketplacePage.vue';
import CommandSettings from './components/CommandSettings.vue';
import SystemSettings from './components/SystemSettings.vue';
import PluginList from './components/PluginList.vue';
import PluginDetail from './components/PluginDetail.vue';
import NotificationCenter from './components/NotificationCenter.vue';
import { createNotifications, notificationKey } from './notifications';
import { ApiError, createApi } from './api';
import type { Mutation, PluginView, Session } from './types';
const route = useRoute(), router = useRouter();
const authenticated = ref(false), tokenInput = ref(''), plugins = shallowRef<PluginView[]>([]), refreshedAt = ref('');
const authMode = ref<Session['mode']>('token'), checkingSession = ref(true), sessionFailed = ref(false);
const pluginsFailed = ref(false), marketBusy = ref(false), marketReconcile = ref(false);
const busy = ref(false), pluginDirty = ref(false), settingsDirty = ref(false), settingsBusy = ref(false), conflicts = ref(new Set<string>());
const systemDirty = ref(false), systemBusy = ref(false);
const notifications = createNotifications(); provide(notificationKey, notifications);
const plugin = computed(() => plugins.value.find(p => p.instance_id === route.params.instanceId));
const conflict = computed(() => !!plugin.value && conflicts.value.has(plugin.value.instance_id));
const detailTab = computed(() => route.name === 'plugin-config' ? 'config' : route.name === 'plugin-logs' ? 'logs' : 'overview');
const api = createApi(() => logout('登录已失效，请重新输入管理 Token。'));
let epoch = 0, refreshAfterBusy = false;
function report(text: string, failed = true, progress = false) { notifications.notify('控制台', text, progress ? 'progress' : failed ? 'error' : 'success'); }
function logout(text = '', unloading = false) {
  if (!unloading) notifications.reset();
  api.clear(); epoch++; refreshAfterBusy = false; authenticated.value = false; tokenInput.value = ''; plugins.value = []; refreshedAt.value = '';
  busy.value = false; checkingSession.value = false; sessionFailed.value = false; pluginsFailed.value = false; authMode.value = 'token';
  pluginDirty.value = false; settingsDirty.value = false; settingsBusy.value = false; systemDirty.value = false; systemBusy.value = false; marketBusy.value = false; marketReconcile.value = false; conflicts.value.clear(); report(text, !!text);
}
const mayDiscard = (dirty: boolean) => !dirty || window.confirm('有未保存更改，是否放弃这些更改？');
const removeGuard = router.beforeEach((to, from) => {
  if (!authenticated.value) return true;
  if (busy.value || settingsBusy.value || systemBusy.value || marketBusy.value) { report('操作进行中，请等待完成后再切换页面。'); return false; }
  if (to.name === from.name && to.params.instanceId === from.params.instanceId) return true;
  return mayDiscard(pluginDirty.value || settingsDirty.value || systemDirty.value);
});
watch([() => route.name, () => route.params.instanceId], () => { pluginDirty.value = false; settingsDirty.value = false; systemDirty.value = false; });
async function perform(task: () => Promise<void>, loading: string) {
  if (busy.value) return;
  const current = epoch; busy.value = true; report(loading, false, true);
  try { await task(); } catch (e) { if (current === epoch) report((e as Error).message); }
  finally { if (current === epoch) { notifications.clearSource('控制台', true); busy.value = false; if (refreshAfterBusy) { refreshAfterBusy = false; marketplaceInstalled(); } } }
}
async function refreshPlugins() {
  const current = epoch;
  try {
    const data = await api.request<{ plugins: PluginView[] }>('/api/plugins');
    if (!Array.isArray(data.plugins)) throw new Error('插件列表格式异常，请重试。');
    plugins.value = data.plugins; refreshedAt.value = new Date().toLocaleTimeString('zh-CN', { hour12: false });
    pluginDirty.value = false; conflicts.value.clear(); pluginsFailed.value = false;
    report(plugins.value.length ? '插件状态已更新。' : '暂无插件，可先通过现有安装方式添加插件。', false);
  } catch (e) { if (current === epoch && authenticated.value) pluginsFailed.value = true; throw e; }
}
function login() {
  if (busy.value) return;
  const value = tokenInput.value.trim(); tokenInput.value = ''; if (!value) { report('请输入管理 Token。'); return; }
  notifications.reset();
  void perform(async () => { const session = await api.request<Session>('/api/session/login', { token: value }); readSession(session); if (!session.authenticated) throw new Error('登录未完成，请重试。'); await refreshPlugins(); }, '正在验证并加载插件…');
}
function readSession(session: Session, restore = false) {
  if (!session || !['token', 'none'].includes(session.mode) || typeof session.authenticated !== 'boolean') throw new Error('会话响应格式异常，请重试。');
  notifications.authenticate(session.authenticated && typeof session.history_scope === 'string' && /^[a-f0-9]{64}$/.test(session.history_scope) ? session.history_scope : undefined, restore && session.authenticated);
  authMode.value = session.mode;
  authenticated.value = session.authenticated;
}
function restoreSession() {
  if (busy.value) return;
  const current = epoch;
  checkingSession.value = true; sessionFailed.value = false;
  void perform(async () => {
    try { const session = await api.request<Session>('/api/session'); readSession(session, true); if (session.authenticated) await refreshPlugins(); else report('', false); }
    catch (e) { if (current === epoch && !authenticated.value) sessionFailed.value = true; throw e; }
    finally { if (current === epoch) checkingSession.value = false; }
  }, '正在恢复登录状态…');
}
function signOut() {
  if (busy.value || settingsBusy.value || systemBusy.value || marketBusy.value || !mayDiscard(pluginDirty.value || settingsDirty.value || systemDirty.value)) return;
  void perform(async () => { await api.request<Session>('/api/session/logout', {}); logout(); }, '正在退出登录…');
}
// A lost response may finish later on the server. Keep reconciling on market
// reads for this session, including after route remounts; never replay the write.
function marketplaceInstalled() {
  if (busy.value) { refreshAfterBusy = true; return; }
  void perform(refreshPlugins, '正在读取安装后的插件状态…');
}
function refresh() { if (mayDiscard(pluginDirty.value)) void perform(refreshPlugins, '正在刷新插件状态…'); }
async function mutate(change: Mutation) {
  const p = plugin.value; if (!p?.editable || conflict.value || (pluginDirty.value && change.action !== 'config')) return;
  const operation = change.action === 'uninstall' || change.action === 'recover';
  if (operation && change.confirm !== true) return;
  const current = epoch; let removed = false;
  await perform(async () => {
    try {
      const { action, ...values } = change;
      const data = await api.request<{ plugin?: PluginView; removed?: boolean; instance_id?: string; private_state_retained?: boolean; recovered?: number; restart_required?: boolean }>(`/api/plugins/${action}`, { instance_id: p.instance_id, expected_updated_at_ms: p.updated_at_ms, ...values });
      if (action === 'uninstall') {
        if (data.removed !== true || data.instance_id !== p.instance_id || data.private_state_retained !== true) throw new Error('卸载响应格式异常，正在重新读取状态。');
        plugins.value = plugins.value.filter(item => item.instance_id !== p.instance_id); removed = true;
        report('插件已卸载，私有数据已保留。', false); return;
      }
      if (!data.plugin || data.plugin.instance_id !== p.instance_id) throw new Error('更新响应格式异常，请刷新确认状态。');
      if (action === 'recover' && (data.restart_required !== true || !data.plugin.restart_required || data.plugin.runtime_state === 'ready'
        || !Number.isSafeInteger(data.recovered) || Number(data.recovered) < 0)) throw new Error('恢复响应格式异常，正在重新读取状态。');
      const updated = data.plugin;
      plugins.value = plugins.value.map(item => item.instance_id === p.instance_id ? updated : item); pluginDirty.value = false;
      report(action === 'recover' ? `已准备恢复 ${data.recovered} 条失败投递。需要重启 Bot；本次没有重放投递。` : updated.error ? `操作已完成，但插件报告错误：${updated.error}` : action === 'apply' ? (updated.pending ? '仍有待应用设置，请检查运行状态。' : '已应用，请查看最新运行状态。') : '已保存；点击“立即应用”后更新运行时。', !!updated.error);
    } catch (e) {
      if (current === epoch && authenticated.value && (operation || (e instanceof ApiError && e.status === 409))) conflicts.value.add(p.instance_id);
      if (operation && current === epoch && authenticated.value) {
        try {
          const data = await api.request<{ plugins: PluginView[] }>('/api/plugins');
          if (!Array.isArray(data.plugins)) throw new Error('插件状态格式异常');
          plugins.value = data.plugins; removed = !data.plugins.some(item => item.instance_id === p.instance_id);
        } catch { /* Keep writes blocked until an explicit successful refresh. */ }
      }
      throw e;
    }
  }, operation ? '正在完成插件运维操作…' : change.action === 'apply' ? '正在应用已保存设置…' : '正在保存设置…');
  if (removed && current === epoch && authenticated.value && route.params.instanceId === p.instance_id) await router.replace({ name: 'plugins' });
}
function beforeUnload(event: BeforeUnloadEvent) { if (pluginDirty.value || settingsDirty.value || systemDirty.value || busy.value || settingsBusy.value || systemBusy.value || marketBusy.value) { event.preventDefault(); event.returnValue = ''; } }
const pageHide = () => { notifications.suspendSession(); logout('', true); };
const pageShow = (event: PageTransitionEvent) => { if (event.persisted) restoreSession(); };
onMounted(() => { window.addEventListener('beforeunload', beforeUnload); window.addEventListener('pagehide', pageHide); window.addEventListener('pageshow', pageShow); restoreSession(); });
onBeforeUnmount(() => { notifications.dispose(); removeGuard(); api.clear(); epoch++; window.removeEventListener('beforeunload', beforeUnload); window.removeEventListener('pagehide', pageHide); window.removeEventListener('pageshow', pageShow); });
</script>
<template>
  <div v-if="!authenticated" class="login-shell">
    <header class="login-masthead"><span class="brand-mark" aria-hidden="true">B.</span>BroKnowMyQQBot</header>
    <main class="login card" aria-labelledby="login-title">
      <span class="eyebrow">管理控制台 / CONTROL PANEL</span><h1 id="login-title">登录控制台</h1><p class="muted">查看运行情况，管理插件与配置。</p>
      <form v-if="!checkingSession && authMode === 'token'" id="login-form" @submit.prevent="login"><label for="token">管理 Token</label><input id="token" v-model="tokenInput" type="password" autocomplete="off" autocapitalize="off" spellcheck="false" required placeholder="输入管理 Token" :disabled="busy"><button class="primary" :disabled="busy">进入控制台</button></form>
      <button v-if="sessionFailed" type="button" :disabled="busy" @click="restoreSession">重试连接</button>
      <p v-for="entry in notifications.state.active" :key="entry.id" :role="entry.tone === 'error' ? 'alert' : 'status'">{{ entry.message }}</p>
      <p class="hint">登录后刷新页面可恢复会话。退出登录、会话过期或服务重启后需重新登录。</p>
    </main><footer class="login-footer">BKMQB — CONTROL PANEL</footer>
  </div>
  <AppShell v-else :busy="busy || settingsBusy || systemBusy || marketBusy" :auth-mode="authMode" @logout="signOut">
    <button v-if="pluginsFailed" id="retry-plugins" type="button" :disabled="busy" @click="refresh">重试加载插件</button>
    <section v-if="!refreshedAt && route.meta.section === 'plugins'" class="panel empty-state" aria-live="polite"><h1 tabindex="-1">{{ pluginsFailed ? '插件状态暂不可用' : '正在读取插件状态' }}</h1><p>{{ pluginsFailed ? '尚未读取到插件列表，请重试加载。' : '正在连接管理服务，请稍候。' }}</p></section>
    <OverviewPage v-else-if="route.name === 'overview'" :plugins="plugins" :refreshed-at="refreshedAt" :busy="busy" :request="api.request" @refresh="refresh" />
    <AdaptersPage v-else-if="route.name === 'adapters'" :request="api.request" :locked="busy" @dirty="systemDirty = $event" @busy="systemBusy = $event" />
    <template v-else-if="route.name === 'logs'"><div class="page-heading"><div class="page-heading-copy"><span class="eyebrow">运行 / LOGS</span><h1 tabindex="-1">日志</h1><p class="muted">查看最近运行记录，按级别和插件实例筛选。</p></div></div><LogsPage :request="api.request" :subscribe-logs="api.subscribeLogs" :plugins="plugins" /></template>
    <template v-else-if="route.name === 'plugins'"><div class="page-heading"><div class="page-heading-copy"><span class="eyebrow">插件 / INSTALLED</span><h1 tabindex="-1">已安装插件</h1><p class="muted">查看实例状态，进入插件管理配置。</p></div><div class="actions"><button id="refresh" type="button" :disabled="busy" @click="refresh">刷新状态</button><RouterLink :to="{ name: 'marketplace' }" class="button-link">浏览插件市场 ↗</RouterLink></div></div><PluginList :plugins="plugins" :busy="busy" /></template>
    <template v-else-if="route.meta.section === 'plugins'">
      <div class="detail-toolbar"><button id="refresh" type="button" :disabled="busy" @click="refresh">刷新状态</button></div>
      <PluginDetail v-if="plugin" :key="plugin.instance_id" :plugin="plugin" :request="api.request" :subscribe-logs="api.subscribeLogs" :tab="detailTab" :busy="busy" :conflict="conflict" @mutate="mutate" @dirty="pluginDirty = $event" @error="report" />
      <section v-else class="panel empty-state"><h1 tabindex="-1">未找到插件实例</h1><p>该实例不在当前管理列表中，可能已移除或未加载。</p><RouterLink :to="{ name: 'plugins' }" class="button-link">返回已安装插件</RouterLink></section>
    </template>
    <template v-else-if="route.name === 'settings'"><div class="page-heading"><div class="page-heading-copy"><span class="eyebrow">系统 / SETTINGS</span><h1 tabindex="-1">设置</h1><p class="muted">管理 Bot 的全局行为。</p></div></div><CommandSettings :request="api.request" :locked="systemBusy" @dirty="settingsDirty = $event" @busy="settingsBusy = $event" /><SystemSettings :request="api.request" :locked="settingsBusy" @dirty="systemDirty = $event" @busy="systemBusy = $event" /></template>
    <MarketplacePage v-else-if="route.name === 'marketplace'" :request="api.request" :reconcile="marketReconcile" @busy="marketBusy = $event" @uncertain="marketReconcile = true" @installed="marketplaceInstalled" />
    <UnavailablePage v-else :kind="String(route.name)" />
  </AppShell>
  <NotificationCenter v-if="authenticated" :center="notifications" />
</template>
