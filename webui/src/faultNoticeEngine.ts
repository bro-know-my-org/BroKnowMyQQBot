import { watch } from 'vue';
import { createFaultDiff } from './faultNotices';
import type { ManagementStatus } from './status';
import type { NotificationCenter } from './notifications';
import type { SettingsScope } from './systemSettings';

/** Root-owned observer: it consumes the singleton store and never opens a stream or fetches. */
export function createFaultNotices(status: Pick<ManagementStatus, 'snapshot' | 'state'>, center: NotificationCenter, enabled: () => boolean, currentBoot: () => string) {
  const model = createFaultDiff(); let boot = '';
  function clearToasts() { for (const entry of [...center.state.active]) if (entry.key?.startsWith('status:')) center.dismiss(entry.id); }
  const stop = watch(() => [enabled(), currentBoot(), status.snapshot.value, status.state.value], () => {
    if (!enabled()) { model.reset(); boot = ''; clearToasts(); return; }
    if (boot !== currentBoot()) { model.reset(); clearToasts(); boot = currentBoot(); }
    const next = status.snapshot.value;
    if (!next || next.boot_id !== boot || status.state.value !== 'open') return;
    for (const notice of model.consume(next, Date.now())) center.notify(notice.source, notice.message, notice.tone, {
      action: notice.destination, key: `status:${boot}:${notice.key}`, silent: notice.silent,
      coalesce: notice.coalesce, baseline: notice.silent && !notice.coalesce,
    });
  }, { immediate: true, flush: 'sync' });
  function settingsSaved(savedBoot: string, scope: SettingsScope) {
    if (!enabled() || savedBoot !== currentBoot() || !/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(savedBoot)) return;
    const name = scope === 'connections' ? '连接配置' : '运行与日志设置';
    center.notify('状态 · 设置重启', `已确认保存${name}；普通设置重启后读取，环境变量覆盖仍优先。此提示不判断当前实际生效情况。`, 'warning', {
      key: `status:${savedBoot}:settings:local:${scope}`, coalesce: true, action: { page: scope === 'connections' ? 'adapters' : 'settings' },
    });
  }
  function dispose() { stop(); model.reset(); }
  return { settingsSaved, dispose };
}
