<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, ref, watch } from 'vue';
import { createProcessControl, type ProcessAction, type ProcessSession } from '../lifecycle';
const props = defineProps<{ bootId: string; busy: boolean; dirty: boolean }>();
const emit = defineEmits<{ lock: [value: boolean]; ready: [session: ProcessSession]; authNeeded: [] }>();
const panel = ref<HTMLElement>(), restartButton = ref<HTMLButtonElement>(), stopButton = ref<HTMLButtonElement>();
const control = createProcessControl({
  // Modal ownership includes acknowledgement of a rejected request.
  lock: () => {}, ready: session => emit('ready', session), authNeeded: () => emit('authNeeded'),
});
const state = control.state;
const open = computed(() => state.value.phase !== 'idle');
const blocked = computed(() => props.busy || props.dirty || !props.bootId || open.value);
const retry = computed(() => ['timeout', 'disconnected', 'external-start'].includes(state.value.phase));
const title = computed(() => {
  if (state.value.phase === 'rejected') return '操作未接受';
  if (state.value.phase === 'timeout') return '尚未确认操作结果';
  if (state.value.phase === 'disconnected') return '服务已断开';
  if (state.value.phase === 'external-start') return '检测到新的服务进程';
  return state.value.action === 'restart' ? '正在重启 Bot' : '正在停止 Bot';
});
function request(action: ProcessAction) {
  if (blocked.value) return;
  const message = action === 'restart'
    ? '确认重启 Bot？将停止接收新事件，等待在途任务完成，重新读取配置并启动。'
    : '确认停止 Bot？将等待在途任务完成并退出。关闭后需从终端或服务管理器启动；外部服务管理器仍可能自动拉起进程。';
  if (!window.confirm(message)) return;
  void control.start(action, props.bootId, props.busy || props.dirty);
}
function keydown(event: KeyboardEvent) {
  if (event.key === 'Escape') { event.preventDefault(); if (state.value.phase === 'rejected') control.dismissRejection(); }
  if (event.key !== 'Tab') return;
  event.preventDefault(); (panel.value?.querySelector<HTMLButtonElement>('button:not(:disabled)') ?? panel.value)?.focus();
}
watch(open, async value => {
  emit('lock', value);
  await nextTick();
  if (value) panel.value?.focus();
  else {
    const button = state.value.action === 'restart' ? restartButton.value : stopButton.value;
    if (button && !button.disabled) button.focus(); else document.getElementById('page-content')?.focus();
  }
}, { flush: 'sync' });
onBeforeUnmount(control.dispose);
</script>
<template>
  <div class="process-controls" aria-label="Bot 进程控制">
    <p v-if="dirty || busy" id="process-control-guard" class="hint">请先完成当前操作并保存或放弃编辑。</p>
    <div class="process-buttons">
      <button id="restart-bot" ref="restartButton" type="button" :disabled="blocked" :aria-describedby="dirty || busy ? 'process-control-guard' : undefined" @click="request('restart')">重启 Bot</button>
      <button id="stop-bot" ref="stopButton" type="button" :disabled="blocked" :aria-describedby="dirty || busy ? 'process-control-guard' : undefined" @click="request('stop')">停止 Bot</button>
    </div>
  </div>
  <Teleport to="body">
    <div v-if="open" class="process-overlay">
      <section ref="panel" class="process-progress panel" role="dialog" aria-modal="true" aria-labelledby="process-control-title" aria-describedby="process-control-status" tabindex="-1" @keydown="keydown">
        <span class="eyebrow">进程控制</span><h2 id="process-control-title">{{ title }}</h2>
        <p id="process-control-status" role="status" aria-live="polite">{{ state.message }}</p>
        <p v-if="state.action === 'stop'" class="hint">关闭后需在终端或服务管理器中启动。外部服务管理器的自动重启策略仍可能再次拉起进程。</p>
        <p v-else class="hint">重启期间请保留此页面。仅在启动标识变化且管理服务就绪后恢复控制台；平台连接状态请查看概览。</p>
        <div v-if="retry || state.phase === 'rejected'" class="actions">
          <button v-if="retry" id="reconnect-bot" type="button" @click="control.reconnect">{{ state.action === 'stop' ? '重新连接' : '重试连接' }}</button>
          <button v-else type="button" @click="control.dismissRejection">返回控制台</button>
        </div>
      </section>
    </div>
  </Teleport>
</template>
<style scoped>
.process-controls { display: flex; justify-content: flex-end; align-items: center; flex-wrap: wrap; gap: 8px 16px; max-width: var(--page-max-width); margin: 0 auto; padding: 12px 40px 0; }
.process-controls .hint { margin: 0; }
.process-buttons { display: flex; flex-wrap: wrap; gap: 8px; }
.process-overlay { position: fixed; z-index: 150; inset: 0; display: grid; align-items: center; justify-items: center; padding: 16px; overflow: auto; background: var(--e-ink-paper); }
.process-progress { width: min(100%, 560px); border: 2px solid var(--e-ink-text); overflow-wrap: anywhere; }
.process-progress h2 + p { margin-top: 16px; }
@media (max-width: 1100px) { .process-controls { padding-inline: 24px; } }
@media (max-width: 480px) { .process-controls { padding-inline: 16px; } }
</style>
