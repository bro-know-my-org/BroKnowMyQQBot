import { afterEach, expect, it, vi } from 'vitest';
import { createApi } from '../src/api';
import { MAX_BACKUP_BYTES, readArtifacts, readRestorePlan, readRestoreResult, restoreMessage } from '../src/backupRestore';
import { backupPlan, ID } from './backupFixture';
afterEach(()=>{vi.unstubAllGlobals();vi.restoreAllMocks();});
it('validates scope, finite tickets, quotas and per-instance authority before showing a plan',()=>{
  expect(readRestorePlan(backupPlan()).operation_id).toBe(ID);
  const changes:Array<(p:ReturnType<typeof backupPlan>)=>void>=[p=>{p.settings_before={};},p=>{p.settings_after.commands={};},p=>{p.operation_id='../escape';},p=>{p.manifest.exclusions=[];},p=>{p.manifest.format_version=2;},p=>{p.manifest.state_rows=16385;},p=>{p.expires_in_seconds=0;},p=>{p.plugins[0]!.retained_grants=['arbitrary.grant'];},p=>{p.plugins.push(p.plugins[0]!);},p=>{p.affected_instances=[];},p=>{p.settings_before={commands:{prefix:'x'.repeat(4097)}};},p=>{p.settings_after={runtime:{queue_capacity:[]}};},p=>{p.settings_before={qq:{app_secret:'secret'}};},p=>{p.settings_after={logging:{files:{directory:'/host/path'}}};},p=>{p.settings_before={commands:{prefix:true}};},p=>{p.settings_before={management:{auth:'none'}};},p=>{p.settings_after={logging:{a:{b:{c:{d:{e:{f:{g:true}}}}}}}};}];
  for(const change of changes){const p=backupPlan();change(p);expect(()=>readRestorePlan(p)).toThrow();}
  expect(()=>readRestoreResult({operation_id:'other',status:'applied'},ID)).toThrow();expect(restoreMessage('unknown')).toContain('尚未确认');expect(restoreMessage('rolled_back')).toContain('回滚');
});
it('only opaque bounded artifacts can form authenticated download links',()=>{
  const item={id:ID,bytes:1024,created_at_ms:1};expect(readArtifacts([item])).toEqual([item]);
  for(const value of [[{...item,id:'../../secrets'}],[{...item,bytes:MAX_BACKUP_BYTES+1}],[item,item],Array(5).fill(item)]) expect(()=>readArtifacts(value)).toThrow();
});
it('uploads the File directly without blob copies; discard is same-origin DELETE with no token in URL',async()=>{
  const fetcher=vi.fn(async (path:string,_init:RequestInit)=>path==='/api/restores/inspect'?new Response(JSON.stringify(backupPlan())):new Response(null,{status:204}));vi.stubGlobal('fetch',fetcher);const api=createApi(vi.fn());const file=new File(['archive'],'fixture.bkm-backup');
  expect(await api.uploadRestore(file)).toEqual(backupPlan());expect(fetcher.mock.calls[0]![1]).toMatchObject({method:'POST',body:file,credentials:'same-origin',mode:'same-origin',cache:'no-store',redirect:'error',headers:{'Content-Type':'application/octet-stream'}});
  await api.discardRestore(ID);expect(fetcher.mock.calls[1]![0]).toBe(`/api/restores/${ID}`);expect(fetcher.mock.calls[1]![1]).toMatchObject({method:'DELETE',credentials:'same-origin'});api.clear();
});
it('rejects invalid filenames/oversize files and IDs before network;401 tears down',async()=>{
  const unauthorized=vi.fn();const fetcher=vi.fn(async()=>new Response('{}',{status:401}));vi.stubGlobal('fetch',fetcher);const api=createApi(unauthorized);const large=new File(['x'],'large.bkm-backup');Object.defineProperty(large,'size',{value:MAX_BACKUP_BYTES+1});
  await expect(api.uploadRestore(large)).rejects.toThrow('128 MiB');await expect(api.uploadRestore(new File(['x'],'bad.zip'))).rejects.toThrow();await expect(api.discardRestore('../host')).rejects.toThrow();expect(fetcher).not.toHaveBeenCalled();
  await expect(api.uploadRestore(new File(['x'],'ok.bkm-backup'))).rejects.toThrow('登录');expect(unauthorized).toHaveBeenCalledOnce();api.clear();
});

it('cancelling a binary upload aborts its owned request without resubmitting',async()=>{
  let signal:AbortSignal|undefined;const fetcher=vi.fn((_path:string,init:RequestInit)=>new Promise<Response>((_resolve,reject)=>{signal=init.signal as AbortSignal;signal.addEventListener('abort',()=>reject(new DOMException('aborted','AbortError')),{once:true});}));vi.stubGlobal('fetch',fetcher);
  const api=createApi(vi.fn()),owner=new AbortController();const pending=api.uploadRestore(new File(['x'],'fixture.bkm-backup'),owner.signal);owner.abort();await expect(pending).rejects.toThrow('取消');expect(signal?.aborted).toBe(true);expect(fetcher).toHaveBeenCalledOnce();api.clear();
});
