import { fileURLToPath } from 'node:url';
import { defineConfig } from 'vitest/config';
import vue from '@vitejs/plugin-vue';

export default defineConfig({
  plugins: [vue()],
  base: '/',
  build: {
    outDir: fileURLToPath(new URL('../src/webui', import.meta.url)),
    emptyOutDir: true,
    cssCodeSplit: false,
    rollupOptions: { output: { entryFileNames: 'app.js', assetFileNames: 'style.css', inlineDynamicImports: true } },
  },
  test: { environment: 'jsdom', clearMocks: true },
});
