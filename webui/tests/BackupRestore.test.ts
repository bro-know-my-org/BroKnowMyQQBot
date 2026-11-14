import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { createApp, h, nextTick, reactive } from 'vue';
import BackupRestore from '../src/components/BackupRestore.vue';
import type { Request, RestoreUpload, RestoreDiscard } from '../src/api';
import { backupPlan, ID, OLD, NEW } from './backupFixture';
const props=reactive({locked:false,bootId:OLD});
const request=vi.fn(),upload=vi.fn(),discard=vi.fn(),dirty=vi.fn(),busy=vi.fn(),restore=vi.fn();
let app:ReturnType<typeof createApp>|undefined;
const flush=async()=>{await new Promise<void>(resolve=>setImmediate(resolve));await nextTick();};
const get=<T extends HTMLElement=HTMLButtonElement>(s:string)=>document.querySelector<T>(s)!;
const click=async(s:string)=>{get<HTMLButtonElement>(s).click();await flush();};
async function mount(){const root=document.createElement('div');document.body.append(root);app=createApp({render:()=>h(BackupRestore,{...props,request:request as Request,upload:upload as RestoreUpload,discard:discard as RestoreDiscard,onDirty:dirty,onBusy:busy,onRestore:restore})});app.mount(root);await flush();}
async function choose(name='fixture.bkm-backup'){const input=get<HTMLInputElement>('#restore-archive');Object.defineProperty(input,'files',{configurable:true,value:[new File(['archive'],name)]});input.dispatchEvent(new Event('change',{bubbles:true}));await flush();}
beforeEach(()=>{vi.clearAllMocks();props.locked=false;props.bootId=OLD;request.mockReset().mockImplementation(async(_path:string,body?:unknown)=>body?{id:ID,bytes:1024,created_at_ms:1}:[]);upload.mockReset().mockResolvedValue(backupPlan());discard.mockReset().mockResolvedValue(undefined);vi.spyOn(window,'confirm').mockReturnValue(true);});
afterEach(()=>{app?.unmount();document.body.replaceChildren();vi.useRealTimers();vi.restoreAllMocks();});
it('states exact sensitive/excluded scope before export; cancel creates nothing, confirmed export exposes native download',async()=>{
  await mount();expect(document.body.textContent).toContain('secrets.env');expect(document.body.textContent).toContain('动作账本');expect(document.body.textContent).toContain('可能敏感');
  vi.mocked(window.confirm).mockReturnValue(false);await click('#create-backup');expect(request.mock.calls.filter(c=>c[1]!==undefined)).toHaveLength(0);
  vi.mocked(window.confirm).mockReturnValue(true);await click('#create-backup');expect(request.mock.calls.filter(c=>c[1]!==undefined)).toHaveLength(1);expect(request.mock.calls.find(c=>c[1]!==undefined)?.slice(0,2)).toEqual(['/api/backups',{confirm_sensitive_plugin_data:true}]);
  const link=get<HTMLAnchorElement>('a[download]');expect(link.getAttribute('href')).toBe(`/api/backups/${ID}`);expect(link.download).toContain('.bkm-backup');vi.mocked(window.confirm).mockReturnValue(false);const event=new MouseEvent('click',{bubbles:true,cancelable:true});expect(link.dispatchEvent(event)).toBe(false);expect(request.mock.calls.filter(c=>c[1]!==undefined)).toHaveLength(1);
});
it('upload only inspects; retained capabilities are shown without adding grants; exactly one final dialog can cancel or authorize',async()=>{
  await mount();await choose();expect(upload).toHaveBeenCalledOnce();expect(dirty).toHaveBeenLastCalledWith(true);expect(restore).not.toHaveBeenCalled();expect(request.mock.calls.filter(c=>c[1]!==undefined)).toHaveLength(0);expect(document.body.textContent).toContain('保持禁用');expect(document.body.textContent).toContain('本次不会新增授权');expect(document.querySelector('input[type=checkbox]')).toBeNull();
  vi.mocked(window.confirm).mockReturnValue(false);await click('#confirm-restore');expect(restore).not.toHaveBeenCalled();expect(document.querySelector('#restore-plan-title')).not.toBeNull();
  vi.mocked(window.confirm).mockClear().mockReturnValue(true);await click('#confirm-restore');expect(window.confirm).toHaveBeenCalledOnce();expect(restore).toHaveBeenCalledExactlyOnceWith(ID);expect(dirty).toHaveBeenLastCalledWith(false);expect(discard).not.toHaveBeenCalled();
});
it('upload ownership blocks duplicate actions and cancellation ignores late completion',async()=>{
  let resolve!:(v:unknown)=>void;upload.mockImplementationOnce(()=>new Promise(r=>{resolve=r;}));await mount();await choose();expect(busy).toHaveBeenLastCalledWith(true);expect(get<HTMLButtonElement>('#create-backup').disabled).toBe(true);expect(get<HTMLInputElement>('#restore-archive').disabled).toBe(true);await click('#cancel-restore-upload');expect((upload.mock.calls[0]![1] as AbortSignal).aborted).toBe(true);resolve(backupPlan());await flush();expect(document.querySelector('#confirm-restore')).toBeNull();expect(busy).toHaveBeenLastCalledWith(false);expect(restore).not.toHaveBeenCalled();
});
it('cancel inspection ticket uses only discard, recovers focus and permits safe reinspection even when discard response is lost',async()=>{
  await mount();await choose();discard.mockRejectedValueOnce(new Error('lost'));await click('#cancel-restore-plan');expect(discard.mock.calls[0]![0]).toBe(ID);expect(document.activeElement).toBe(get('#restore-archive'));expect(dirty).toHaveBeenLastCalledWith(false);expect(document.body.textContent).toContain('自动过期');expect(restore).not.toHaveBeenCalled();await choose();expect(upload).toHaveBeenCalledTimes(2);
});
it('expiry, boot change and parent dirty/busy locks stop final restoration',async()=>{
  await mount();const plan=backupPlan();plan.expires_in_seconds=1;upload.mockResolvedValueOnce(plan);vi.useFakeTimers({toFake:['setTimeout','clearTimeout','performance']});await choose();props.bootId=NEW;await nextTick();expect(get<HTMLButtonElement>('#confirm-restore').disabled).toBe(true);props.bootId=OLD;props.locked=true;await nextTick();expect(get<HTMLButtonElement>('#confirm-restore').disabled).toBe(true);props.locked=false;await vi.advanceTimersByTimeAsync(1001);await nextTick();expect(get<HTMLButtonElement>('#confirm-restore').disabled).toBe(true);await click('#confirm-restore');expect(restore).not.toHaveBeenCalled();
});
it('lost export response reconciles with one list read and never repeats export',async()=>{
  await mount();request.mockRejectedValueOnce(new Error('unknown export'));await click('#create-backup');expect(request.mock.calls.filter(c=>c[1]!==undefined)).toHaveLength(1);expect(request.mock.calls.filter(c=>c[1]===undefined)).toHaveLength(2);expect(document.body.textContent).toContain('unknown export');
});
it('failed upload can be reselected; unmount aborts owned pending checks',async()=>{
  await mount();upload.mockRejectedValueOnce(new Error('hash mismatch'));await choose();expect(document.body.textContent).toContain('hash mismatch');await choose();expect(upload).toHaveBeenCalledTimes(2);await click('#cancel-restore-plan');let resolve!:(v:unknown)=>void;upload.mockImplementationOnce(()=>new Promise(r=>{resolve=r;}));await choose();const signal=upload.mock.calls[2]![1] as AbortSignal;app?.unmount();app=undefined;expect(signal.aborted).toBe(true);resolve(backupPlan());await flush();expect(restore).not.toHaveBeenCalled();
});
