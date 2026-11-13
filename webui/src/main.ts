import { createApp } from 'vue';
import App from './App.vue';
import { createManagementRouter } from './navigation';
import './style.css';

const router = createManagementRouter();
const app = createApp(App).use(router);
void router.isReady().then(() => app.mount('#app'));
