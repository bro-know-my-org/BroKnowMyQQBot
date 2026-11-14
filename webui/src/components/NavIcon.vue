<script setup lang="ts">
defineProps<{ name: string }>();
const paths: Record<string, string> = {
  overview: 'M3 3h7v7H3z M14 3h7v7h-7z M3 14h7v7H3z M14 14h7v7h-7z',
  logs: 'M5 3h14v18H5z M8 7h8 M8 11h8 M8 15h5',
  plugins: 'M3 8h6V3h6v5h6v6h-6v7H9v-7H3z',
  marketplace: 'M3 9h18l-2-6H5z M5 9v12h14V9 M9 21v-7h6v7',
  adapters: 'M8 3v5 M16 3v5 M5 8h14v3a7 7 0 0 1-14 0z M12 18v3',
  settings: 'M3 6h7 M14 6h7 M3 12h11 M18 12h3 M3 18h3 M10 18h11 M10 3v6 M14 9v6 M6 15v6',
  collapse: 'M9 6l-6 6 6 6 M3 12h18',
  expand: 'M15 6l6 6-6 6 M3 12h18',
  menu: 'M3 6h18 M3 12h18 M3 18h18',
  close: 'M5 5l14 14 M5 19L19 5',
  bell: 'M5 17h14l-2-3V9a5 5 0 0 0-10 0v5z M10 21h4 M12 2v2',
};
</script>
<template><svg class="nav-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="square" stroke-linejoin="miter" aria-hidden="true" focusable="false"><path :d="paths[name] || paths.overview" /></svg></template>
