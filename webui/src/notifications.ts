import { computed, inject, onBeforeUnmount, reactive, watch, type InjectionKey } from 'vue';

export type NoticeTone = 'info' | 'success' | 'warning' | 'error' | 'progress';
export interface NoticeEntry { id: number; source: string; message: string; tone: NoticeTone; createdAt: number; unread: boolean }
export type NotificationCenter = ReturnType<typeof createNotifications>;
export const notificationKey: InjectionKey<NotificationCenter> = Symbol('notifications');

/** Per-session, memory-only notifications. Background time never consumes a toast's lifetime. */
export function createNotifications() {
  const state = reactive<{ active: NoticeEntry[]; history: NoticeEntry[]; open: boolean }>({ active: [], history: [], open: false });
  const clocks = new Map<number, { remaining: number; started?: number; timer?: ReturnType<typeof setTimeout>; pauses: Set<string> }>();
  let sequence = 0, focused = document.hasFocus(), suspended = false, disposed = false;
  const foreground = () => focused && !suspended && document.visibilityState === 'visible';
  const unread = computed(() => state.history.filter(entry => entry.unread).length);
  function stop(id: number) {
    const clock = clocks.get(id); if (!clock) return;
    if (clock.timer !== undefined) clearTimeout(clock.timer);
    if (clock.started !== undefined) clock.remaining = Math.max(0, clock.remaining - (performance.now() - clock.started));
    clock.timer = undefined; clock.started = undefined;
  }
  function dismiss(id: number) { stop(id); clocks.delete(id); state.active = state.active.filter(entry => entry.id !== id); }
  function sync() {
    if (disposed) return;
    if (foreground() && state.open) for (const entry of state.history) entry.unread = false;
    for (const [id, clock] of clocks) {
      if (!foreground() || state.open || clock.pauses.size) { stop(id); continue; }
      if (clock.timer !== undefined) continue;
      clock.started = performance.now(); clock.timer = setTimeout(() => dismiss(id), clock.remaining);
    }
  }
  function clearSource(source: string, progressOnly = false) {
    for (const entry of [...state.active]) if (entry.source === source && (!progressOnly || entry.tone === 'progress')) dismiss(entry.id);
  }
  function notify(source: string, message: string, tone: NoticeTone = 'info') {
    if (disposed) return;
    clearSource(source);
    if (!message) return;
    const entry: NoticeEntry = { id: ++sequence, source, message, tone, createdAt: Date.now(), unread: !(state.open && foreground()) };
    if (tone !== 'progress') {
      state.history.unshift(entry);
      state.history.splice(100);
    }
    state.active.push(entry);
    while (state.active.length > 3) dismiss(state.active[0]!.id);
    if (tone === 'info' || tone === 'success') clocks.set(entry.id, { remaining: 5000, pauses: new Set() });
    sync();
    return entry.id;
  }
  function setPaused(id: number, reason: 'pointer' | 'focus', paused: boolean) {
    const clock = clocks.get(id); if (!clock) return;
    if (paused) clock.pauses.add(reason); else clock.pauses.delete(reason);
    sync();
  }
  function setOpen(value: boolean) { state.open = value; sync(); }
  function setSuspended(value: boolean) { suspended = value; sync(); }
  function clearHistory() { for (const entry of [...state.active]) if (entry.tone !== 'progress') dismiss(entry.id); state.history = []; }
  function reset() { for (const entry of [...state.active]) dismiss(entry.id); state.history = []; state.open = false; }
  const focus = () => { focused = true; sync(); }, blur = () => { focused = false; sync(); };
  function dispose() {
    reset(); disposed = true;
    window.removeEventListener('focus', focus); window.removeEventListener('blur', blur); document.removeEventListener('visibilitychange', sync);
  }
  window.addEventListener('focus', focus); window.addEventListener('blur', blur); document.addEventListener('visibilitychange', sync);
  return { state, unread, notify, dismiss, clearSource, setPaused, setOpen, setSuspended, clearHistory, reset, dispose };
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
