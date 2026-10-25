import { useState, type Dispatch, type SetStateAction } from "react";
import { Background, Controls, MarkerType, ReactFlow, type Edge, type Node } from "@xyflow/react";
import { GitBranch, Loader2, Maximize2, Minimize2, Network, Pause, Play, Plus, RotateCcw, Save, Square, Trash2 } from "lucide-react";
import type { Command, EditablePlanNode, Plan, PlanChange, PlanLayout, Receipt } from "../types";
import { canEditPlanNode, canRemovePlanNode, connectPlanNodes, disconnectPlanNodes, filterPlanLayout, getPlanDraft, graphPositions, planEditState, planLayoutChanged, planNodeBudgetUsed, removePlanNode, repairTargets, updatePlanDrafts, updatePlanLayouts, type PlanDraft, type PlanLayoutDraft } from "../plan-state";
import { Button } from "./ui/button";
import { Status } from "./status";
import { PlanChanges } from "./plan-changes";
import { ExecutionEvidence } from "./execution-evidence";

export function PlanGraph({ plan, editableNodes, layout, selectedId, disabled, expanded = false, onSelect, onMove, onConnect, onRemoveEdge }: {
  plan: Plan; editableNodes?: EditablePlanNode[]; layout: PlanLayout; selectedId?: string; disabled: boolean;
  expanded?: boolean;
  onSelect: (id: string) => void; onMove: (id: string, position: { x: number; y: number }) => void;
  onConnect: (source: string, target: string) => void; onRemoveEdge: (source: string, target: string) => void;
}) {
  const [selectedEdgeId, setSelectedEdgeId] = useState<string>();
  const items = editableNodes ?? plan.nodes;
  const positions = graphPositions(items);
  const nodes: Node[] = items.map((node, index) => {
    const status = plan.status === "draft" ? "pending" : plan.nodes.find((item) => item.id === node.id)?.status ?? "pending";
    const locked = !canEditPlanNode(plan, node.id);
    return {
      id: node.id, selected: node.id === selectedId,
      position: layout[node.id] ?? positions.get(node.id)!,
      data: { label: (
        <div className="flow-node-content">
          <span className="flow-step">任务 {String(index + 1).padStart(2, "0")}{locked ? " · 已锁定" : ""}</span>
          <strong>{node.title || "未命名任务"}</strong>
          {(plan.agents?.length ?? 0) > 0 && <span className="flow-agent">{plan.agents?.find((agent) => agent.id === (node.agent_id || plan.default_agent_id))?.name ?? "默认角色"}</span>}
          <Status status={status} />
        </div>
      ) },
      className: `plan-node plan-${status}`,
      draggable: !disabled,
      connectable: !disabled,
    };
  });
  const ids = new Set(items.map((node) => node.id));
  const edges: Edge[] = items.flatMap((node) => node.depends_on.filter((dependency) => ids.has(dependency)).map((dependency) => ({
    id: encodeURIComponent(JSON.stringify([dependency, node.id])), source: dependency, target: node.id,
    selected: selectedEdgeId === encodeURIComponent(JSON.stringify([dependency, node.id])),
    ariaLabel: `依赖：${items.find((item) => item.id === dependency)?.title ?? dependency} → ${node.title}，双击删除`,
    type: "smoothstep", animated: plan.nodes.find((item) => item.id === node.id)?.status === "running",
    markerEnd: { type: MarkerType.ArrowClosed, color: "var(--accent)" },
    style: { stroke: "var(--accent)", strokeWidth: 1.6 }, interactionWidth: 24,
  })));
  const selectedEdge = edges.find((edge) => edge.id === selectedEdgeId);
  const removeSelectedEdge = () => {
    if (!disabled && selectedEdge) onRemoveEdge(selectedEdge.source, selectedEdge.target);
  };
  return (
    <div className="plan-graph-container">
    <div className="plan-graph" aria-label="计划编排画布" onKeyDown={(event) => {
      const target = event.target as HTMLElement;
      if (["INPUT", "TEXTAREA", "SELECT"].includes(target.tagName)) return;
      if (selectedEdge && ["Delete", "Backspace"].includes(event.key)) {
        event.preventDefault();
        event.stopPropagation();
        removeSelectedEdge();
      }
    }}>
      <ReactFlow nodes={nodes} edges={edges} fitView fitViewOptions={{ padding: 0.15, maxZoom: 1 }}
        nodesConnectable={!disabled} nodesDraggable={!disabled} elementsSelectable minZoom={0.1} maxZoom={1.8}
        zoomOnScroll={!expanded} panOnScroll={expanded}
        deleteKeyCode={null}
        onNodeClick={(_, node) => onSelect(node.id)}
        onNodesChange={(changes) => changes.forEach((change) => {
          if (change.type === "position" && change.position && !disabled) onMove(change.id, change.position);
          if (change.type === "select" && change.selected) onSelect(change.id);
        })}
        onEdgesChange={(changes) => changes.forEach((change) => {
          if (change.type === "select") setSelectedEdgeId((current) => change.selected ? change.id : current === change.id ? undefined : current);
        })}
        onConnect={(connection) => { if (connection.source && connection.target) onConnect(connection.source, connection.target); }}
        isValidConnection={(connection) => !!connection.source && !!connection.target && !!connectPlanNodes(plan, items, connection.source, connection.target)}
        onEdgeDoubleClick={(_, edge) => { if (!disabled) onRemoveEdge(edge.source, edge.target); }}
        ariaLabelConfig={{
          "controls.ariaLabel": "画布控制", "controls.zoomIn.ariaLabel": "放大画布",
          "controls.zoomOut.ariaLabel": "缩小画布", "controls.fitView.ariaLabel": "适应画布",
          "controls.interactive.ariaLabel": "切换交互",
          "node.a11yDescription.default": "按回车选择任务，查看任务详情。",
          "node.a11yDescription.keyboardDisabled": "点击任务查看详情。",
          "edge.a11yDescription.default": "按回车选择依赖，按删除键或使用下方按钮删除。已执行任务的依赖不可修改。",
        }}
      >
        <Background gap={18} size={1} color="var(--line)" />
        <Controls showInteractive={false} />
      </ReactFlow>
    </div>
    {selectedEdge && <div className="plan-edge-actions" role="status">
      <span>依赖：{items.find((node) => node.id === selectedEdge.source)?.title || selectedEdge.source} → {items.find((node) => node.id === selectedEdge.target)?.title || selectedEdge.target}</span>
      <Button size="small" variant="secondary" disabled={disabled || !canEditPlanNode(plan, selectedEdge.target)} onClick={removeSelectedEdge}><Trash2 size={12} />删除此依赖</Button>
    </div>}
    </div>
  );
}

