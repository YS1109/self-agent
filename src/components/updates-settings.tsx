import { useEffect, useRef, useState } from "react";
import { Button } from "./ui/button";
import { providerUuid } from "../providers-client";
import { createUpdatesController, emptyUpdatesView } from "../updates-client";
import type { UpdateReceipt, UpdatesClient, UpdatesView } from "../updates-client";
import "./reliability-settings.css";
const STATES:Record<UpdateReceipt["status"],string>={pending:"等待一次上传",verifying:"正在验证与暂存",verified:"历史验证已通过",failed:"验证未通过",interrupted:"已中断，不重放",unknown:"结果未知，不重放"};
function Receipt({receipt}:{receipt:UpdateReceipt}) {
  return <article className="reliability-receipt"><strong>{STATES[receipt.status]}</strong><p>操作 UUID：<code>{receipt.id}</code></p><p>{receipt.metadata.version} · 序列 {receipt.metadata.sequence} · {receipt.metadata.target} · {receipt.metadata.bytes} 字节</p><p>产物 SHA-256：<code>{receipt.metadata.sha256}</code></p><p>信任配置版本：{receipt.config_revision}</p>{receipt.reason&&<p className="help-text">记录原因：<code>{receipt.reason}</code></p>}{receipt.status==="unknown"&&<p className="execution-warning">可能已有暂存或版本高水位记录。未知不等于未发生，不要自动重复上传。</p>}</article>;
}
export function UpdatesSettings({client,disabled=false}:{client:UpdatesClient;disabled?:boolean}) {
  const [state,setState]=useState<UpdatesView>(emptyUpdatesView),[lookup,setLookup]=useState("");
  const controller=useRef<ReturnType<typeof createUpdatesController>|null>(null);
  useEffect(()=>{const current=createUpdatesController(client,setState);controller.current=current;void current.refresh();return()=>{current.dispose();if(controller.current===current)controller.current=null;};},[client]);
  const config=state.status?.configuration;
  return <section className="reliability-settings" aria-label="本机签名验证与暂存">
    <h3>本机签名验证与暂存</h3>
    <p className="execution-warning">生产自动更新与安装始终禁用。这里只校验本机所有者提供的签名清单和产物字节；不会联网下载、解压、执行或安装。验证通过不等于已签名发布、已安装或已完成平台安装验收。</p>
    <div className="reliability-actions"><Button size="small" variant="secondary" type="button" disabled={disabled||state.loading} onClick={()=>void controller.current?.refresh()}>{state.loading?"读取中…":"刷新验证状态"}</Button><span>{state.status?.mode==="verification_only"?"仅验证信任已配置":state.status?.mode==="verification_blocked"?"验证已安全阻断":state.status?"验证信任未启用":"状态尚未确认"}</span></div>
    {state.notice&&<p role="alert" className="task-error">{state.notice}</p>}
    {state.status?.blocked_reason&&<p className="execution-warning">私有高水位见证或暂存目录无法确认：<code>{state.status.blocked_reason}</code>。未自动清理、降低版本限制或启用更新；普通任务状态不由此推断。</p>}
    {config?<dl><dt>本机信任配置版本</dt><dd>{config.revision} · {config.enabled?"仅验证启用":"已停用"}</dd><dt>公钥标识</dt><dd><code>{config.key_id}</code></dd><dt>已确认公钥指纹（SHA-256）</dt><dd><code>{config.fingerprint}</code></dd><dt>固定签名源标识（不会访问）</dt><dd><code>{config.source}</code></dd></dl>:<p className="help-text">尚无已读取的验证信任配置。请求不能自行选择信任公钥；本机所有者必须先独立核对公钥指纹和固定来源，再通过 CLI 明确配置。本轮开发仅使用临时测试密钥，不代表已选择真实发布身份。</p>}
    <p className="help-text">使用本机 CLI <code>self-agent-cli updates --help</code> 查看信任配置、一次验证上传、取消与暂存清理用法。此页只读观察，不提供不可用的“安装”按钮。页面关闭只停止观察，不取消后台已接收的验证。</p>
    {state.status?.staging&&<p className="help-text">暂存目录：{state.status.staging.files} 项、{state.status.staging.bytes} 字节 / 512 MiB。单产物最多 256 MiB；同时仅接受一次上传。历史与高水位有界保留，不自动删除来释放授权容量。</p>}
    <form className="reliability-lookup" onSubmit={event=>{event.preventDefault();if(providerUuid(lookup))void controller.current?.inspect(lookup);}}><label>原验证操作 UUID<input aria-label="验证操作 UUID" value={lookup} maxLength={36} onChange={event=>setLookup(event.target.value)} disabled={disabled||state.loading}/></label><Button type="submit" size="small" variant="secondary" disabled={disabled||state.loading||!providerUuid(lookup)}>只读复核暂存状态</Button></form>
    {state.selected&&<div aria-label="当前暂存复核"><p className="help-text">{state.selected.current_stage_verified?"本次读取已核对当前信任、高水位和实际暂存摘要；仍未执行或安装。":"当前暂存权威未确认；历史成功记录不能代替文件、权限或当前信任检查。"}</p><Receipt receipt={state.selected.receipt}/></div>}
    <details><summary>独立目标的版本高水位</summary><p className="help-text">更换公钥或来源不会降低高水位；旧 SQLite 备份恢复也必须合并数据目录外层保留的见证。这不是防恶意本机用户的硬件单调计数器。</p>{state.status?.floors.map(floor=><p key={floor.target}><code>{floor.target}</code> · {floor.version} · 序列 {floor.sequence}</p>)}</details>
    <h4>最近最多 32 条历史验证记录</h4><p className="help-text">历史回执不证明文件仍存在或当前可用；必须用原 UUID 单独复核。没有在线安装、自动恢复残留文件或重放上传。</p><div className="reliability-history">{state.status?.receipts.map(receipt=><Receipt key={receipt.id} receipt={receipt}/>)}{state.status?.receipts.length===0&&<p className="help-text">当前窗口没有验证记录。</p>}</div>
  </section>;
}
