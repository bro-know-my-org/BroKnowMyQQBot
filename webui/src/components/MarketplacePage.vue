<script setup lang="ts">
import { nextTick, onBeforeUnmount, onMounted, ref, shallowRef } from 'vue';
import { RouterLink } from 'vue-router';
import type { Request } from '../api';
import type { Catalogue, CatalogueItem, InstallResult } from '../types';
import { pluginRoute } from '../navigation';
import '../marketplace.css';
const props = defineProps<{ request: Request; reconcile?: boolean }>();
const emit = defineEmits<{ busy: [value: boolean]; installed: []; uncertain: [] }>();
const query = ref(''), submittedQuery = ref(''), offset = ref(0), catalogue = shallowRef<Catalogue>(), selected = shallowRef<CatalogueItem>();
const detailTitle = ref<HTMLElement>();
const loading = ref(false), installing = ref(false), message = ref(''), error = ref('');
const grants = ref<string[]>([]), confirmed = ref(false), stale = ref(false);
let alive = true, detailsTrigger: HTMLElement | undefined;
onBeforeUnmount(() => { alive = false; emit('busy', false); });
const report = (e: unknown) => { if (alive) error.value = (e as Error).message; };
async function load(nextOffset = 0, newSearch = false) {
  const search = newSearch ? query.value : submittedQuery.value;
  if (loading.value || installing.value) return;
  loading.value = true; error.value = ''; selected.value = undefined;
  try {
    const result = await props.request<Catalogue>(`/api/marketplace?q=${encodeURIComponent(search)}&offset=${nextOffset}&limit=20`);
    if (alive) { catalogue.value = result; offset.value = nextOffset; submittedQuery.value = search; if (props.reconcile) emit('installed'); }
  } catch (e) { report(e); } finally { if (alive) loading.value = false; }
}
async function details(item: CatalogueItem, event: Event) {
  detailsTrigger = event.currentTarget as HTMLElement;
  if (loading.value || installing.value) return;
  loading.value = true; error.value = ''; message.value = ''; selected.value = undefined;
  try {
    const result = await props.request<Catalogue>(`/api/marketplace/details?selector=${encodeURIComponent(item.slug)}`);
    if (!result.plugins[0]) throw new Error('条目已不存在，请刷新市场。');
    if (alive) {
      selected.value = result.plugins[0]; confirmed.value = false; grants.value = []; stale.value = false;
      if (props.reconcile) emit('installed');
      await nextTick(); if (alive) detailTitle.value?.focus();
    }
  } catch (e) { report(e); } finally { if (alive) loading.value = false; }
}
async function closeDetails() { selected.value = undefined; await nextTick(); detailsTrigger?.focus(); }
async function install() {
  const item = selected.value;
  if (!item || !confirmed.value || stale.value || installing.value || !item.manageable) return;
  installing.value = true; emit('busy', true); error.value = ''; message.value = '';
  try {
    const result = await props.request<InstallResult>(`/api/marketplace/${item.installed ? 'update' : 'install'}`, {
      selector: item.slug, selection: item.selection, version: item.latest.version, sha256: item.latest.sha256,
      expected_updated_at_ms: item.installed?.updated_at_ms ?? null, grants: grants.value, confirm_unsigned: confirmed.value,
    });
    if (alive) {
      stale.value = true; confirmed.value = false;
      const updated: CatalogueItem = { ...item, installed: { version: result.version, enabled: result.enabled,
        updated_at_ms: result.updated_at_ms, granted_permissions: item.capabilities.filter(capability => !result.missing_permissions.includes(capability)),
        runtime_state: item.installed?.runtime_state ?? 'not_loaded' } };
      selected.value = updated;
      if (catalogue.value) catalogue.value = { ...catalogue.value, plugins: catalogue.value.plugins.map(p => p.plugin_id === item.plugin_id ? updated : p) };
      message.value = `已保存 ${result.instance_id} · ${result.version}。需要重启 Bot 后加载（RESTART REQUIRED）。${result.enabled ? '' : ' 当前保持禁用。'}${result.missing_permissions.length ? ` 缺少权限：${result.missing_permissions.join('、')}。` : ''}${result.config_incomplete ? ' 配置尚未完成，请在已安装插件中补全。' : ''}`;
      emit('installed');
    }
  } catch (e) { emit('uncertain'); stale.value = true; confirmed.value = false; report(e); }
  finally { if (alive) { installing.value = false; emit('busy', false); } }
}
onMounted(() => { void load(); });
</script>
<template>
  <div class="page-heading"><div class="page-heading-copy"><span class="eyebrow">插件 / MARKETPLACE</span><h1 tabindex="-1">插件市场</h1><p class="muted">从配置的 GitHub 市场查找插件。安装与更新保存后，需要重启 Bot 才会加载。</p></div></div>
  <p class="notice">市场包信任等级：local-wasm / unsigned（未签名）。固定哈希用于核对包内容，不代表签名或发布者身份验证。</p>
  <form class="market-search panel" @submit.prevent="load(0, true)"><label for="market-search">搜索插件</label><input id="market-search" v-model="query" maxlength="128" type="search" placeholder="名称、插件 ID、命令" :disabled="loading || installing"><button :disabled="loading || installing">搜索</button><button type="button" :disabled="loading || installing" @click="load(offset)">刷新市场</button></form>
  <p v-if="catalogue" class="hint market-source">索引来源：{{ catalogue.source }} · {{ catalogue.total }} 个匹配条目 · 当前结果：{{ submittedQuery || '全部' }}。当前管理默认实例 /default。</p>
  <p v-if="loading" role="status">正在读取市场…</p>
  <div v-if="error" class="notice error" role="alert">{{ error }} <button type="button" :disabled="loading || installing" @click="load(offset)">重试加载市场</button></div>
  <p v-if="message" class="notice" role="status">{{ message }}</p>
  <section v-if="selected" class="panel market-details" aria-labelledby="market-detail-title">
    <div class="section-heading"><h2 id="market-detail-title" ref="detailTitle" tabindex="-1">{{ selected.name }} · {{ selected.latest.version }}</h2><button type="button" :disabled="installing" @click="closeDetails">关闭详情</button></div>
    <p>{{ selected.summary }}</p><p class="mono">{{ selected.plugin_id }}</p>
    <dl class="market-metadata"><dt>源码</dt><dd><a :href="selected.repository" target="_blank" rel="noopener noreferrer">{{ selected.repository }}</a></dd><dt>版本与协议</dt><dd>{{ selected.latest.version }} · BPP {{ selected.latest.protocol }}</dd><dt>包来源</dt><dd>{{ selected.latest.download }}</dd><dt>SHA-256</dt><dd class="mono">{{ selected.latest.sha256 }}</dd><dt>实例</dt><dd>{{ selected.instance_id }}</dd><dt>已安装</dt><dd>{{ selected.installed ? `${selected.installed.version} · ${selected.installed.enabled ? '已启用' : '已禁用'} · ${selected.installed.runtime_state}` : '尚未安装默认实例' }}</dd></dl>
    <p v-if="!selected.manageable" class="notice">此实例由声明式文件或随包配置管理，请在其来源中修改。</p>
    <fieldset :disabled="installing || stale || !selected.manageable"><legend>请求权限 · 逐项选择本次新增授权</legend>
      <p class="hint">未勾选的新增权限不会授予。已有授权在更新时保留；缺少必要权限或配置的安装将保持禁用。</p>
      <p v-if="!selected.capabilities.length">此插件未请求权限。</p>
      <label v-for="capability in selected.capabilities" :key="capability" class="market-permission"><template v-if="selected.installed?.granted_permissions.includes(capability)"><span aria-hidden="true">✓</span><span>{{ capability }}（已授权，更新时保留）</span></template><template v-else><input v-model="grants" type="checkbox" :value="capability"><span>{{ capability }}</span></template></label>
      <label class="market-permission market-confirm"><input v-model="confirmed" type="checkbox"><span>我确认保存此版本的未签名插件及所选权限，并知悉需要重启 Bot。</span></label>
      <button class="primary" type="button" :disabled="!confirmed" @click="install">{{ installing ? '正在校验并保存…' : selected.installed ? '确认更新所选版本' : '确认安装所选版本' }}</button>
    </fieldset>
    <p v-if="stale" class="hint">本次选择已结束。再次操作前，请重新打开详情核对最新状态。</p>
    <RouterLink v-if="selected.installed || message" :to="pluginRoute(selected.instance_id)" class="button-link" :aria-disabled="installing" @click="installing && $event.preventDefault()">管理已安装实例</RouterLink>
  </section>
  <section v-if="catalogue" aria-label="市场插件列表">
    <p v-if="!catalogue.plugins.length" class="panel empty-state">{{ catalogue.total ? '当前页没有条目，请返回上一页。' : '没有匹配的插件。请更换搜索词或刷新市场。' }}</p>
    <ul v-else class="market-list"><li v-for="item in catalogue.plugins" :key="item.plugin_id" class="panel"><h2>{{ item.name }}</h2><p>{{ item.summary }}</p><p class="mono">{{ item.plugin_id }}</p><p>{{ item.latest.version }} · {{ item.installed ? `已安装 ${item.installed.version}${item.installed.enabled ? ' / 已启用' : ' / 已禁用'}` : '尚未安装默认实例' }}</p><button type="button" :disabled="loading || installing" @click="details(item, $event)">查看详情与权限</button></li></ul>
    <nav v-if="catalogue.total" class="actions" aria-label="市场分页"><button type="button" :disabled="offset === 0 || loading || installing" @click="load(Math.max(0, offset - 20))">上一页</button><span>{{ offset + 1 }}–{{ offset + catalogue.plugins.length }} / {{ catalogue.total }}</span><button type="button" :disabled="offset + catalogue.plugins.length >= catalogue.total || loading || installing" @click="load(offset + 20)">下一页</button></nav>
  </section>
</template>
