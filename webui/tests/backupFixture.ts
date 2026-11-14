import type { RestorePlan } from '../src/backupRestore';
export const OLD = '11111111-1111-4111-8111-111111111111', NEW = '22222222-2222-4222-8222-222222222222', ID = '33333333-3333-4333-8333-333333333333';
function settings(prefix: string) {
  return { commands: { prefix },
    runtime: { queue_capacity: 32, event_concurrency: 32, handler_timeout_seconds: 30, shutdown_timeout_seconds: 20, dedup_capacity: 16384 },
    qq: { enabled: true, environment: 'production', transport: 'websocket', public_guild_messages: false, private_guild_messages: false, direct_messages: false, extended_events: false, forum_events: false, audio_events: false, open_forum_events: false, audio_live_member_events: false, check_only: false,
      webhook: { path: '/callbacks/qq', timestamp_tolerance_seconds: 300, max_body_bytes: 1048576, max_request_concurrency: 64, request_timeout_seconds: 10 } },
    onebot11: { enabled: false, action_timeout_seconds: 15, max_message_bytes: 1048576, max_pending_actions: 256 },
    logging: { console: { enabled: true, language: 'en', ansi: true, filter: 'info' }, files: { enabled: true, filter: 'debug', zstd_level: 3, buffer_lines: 256, runtime_max_file_mb: 64, runtime_max_total_mb: 512, messages_max_file_mb: 256, messages_max_total_mb: 2048 } } };
}
export function backupPlan(): RestorePlan {
  return { operation_id: ID, boot_id: OLD, expires_in_seconds: 900,
    manifest: { format_version: 1, created_at_ms: 1, scope: 'bot-settings+plugin-installations+verified-packages+private-kv', exclusions: ['secrets.env','process-environment','host-paths-and-auth','logs','command-ledger','outbox','deliveries','scheduled-tasks','health'], installation_count: 1, state_rows: 1, package_count: 1 },
    plugins: [{ instance_id: 'dev.sample/default', plugin_id: 'dev.sample', version: '1.0', declaration_owned: false, requested: ['message.reply','storage.private'], retained_grants: [] }],
    affected_instances: ['dev.sample/default'], changed_settings: ['commands.prefix'], settings_before: settings('!'), settings_after: settings('/') };
}
