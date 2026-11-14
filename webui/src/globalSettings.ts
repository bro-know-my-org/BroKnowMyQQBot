import { flag, number, text, type Group } from './systemSettings';

export const globalGroups: Group[] = [
  { key: 'runtime', title: '运行队列与超时', hint: '队列、事件体和去重容量受组合内存预算限制；服务器会校验完整配置。', fields: [
    number('queue_capacity', '事件队列容量', 16384), number('event_concurrency', '事件处理并发', Number.MAX_SAFE_INTEGER),
    number('handler_timeout_seconds', '处理超时（秒）', 3600), number('shutdown_timeout_seconds', '关闭等待超时（秒）', 300), number('dedup_capacity', '事件去重容量', 262144),
  ] },
  { key: 'console', title: '控制台日志', fields: [
    flag('enabled', '启用控制台日志'), text('language', '控制台语言', ['en', 'zh-CN']), flag('ansi', '终端颜色（非终端自动禁用）'), text('filter', '控制台过滤器'),
    flag('message_content', '显式采集控制台消息正文', '可能包含私人消息。仅在确认终端访问权限后开启；此开关不影响 WebUI 临时正文订阅。'),
  ] },
  { key: 'files', title: '文件日志与容量保留', hint: '沿用现有日志目录与归档方式；按容量清理旧归档，不按天数保留。当前进程日志目录不变。', fields: [
    flag('enabled', '启用文件日志'), text('filter', '文件日志过滤器'),
    flag('message_content', '显式归档消息正文', '可能将私人消息保存到磁盘。请先确认目录权限与容量，再主动开启。'),
    number('runtime_max_file_mb', '运行日志单文件上限（MiB）', 1048576), number('runtime_max_total_mb', '运行日志总容量（MiB）', 1048576),
    number('messages_max_file_mb', '消息归档单文件上限（MiB）', 1048576), number('messages_max_total_mb', '消息归档总容量（MiB）', 1048576),
  ] },
];
