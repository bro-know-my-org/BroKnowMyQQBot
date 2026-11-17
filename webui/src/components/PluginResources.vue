<script setup lang="ts">
import { computed, onBeforeUnmount, ref, shallowRef, watch } from 'vue';
import SchemaEditor from './SchemaEditor.vue';
import { ApiError, type Request } from '../api';
import { boundedText, readManagement, readResourcePage, type ManagementView, type ResourcePage, type ResourceRecord, type ResourceOperation } from '../pluginManagement';
import { isObject, json, type Config } from '../types';
import '../pluginResources.css';
const props = defineProps<{ instanceId: string; updatedAt: number; revision?: string; bootId?: string; request: Request; busy: boolean }>();
const emit = defineEmits<{ dirty: [value: boolean]; busy: [value: boolean] }>();
const view = shallowRef<ManagementView>(), page = shallowRef<ResourcePage>(), selected = ref(''), search = ref(''), query = ref('');
const cursors = ref<Array<string | null>>([null]), index = ref(0), reading = ref(false), writing = ref(false), error = ref(''), message = ref('');
const protection = ref<'ready' | 'conflict' | 'unknown'>('ready');
const draft = shallowRef<{ operation: 'create' | 'update'; id: string | null; values: Config; schema: Config | null }>();
const editor = ref<InstanceType<typeof SchemaEditor>>();
const resource = computed(() => view.value?.descriptor?.resources.find(r => r.id === selected.value));
const listed = computed(() => resource.value?.operations.includes('list') && (view.value?.editable || !view.value?.unavailable_reason));
const dirty = computed(() => !!draft.value || protection.value === 'unknown');
const blocked = computed(() => props.busy || reading.value || writing.value);
const writable = computed(() => !!view.value?.editable && protection.value === 'ready' && !!page.value && !blocked.value);
watch(dirty, value => emit('dirty', value), { flush: 'sync' });
let generation = 0, alive = true, queued = false, controller: AbortController | undefined;
function cancel() { generation++; controller?.abort(); controller = undefined; reading.value = false; }
function sameGeneration(current: number) { return alive && current === generation; }
function acknowledge() { draft.value = undefined; protection.value = 'ready'; }
async function load(explicit = false, silent = false) {
  if (!alive || props.busy || writing.value) { queued = true; return; }
  if (!explicit && (dirty.value || protection.value !== 'ready')) return;
  if (explicit && dirty.value && !window.confirm('重新读取将放弃资源草稿；请先复制需要保留的内容。是否继续？')) return;
  cancel(); const current = generation, owner = new AbortController(); controller = owner; reading.value = true; queued = false;
  const instance = props.instanceId, observedRevision = props.revision, observedUpdatedAt = props.updatedAt, observedBoot = props.bootId;
  if (!silent) { error.value = ''; message.value = ''; }
  try {
    const next = readManagement(await props.request(`/api/plugins/management?${new URLSearchParams({ instance_id: instance })}`, undefined, owner.signal), instance);
    if (!sameGeneration(current) || owner.signal.aborted) return;
    const id = next.descriptor?.resources.some(r => r.id === selected.value) ? selected.value : next.descriptor?.resources[0]?.id ?? '';
    const definition = next.descriptor?.resources.find(r => r.id === id);
    let result: ResourcePage = { instance_id: instance, plugin_updated_at_ms: next.plugin_updated_at_ms, revision: next.revision, data: { records: [], next_cursor: null } };
    if (definition?.operations.includes('list') && (next.editable || !next.unavailable_reason)) {
      const params = new URLSearchParams({ instance_id: instance, resource: id, search: query.value, limit: '20' });
      const cursor = cursors.value[index.value]; if (cursor !== null && cursor !== undefined) params.set('cursor', cursor);
      result = readResourcePage(await props.request(`/api/plugins/management/resource?${params}`, undefined, owner.signal), instance, 20);
      if (result.plugin_updated_at_ms !== next.plugin_updated_at_ms || BigInt(result.revision) < BigInt(next.revision)) throw new Error('插件资源状态已变化，请重新读取。');
    }
    if (!sameGeneration(current) || owner.signal.aborted) return;
    if (observedRevision !== props.revision || observedUpdatedAt !== props.updatedAt || observedBoot !== props.bootId) { queued = true; return; }
    if (props.updatedAt > result.plugin_updated_at_ms || (props.revision !== undefined && BigInt(props.revision) > BigInt(result.revision))) throw new Error('资源快照已过期，请重新读取。');
    view.value = { ...next, revision: result.revision }; page.value = result; selected.value = id;
    if (explicit) acknowledge(); error.value = '';
  } catch (e) {
    if (sameGeneration(current) && !owner.signal.aborted) { error.value = e instanceof ApiError && e.status > 0 ? '插件资源暂不可用，请重新读取。' : (e as Error).message; if (view.value && protection.value !== 'unknown') protection.value = 'conflict'; }
  } finally { if (sameGeneration(current)) { reading.value = false; controller = undefined; pump(); } }
}
function pump() { if (queued && !blocked.value && !dirty.value && protection.value === 'ready' && !document.hidden) void load(false, true); }
watch([() => props.revision, () => props.updatedAt, () => props.bootId], (_next, previous) => {
  if (!view.value) return;
  const savedChanged = props.updatedAt !== previous[1] && props.updatedAt !== view.value.plugin_updated_at_ms;
  const dataChanged = props.revision !== undefined && BigInt(props.revision) > BigInt(view.value.revision);
  if (!savedChanged && !dataChanged && props.bootId === previous[2]) return;
  if (dirty.value || protection.value !== 'ready') { if (protection.value !== 'unknown') protection.value = 'conflict'; if (!writing.value) cancel(); return; }
  queued = true; pump();
});
watch(() => props.busy, pump);
watch(() => props.instanceId, () => {
  cancel(); view.value = undefined; page.value = undefined; draft.value = undefined; protection.value = 'ready'; selected.value = ''; search.value = ''; query.value = ''; cursors.value = [null]; index.value = 0; error.value = ''; message.value = ''; if (writing.value) { writing.value = false; emit('busy', false); } void load();
}, { immediate: true });
onBeforeUnmount(() => { alive = false; cancel(); emit('dirty', false); emit('busy', false); document.removeEventListener('visibilitychange', pump); });
document.addEventListener('visibilitychange', pump);
function allowDiscard() { return !draft.value || window.confirm('有未保存资源，是否放弃这些更改？'); }
function select(event: Event) {
  const target = event.target as HTMLSelectElement;
  if (blocked.value || protection.value !== 'ready' || !allowDiscard()) { target.value = selected.value; return; }
  draft.value = undefined; page.value = undefined; selected.value = target.value; cursors.value = [null]; index.value = 0; search.value = ''; query.value = ''; void load();
}
function browse(direction = 0) {
  if (blocked.value || protection.value !== 'ready' || draft.value) return;
  if (!boundedText(search.value, 256)) { error.value = '搜索内容最多 256 字节。'; return; }
  if (!direction) { query.value = search.value; cursors.value = [null]; index.value = 0; }
  else if (direction < 0 && index.value > 0) index.value--;
  else if (direction > 0 && page.value && page.value.data.next_cursor !== null && index.value < 99 && !cursors.value.slice(0, index.value + 1).includes(page.value.data.next_cursor)) { cursors.value = [...cursors.value.slice(0, index.value + 1), page.value.data.next_cursor]; index.value++; }
  else return;
  void load();
}
function edit(record?: ResourceRecord) {
  const r = resource.value, operation = record ? 'update' : 'create';
  if (!r || !writable.value || !r.operations.includes(operation) || !allowDiscard()) return;
  draft.value = { operation, id: record?.id ?? null, values: structuredClone(record?.values ?? {}), schema: isObject(r.schema) ? r.schema : null }; message.value = ''; error.value = '';
}
function discard() { if (!blocked.value && allowDiscard()) draft.value = undefined; }
async function mutate(operation: Exclude<ResourceOperation, 'list'>, record?: ResourceRecord) {
  const r = resource.value, snapshot = page.value;
  if (!r || !snapshot || !writable.value || !r.operations.includes(operation) || (operation === 'delete' && draft.value)) return;
  let input: Config | null = null;
  if (operation !== 'delete') {
    if (!draft.value || draft.value.operation !== operation) return;
    try { input = editor.value!.read(); } catch (e) { error.value = (e as Error).message; return; }
  } else if (!record || !window.confirm(`确认删除“${r.title}”中的记录 ${record.id}？此操作立即生效。`)) return;
  cancel(); const current = generation, owner = new AbortController(); controller = owner;
  writing.value = true; emit('busy', true); error.value = ''; message.value = ''; const observedBoot = props.bootId, observedUpdatedAt = props.updatedAt;
  try {
    const result = readResourcePage(await props.request('/api/plugins/management/resource', { instance_id: props.instanceId, resource: r.id, operation, record_id: record?.id ?? draft.value?.id ?? null, input, expected_revision: snapshot.revision, expected_updated_at_ms: snapshot.plugin_updated_at_ms, confirm: operation === 'delete' }, owner.signal), props.instanceId);
    if (!sameGeneration(current) || owner.signal.aborted) return;
    if (result.plugin_updated_at_ms !== snapshot.plugin_updated_at_ms || BigInt(result.revision) < BigInt(snapshot.revision)) throw new Error('写入响应状态异常。');
    if (props.bootId !== observedBoot || (props.updatedAt !== observedUpdatedAt && props.updatedAt !== result.plugin_updated_at_ms) || (props.revision !== undefined && BigInt(props.revision) > BigInt(result.revision))) throw new ApiError('状态已变化。', 409);
    page.value = result; view.value = { ...view.value!, revision: result.revision }; acknowledge();
    message.value = '资源已保存。'; cursors.value = [null]; index.value = 0; queued = true;
  } catch (e) {
    if (!sameGeneration(current) || owner.signal.aborted) return;
    const status = e instanceof ApiError ? e.status : 0;
    if (status === 422) error.value = '资源内容未通过校验，请修改后再保存。';
    else { protection.value = status === 409 || status === 503 ? 'conflict' : 'unknown'; error.value = status === 409 ? '资源已被其他操作更新，草稿仍保留。' : status === 503 ? '插件资源暂不可用，草稿仍保留。' : '写入结果未知，草稿仍保留。不会自动重复写入，请显式重新读取确认。'; }
  } finally { if (sameGeneration(current)) { controller = undefined; writing.value = false; emit('busy', false); pump(); } }
}
</script>
<template>
  <section v-if="view?.descriptor || error" class="detail-section plugin-resources" aria-label="插件资源">
    <h2>插件资源</h2>
    <p class="hint">管理插件声明的业务数据，保存后立即生效。</p>
    <p v-if="error" class="notice error" role="alert">{{ error }}</p><p v-if="message" role="status">{{ message }}</p>
    <p v-if="protection !== 'ready'" class="notice">{{ protection === 'unknown' ? '写入结果尚未确认。' : '数据或插件设置已变化。' }}请先复制草稿，再重新读取；当前内容不会自动覆盖。</p>
    <button type="button" data-resource="reload" :disabled="blocked" @click="load(true)">重新读取资源</button>
    <template v-if="view?.descriptor">
      <p v-if="!view.editable" class="notice">{{ view.unavailable_reason || '当前来源不支持资源写入。' }}</p>
      <label v-if="view.descriptor.resources.length > 1">资源类型<select :value="selected" :disabled="blocked || protection !== 'ready'" @change="select"><option v-for="r in view.descriptor.resources" :key="r.id" :value="r.id">{{ r.title }}</option></select></label>
      <template v-if="resource">
        <h3>{{ resource.title }}</h3><p v-if="resource.description" class="hint">{{ resource.description }}</p>
        <form v-if="listed" class="actions" @submit.prevent="browse()"><label>搜索资源<input v-model="search" data-resource="search" maxlength="256" :disabled="blocked || !!draft || protection !== 'ready'"></label><button :disabled="blocked || !!draft || protection !== 'ready'">搜索</button></form>
        <div class="resource-table" v-if="page && listed"><table><thead><tr><th scope="col">记录</th><th v-for="c in resource.columns" :key="c.key" scope="col">{{ c.title }}</th><th scope="col">操作</th></tr></thead><tbody><tr v-for="record in page.data.records" :key="record.id"><th scope="row">{{ record.id }}</th><td v-for="c in resource.columns" :key="c.key">{{ Object.hasOwn(record.values, c.key) ? (typeof record.values[c.key] === 'string' ? record.values[c.key] : json(record.values[c.key])) : '—' }}</td><td><div class="actions"><button v-if="resource.operations.includes('update')" type="button" data-resource="edit" :disabled="!writable" @click="edit(record)">编辑</button><button v-if="resource.operations.includes('delete')" type="button" data-resource="delete" :disabled="!writable || !!draft" @click="mutate('delete', record)">删除</button></div></td></tr></tbody></table><p v-if="!page.data.records.length" class="muted">暂无资源记录。</p></div>
        <div v-if="listed" class="actions"><button type="button" data-resource="previous" :disabled="blocked || !!draft || protection !== 'ready' || index === 0" @click="browse(-1)">上一页</button><span>第 {{ index + 1 }} 页 · 每页最多 20 条</span><button type="button" data-resource="next" :disabled="blocked || !!draft || protection !== 'ready' || page?.data.next_cursor == null || index >= 99 || cursors.slice(0, index + 1).includes(page.data.next_cursor)" @click="browse(1)">下一页</button></div>
        <p v-if="index >= 99" class="hint">已达 100 页浏览上限，请缩小搜索范围。</p>
        <button v-if="resource.operations.includes('create')" type="button" data-resource="create" :disabled="!writable" @click="edit()">新增{{ resource.title }}</button>
        <form v-if="draft" class="resource-draft" @submit.prevent="mutate(draft.operation)"><h3>{{ draft.operation === 'create' ? '新增记录' : `编辑记录 ${draft.id}` }}</h3><SchemaEditor ref="editor" :config="draft.values" :schema="draft.schema" id-prefix="resource-" :disabled="blocked || !view.editable" /><p class="hint">资源草稿与上方插件配置分别保存。</p><div class="actions"><button data-resource="save" class="primary" :disabled="!writable">保存资源</button><button type="button" data-resource="discard" :disabled="blocked" @click="discard">放弃资源编辑</button></div></form>
      </template>
    </template>
  </section>
</template>
