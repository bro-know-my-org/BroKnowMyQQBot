export const systemSettingsFixture = () => ({
  revision: 'a'.repeat(64), apply: 'restart_required', credential_presence: 'unknown',
  connections: {
    qq: { enabled: true, environment: 'production', transport: 'websocket', public_guild_messages: false, private_guild_messages: false, direct_messages: false,
      extended_events: false, forum_events: false, audio_events: false, open_forum_events: false, audio_live_member_events: false, check_only: false,
      webhook: { listen: '127.0.0.1:8080', path: '/callbacks/qq', timestamp_tolerance_seconds: 300, max_body_bytes: 1048576, max_request_concurrency: 64, request_timeout_seconds: 10 } },
    onebot11: { enabled: false, listen: '127.0.0.1:6700', allow_insecure_remote: false, action_timeout_seconds: 15, max_message_bytes: 1048576, max_pending_actions: 256 },
  },
  global: {
    runtime: { queue_capacity: 32, event_concurrency: 32, handler_timeout_seconds: 30, shutdown_timeout_seconds: 20, dedup_capacity: 16384 },
    logging: { console: { enabled: true, language: 'en', ansi: true, filter: 'info', message_content: false },
      files: { enabled: true, filter: 'debug', message_content: false, runtime_max_file_mb: 64, runtime_max_total_mb: 512, messages_max_file_mb: 256, messages_max_total_mb: 2048 } },
  },
});
