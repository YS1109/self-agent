import { useEffect, useRef, useState } from "react";
import type { ManagedCapabilities, McpServerView } from "../capabilities-client";
import { createMcpCredentialController, type McpCredentialClient, type McpCredentialOperation, type McpCredentialState } from "../mcp-credentials-client";
import { consumeCredentialInput, credentialStateLabel } from "../providers-client";
import { Button } from "./ui/button";

type Controller = ReturnType<typeof createMcpCredentialController>;
export interface McpCredentialsProps {
  registry: ManagedCapabilities; client: McpCredentialClient; disabled?: boolean;
  onRefresh?: () => void | Promise<void>; onBusyChange?: (busy: boolean) => void;
}
export function McpCredentials({ registry, client, disabled = false, onRefresh, onBusyChange }: McpCredentialsProps) {
  const [state, setState] = useState<McpCredentialState>({ busy: false, notice: "", uncertainId: null, operation: null });
  const controller = useRef<Controller | null>(null), initial = useRef(registry), refresh = useRef(onRefresh), busy = useRef(onBusyChange);
  refresh.current = onRefresh; busy.current = onBusyChange; initial.current = registry;
  useEffect(() => {
    const current = createMcpCredentialController(client, initial.current, next => { setState(next); busy.current?.(next.busy || !!next.uncertainId || !!next.operation && (["pending", "inflight"].includes(next.operation.status) || next.operation.cleanup_status === "pending")); }, { onRefresh: () => refresh.current?.() });
    controller.current = current; current.replaceRegistry(initial.current);
    return () => { current.dispose(); busy.current?.(false); if (controller.current === current) controller.current = null; };
  }, [client]);
  useEffect(() => { controller.current?.replaceRegistry(registry); }, [registry]);
  const pending = !!state.uncertainId || !!state.operation && (["pending", "inflight"].includes(state.operation.status) || state.operation.cleanup_status === "pending");
  const blocked = disabled || state.busy || pending;
  const operations = [...(registry.credential_operations ?? [])];
  if (state.operation) { const index = operations.findIndex(op => op.id === state.operation!.id); if (index >= 0) operations[index] = state.operation; else operations.unshift(state.operation); }
  return <section className="mcp-credentials" aria-label="MCP 系统凭据槽">
    <h4>MCP 系统凭据槽</h4>
    <p className="help-text">值只经专用入口写入系统凭据存储，不进入服务参数、公开记录或技能正文。启动时只交给绑定的可信服务；MCP 是未沙箱化进程，服务本身仍可能读取、保存或发送凭据。</p>
    <p className="execution-warning">设置、轮换或删除会改变服务修订并使旧工具目录失效，必须重新明确发现工具，不会自动启动。修改程序、参数、协议或声明槽将解除全部旧绑定；停用/轮换不能召回已运行进程拿到的凭据。</p>
    {registry.mcp_servers.map(server => <div key={`${server.id}:${server.revision}`} className="mcp-credential-server"><strong>{server.name} · 修订 {server.revision}</strong>
      {!server.credential_slots.length && <p className="help-text">尚未声明槽；请在服务配置中填写环境变量名称，不要填写值或存储引用。</p>}
      {server.credential_slots.map(slot => <McpSlotCredentialForm key={slot} server={server} slot={slot} disabled={blocked || operations.some(op => op.owner_id === server.id && ["pending", "inflight"].includes(op.status))}
        onSet={(secret, confirmed) => controller.current?.setCredential(server, slot, secret, confirmed) ?? Promise.resolve(undefined)}
        onDelete={confirmed => controller.current?.deleteCredential(server, slot, confirmed) ?? Promise.resolve(undefined)} />)}
    </div>)}
    {state.notice && <p role="status" className="capability-notice">{state.notice}</p>}
    {state.uncertainId && <p className="execution-warning">未确认操作 <code>{state.uncertainId}</code>。关闭页面不会撤销后台动作；重开后先刷新审计记录，不能把找不到记录当作没有写入。</p>}
    {(state.uncertainId || state.operation) && <Button type="button" size="small" variant="secondary" disabled={disabled || state.busy} onClick={() => void controller.current?.inspect()}>只读检查原 MCP 凭据操作</Button>}
    <h5>凭据审计与孤立项清理</h5>
    <p className="help-text">包含已删除服务的待清理记录。原写入/删除结果和后续清理结果独立记录；不会将原未知结果改写为成功。</p>
    {!operations.length && <p className="help-text">暂无凭据操作记录。</p>}
    {operations.map(op => <McpCredentialOperationCard key={`${op.id}:${op.updated_at}:${op.cleanup_status}`} operation={op} disabled={blocked} onCleanup={confirmed => controller.current?.cleanupCredential(op, confirmed) ?? Promise.resolve(undefined)} />)}
  </section>;
}
export function McpSlotCredentialForm({ server, slot, disabled, onSet, onDelete }: { server: McpServerView; slot: string; disabled: boolean; onSet: (secret: string, confirmed: boolean) => Promise<unknown>; onDelete: (confirmed: boolean) => Promise<unknown> }) {
  const input = useRef<HTMLInputElement | HTMLTextAreaElement | null>(null), submitting = useRef(false);
  const [confirmed, setConfirmed] = useState(false), [deleting, setDeleting] = useState(false), [multiline, setMultiline] = useState(false);
  const state = server.credential_states?.find(item => item.slot === slot);
  return <div className="mcp-credential-slot" aria-label={`MCP 凭据槽 ${slot}`}>
    <p><code>{slot}</code> · {state ? credentialStateLabel(state.status) : "绑定状态未返回，请刷新核对"}</p>
    {state?.configured && <p className="help-text">存在绑定不代表系统存储已解锁、凭据有效或工具使用成功。</p>}
    <form className="capability-fields" autoComplete="off" onSubmit={event => {
      event.preventDefault(); const secret = consumeCredentialInput(input.current);
      if (submitting.current || disabled || !confirmed) return;
      submitting.current = true; setConfirmed(false);
      void Promise.resolve().then(() => onSet(secret, true)).finally(() => { submitting.current = false; });
    }}>
      <label>{state?.configured ? "新凭据（轮换）" : "新凭据"}{multiline
        ? <textarea ref={node => { input.current = node; }} aria-label={`${slot} 多行凭据（明文）`} autoComplete="off" spellCheck={false} autoCapitalize="none" maxLength={2048} rows={3} disabled={disabled} />
        : <input ref={node => { input.current = node; }} aria-label={`${slot} 凭据`} type="password" autoComplete="new-password" autoCapitalize="none" spellCheck={false} maxLength={2048} disabled={disabled} />}</label>
      <label className="execution-checkbox"><input type="checkbox" checked={multiline} disabled={disabled} onChange={event => { consumeCredentialInput(input.current); setConfirmed(false); setMultiline(event.target.checked); }} />使用多行明文编辑（将清空当前输入；注意屏幕隐私）</label>
      <p className="help-text">最多 2048 个 UTF-8 字节，禁止空字符。提交即清空输入，不回显；这不承诺浏览器内存可彻底擦除。</p>
      <label className="execution-checkbox"><input type="checkbox" checked={confirmed} disabled={disabled} onChange={event => setConfirmed(event.target.checked)} />我确认将此值仅绑定到 {server.name} 的 {slot}（修订 {server.revision}）</label>
      <Button type="submit" size="small" disabled={disabled || !confirmed}>提交 MCP 槽凭据</Button>
    </form>
    <label className="execution-checkbox"><input type="checkbox" checked={deleting} disabled={disabled} onChange={event => setDeleting(event.target.checked)} />确认解除此槽绑定并删除系统凭据；旧进程持有的值无法召回</label>
    <Button type="button" size="small" variant="secondary" disabled={disabled || !deleting || !state?.configured} onClick={() => { setDeleting(false); consumeCredentialInput(input.current); void onDelete(true); }}>解绑并删除此槽凭据</Button>
  </div>;
}
const operationReason = (reason: string | null) => ({ mcp_credential_detached: "程序、参数、协议或槽声明改变，旧凭据已脱离绑定。", mcp_revision_changed: "服务修订已改变，新写入未绑定当前服务。", vault_rejected: "系统凭据操作被拒绝或不可用；没有退化为明文存储。", vault_outcome_unknown: "系统操作结果无法确认，不自动重放。", daemon_interrupted_no_replay: "后台中断，原操作结果未知且没有重放。" })[reason ?? ""] ?? (reason ? "后台记录了失败或清理原因；未显示底层错误。" : "");
export function McpCredentialOperationCard({ operation: op, disabled, onCleanup }: { operation: McpCredentialOperation; disabled: boolean; onCleanup: (confirmed: boolean) => Promise<unknown> }) {
  const [confirmed, setConfirmed] = useState(false);
  return <article className="mcp-credential-operation" aria-label="MCP 凭据操作记录">
    <p><strong>{credentialStateLabel(op.status)}</strong> · {({ set: "写入", delete: "解绑并删除", cleanup: "清理" })[op.kind] ?? "操作"} · 槽 <code>{op.slot}</code></p>
    <p className="help-text">操作 <code>{op.id}</code> · 服务 <code>{op.owner_id}</code></p>
    {op.reason && <p className="help-text">{operationReason(op.reason)}</p>}
    {op.status === "unknown" && <p className="execution-warning">原操作结果仍未知；可能已产生系统副作用，不自动重发。</p>}
    {op.cleanup_status && <p className="help-text">{op.cleanup_status === "completed" ? op.status === "unknown" ? "清理已确认完成；原操作结果仍未知。" : "后续清理已确认完成；不改变原操作结果。" : op.cleanup_status === "pending" ? "清理已登记，尚未确认完成。" : op.cleanup_status === "rejected" ? "后续清理被拒绝；原操作结果未改变。" : "后续清理结果未知；不自动重放，原操作结果未改变。"}</p>}
    {op.can_cleanup && <><label className="execution-checkbox"><input type="checkbox" checked={confirmed} disabled={disabled} onChange={event => setConfirmed(event.target.checked)} />确认清理该 MCP 操作关联的孤立凭据（不是重新写入）</label><Button type="button" size="small" variant="secondary" disabled={disabled || !confirmed} onClick={() => { setConfirmed(false); void onCleanup(true); }}>清理此 MCP 凭据</Button></>}
  </article>;
}
