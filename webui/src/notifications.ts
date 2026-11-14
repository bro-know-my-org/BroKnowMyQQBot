import { computed, inject, onBeforeUnmount, reactive, watch, type InjectionKey } from 'vue';
import { noticeStorage } from './noticeStorage';
import { readNoticeAction, readNoticeKey, type NoticeAction } from './noticeActions';

export type NoticeTone = 'info' | 'success' | 'warning' | 'error' | 'progress';
export type NoticeSurface = 'toast' | 'history';
export interface NoticeEntry { id: number; source: string; message: string; tone: NoticeTone; createdAt: number; unread: boolean; action?: NoticeAction; key?: string }
export type NotificationCenter = ReturnType<typeof createNotifications>;
export const notificationKey: InjectionKey<NotificationCenter> = Symbol('notifications');

/** Per-session notifications. Background time never consumes a toast's lifetime. */
export function createNotifications() {
  const state = reactive<{ active: NoticeEntry[]; history: NoticeEntry[]; open: boolean }>({ active: [], history: [], open: false });
  const persistence = noticeStorage();
  let scope: string | undefined;
  const stopSaving = watch(() => state.history, entries => { if (scope) persistence.save(scope, entries); }, { deep: true, flush: 'sync' });
  const clocks = new Map<number, { remaining: number; started?: number; timer?: ReturnType<typeof setTimeout>; pauses: Set<string> }>();
  const pauses = new Map<number, Set<string>>();
  const visible = { toast: new Set<number>(), history: new Set<number>() };
  let sequence = 0, focused = document.hasFocus(), suspended = false, disposed = false;
  const foreground = () => focused && !suspended && document.visibilityState === 'visible';
  const unread = computed(() => state.history.filter(entry => entry.unread).length);
  function stop(id: number) {
    const clock = clocks.get(id); if (!clock) return;
    if (clock.timer !== undefined) clearTimeout(clock.timer);
    if (clock.started !== undefined) clock.remaining = Math.max(0, clock.remaining - (performance.now() - clock.started));
    clock.timer = undefined; clock.started = undefined;
  }
  function dismiss(id: number) { stop(id); clocks.delete(id); pauses.delete(id); visible.toast.delete(id); state.active = state.active.filter(entry => entry.id !== id); }
  function sync() {
    if (disposed) return;
    if (foreground()) for (const entry of state.history) if (visible[state.open ? 'history' : 'toast'].has(entry.id)) entry.unread = false;
    for (const [id, clock] of clocks) {
      if (!foreground() || state.open || clock.pauses.size) { stop(id); continue; }
      if (clock.timer !== undefined) continue;
      clock.started = performance.now(); clock.timer = setTimeout(() => dismiss(id), clock.remaining);
    }
  }
  function clearSource(source: string, progressOnly = false) {
    for (const entry of [...state.active]) if (entry.source === source && (!progressOnly || entry.tone === 'progress')) dismiss(entry.id);
  }
  function notify(source: string, message: string, tone: NoticeTone = 'info', options: { silent?: boolean; action?: NoticeAction; key?: string; coalesce?: boolean; baseline?: boolean } = {}) {
    if (disposed) return;
    if (tone === 'progress' && (options.silent || options.coalesce || options.baseline)) return; // Ephemeral progress has no history entry to deliver silently.
    if (!message) { if (!options.silent) clearSource(source); return; }
    const action = readNoticeAction(options.action), key = readNoticeKey(options.key);
    const previous = key ? state.history.find(entry => entry.key === key) : undefined;
    if (previous && options.baseline && previous.tone === tone) return previous.id;
    if (previous && options.coalesce) {
      Object.assign(previous, { message, tone, action, createdAt: Date.now(), unread: true });
      stop(previous.id); clocks.delete(previous.id);
      if (state.active.some(entry => entry.id === previous.id) && (tone === 'info' || tone === 'success')) clocks.set(previous.id, { remaining: 5000, pauses: pauses.get(previous.id) ?? new Set() });
      state.history = [previous, ...state.history.filter(entry => entry.id !== previous.id)];
      sync(); return previous.id;
    }
    if (!options.silent) { if (key) { if (previous) dismiss(previous.id); } else clearSource(source); }
    const entry: NoticeEntry = { id: ++sequence, source, message, tone, createdAt: Date.now(), unread: true, ...(action ? { action } : {}), ...(key ? { key } : {}) };
    if (tone !== 'progress') {
      state.history.unshift(entry);
      for (const removed of state.history.splice(100)) visible.history.delete(removed.id);
    }
    if (!options.silent) {
      state.active.push(entry);
      while (state.active.length > 3) dismiss(state.active[0]!.id);
      if (tone === 'info' || tone === 'success') clocks.set(entry.id, { remaining: 5000, pauses: new Set() });
    }
    sync();
    return entry.id;
  }
  function setPaused(id: number, reason: 'pointer' | 'focus', paused: boolean) {
    if (!state.active.some(entry => entry.id === id)) return;
    const reasons = pauses.get(id) ?? clocks.get(id)?.pauses ?? new Set<string>(); pauses.set(id, reasons);
    if (paused) reasons.add(reason); else reasons.delete(reason);
    const clock = clocks.get(id); if (clock) clock.pauses = reasons;
    sync();
  }
  function setOpen(value: boolean) { state.open = value; sync(); }
  function setVisible(id: number, surface: NoticeSurface, value: boolean) {
    if (disposed) return;
    if (value && state.history.some(entry => entry.id === id)) visible[surface].add(id); else visible[surface].delete(id);
    sync();
  }
  function markRead(id: number) { const entry = state.history.find(entry => entry.id === id); if (entry) entry.unread = false; }
  function markAllRead() { for (const entry of state.history) entry.unread = false; }
  function setSuspended(value: boolean) { suspended = value; sync(); }
  function clearHistory() { for (const entry of [...state.active]) if (entry.tone !== 'progress') dismiss(entry.id); visible.history.clear(); state.history = []; }
  function authenticate(nextScope: string | undefined, restore: boolean) {
    const previous = restore && nextScope ? persistence.load(nextScope) : [];
    reset(); scope = nextScope;
    // Storage identifiers are data, never authority over the live ID sequence.
    state.history = previous.map(entry => ({ ...entry, id: ++sequence }));
  }
  function suspendSession() { scope = undefined; reset(false); }
  function reset(clearStored = true) { scope = undefined; if (clearStored) persistence.clear(); for (const entry of [...state.active]) dismiss(entry.id); visible.history.clear(); state.history = []; state.open = false; }
  const focus = () => { focused = true; sync(); }, blur = () => { focused = false; sync(); };
  function dispose() {
    suspendSession(); stopSaving(); disposed = true;
    window.removeEventListener('focus', focus); window.removeEventListener('blur', blur); document.removeEventListener('visibilitychange', sync);
  }
  window.addEventListener('focus', focus); window.addEventListener('blur', blur); document.addEventListener('visibilitychange', sync);
  return { state, unread, notify, dismiss, clearSource, setPaused, setOpen, setVisible, markRead, markAllRead, setSuspended, clearHistory, authenticate, suspendSession, reset, dispose };
}

/** Standalone pages retain their inline feedback; the application provides the floating center. */
export function useNoticeSource(source: string, message: () => string, tone: () => NoticeTone) {
  const center = inject(notificationKey, undefined);
  if (center) {
    watch([message, tone], ([text, kind]) => center.notify(source, text, kind), { immediate: true });
    onBeforeUnmount(() => center.clearSource(source, true));
  }
  return center;
}
