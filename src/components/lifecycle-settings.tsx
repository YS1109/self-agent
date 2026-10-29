import {useEffect,useRef,useState} from "react";
import {createLifecycleController,lifecycleRecovery,type LifecycleClient,type LifecycleRecovery,type LifecycleView} from "../lifecycle-client";
import {Button} from "./ui/button";
export function LifecycleSettings({client,disabled=false,recovery=lifecycleRecovery}:{client:LifecycleClient;disabled?:boolean;recovery?:LifecycleRecovery}){
 const [view,setView]=useState<LifecycleView>({snapshot:null,pendingId:null,reading:false,writing:false,fresh:false,storageAvailable:true,notice:""});
 const [confirmed,setConfirmed]=useState(false),[finish,setFinish]=useState(false);
 const controller=useRef<ReturnType<typeof createLifecycleController>|null>(null);
 useEffect(()=>{const current=createLifecycleController(client,recovery,setView);controller.current=current;setView(current.getState());void current.refresh();return()=>{current.dispose();if(controller.current===current)controller.current=null;};},[client,recovery]);
 const blocked=disabled||view.writing||view.reading||!view.fresh||!!view.pendingId||!view.storageAvailable||view.snapshot?.state!=="running";
 return <section className="reliability-settings" aria-label="后台生命周期"><h3>后台生命周期</h3>
 <p className="help-text">仅本机所有者可关闭整个 Rust 后台；QQ、H5 和其他窗口的运行也会停止。运行、排队及暂停中的任务和计划会取消，重启不会自动恢复；这不是暂停后继续。关闭桌面窗口本身不会停止后台。停止不会撤销已写入文件或自动重放未知操作。</p>
 <Button type="button" size="small" variant="secondary" disabled={view.reading||view.writing} onClick={()=>{setConfirmed(false);void controller.current?.refresh();}}>只读刷新后台状态</Button>
 {view.snapshot&&<p>最后观察：{view.snapshot.state==="running"?"运行中":view.snapshot.state==="draining"?"正在清理 / 停止尚未确认":"已确认停止"} · 原工作在途 {view.snapshot.workers.pending_workers} · 未确认工作 {view.snapshot.workers.unknown_workers+view.snapshot.workers.unknown_admissions}<br/>实例 <code>{view.snapshot.instance_id}</code></p>}
 {view.notice&&<p role="status" className="reliability-notice">{view.notice}</p>}
 {view.pendingId&&<><p className="execution-warning">固定观察实例 <code>{view.pendingId}</code>。只保存在本机 sessionStorage；刷新/后退只读恢复，绝不自动重发。离线或新实例不是原实例停止的证据。</p><label className="execution-checkbox"><input type="checkbox" checked={finish} disabled={view.writing} onChange={e=>setFinish(e.target.checked)}/>我理解这里只结束观察，不确认、取消或重放原清理</label><Button type="button" size="small" variant="secondary" disabled={!finish||view.writing} onClick={()=>{if(controller.current?.finishObservation(finish)){setFinish(false);setConfirmed(false);}}}>结束本机观察</Button></>}
 <label className="execution-checkbox"><input type="checkbox" checked={confirmed} disabled={blocked} onChange={e=>setConfirmed(e.target.checked)}/>我确认关闭当前已读取实例，并取消运行、排队及暂停任务/计划、停止所有入口；已写入文件保持不变</label>
 <Button type="button" size="small" disabled={blocked||!confirmed} onClick={()=>{setConfirmed(false);void controller.current?.shutdown(confirmed);}}>{view.writing?"等待原实例清理…":"只发送一次关闭请求"}</Button>
 </section>;
}
