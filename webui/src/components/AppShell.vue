<script setup lang="ts">
import { inject, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue';
import { RouterLink, useRoute } from 'vue-router';
import { navigation } from '../navigation';
import NavIcon from './NavIcon.vue';
import NotificationButton from './NotificationButton.vue';
import { notificationKey } from '../notifications';
defineProps<{ busy: boolean; authMode?: 'token' | 'none' }>();
defineEmits<{ logout: [] }>();
const route = useRoute(), collapsed = ref(false), mobileOpen = ref(false);
const notifications = inject(notificationKey, undefined);
const sidebar = ref<HTMLElement>(), menu = ref<HTMLButtonElement>(), content = ref<HTMLElement>();
const originalOverflow = document.body.style.overflow;
try { collapsed.value = localStorage.getItem('bkmqb.sidebar.collapsed') === 'true'; } catch { /* Preference storage is optional. */ }
function toggleCollapsed() {
  collapsed.value = !collapsed.value;
  try { localStorage.setItem('bkmqb.sidebar.collapsed', String(collapsed.value)); } catch { /* Keep the preference for this page only. */ }
}
async function openMenu() { mobileOpen.value = true; await nextTick(); sidebar.value?.querySelector<HTMLElement>('.mobile-close')?.focus(); }
async function closeMenu(restoreFocus = true) { mobileOpen.value = false; if (restoreFocus) { await nextTick(); menu.value?.focus(); } }
async function skipToContent() { await closeMenu(false); await nextTick(); content.value?.focus(); }
function keydown(event: KeyboardEvent) {
  if (!mobileOpen.value) return;
  if (event.key === 'Escape') { event.preventDefault(); void closeMenu(); }
  if (event.key !== 'Tab') return;
  const elements = Array.from(sidebar.value?.querySelectorAll<HTMLElement>('a[href], button:not(:disabled)') ?? []).filter(el => el.getClientRects().length);
  const first = elements[0], last = elements.at(-1);
  if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last?.focus(); }
  else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first?.focus(); }
}
let wasMobile = window.innerWidth <= 800;
let lastFocused = document.activeElement;
function rememberFocus(event: FocusEvent) { if (event.target instanceof HTMLElement && event.target !== document.body) lastFocused = event.target; }
async function resize() {
  const isMobile = window.innerWidth <= 800;
  if (isMobile === wasMobile) return;
  wasMobile = isMobile;
  // CSS may hide and blur a control before the browser dispatches resize.
  const focused = document.activeElement === document.body ? lastFocused : document.activeElement;
  const inSidebar = !!focused && !!sidebar.value?.contains(focused);
  await closeMenu(false); await nextTick();
  if (isMobile && inSidebar) menu.value?.focus();
  else if (!isMobile && (inSidebar || focused === menu.value)) sidebar.value?.querySelector<HTMLElement>('.nav-item.active')?.focus();
}
watch(mobileOpen, value => { notifications?.setSuspended(value); document.body.style.overflow = value ? 'hidden' : originalOverflow; });
watch([() => route.name, () => route.params.instanceId], async () => {
  void closeMenu(false); document.title = `${String(route.meta.title)} · BroKnowMyQQBot`;
  await nextTick(); document.querySelector<HTMLElement>('#page-content h1')?.focus();
});
onMounted(() => { document.title = `${String(route.meta.title)} · BroKnowMyQQBot`; document.addEventListener('keydown', keydown); document.addEventListener('focusin', rememberFocus); window.addEventListener('resize', resize); });
onBeforeUnmount(() => { notifications?.setSuspended(false); document.body.style.overflow = originalOverflow; document.removeEventListener('keydown', keydown); document.removeEventListener('focusin', rememberFocus); window.removeEventListener('resize', resize); });
</script>
<template>
  <div class="app-shell" :class="{ 'is-collapsed': collapsed, 'is-mobile-open': mobileOpen }">
    <a class="skip-link" href="#page-content" @click.prevent="skipToContent">跳到主要内容</a>
    <button v-if="mobileOpen" class="sidebar-backdrop" type="button" tabindex="-1" aria-label="关闭导航菜单" @click="closeMenu()" />
    <aside id="main-sidebar" ref="sidebar" class="app-sidebar" aria-label="主导航" :role="mobileOpen ? 'dialog' : undefined" :aria-modal="mobileOpen || undefined">
      <div class="sidebar-brand"><span class="brand-mark" aria-hidden="true">B.</span><span class="brand-name">BroKnowMyQQBot<small>管理控制台</small></span><button class="icon-button mobile-close" type="button" aria-label="关闭导航菜单" @click="closeMenu()"><NavIcon name="close" /></button></div>
      <nav class="sidebar-nav" aria-label="主要页面">
        <div v-for="group in navigation" :key="group.label" class="nav-group"><p class="nav-group-label">{{ group.label }}</p>
          <RouterLink v-for="item in group.items" :key="item.name" :to="{ name: item.name }" class="nav-item" :class="{ active: route.meta.section === item.name }" :aria-current="route.meta.section === item.name ? 'page' : undefined" :aria-label="item.label" :title="collapsed ? item.label : undefined"><NavIcon :name="item.icon" /><span class="nav-label">{{ item.label }}</span><span v-if="route.meta.section === item.name" class="nav-current" aria-hidden="true">▪</span></RouterLink>
        </div>
      </nav>
      <div class="sidebar-bottom"><button id="sidebar-toggle" class="sidebar-toggle" type="button" :aria-label="collapsed ? '展开侧栏' : '收起侧栏'" :title="collapsed ? '展开侧栏' : undefined" :aria-expanded="!collapsed" @click="toggleCollapsed"><NavIcon :name="collapsed ? 'expand' : 'collapse'" /><span class="nav-label">收起侧栏</span></button><p class="sidebar-footnote">BKMQB / CONTROL PANEL</p></div>
    </aside>
    <div class="app-body" :inert="mobileOpen || undefined">
      <header class="topbar"><div class="breadcrumb"><button ref="menu" class="icon-button mobile-menu-toggle" type="button" aria-label="打开导航菜单" aria-controls="main-sidebar" :aria-expanded="mobileOpen" @click="openMenu"><NavIcon name="menu" /></button><span class="muted">控制台</span><span aria-hidden="true">/</span><span>{{ route.meta.title }}</span></div><div class="topbar-actions"><NotificationButton /><span v-if="authMode === 'none'" class="status-marker"><span class="status-shape is-offline" aria-hidden="true" />本地免认证</span><button v-else id="logout" class="quiet" type="button" :disabled="busy" @click="$emit('logout')">退出登录</button></div></header>
      <slot name="process-controls" />
      <main id="page-content" ref="content" class="page-content" tabindex="-1"><slot /></main>
    </div>
  </div>
</template>
