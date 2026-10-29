import { useCallback, useEffect, useRef, useState } from "react";
import { HISTORY_KINDS, HistoryError, historyErrorText, parseHistoryPage, parseHistoryTask, safeHistoryId, type HistoryClient, type HistoryItem, type HistoryKind, type HistoryMessage, type HistoryMessageWindow, type HistoryPage, type HistoryTaskDetail } from "../history-client";
import { activeTask, type Command, type Delivery, type ExecutionRequest, type Plan, type PlanChange, type Receipt, type Session, type TaskRun } from "../types";
import { canContinue } from "../task-controls";
import { ExecutionEvidence } from "./execution-evidence";
import "./history-browser.css";

const LABELS: Record<HistoryKind, string> = { sessions: "会话", tasks: "任务", plans: "计划", plan_changes: "计划调整", deliveries: "回传记录", execution_requests: "问题与授权记录", messages: "会话消息" };
const STATES: Record<string, string> = { queued: "排队中", pending: "待处理", running: "执行中", waiting_input: "等待输入", completed: "已完成", failed: "失败", cancelled: "已取消", paused: "已暂停", draft: "草稿", reviewing: "回顾中", ready: "待审阅", applied: "已应用", discarded: "已丢弃", sent: "已发送", sending: "发送中", answered: "已回答", approved: "已批准", denied: "已拒绝", expired: "已过期", unknown: "结果未知" };
const status = (value: string) => STATES[value] ?? `后台状态：${value}`;
type CommandHandler = (command: Command) => Promise<Receipt | undefined>;
export interface HistoryBrowserProps {
  open: boolean; client: HistoryClient; onClose: () => void; sessionId?: string;
  /** Changing the authoritative daemon epoch drops every historical cursor. */
  epoch?: string; initialKind?: HistoryKind;
  onSelectSession?: (id: string) => void | Promise<void>;
  onCommand?: CommandHandler; disabled?: boolean;
}

