import { ApiError, readLogPage } from './api';
import { isObject, type LogEntry } from './types';
export interface ArchiveFile { selector: string; name: string; month: string; bytes: number; compressed: boolean; available: boolean }
export interface ArchiveInventory { enabled: boolean; files: ArchiveFile[]; truncated: boolean }
export interface ArchivePage { selector: string; entries: LogEntry[]; next_cursor: string | null; skipped: number; scanned: number; limited: boolean }
const integer = (value: unknown): value is number => Number.isSafeInteger(value) && Number(value) >= 0;
const selector = (value: unknown): value is string => typeof value === 'string' && /^[a-f0-9]{64}$/.test(value);
const invalid = () => new ApiError('历史日志响应格式异常，请刷新文件列表后重试。');
export function readArchiveInventory(value: unknown): ArchiveInventory {
  if (!isObject(value) || typeof value.enabled !== 'boolean' || typeof value.truncated !== 'boolean' || !Array.isArray(value.files) || value.files.length > 128) throw invalid();
  const seen = new Set<string>();
  const files = value.files.map((file: unknown): ArchiveFile => {
    if (!isObject(file) || !selector(file.selector) || seen.has(file.selector) || typeof file.name !== 'string' || file.name.length > 192
      || !/^[\w.+-]+\.jsonl(?:\.zst)?$/.test(file.name) || typeof file.month !== 'string' || !/^\d{4}-\d{2}$/.test(file.month)
      || !integer(file.bytes) || typeof file.compressed !== 'boolean' || typeof file.available !== 'boolean') throw invalid();
    seen.add(file.selector);
    return { selector: file.selector, name: file.name, month: file.month, bytes: file.bytes, compressed: file.compressed, available: file.available };
  });
  if (!value.enabled && files.length) throw invalid();
  return { enabled: value.enabled, files, truncated: value.truncated };
}
export function readArchivePage(value: unknown, expected: string): ArchivePage {
  if (!isObject(value) || value.selector !== expected || !selector(value.selector) || !integer(value.skipped) || !integer(value.scanned)
    || value.scanned > 5000 || value.skipped > value.scanned || typeof value.limited !== 'boolean'
    || !(value.next_cursor === null || (typeof value.next_cursor === 'string' && /^[A-Za-z0-9_-]{1,160}$/.test(value.next_cursor)))
    || !Array.isArray(value.entries)) throw invalid();
  const entries = readLogPage({ session_id: expected, next_cursor: 100000, dropped: false, has_more: false, entries: value.entries }).entries;
  if (entries.some(e => e.category !== 'runtime' || e.content !== undefined) || entries.length + value.skipped !== value.scanned
    || (value.next_cursor !== null && (value.limited || !value.scanned))) throw invalid();
  return { selector: expected, entries, next_cursor: value.next_cursor as string | null, skipped: value.skipped, scanned: value.scanned, limited: value.limited };
}
