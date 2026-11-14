import type { LogEntry } from './types';

export function messageDirection(entry: LogEntry): string {
  if (entry.fields.direction === 'inbound') return '入站';
  if (entry.fields.direction === 'outbound') return '出站';
  return entry.fields.direction || '方向未标注';
}

export function isMessageResult(entry: LogEntry): boolean {
  return entry.fields.record_kind === 'message.sent' || entry.fields.record_kind === 'message.failed';
}
