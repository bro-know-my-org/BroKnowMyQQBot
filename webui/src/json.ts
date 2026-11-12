// This UI uses JavaScript numbers, so it must reject integers that cannot be represented safely.
export function assertSafeNumbers(value: unknown): void {
  const pending: unknown[] = [value];
  while (pending.length) {
    const item = pending.pop();
    if (typeof item === 'number' && (!Number.isFinite(item) || (Number.isInteger(item) && !Number.isSafeInteger(item)))) {
      throw new Error('数字超出安全整数范围，已拒绝处理以避免精度丢失。');
    }
    if (item !== null && typeof item === 'object') for (const child of Object.values(item)) pending.push(child);
  }
}
export function parseSafeJson(text: string): unknown {
  const value: unknown = JSON.parse(text); assertSafeNumbers(value); return value;
}
