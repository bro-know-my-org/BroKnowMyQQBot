<script setup lang="ts">
import { nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue';
import type { NotificationCenter, NoticeTone } from '../notifications';
import '../notifications.css';
const props = defineProps<{ center: NotificationCenter }>();
const closeButton = ref<HTMLButtonElement>();
const labels: Record<NoticeTone, string> = { info: '提示', success: '成功', warning: '请留意', error: '错误', progress: '进行中' };
const marks: Record<NoticeTone, string> = { info: '▪', success: '✓', warning: '◇', error: '!', progress: '…' };
let previousFocus: HTMLElement | undefined, alive = true;
const modalOpen = () => !!document.querySelector('[aria-modal="true"]');
const time = (timestamp: number) => new Date(timestamp).toLocaleString('zh-CN', { month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', second: '2-digit', hour12: false });
watch(() => props.center.state.open, async open => {
  if (!open) return;
  previousFocus = document.activeElement instanceof HTMLElement ? document.activeElement : undefined;
  await nextTick();
  if (props.center.state.open && !modalOpen()) closeButton.value?.focus();
});
async function close() {
  props.center.setOpen(false); await nextTick();
  if (previousFocus?.isConnected && !previousFocus.closest('[inert]') && previousFocus.getClientRects().length && !modalOpen()) previousFocus.focus();
  previousFocus = undefined;
}
watch(() => props.center.state.active.map(entry => entry.id), async ids => {
  const toast = document.activeElement?.closest<HTMLElement>('[data-notification-id]');
  if (!toast || ids.includes(Number(toast.dataset.notificationId))) return;
  await nextTick();
  if (!alive || document.activeElement !== document.body || props.center.state.open || modalOpen()) return;
  const target = document.querySelector<HTMLElement>('.notification-toasts:not([inert]) .notification-close') ?? document.querySelector<HTMLElement>('[aria-controls="notification-history"]');
  if (target?.isConnected && !target.closest('[inert]') && target.getClientRects().length) target.focus();
});
function keydown(event: KeyboardEvent) {
  if (event.key === 'Escape' && props.center.state.open && !modalOpen()) { event.preventDefault(); void close(); }
}
function focusOut(id: number, event: FocusEvent) {
  if (!(event.relatedTarget instanceof Node) || !(event.currentTarget as HTMLElement).contains(event.relatedTarget)) props.center.setPaused(id, 'focus', false);
}
onMounted(() => document.addEventListener('keydown', keydown));
onBeforeUnmount(() => { alive = false; document.removeEventListener('keydown', keydown); });
</script>
<template>
  <section class="notification-toasts" aria-label="操作通知" :inert="center.state.open || undefined">
    <article v-for="entry in center.state.active" :key="entry.id" :data-notification-id="entry.id" class="notification-toast" :class="`notification-${entry.tone}`" @mouseenter="center.setPaused(entry.id, 'pointer', true)" @mouseleave="center.setPaused(entry.id, 'pointer', false)" @focusin="center.setPaused(entry.id, 'focus', true)" @focusout="focusOut(entry.id, $event)">
      <div class="notification-heading"><strong><span aria-hidden="true">{{ marks[entry.tone] }}</span> {{ labels[entry.tone] }}</strong><button type="button" class="notification-close" aria-label="关闭通知" @click="center.dismiss(entry.id)">×</button></div>
      <p :role="entry.tone === 'error' ? 'alert' : 'status'" aria-atomic="true">{{ entry.message }}</p>
      <small>{{ entry.source }} · {{ entry.tone === 'info' || entry.tone === 'success' ? '停留时暂停，关闭后可在历史查看' : '请手动关闭' }}</small>
    </article>
  </section>
  <aside v-if="center.state.open" id="notification-history" class="notification-history" aria-labelledby="notification-history-title">
    <div class="notification-history-header"><h2 id="notification-history-title">通知历史</h2><button ref="closeButton" type="button" class="notification-close" aria-label="关闭通知历史" @click="close">×</button></div>
    <p class="hint">本次页面会话最近 100 条，刷新或退出后清空。</p>
    <div class="notification-history-actions"><button type="button" :disabled="!center.unread.value" @click="center.setOpen(true)">全部已读</button><button type="button" :disabled="!center.state.history.length" @click="center.clearHistory">清空历史</button></div>
    <ol v-if="center.state.history.length" class="notification-history-list">
      <li v-for="entry in center.state.history" :key="entry.id"><div class="notification-heading"><strong><span aria-hidden="true">{{ marks[entry.tone] }}</span> {{ labels[entry.tone] }}<span v-if="entry.unread"> · 未读</span></strong><time :datetime="new Date(entry.createdAt).toISOString()">{{ time(entry.createdAt) }}</time></div><p>{{ entry.message }}</p><small>{{ entry.source }}</small></li>
    </ol>
    <p v-else class="notification-history-empty">暂无历史通知</p>
  </aside>
</template>
