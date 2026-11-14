import type { NoticeEntry } from './notifications';
import { readNoticeAction, readNoticeKey } from './noticeActions';

export const NOTICE_STORAGE_KEY = 'bkmqb.notice-history.v1';
const MAX_PAYLOAD = 512 * 1024;
const scopeValid = (scope: unknown): scope is string => typeof scope === 'string' && /^[a-f0-9]{64}$/.test(scope);
const text = (value: unknown, limit: number): value is string => typeof value === 'string' && value.length > 0 && value.length <= limit;
function entry(value: unknown): NoticeEntry | undefined {
  if (!value || typeof value !== 'object') return;
  const v = value as Record<string, unknown>;
  if (!Number.isSafeInteger(v.id) || Number(v.id) < 1 || !text(v.source, 128) || !text(v.message, 2048)
    || !['info', 'success', 'warning', 'error'].includes(String(v.tone)) || typeof v.unread !== 'boolean'
    || !Number.isSafeInteger(v.createdAt) || Number(v.createdAt) < 0 || Number(v.createdAt) > 8640000000000000) return;
  // No API objects, tokens, clocks, progress or extra properties enter storage.
  const safe = (s: string) => s.replace(/\b(?:bearer\s+\S+|(?:token|secret|password|authorization|api[_-]?key)\s*[:=]\s*[^\s,;]+)/gi, '[REDACTED]');
  const action = readNoticeAction(v.action), key = readNoticeKey(v.key);
  return { ...(action ? { action } : {}), ...(key ? { key } : {}), id: Number(v.id), source: safe(v.source), message: safe(v.message), tone: v.tone as NoticeEntry['tone'], createdAt: Number(v.createdAt), unread: v.unread };
}

export function noticeStorage(access: () => Storage = () => window.sessionStorage) {
  const clear = () => { try { access().removeItem(NOTICE_STORAGE_KEY); } catch { /* Storage may be disabled. */ } };
  return {
    clear,
    load(scope: string): NoticeEntry[] {
      try {
        const raw = access().getItem(NOTICE_STORAGE_KEY);
        if (!raw) return [];
        if (raw.length > MAX_PAYLOAD) { clear(); return []; }
        const parsed = JSON.parse(raw) as Record<string, unknown>;
        if (!parsed || parsed.version !== 1 || !scopeValid(scope) || parsed.scope !== scope || !Array.isArray(parsed.entries) || parsed.entries.length > 100) { clear(); return []; }
        const entries = parsed.entries.map(entry);
        if (entries.some(e => !e) || new Set(entries.map(e => e!.id)).size !== entries.length) { clear(); return []; }
        return entries as NoticeEntry[];
      } catch { clear(); return []; }
    },
    save(scope: string, entries: NoticeEntry[]) {
      if (!scopeValid(scope)) { clear(); return; }
      try {
        const payload = JSON.stringify({ version: 1, scope, entries: entries.slice(0, 100).map(entry).filter(Boolean) });
        if (payload.length <= MAX_PAYLOAD) access().setItem(NOTICE_STORAGE_KEY, payload); else clear();
      } catch { clear(); }
    },
  };
}
