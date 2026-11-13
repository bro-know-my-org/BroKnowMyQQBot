import { createMemoryHistory, createRouter, createWebHashHistory } from 'vue-router';

export const navigation = [
  { label: '运行', items: [{ name: 'overview', label: '概览', icon: 'overview' }, { name: 'logs', label: '日志', icon: 'logs' }] },
  { label: '插件', items: [{ name: 'plugins', label: '已安装', icon: 'plugins' }, { name: 'marketplace', label: '插件市场', icon: 'marketplace' }] },
  { label: '系统', items: [{ name: 'adapters', label: '连接与适配器', icon: 'adapters' }, { name: 'settings', label: '设置', icon: 'settings' }] },
];

// The root owns the authenticated data and selects each screen by its route.
export function createManagementRouter(memory = false) {
  return createRouter({
    history: memory ? createMemoryHistory() : createWebHashHistory(),
    routes: [
      { path: '/', name: 'overview', meta: { title: '概览', section: 'overview' } },
      { path: '/plugins', name: 'plugins', meta: { title: '已安装', section: 'plugins' } },
      { path: '/plugins/:instanceId', name: 'plugin-overview', meta: { title: '插件总览', section: 'plugins' } },
      { path: '/plugins/:instanceId/config', name: 'plugin-config', meta: { title: '插件配置', section: 'plugins' } },
      { path: '/plugins/:instanceId/logs', name: 'plugin-logs', meta: { title: '插件日志', section: 'plugins' } },
      { path: '/marketplace', name: 'marketplace', meta: { title: '插件市场', section: 'marketplace' } },
      { path: '/logs', name: 'logs', meta: { title: '日志', section: 'logs' } },
      { path: '/adapters', name: 'adapters', meta: { title: '连接与适配器', section: 'adapters' } },
      { path: '/settings', name: 'settings', meta: { title: '设置', section: 'settings' } },
      { path: '/:pathMatch(.*)*', name: 'not-found', meta: { title: '页面不存在', section: '' } },
    ].map(route => ({ ...route, components: {} })),
  });
}

export const pluginRoute = (instanceId: string, tab: 'overview' | 'config' | 'logs' = 'overview') => ({ name: `plugin-${tab}`, params: { instanceId } });