export function PlanPanel({
  plans, selectedId, onSelect, connected, busy, onCommand, changes = [], drafts, setDrafts, layouts, setLayouts,
}: {
  plans: Plan[];
  selectedId?: string;
  onSelect: (id: string) => void;
  connected: boolean;
  busy: boolean;
  onCommand: (command: Command) => Promise<Receipt | undefined>;
  changes?: PlanChange[];
  drafts: Record<string, PlanDraft>;
  setDrafts: Dispatch<SetStateAction<Record<string, PlanDraft>>>;
  layouts: Record<string, PlanLayoutDraft>;
  setLayouts: Dispatch<SetStateAction<Record<string, PlanLayoutDraft>>>;
}) {
  const [view, setView] = useState<"tasks" | "graph">("tasks");
  const [expanded, setExpanded] = useState(false);
  const [selectedNodes, setSelectedNodes] = useState<Record<string, string>>({});
  const [graphError, setGraphError] = useState("");
  const plan = plans.find((item) => item.id === selectedId) ?? plans[0];
  if (!plan) return (
    <div className="empty-panel">
      <div className="empty-panel-icon"><Network /></div>
      <h3>把目标交给主智能体</h3>
      <p>发送目标后，主智能体会在后台生成计划。你可以调整任务与依赖，审阅后启动。</p>
    </div>
  );
  const draft = getPlanDraft(plan, drafts);
  const { editable, dirty, stale, validation, canStart } = planEditState(plan, draft);
  const active = ["running", "paused", "reviewing", "waiting_input"].includes(plan.status);
  const budgetUsed = plan.round >= plan.max_rounds;
  const nodeBudgetUsed = planNodeBudgetUsed(plan, draft.nodes);
  const selectedNodeId = draft.nodes.some((node) => node.id === selectedNodes[plan.id]) ? selectedNodes[plan.id] : draft.nodes[0]?.id;
  const layout = layouts[plan.id] ?? { revision: plan.layout_revision ?? 0, positions: plan.layout ?? {} };
  const layoutDirty = planLayoutChanged(plan, layout.positions, draft.nodes);
  const layoutStale = layoutDirty && layout.revision !== (plan.layout_revision ?? 0);
  const selectNode = (id: string) => setSelectedNodes((current) => ({ ...current, [plan.id]: id }));
  const changeLayout = (id: string, position: { x: number; y: number }) => setLayouts((current) => {
    const previous = current[plan.id] ?? { revision: plan.layout_revision ?? 0, positions: plan.layout ?? {} };
    return updatePlanLayouts(plan, draft.nodes, current, { ...previous, positions: { ...previous.positions, [id]: position } });
  });
  const edit = (nodes: EditablePlanNode[]) => {
    setGraphError("");
    setDrafts((current) => updatePlanDrafts(plan, current, { ...draft, nodes }));
  };
  const changeNode = (id: string, values: Partial<EditablePlanNode>) =>
    edit(draft.nodes.map((node) => node.id === id ? { ...node, ...values } : node));
  async function save() {
    if (validation || stale) return;
    const savedId = plan.id;
    const receipt = await onCommand({
      type: plan.status === "draft" ? "update_plan" : "patch_plan", plan_id: savedId, expected_revision: draft.revision, nodes: draft.nodes,
    });
    if (receipt) setDrafts((current) => {
      const next = { ...current };
      delete next[savedId];
      return next;
    });
  }
  async function saveLayout() {
    if (layoutStale || dirty) return;
    const savedId = plan.id;
    const receipt = await onCommand({ type: "set_plan_layout", plan_id: savedId, expected_layout_revision: layout.revision, layout: filterPlanLayout(layout.positions, plan.nodes) });
    if (receipt) setLayouts((current) => {
      const next = { ...current };
      delete next[savedId];
      return next;
    });
  }
  function addNode() {
    const id = `step-${crypto.randomUUID()}`;
    edit([...draft.nodes, { id, title: "", objective: "", depends_on: [], repairs: [], verifies: [] }]);
    selectNode(id);
  }
  return (
    <div className={`plan-panel ${expanded ? "plan-panel-expanded" : ""}`}>
      <div className="plan-canvas-heading">
        <span title={plan.title}>{expanded ? plan.title : "计划与编排"}</span>
        <Button variant="ghost" size="small" onClick={() => { setExpanded(!expanded); if (!expanded) setView("graph"); }}>
          {expanded ? <Minimize2 size={13} /> : <Maximize2 size={13} />}{expanded ? "回到对话" : "展开画布"}
        </Button>
      </div>
      {expanded && <div className="plan-expanded-summary">
        <select aria-label="画布目标计划" value={plan.id} onChange={(event) => onSelect(event.target.value)}>
          {plans.map((item) => <option key={item.id} value={item.id}>{item.title}</option>)}
        </select>
        <Status status={plan.status} /><span>版本 {plan.revision}</span><span className="plan-expanded-round">{plan.nodes.length} 个任务 · 回顾 {plan.round}/{plan.max_rounds}</span>
        {plan.execution_mode === "demo" && <span>演示 · 不修改项目</span>}
        {active && <Button variant="secondary" size="small" disabled={busy || !connected} onClick={() => void onCommand({ type: plan.status === "paused" ? "resume_plan" : "pause_plan", plan_id: plan.id, expected_revision: plan.revision })}>{plan.status === "paused" ? <Play size={12} /> : <Pause size={12} />}{plan.status === "paused" ? "恢复派发" : "暂停派发"}</Button>}
      </div>}
      <div className="plan-selector">
        <label htmlFor="plan-selection">会话计划</label>
        <select id="plan-selection" value={plan.id} onChange={(event) => onSelect(event.target.value)}>
          {plans.map((item) => <option key={item.id} value={item.id}>{item.title}</option>)}
        </select>
      </div>
      <div className="plan-overview">
        <div className="plan-overview-top"><Status status={plan.status} /><span>版本 {plan.revision}</span></div>
        <h2>{plan.title}</h2>
        <p>{plan.execution_mode === "demo" ? "演示计划 · 不修改项目" : "原生编排 · 真实项目执行"}</p>
        <div className="plan-round"><GitBranch size={13} /><span>回顾轮次 {plan.round} / {plan.max_rounds}</span><span>{plan.nodes.length} 个任务</span></div>
      </div>
      {plan.status === "draft" && <div className="plan-explanation">先审阅任务描述和依赖，再启动计划。保存不会启动执行。</div>}
      {active && <div className="plan-runtime-controls">
        <div><strong>{plan.status === "paused" ? "计划已暂停" : "运行中仍可调整计划"}</strong><p>{plan.status === "paused" ? "停止派发后续任务，已开始的任务会继续完成。保存修改后可恢复。" : "可调整尚未开始的任务、角色与依赖，也可追加修复和验收节点。"}</p></div>
        {["running", "reviewing", "waiting_input", "paused"].includes(plan.status) && <Button variant="secondary" size="small" disabled={busy || !connected} onClick={() => void onCommand({ type: plan.status === "paused" ? "resume_plan" : "pause_plan", plan_id: plan.id, expected_revision: plan.revision })}>
          {plan.status === "paused" ? <Play size={13} /> : <Pause size={13} />}{plan.status === "paused" ? "恢复派发" : "暂停派发"}
        </Button>}
        <span className="help-text">如需立即停止正在执行的任务，请取消计划。</span>
      </div>}
      {plan.status === "reviewing" && <div className="plan-explanation"><Loader2 size={13} className="spin" /> 主智能体正在回顾真实结果，决定完成、修复或请求你介入。</div>}
      {plan.status === "waiting_input" && <div className="plan-attention" role="status">
        <strong>需要你介入</strong>
        <p>{plan.error || plan.summary || "请查看执行结果，在会话中补充决定或新的目标。"}</p>
        {budgetUsed && <p>已达到本计划的回顾轮次上限。请发送补充目标，生成新计划。</p>}
      </div>}
      {plan.error && plan.status !== "waiting_input" && <div className="plan-attention" role="alert">{plan.error}</div>}
      {plan.summary && plan.status !== "waiting_input" && <p className="plan-summary">{plan.summary}</p>}
      <div className="plan-view-tabs">
        <button className={view === "tasks" ? "selected" : ""} onClick={() => setView("tasks")}>任务与依赖</button>
        <button className={view === "graph" ? "selected" : ""} onClick={() => setView("graph")}>编排画布</button>
        {dirty && <span>未保存</span>}
      </div>
      {stale && <div className="plan-attention" role="alert">执行进度或计划已更新，你的编辑已保留，暂不能保存。重新加载会用后台版本替换此处草稿，请先保留需要的内容。
        <Button variant="secondary" size="small" onClick={() => setDrafts((current) => updatePlanDrafts(plan, current))}><RotateCcw size={12} />重新加载后台版本</Button>
      </div>}
      {view === "graph" && <div className="plan-canvas">
        <p className="help-text" title="拖动节点调整布局；从下方端点拉线到目标上方建立依赖。选择连线后可按删除键或点击删除按钮，也可双击删除。按 Tab 定位任务与连线，按回车选择；节点详情也可编辑依赖。">{expanded ? "拖动排布 · 拉线建立依赖 · 选择任务编辑详情 · 滚动平移，按钮缩放" : "拖动节点调整布局；从下方端点拉线到目标上方建立依赖。选择连线后可按删除键或点击删除按钮，也可双击删除。按 Tab 定位任务与连线，按回车选择；节点详情也可编辑依赖。"}</p>
        <PlanGraph key={`${plan.id}-${expanded ? "expanded" : "compact"}`} plan={plan} editableNodes={editable ? draft.nodes : undefined} layout={layout.positions} selectedId={selectedNodeId} disabled={busy} expanded={expanded}
          onSelect={selectNode} onMove={changeLayout}
          onConnect={(source, target) => {
            const next = connectPlanNodes(plan, draft.nodes, source, target);
            if (next) edit(next);
            else setGraphError("无法添加依赖：目标已锁定，或连线会造成循环依赖。");
          }}
          onRemoveEdge={(source, target) => {
            if (busy || !canEditPlanNode(plan, target)) { setGraphError("已开始或历史任务的依赖不可修改。"); return; }
            const next = disconnectPlanNodes(plan, draft.nodes, source, target);
            if (next) edit(next);
          }}
        />
        {graphError && <p className="plan-validation" role="alert">{graphError}</p>}
        {layoutStale && <div className="plan-attention">其他入口已更新布局，本地位置已保留。重新加载会替换本地位置。
          <Button size="small" variant="secondary" onClick={() => setLayouts((current) => { const next = { ...current }; delete next[plan.id]; return next; })}>重新加载布局</Button>
        </div>}
        <div className="plan-layout-actions">
          <Button size="small" variant="secondary" disabled={busy || !connected || !layoutDirty || layoutStale || dirty} title={dirty ? "请先保存任务结构，再保存布局" : "布局单独保存，不改变计划执行版本"} onClick={() => void saveLayout()}><Save size={12} />保存布局</Button>
          <Button size="small" variant="ghost" disabled={busy} onClick={() => setLayouts((current) => updatePlanLayouts(plan, draft.nodes, current, { ...layout, positions: Object.fromEntries(graphPositions(draft.nodes)) }))}>自动布局</Button>
          {layoutDirty && <span className="help-text">{dirty ? "请先保存任务结构" : "布局未保存"} · 不影响执行</span>}
        </div>
      </div>}
      <div className={`plan-inspector ${view === "graph" ? "plan-graph-inspector" : ""}`}>
        {expanded && <>
          <div className="section-label">任务详情</div>
          {(plan.summary || plan.error || plan.status === "waiting_input") && <details className="plan-expanded-context">
            <summary>{plan.status === "waiting_input" ? "计划需要你介入" : "计划回顾与说明"}</summary>
            <p>{plan.error || plan.summary || "查看执行结果后补充要求，或重新请求回顾。"}</p>
            {budgetUsed && <p>已达到本计划的回顾轮次上限。</p>}
          </details>}
          {stale && <div className="plan-attention" role="alert">后台已有新版本，未保存草稿已保留。重新加载将替换草稿。
            <Button variant="secondary" size="small" onClick={() => setDrafts((current) => updatePlanDrafts(plan, current))}><RotateCcw size={12} />重新加载后台版本</Button>
          </div>}
        </>}
        <div className="plan-node-list">
          {(editable ? draft.nodes : plan.nodes).map((node, index) => {
            if (view === "graph" && node.id !== selectedNodeId) return null;
            const nodeEditable = editable && canEditPlanNode(plan, node.id);
            const result = plan.nodes.find((item) => item.id === node.id);
            const failureTargets = repairTargets(plan, node.id);
            const titleFor = (id: string) => plan.nodes.find((item) => item.id === id)?.title ?? draft.nodes.find((item) => item.id === id)?.title ?? id;
            return <article className={`plan-editor-node ${nodeEditable ? "" : "plan-node-locked"}`} key={node.id}>
              <div className="plan-editor-heading">
                <span>任务 {index + 1}{!nodeEditable ? " · 已锁定" : ""}</span>
                <div>{result && <Status status={result.status} />}{nodeEditable && <button type="button" aria-label={`删除任务 ${index + 1}`} title="删除尚未开始的任务" disabled={draft.nodes.length <= 1 || busy || !canRemovePlanNode(plan, draft.nodes, node.id)} onClick={() => edit(removePlanNode(draft.nodes, node.id))}><Trash2 size={13} /></button>}</div>
              </div>
              <fieldset className="plan-node-fields" disabled={busy || !nodeEditable}>
              <label>任务标题<input value={node.title} onChange={(event) => changeNode(node.id, { title: event.target.value })} /></label>
              <label>任务描述<textarea value={node.objective} rows={3} onChange={(event) => changeNode(node.id, { objective: event.target.value })} /></label>
              {!!plan.agents?.length && <label>执行角色
                <select value={node.agent_id || ""} onChange={(event) => changeNode(node.id, { agent_id: event.target.value || undefined })}>
                  <option value="">计划默认 · {plan.agents.find((agent) => agent.id === plan.default_agent_id)?.name ?? "默认角色"}</option>
                  {plan.agents.map((agent) => <option key={agent.id} value={agent.id}>{agent.name} · {agent.executor === "codex" ? "Codex CLI" : "原生工具循环"}</option>)}
                </select>
              </label>}
              <div className="dependency-label">依赖任务</div>
              {nodeEditable ? <div className="dependency-options">
                {draft.nodes.filter((item) => item.id !== node.id).map((item) => <label key={item.id}>
                  <input type="checkbox" checked={node.depends_on.includes(item.id)} onChange={(event) => changeNode(node.id, {
                    depends_on: event.target.checked ? [...node.depends_on, item.id] : node.depends_on.filter((id) => id !== item.id),
                  })} />
                  <span>{item.title || "未命名任务"}</span>
                </label>)}
                {draft.nodes.length === 1 && <span className="help-text">无其他任务，可直接执行。</span>}
              </div> : <p className="dependency-readonly">{node.depends_on.length ? node.depends_on.map((id) => plan.nodes.find((item) => item.id === id)?.title ?? id).join("、") : "无依赖"}</p>}
              {(failureTargets.length > 0 || !!node.repairs?.length || !!node.verifies?.length) && <div className="node-repair-fields">
                {(["repairs", "verifies"] as const).map((relation) => <div key={relation}>
                  <div className="dependency-label">{relation === "repairs" ? "修复的失败或受阻任务" : "验收的失败或受阻任务"}</div>
                  {nodeEditable ? <div className="dependency-options">
                    {failureTargets.map((target) => <label key={target.id}><input type="checkbox" checked={node[relation]?.includes(target.id) ?? false} onChange={(event) => changeNode(node.id, {
                      [relation]: event.target.checked ? [...(node[relation] ?? []), target.id] : (node[relation] ?? []).filter((id) => id !== target.id),
                    })} /><span>{target.title}{target.status === "blocked" ? " · 依赖受阻" : target.status === "cancelled" ? " · 已取消" : " · 失败"}{target.resolved_by ? " · 已解决" : ""}</span></label>)}
                    {!failureTargets.length && <span className="help-text">暂无失败任务</span>}
                  </div> : <p className="dependency-readonly">{node[relation]?.length ? node[relation]!.map(titleFor).join("、") : "无"}</p>}
                </div>)}
              </div>}
              {(nodeEditable || node.acceptance_command) && <label>验收命令{(node.verifies?.length ?? 0) > 0 ? " · 必填" : " · 可选"}<input value={node.acceptance_command ?? ""} placeholder="例如：pnpm test" onChange={(event) => changeNode(node.id, { acceptance_command: event.target.value || undefined })} /></label>}
              {(node.verifies?.length ?? 0) > 0 && <p className="help-text">验收节点需依赖对应的修复节点，并实际运行此命令。成功文字不会覆盖失败历史。</p>}
              </fieldset>
              {result?.resolved_by && <p className="node-resolution">失败历史已保留 · 经「{titleFor(result.resolved_by)}」验收解决</p>}
              {result?.error && <p className="error-text">{result.error}</p>}
              {result?.result && <details className="node-result"><summary>查看执行结果</summary><pre>{result.result}</pre></details>}
              <ExecutionEvidence evidence={result?.evidence} />
            </article>;
          })}
          {view === "graph" && !draft.nodes.some((node) => node.id === selectedNodeId) && <p className="help-text">点击画布中的任务，查看状态与执行证据。</p>}
          {editable && <Button variant="secondary" size="small" disabled={busy || nodeBudgetUsed >= 12} title={plan.status === "draft" ? "最多 12 个任务" : "累计最多创建 12 个任务，删除已保存任务不会恢复额度"} onClick={addNode}><Plus size={13} />追加任务 <span>{plan.status === "draft" ? "" : "累计创建 "}{nodeBudgetUsed}/12</span></Button>}
        </div>
        <PlanChanges plan={plan} changes={changes} busy={busy} connected={connected} localDirty={dirty} onCommand={onCommand} />
      </div>
      <div className="plan-footer">
      {validation && <div className="plan-validation" role="alert">{validation}</div>}
      <div className="plan-actions">
        {editable && <>
          <Button variant="secondary" size="small" disabled={busy || !connected || !dirty || stale || !!validation} onClick={() => void save()}><Save size={13} />{plan.status === "draft" ? "保存计划" : "保存后续调整"}</Button>
          {plan.status === "draft" && <Button size="small" disabled={busy || !connected || !canStart} title={dirty ? "请先保存修改，再启动计划" : "按当前保存版本启动"} onClick={() => void onCommand({ type: "start_plan", plan_id: plan.id, expected_revision: plan.revision })}><Play size={13} />启动计划</Button>}
        </>}
        {plan.status === "waiting_input" && <Button size="small" disabled={busy || !connected || budgetUsed || dirty || stale} onClick={() => void onCommand({ type: "start_plan", plan_id: plan.id, expected_revision: plan.revision })}><RotateCcw size={13} />重新请求回顾</Button>}
        {(editable || active) && <Button variant="danger" size="small" disabled={busy || !connected} onClick={() => void onCommand({ type: "cancel_plan", plan_id: plan.id })}><Square size={12} />取消计划</Button>}
      </div>
      {editable && dirty && <p className="plan-save-note">修改仅保留在此设备的编辑区。{plan.status === "draft" ? "保存成功后才能启动。" : plan.status === "paused" ? "保存后仍保持暂停，请手动恢复派发。" : "保存后后台将按最新计划派发后续任务；可先暂停再编辑。"}</p>}
      </div>
    </div>
  );
}
