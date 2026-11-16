import type { CatalogueItem } from './types';
import { compareSemVer } from './semver';

export function marketplaceVersion(item: CatalogueItem): { allowed: boolean; label: string; message: string } {
  const installed = item.installed;
  if (!installed) return { allowed: true, label: '确认安装所选版本', message: '尚未安装默认实例。' };
  const candidate = item.latest.version;
  const saved = compareSemVer(candidate, installed.saved_version ?? installed.version);
  const loaded = installed.loaded_version === null ? 0 : compareSemVer(candidate, installed.loaded_version);
  if (saved === null || loaded === null) return { allowed: false, label: '版本无法比较', message: '版本无法比较：市场、已保存或实际加载版本不是有效的 SemVer 2（核心数字须在 u64 范围内），已禁用更新。' };
  if (loaded < 0) {
    let suffix = '';
    if (saved === 0) suffix = '此版本已保存。';
    else if (saved < 0) suffix = '也低于已保存版本。';
    return { allowed: false, label: '市场版本较旧，不降级', message: `市场版本较旧：低于实际加载版本，不降级。${suffix}` };
  }
  if (saved < 0) return { allowed: false, label: '市场版本较旧，不降级', message: '市场版本较旧：低于已保存版本，不降级。' };
  if (saved === 0) return { allowed: false, label: installed.restart_required ? '已保存此版本' : '已安装此版本', message: '此版本已安装 / 已保存，无需重复更新（构建元数据不影响版本优先级）。' };
  return { allowed: true, label: '确认更新所选版本', message: installed.loaded_version !== null && loaded === 0 ? '可修正已保存的旧版本，与实际加载版本保持一致；保存后仍请核对重启状态。' : '市场版本高于已保存版本，可更新；保存后需重启加载。' };
}
