import { useEffect, useRef, useState } from "react";
import { backupRequestError, createReliabilityController, diagnosticJson, localBackupRecovery, MAX_BACKUP_OBSERVATIONS, type BackupReceipt, type BackupRecovery, type ReliabilityClient, type ReliabilityState } from "../reliability-client";
import { providerUuid } from "../providers-client";
import { Button } from "./ui/button";
import "./reliability-settings.css";
const initial: ReliabilityState = { status: null, writing: false, loading: false, inspecting: false, diagnosticLoading: false, notice: "", diagnosticNotice: "", recoveryId: null, observationHistory: [], selected: null, diagnostic: null };
const STATE = { pending: "已登记 · 未完成", running: "正在导出 · 未完成", completed: "后台确认备份完成", failed: "备份失败", unknown: "备份结果未知" };
const REASON = { export_failed: "导出失败，请核对原目标目录；不会覆盖或自动重发。", cancelled: "后台报告导出取消，不代表所有临时材料都已清理。", deadline: "导出达到截止时间；请核对目标目录，不自动重发。", restarted: "后台中断后保留未知结果，未重放旧备份。", outcome_uncertain: "导出是否完成无法确认，请核对原记录与目标目录。" };
export function BackupReceiptCard({ receipt }: { receipt: BackupReceipt }) {
  return <article className="reliability-receipt" aria-label="备份操作回执"><strong>{STATE[receipt.state]}</strong><p>操作 ID：<code>{receipt.id}</code></p>
    {receipt.reason && <p className="help-text">{REASON[receipt.reason]}</p>}
    {receipt.state === "unknown" && <p className="execution-warning">目标目录可能已有材料；不要把未知当作未写入，也不要自动重发或覆盖。</p>}
    {receipt.manifest && <dl><dt>格式 / 数据库 schema</dt><dd>{receipt.manifest.format} / {receipt.manifest.schema}</dd><dt>数据库影像字节数</dt><dd>{receipt.manifest.bytes}</dd><dt>SHA-256</dt><dd><code>{receipt.manifest.sha256}</code></dd></dl>}
    {receipt.state === "completed" && <p className="help-text">摘要用于损坏检查，不认证备份来源；完成回执不等于已经验证恢复成功。</p>}
  </article>;
}
export function downloadDiagnostic(document: unknown) {
  // Revalidate at the export boundary; a snapshot or an arbitrary JSON body is never downloadable here.
  const blob = new Blob([diagnosticJson(document)], { type: "application/json;charset=utf-8" });
  const url = URL.createObjectURL(blob), anchor = window.document.createElement("a");
  try { anchor.href = url; anchor.download = "self-agent-diagnostics.json"; anchor.click(); }
  finally { setTimeout(() => URL.revokeObjectURL(url), 0); }
}
export function ReliabilitySettings({ client, disabled = false, recovery = localBackupRecovery }: { client: ReliabilityClient; disabled?: boolean; recovery?: BackupRecovery }) {
  const [state, setState] = useState<ReliabilityState>(initial), [parent, setParent] = useState(""), [name, setName] = useState(""), [confirmed, setConfirmed] = useState(false), [lookup, setLookup] = useState(""), [downloadError, setDownloadError] = useState(""), [finishConfirmed, setFinishConfirmed] = useState(false);
  const controller = useRef<ReturnType<typeof createReliabilityController> | null>(null);
  useEffect(() => { const current = createReliabilityController(client, setState, recovery); controller.current = current; current.start(); return () => { current.dispose(); if (controller.current === current) controller.current = null; }; }, [client, recovery]);
  const blocked = disabled || state.writing || !state.status?.backup_available || !!state.recoveryId || !!state.status?.backup_jobs.some(job => job.state === "pending" || job.state === "running") || (state.status?.backup_jobs.length ?? 0) >= 256 || state.observationHistory.length >= MAX_BACKUP_OBSERVATIONS;
  const jobs = [...(state.status?.backup_jobs ?? [])];
  if (state.selected && !jobs.some(job => job.id === state.selected!.id)) jobs.push(state.selected);
  return <section className="reliability-settings" aria-label="本机备份与诊断">
    <h3>本机备份与诊断</h3><p className="help-text">仅本机所有者使用，不向 QQ 或其他远端身份开放后台管理权限。配置状态、备份接受、导出完成和恢复验证是独立状态。</p>
    <div className="reliability-actions"><Button type="button" size="small" variant="secondary" disabled={disabled || state.loading} onClick={() => void controller.current?.refresh()}>{state.loading ? "读取中…" : "刷新可靠性状态"}</Button><span>{state.status ? `数据库 schema ${state.status.schema_version} · ${state.status.backup_available ? "在线备份入口可用" : "当前在线备份不可用"}` : "可靠性状态尚未确认"}</span></div>
    <h4>导出私有历史备份</h4>
    <p className="execution-warning">备份包含会话、任务、配置和可能敏感的历史文本，不是脱敏诊断。它可能包含系统凭据引用，但不包含系统 vault 中的凭据值；历史正文若曾写入秘密也可能被备份。请保存在受保护目录，不要作为诊断材料公开分享。</p>
    <form className="reliability-fields" onSubmit={event => { event.preventDefault(); if (!blocked && confirmed) { setConfirmed(false); void controller.current?.backup(parent, name, true); } }}>
      <label>后台机器上已存在的父目录（绝对路径）<input aria-label="备份父目录" value={parent} maxLength={4096} autoComplete="off" disabled={blocked} onChange={event => { setParent(event.target.value); setConfirmed(false); }} placeholder="/absolute/private/backups" /></label>
      <label>新备份名称<input aria-label="新备份名称" value={name} maxLength={80} autoComplete="off" disabled={blocked} onChange={event => { setName(event.target.value); setConfirmed(false); }} placeholder="self-agent-backup-1" /></label>
      <p className="help-text">名称限 1–80 位 ASCII 字母、数字、短横线或下划线；新建且不覆盖。单数据库影像上限 128 MiB，在线导出截止时间 30 秒，保留最多 256 条备份操作记录。取消页面观察不会取消已接受的导出。</p>
      <label className="execution-checkbox"><input type="checkbox" checked={confirmed} disabled={blocked} onChange={event => setConfirmed(event.target.checked)} />我已核对本机目标目录，确认导出可能敏感的私有历史；不会公开分享此备份</label>
      <Button type="submit" size="small" disabled={blocked || !confirmed || !!backupRequestError(parent, name)}>{state.writing ? "提交中…" : "创建一次备份"}</Button>
    </form>
    {state.notice && <p role="status" className="reliability-notice">{state.notice}</p>}
    {state.recoveryId && <p className="execution-warning">待确认操作 <code>{state.recoveryId}</code> 已保存在本机浏览器存储（只保存 UUID，不保存路径或备份内容）。关闭、后退或刷新后只读恢复，不会重复 POST。</p>}
    {state.recoveryId && <div className="reliability-receipt"><label className="execution-checkbox"><input type="checkbox" checked={finishConfirmed} disabled={disabled || state.writing || state.inspecting} onChange={event => setFinishConfirmed(event.target.checked)} />我理解旧操作可能仍在运行或已写入，将检查旧目标；这里只结束本机观察，不撤销操作、不删除后台记录。后续备份会明确选择不同的新名称。</label>
      <Button type="button" size="small" variant="secondary" disabled={disabled || !finishConfirmed || state.writing || state.inspecting} onClick={() => { if (controller.current?.finishObservation(finishConfirmed)) { setName(""); setConfirmed(false); } setFinishConfirmed(false); }}>保留 UUID 并结束本机观察</Button></div>}
    {state.observationHistory.length > 0 && <details className="reliability-receipt"><summary>已结束主动观察的操作 · {state.observationHistory.length}/{MAX_BACKUP_OBSERVATIONS}</summary><p className="help-text">不是取消或成功记录。只保留 UUID，不保存目标路径；满 64 项时停止接受新观察，不静默删除历史。</p>{state.observationHistory.map(id => <div className="reliability-actions" key={id}><code>{id}</code><Button type="button" size="small" variant="secondary" disabled={disabled || state.writing || state.inspecting} onClick={() => void controller.current?.inspect(id)}>只读核对此历史操作</Button></div>)}</details>}
    <form className="reliability-lookup" onSubmit={event => { event.preventDefault(); if (providerUuid(lookup)) void controller.current?.inspect(lookup); }}>
      <label>查询已有操作 UUID<input aria-label="备份操作 UUID" value={lookup} maxLength={36} disabled={disabled || state.writing || state.inspecting} onChange={event => setLookup(event.target.value)} /></label>
      <Button type="submit" size="small" variant="secondary" disabled={disabled || state.writing || state.inspecting || !providerUuid(lookup)}>只读查询原操作</Button>
      {state.recoveryId && <Button type="button" size="small" variant="secondary" disabled={disabled || state.writing || state.inspecting} onClick={() => void controller.current?.inspect(state.recoveryId!)}>检查待确认备份</Button>}
    </form>
    <div className="reliability-history">{jobs.length ? jobs.map(job => <BackupReceiptCard key={job.id} receipt={job} />) : <p className="help-text">{state.status ? "尚无已登记的备份记录；这不能判断未确认网络请求是否最终送达。" : "尚未读取备份历史。"}</p>}</div>
    <h4>恢复与更新边界</h4><p className="help-text">恢复仅限离线 CLI：先停止后台，独立核对可信备份和完整 SHA-256，再按 CLI 帮助确认。这里没有在线恢复或后台启停按钮；不能在后台运行时替换数据库。恢复会撤销旧执行与远端授权、禁用服务及引用，不承诺恢复系统 vault 的秘密。</p>
    <p className="help-text">未配置可信生产发布源，更新功能禁用；此页不会下载安装器、执行更新或声称已经签名发布。</p>
    <h4>固定字段诊断</h4><p className="help-text">仅导出版本、固定状态和聚合数量（最多 16 KiB），不含凭据、私有引用/摘要、标识符、路径、URL、提示词、工具输出或日志。它不是备份，也不检查或解锁系统 vault。连接检查历史未记录，不能用已配置数量或任务成功数代替连接验证。</p>
    <div className="reliability-actions"><Button type="button" size="small" variant="secondary" disabled={disabled || state.diagnosticLoading} onClick={() => { setDownloadError(""); void controller.current?.diagnostics(); }}>{state.diagnosticLoading ? "读取诊断中…" : "读取固定字段诊断"}</Button>
      <Button type="button" size="small" variant="secondary" disabled={!state.diagnostic || state.diagnosticLoading} onClick={() => { try { downloadDiagnostic(state.diagnostic); setDownloadError(""); } catch { setDownloadError("诊断下载未能启动；没有导出其他数据。"); } }}>下载已展示的诊断 JSON</Button></div>
    {(state.diagnosticNotice || downloadError) && <p role="alert" className="task-error">{state.diagnosticNotice || downloadError}</p>}
    {state.diagnostic && <pre className="reliability-diagnostic" aria-label="已校验的诊断 JSON">{diagnosticJson(state.diagnostic)}</pre>}
  </section>;
}
