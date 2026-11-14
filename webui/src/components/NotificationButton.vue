<script setup lang="ts">
import { inject } from 'vue';
import { notificationKey } from '../notifications';
import NavIcon from './NavIcon.vue';
const center = inject(notificationKey, undefined);
</script>
<template>
  <button v-if="center" type="button" class="notification-trigger" :aria-label="`通知历史，${center.unread.value} 条未读`" aria-controls="notification-history" :aria-expanded="center.state.open" @click="center.setOpen(!center.state.open)"><NavIcon name="bell" /><span>通知</span><span v-if="center.unread.value" class="notification-count">{{ center.unread.value }}</span></button>
</template>