export function HistoryBrowser(props: HistoryBrowserProps) {
  if (!props.open) return null;
  return <HistoryBrowserContent key={`${props.epoch ?? ""}:${props.sessionId ?? ""}:${props.initialKind ?? "tasks"}`} {...props} />;
}
function HistoryBrowserContent({ client, onClose, sessionId, epoch, initialKind = "tasks", onSelectSession, onCommand, disabled = false }: HistoryBrowserProps) {
  const [kind, setKind] = useState<HistoryKind>(initialKind);
  const [page, setPage] = useState<HistoryPage>();
  const [pageKind, setPageKind] = useState<HistoryKind>();
  const [cursor, setCursor] = useState<string>();
  const [trail, setTrail] = useState<(string | undefined)[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");
  const [observed, setObserved] = useState("");
  const [taskId, setTaskId] = useState<string>();
  const [selecting, setSelecting] = useState(false);
  const alive = useRef(false), pending = useRef(false), generation = useRef(0), selectPending = useRef(false);
  const read = useRef<AbortController | undefined>(undefined);
  const scope = kind === "sessions" ? undefined : sessionId;
  const load = useCallback(async (next?: string, previous: (string | undefined)[] = [], anchor?: string, checkEpoch = true) => {
    const version = ++generation.current;
    read.current?.abort(); read.current = new AbortController();
    pending.current = true; setLoading(true); setError(""); setTaskId(undefined);
    const query = { kind, sessionId: scope, cursor: next };
    try {
      const value = parseHistoryPage(await client.page(query, read.current.signal), query);
      if (!alive.current || version !== generation.current) return;
      const expectedEpoch = anchor ?? (checkEpoch ? epoch : undefined);
      if (expectedEpoch && value.anchor !== expectedEpoch) throw new HistoryError("history_reset");
      if (value.next_cursor && previous.includes(value.next_cursor)) throw new HistoryError("invalid");
      setPage(value); setPageKind(kind); setCursor(value.current_cursor); setTrail(previous.slice(-32)); setObserved(new Date().toLocaleString("zh-CN"));
    } catch (reason) {
      if (!alive.current || version !== generation.current) return;
      if (reason instanceof HistoryError && reason.code === "history_reset") { setPage(undefined); setCursor(undefined); setTrail([]); setObserved(""); }
      setError(historyErrorText(reason));
    } finally {
      if (alive.current && version === generation.current) { pending.current = false; setLoading(false); }
    }
  }, [client, kind, scope, epoch]);
  useEffect(() => {
    alive.current = true; setPage(undefined); setCursor(undefined); setTrail([]); setError(""); setTaskId(undefined); setObserved("");
    if (kind !== "messages" || scope) void load();
    return () => { alive.current = false; ++generation.current; read.current?.abort(); pending.current = false; };
  }, [load, kind, scope]);
  const select = async (id: string) => {
    if (!onSelectSession || !safeHistoryId(id) || selectPending.current || disabled) return;
    selectPending.current = true; setSelecting(true);
    try { await onSelectSession(id); }
    catch { if (alive.current) setError("选择会话未确认，请核对后台当前选择；没有自动重发。"); }
    finally { selectPending.current = false; if (alive.current) setSelecting(false); }
  };
  const visible = pageKind === kind ? page : undefined;
  return <aside className="history-browser" aria-label="历史浏览器">
    <header><div><h2>只读历史</h2><p>分页查看，不合并进实时对话。内容是读取时的记录，不代表持续实时状态。</p></div><button onClick={onClose} aria-label="关闭历史浏览器">关闭</button></header>
    <nav aria-label="历史类型">{HISTORY_KINDS.map(value => <button key={value} aria-pressed={kind === value} onClick={() => setKind(value)} disabled={value === "messages" && !sessionId}>{LABELS[value]}</button>)}</nav>
    <p className="history-help">范围：{scope ? `会话 ${scope}` : "全部会话"}。每页最多 80 条、2 MiB；超大记录会明确列出，不代表被删除。</p>
    {kind === "messages" && !scope && <p>请先选择一个会话，再浏览其历史消息。</p>}
    {error && <p role="alert">{error}</p>}
    <div className="history-pagination">
      <button disabled={loading || kind === "messages" && !scope} onClick={() => { if (!pending.current) void load(undefined, [], undefined, false); }}>重新读取首页</button>
      <button disabled={loading || !visible || !trail.length} onClick={() => { if (!pending.current) void load(trail[trail.length - 1], trail.slice(0, -1), visible?.anchor); }}>上一页</button>
      <button disabled={loading || !visible?.has_more} onClick={() => { if (!pending.current && visible?.next_cursor) void load(visible.next_cursor, [...trail, cursor].slice(-32), visible.anchor); }}>{kind === "messages" ? "更早一页" : "下一页"}</button>
      <button disabled={loading || !visible} onClick={() => { if (!pending.current) void load(cursor, trail, visible?.anchor); }}>刷新当前页</button>
    </div>
    <p className="history-help">只保留当前页和最近 32 个返回位置；更早记录可从首页重新浏览。关闭窗口不会撤回后台已接收的指令。</p>
    {loading && <p role="status">正在只读获取历史…</p>}
    {visible && <section aria-label="当前历史页" aria-busy={loading}>
      <p>读取时刻：{observed} · 已展示 {visible.returned} 条 / 总计 {visible.total} 条{visible.offset !== undefined && ` · 消息起始位置 ${visible.offset}`} · {visible.has_more ? kind === "messages" ? "还有更早消息" : "还有后续记录" : kind === "messages" ? "已到最早消息" : "已到当前历史末尾"}</p>
      {!visible.total && <p>后台确认此范围没有记录。</p>}
      {!!visible.total && !visible.returned && !visible.unavailable.length && <p>本页没有可展示记录；不代表整个历史为空。</p>}
      {visible.items.map(item => <HistoryRecord key={item.id} item={item} kind={kind} messages={visible.session_messages?.[item.id]} onTask={id => setTaskId(id)} onSelect={onSelectSession ? id => void select(id) : undefined} disabled={disabled || selecting || loading} />)}
      {visible.unavailable.map(item => <article className="history-record" key={item.id}><strong>记录未载入</strong><code>{item.id}</code>{item.offset !== undefined && <p>消息绝对位置 {item.offset}</p>}<p>超过单页读取上限，未截断或删除。任务可尝试单条详情；仍超限时请通过离线备份检查。</p>{kind === "tasks" && safeHistoryId(item.id) && <button disabled={loading} onClick={() => setTaskId(item.id)}>读取任务详情</button>}</article>)}
    </section>}
    {pageKind === kind && taskId && <HistoryTaskView key={taskId} client={client} id={taskId} onClose={() => setTaskId(undefined)} onHistoryReset={() => { ++generation.current; read.current?.abort(); pending.current = false; setLoading(false); setPage(undefined); setCursor(undefined); setTrail([]); setTaskId(undefined); setObserved(""); setError(historyErrorText(new HistoryError("history_reset"))); }} onCommand={onCommand} disabled={disabled} />}
  </aside>;
}

function RequestRecord({ value }: { value: ExecutionRequest }) {
  return <div><strong>{value.kind === "question" ? "问题" : "逐次授权"} · {status(value.status)}</strong><p>{value.prompt}</p><p className="history-help">任务 {value.task_id} · 修订 {value.revision} · 仅查看，不在历史页回答或批准。</p>
    {value.action_digest && <p>精确操作摘要：<code>{value.action_digest}</code></p>}
    {value.action && <details><summary>授权绑定操作（只读）</summary><pre>{JSON.stringify(value.action, null, 2)}</pre></details>}
    {value.answer !== null && <pre>{value.answer}</pre>}
  </div>;
}
function HistoryRecord({ item, kind, messages, onTask, onSelect, disabled }: { item: HistoryItem; kind: HistoryKind; messages?: HistoryMessageWindow; onTask: (id: string) => void; onSelect?: (id: string) => void; disabled: boolean }) {
  const safe = safeHistoryId(item.id);
  let content;
  if (kind === "sessions") {
    const value = item as Session;
    content = <><h3>{value.title}</h3><p>{value.project_path}</p><p>{value.created_at} · 更新 {value.updated_at}</p>{onSelect && safe && <button disabled={disabled} onClick={() => onSelect(value.id)}>选择此会话</button>}{messages && <p>消息总计 {messages.total} 条 · 本页载入 {messages.returned} 条{messages.has_more_before && " · 更早消息未载入"}</p>}<p className="history-help">这里只展示会话头；消息请到会话消息分页查看，不把此页嵌入实时对话，未载入不等于空历史。</p></>;
  } else if (kind === "tasks") {
    const value = item as TaskRun;
    content = <><h3>{value.title}</h3><p>{status(value.status)} · {value.execution_mode === "demo" ? "演示任务" : "真实执行任务"}</p><p>{value.created_at}</p>{safe && <button disabled={disabled} onClick={() => onTask(value.id)}>读取任务详情</button>}</>;
  } else if (kind === "messages") {
    const value = item as HistoryMessage;
    content = <><h3>{{ user: "用户", assistant: "助手", system: "系统" }[value.role]} · {value.created_at}</h3><p>消息绝对位置 {value.history_offset}</p><pre>{value.content}</pre>{value.task_id && <p>关联任务 {value.task_id}{safeHistoryId(value.task_id) && <button disabled={disabled} onClick={() => onTask(value.task_id!)}>读取任务详情</button>}</p>}</>;
  } else if (kind === "execution_requests") {
    content = <RequestRecord value={item as ExecutionRequest} />;
  } else if (kind === "deliveries") {
    const value = item as Delivery;
    content = <><h3>{status(value.status)} · 尝试 {value.attempts} 次</h3><p>目标 {value.target.channel_id} / {value.target.actor_id} / {value.target.conversation_id}</p><p>任务 {value.task_id}</p><details><summary>已记录的回传内容</summary><pre>{typeof value.payload === "string" ? value.payload : JSON.stringify(value.payload, null, 2)}</pre></details><p className="history-help">历史状态不是本页重新投递；本页不发送或重试消息。</p></>;
  } else {
    const value = item as Plan | PlanChange;
    content = <><h3>{"title" in value ? value.title : "计划调整"} · {status(value.status)}</h3>{"content" in value && <pre>{value.content}</pre>}<p>{"revision" in value ? `修订 ${value.revision}` : `基于修订 ${value.base_revision}`} · {value.nodes.length} 个节点</p><details><summary>节点记录（只读）</summary>{value.nodes.map(node => <section key={node.id}><strong>{node.title}</strong><pre>{node.objective}</pre><p>依赖：{node.depends_on.join("、") || "无"}</p></section>)}</details></>;
  }
  return <article className="history-record"><code>{item.id}</code>{content}{!safe && <p className="history-help">此旧记录标识不支持安全操作路径，仅展示；未丢弃记录。</p>}</article>;
}

export function HistoryTaskView({ client, id, onClose, onHistoryReset, onCommand, disabled }: { client: HistoryClient; id: string; onClose: () => void; onHistoryReset?: () => void; onCommand?: CommandHandler; disabled: boolean }) {
  const [detail, setDetail] = useState<HistoryTaskDetail>();
  const [error, setError] = useState("");
  const [observed, setObserved] = useState("");
  const [loading, setLoading] = useState(false), [busy, setBusy] = useState(false);
  const [uncertain, setUncertain] = useState(false), [notice, setNotice] = useState("");
  const [continuation, setContinuation] = useState("");
  const [showRaw, setShowRaw] = useState(false);
  const alive = useRef(false), generation = useRef(0), commandPending = useRef(false), readPending = useRef(false);
  const read = useRef<AbortController | undefined>(undefined);
  const historyReset = useRef(onHistoryReset); historyReset.current = onHistoryReset;
  const refresh = useCallback(async () => {
    const version = ++generation.current; read.current?.abort(); read.current = new AbortController();
    readPending.current = true; setLoading(true); setError("");
    try {
      const value = parseHistoryTask(await client.task(id, read.current.signal), id);
      if (!alive.current || version !== generation.current) return;
      setDetail(value); setObserved(new Date().toLocaleString("zh-CN"));
    } catch (reason) {
      if (alive.current && version === generation.current) { setError(historyErrorText(reason)); setDetail(undefined); if (reason instanceof HistoryError && reason.code === "history_reset") historyReset.current?.(); }
    } finally { if (alive.current && version === generation.current) { readPending.current = false; setLoading(false); } }
  }, [client, id]);
  useEffect(() => { alive.current = true; void refresh(); return () => { alive.current = false; ++generation.current; read.current?.abort(); }; }, [refresh]);
  async function command(value: Command) {
    if (!onCommand || commandPending.current || disabled || uncertain || loading) return;
    commandPending.current = true; setBusy(true); setNotice("");
    try {
      const receipt = await onCommand(value);
      if (!alive.current) return;
      if (receipt) { setNotice(`后台已返回指令回执 ${receipt.command_id}；受理不代表任务已完成。${receipt.task_id ? ` 任务 ${receipt.task_id}` : ""}`); setContinuation(""); await refresh(); }
      else { setUncertain(true); setNotice("没有取得指令回执；请核对所有者入口提示及后台原记录，不会自动重发或推断已执行。"); }
    } catch {
      if (alive.current) { setUncertain(true); setNotice("提交结果未确认；不会自动重发。请检查后台原指令与任务记录，不要把只读刷新当作已撤回或已完成。"); }
    } finally { commandPending.current = false; if (alive.current) setBusy(false); }
  }
  const value = detail?.task;
  return <section className="history-task-detail" aria-label={`历史任务详情 ${id}`}>
    <header><h3>任务详情（捕获记录）</h3><button onClick={onClose}>关闭任务详情</button></header>
    <code>{id}</code><p className="history-help">单条详情最多 8 MiB；未接入实时增量，不自动继续、取消或重放任务。</p>
    <button disabled={loading} onClick={() => { if (!readPending.current) void refresh(); }}>只读刷新任务详情</button>
    {loading && <p role="status">正在读取任务详情…</p>}{error && <p role="alert">{error}</p>}{notice && <p role="status">{notice}</p>}
    {value && <><p>读取时刻：{observed} · 后台事件游标 {detail.event_cursor}</p><h4>{value.title}</h4><p>{status(value.status)} · {value.execution_mode === "demo" ? "演示任务" : "真实执行任务"}</p>
      <p>会话 {value.session_id} · 创建 {value.created_at} · 更新 {value.updated_at}</p>
      <p>清理状态：{value.execution_cleanup === "settled" ? "已记录结束（详见原始记录中的后台或所有者确认来源）" : value.execution_cleanup === "pending" ? "仍待确认，任务终态不代表副作用已停止" : value.execution_cleanup === "unknown" ? "未知，需检查项目隔离与进程状态" : "未记录，不推断已清理"}</p>
      {value.error && <pre role="alert">{value.error}</pre>}{value.result !== undefined && <section><h4>任务结果</h4><pre>{value.result}</pre></section>}
      {value.stream_output !== undefined && <details><summary>已保留的执行过程回复</summary><pre>{value.stream_output}</pre>{value.stream_output_truncated && <p>过程回复达到后台既有保留上限；这不是本页截短任务结果。</p>}</details>}
      <details><summary>执行步骤 · {value.steps.length}</summary>{value.steps.map(step => <div key={step.id}><strong>{step.title} · {status(step.status)}</strong>{step.detail && <pre>{step.detail}</pre>}</div>)}</details>
      <ExecutionEvidence evidence={value.evidence} />
      <section aria-label="任务问题与授权范围"><h4>问题与授权记录（只读） · 已载入 {detail.history.execution_requests.returned} / {detail.history.execution_requests.total}</h4>
        {detail.history.execution_requests.has_more && <p role="status">还有 {detail.history.execution_requests.omitted} 条记录未载入，其中待处理 {detail.history.execution_requests.pending} 条；不能认定没有待批准。请到问题与授权历史分页查看；本页不回答或批准。</p>}
        {detail.history.execution_requests.total === 0 && <p>后台确认此任务没有问题与授权记录。</p>}
        {detail.history.execution_requests.unavailable.map(item => <p key={item.id}>请求 {item.id} 超出本次详情预算，未载入；不是已拒绝、已批准或不存在。</p>)}
        {detail.execution_requests.map(item => <article className="history-record" key={item.id}><RequestRecord value={item} /></article>)}
      </section>
      <details><summary>保留事件 · {detail.history.events.returned} / {detail.history.events.total}</summary>{detail.history.events.has_more && <p>本详情不是完整事件历史，不推断缺少的工具步骤或结果。</p>}{detail.events.map(event => <p key={event.id}>#{event.sequence} · {event.kind} · {event.message}</p>)}</details>
      <button onClick={() => setShowRaw(v => !v)}>{showRaw ? "隐藏完整任务记录" : "展示完整任务记录（只读）"}</button>{showRaw && <pre>{JSON.stringify(value, null, 2)}</pre>}
      {onCommand && <section className="history-owner-actions"><h4>所有者显式操作</h4><p>操作固定针对任务 {value.id}，仍通过现有所有者指令入口校验。记录可能已变化，以后台拒绝或回执为准。</p>
        {canContinue(value) && <><label>继续要求（可选）<textarea value={continuation} onChange={event => setContinuation(event.target.value)} disabled={busy || uncertain} /></label><button disabled={disabled || loading || busy || uncertain} onClick={() => void command({ type: "continue_task", task_id: value.id, ...(continuation.trim() ? { content: continuation.trim() } : {}) })}>继续此任务 · 创建新任务</button></>}
        {activeTask(value.status) && <button disabled={disabled || loading || busy || uncertain || value.task_kind === "plan" && (!value.plan_id || !safeHistoryId(value.plan_id))} onClick={() => void command(value.task_kind === "plan" && value.plan_id ? { type: "cancel_plan", plan_id: value.plan_id } : { type: "cancel_task", task_id: value.id })}>取消此任务</button>}
      </section>}
    </>}
  </section>;
}
