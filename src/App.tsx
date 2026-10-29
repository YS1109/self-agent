import {GatewaySettings} from "./components/gateway-settings";
import {EMPTY_GATEWAY} from "./gateway-client";
import { ReliabilitySettings } from "./components/reliability-settings";
import {QqSettings} from "./components/qq-settings";
import {EMPTY_QQ} from "./qq-client";
import { EMPTY_PROVIDERS, namedModelRequest } from "./providers-client";
import { ProvidersSettings } from "./components/providers-settings";
import {
  useEffect,
  useRef,
  useState,
  type FormEvent,
  type ReactNode,
} from "react";
import * as Dialog from "@radix-ui/react-dialog";
import {
  ArrowDown,
  ArrowUp,
  ArrowUpRight,
  Blocks,
  Check,
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  CircleDot,
  Code2,
  FileCode2,
  Folder,
  FolderOpen,
  GitBranch,
  Globe2,
  Layers3,
  Loader2,
  MessageSquare,
  Moon,
  Network,
  PanelBottom,
  PanelLeft,
  PanelRight,
  Plus,
  Radio,
  RefreshCw,
  Send,
  Settings2,
  ShieldCheck,
  Sparkles,
  Square,
  Sun,
  Terminal,
  Waypoints,
  WifiOff,
  X,
  Zap,
} from "lucide-react";
import { api, dispatch, capabilityClient, providersClient, mcpCredentialClient, qqClient, qqCredentialClient, gatewayClient, gatewayCredentialClient, reliabilityClient } from "./api";
import { useDaemonSnapshot } from "./use-daemon-snapshot";
import { LiveTaskMessage, ToolActivity, TaskUsage, TaskContext } from "./components/task-stream";
import { TaskSupplements, ContinueTask } from "./components/task-controls";
import { TaskChanges } from "./components/task-changes";
import { canContinue, canSteer, createCommandGate, inputCommand, type InputTarget } from "./task-controls";
import { PlanPanel } from "./components/plan-panel";
import { PlanChanges } from "./components/plan-changes";
import { ExecutionEvidence } from "./components/execution-evidence";
import { Status } from "./components/status";
import { AgentSettings, DEFAULT_AGENT, normalizeSettings } from "./components/agent-settings";
import { QuickModelSelect, useModelCatalog } from "./components/model-select";
import { ProviderSettings } from "./components/provider-settings";
import { ExecutionCleanupNotice, ExecutionSettings, TaskExecutionDetails } from "./components/execution-settings";
import { CapabilitiesSettings, CapabilityScopeSummary } from "./components/capabilities-settings";
import { EMPTY_CAPABILITIES } from "./capabilities-client";
import { ExecutionRequests } from "./components/execution-requests";
import { defaultExecutionControls, controlsForTask, executionControlsError, settingsExecutionError } from "./execution-controls";
import { ModelContextSettings } from "./components/model-context-settings";
import { applyUiScale, DEFAULT_UI_SCALE, readUiScale, saveUiScale, UI_SCALE_OPTIONS } from "./ui-preferences";
import { syncNativeTheme, type AppTheme } from "./theme";
import { capabilityLabel, eventLabel, kindLabel, localizeSystemText } from "./labels";
import { Button } from "./components/ui/button";
import { Select } from "./components/ui/select";
import { agentModelLabel } from "./agent-model";
import { discardCleanPlanDraft, hasUnsavedPlanDraft, selectedSessionPlan, updatePlanLayouts, type PlanDraft, type PlanLayoutDraft } from "./plan-state";
import {
  activeTask,
  DESKTOP_SOURCE,
  REMOTE_SOURCE,
  sameSource,
  type Command,
  type Settings,
  type SourceContext,
  type TaskRun,
  type TaskControls,
} from "./types";

type Panel = "plan" | "changes" | "artifacts" | "task";
type Modal = "new" | "settings" | "remote" | "capabilities" | "providers" | "qq" | "gateway" | "reliability" | null;
const time = (value: string) =>
  new Date(value).toLocaleTimeString("zh-CN", {
    hour: "2-digit",
    minute: "2-digit",
  });
const projectName = (path: string) =>
  path.split(/[\\/]/).filter(Boolean).at(-1) ?? "未设置项目";
const errorMessage = (value: unknown) =>
  value instanceof Error ? value.message : String(value);

function ModalFrame({
  open,
  onClose,
  title,
  description,
  children,
  wide = false,
  error,
}: {
  error?: string;
  open: boolean;
  onClose: () => void;
  title: string;
  description: string;
  children: ReactNode;
  wide?: boolean;
}) {
  return (
    <Dialog.Root open={open} onOpenChange={(v) => !v && onClose()}>
      <Dialog.Portal>
        <Dialog.Overlay className="modal-overlay" />
        <Dialog.Content className={`modal ${wide ? "modal-wide" : ""}`}>
          <div className="modal-heading">
            <div>
              <Dialog.Title>{title}</Dialog.Title>
              <Dialog.Description>{description}</Dialog.Description>
            </div>
            <Dialog.Close asChild>
              <Button variant="ghost" size="icon" aria-label="关闭">
                <X size={18} />
              </Button>
            </Dialog.Close>
          </div>
          {error && (
            <div className="modal-error" role="alert">
              {error}
            </div>
          )}
          {children}
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
function TaskCard({
  task,
  selected,
  onClick,
}: {
  task: TaskRun;
  selected: boolean;
  onClick: () => void;
}) {
  const done = task.steps.filter((step) => step.status === "completed").length;
  return (
    <button
      className={`task-card ${selected ? "task-selected" : ""}`}
      onClick={onClick}
    >
      <div className="task-icon">
        <Waypoints size={18} />
      </div>
      <div className="task-card-content">
        <div className="task-card-top">
          <span>{task.task_kind === "main" ? "主智能体执行" : task.task_kind === "plan" ? "计划执行" : task.plan_id ? "计划子任务" : "远程派发"}</span>
          <Status status={task.status} />
        </div>
        <strong>{task.title}</strong>
        <div className="task-card-meta">
          <span>
            {task.execution_mode === "demo"
              ? "演示模式 · 模拟执行"
              : task.task_kind === "plan" ? "主智能体编排" : task.agent ? `${task.agent.name} · ${task.agent.executor === "codex" ? "Codex CLI" : "原生执行器"}` : "原生执行器"}
          </span>
          <span>
            {done}/{task.steps.length} 步骤
          </span>
        </div>
        {task.agent && <div className="task-model-preset" title={`模型预设：${agentModelLabel(task.agent)}`}>模型预设：{agentModelLabel(task.agent)}</div>}
        <ExecutionCleanupNotice task={task} />
        {task.steps.length > 0 && (
          <div className="task-progress">
            <span style={{ width: `${(done / task.steps.length) * 100}%` }} />
          </div>
        )}
      </div>
      <ChevronRight size={16} className="muted" />
    </button>
  );
}
function EmptyPanel({
  icon,
  title,
  detail,
}: {
  icon: ReactNode;
  title: string;
  detail: string;
}) {
  return (
    <div className="empty-panel">
      <div className="empty-panel-icon">{icon}</div>
      <h3>{title}</h3>
      <p>{detail}</p>
    </div>
  );
}

export default function App() {
  const { snapshot, connected, checked, connectionError, eventStatus, refresh } = useDaemonSnapshot();
  const registry = snapshot.managed_capabilities ?? EMPTY_CAPABILITIES;
  const providers = snapshot.named_providers ?? EMPTY_PROVIDERS;
  const [notice, setNotice] = useState("");
  const [busy, setBusy] = useState(false);
  const [modal, setModal] = useState<Modal>(null);
  const [sidebar, setSidebar] = useState(() => window.innerWidth > 760);
  const [panel, setPanel] = useState<Panel | null>(null);
  const [activity, setActivity] = useState(false);
  const [taskId, setTaskId] = useState<string>();
  const [planIds, setPlanIds] = useState<Record<string, string>>({});
  const [planDrafts, setPlanDrafts] = useState<Record<string, PlanDraft>>({});
  const [planLayouts, setPlanLayouts] = useState<Record<string, PlanLayoutDraft>>({});
  const commandGate = useRef(createCommandGate());
  const [inputTargets, setInputTargets] = useState<Record<string, InputTarget>>({});
  const [executionDrafts, setExecutionDrafts] = useState<Record<string, TaskControls | undefined>>({});
  const [drafts, setDrafts] = useState<Record<string, string>>({});
  const [settings, setSettings] = useState<Settings>({
    base_url: "",
    model: "",
    configured: false,
    execution_mode: "native",
    agents: [{ ...DEFAULT_AGENT }],
    default_agent_id: DEFAULT_AGENT.id,
    codex_binary: "codex",
    main_executor: "native",
    models_endpoint: "",
    model_contexts: {},
  });
  const [settingsForm, setSettingsForm] = useState<Settings>(settings);
  const [theme, setTheme] = useState<AppTheme>(
    () => localStorage.getItem("harness-theme") === "dark" ? "dark" : "light",
  );
  const [uiScale, setUiScale] = useState(readUiScale);
  const [createTitle, setCreateTitle] = useState("");
  const [createPath, setCreatePath] = useState("");
  const [apiKey, setApiKey] = useState("");
  const [codexStatus, setCodexStatus] = useState("");
  const [checkingCodex, setCheckingCodex] = useState(false);
  const quickModels = useModelCatalog(settings.main_executor === "codex" ? {executor:"codex"} : namedModelRequest(settings),connected && (settings.main_executor === "codex" || !!settings.base_url || settings.provider_source === "local_codex" || (settings.provider_source === "named" && !!settings.provider_id)));
  const nativeModels = useModelCatalog(namedModelRequest(settingsForm,apiKey),connected && modal === "settings" && (!!settingsForm.base_url || settingsForm.provider_source === "local_codex" || (settingsForm.provider_source === "named" && !!settingsForm.provider_id)));
  const codexModels = useModelCatalog({ executor: "codex" }, connected && modal === "settings"
    && settingsForm.codex_binary === settings.codex_binary && (settingsForm.main_executor === "codex" || settingsForm.agents.some((agent) => agent.executor === "codex")));
  const [inputModes, setInputModes] = useState<Record<string, "goal" | "change">>({});
  const [remotePlanIds, setRemotePlanIds] = useState<Record<string, string>>({});
  const [remoteDrafts, setRemoteDrafts] = useState<Record<string, string>>({});
  const [remoteSource, setRemoteSource] =
    useState<SourceContext>(REMOTE_SOURCE);
  const [remoteReceipt, setRemoteReceipt] = useState("");
  const scrollRef = useRef<HTMLDivElement>(null);
  const followScroll = useRef(true);
  const sessionId = snapshot.selections.find((s) =>
    sameSource(s.source, DESKTOP_SOURCE),
  )?.session_id;
  const currentSessionId = useRef(sessionId);
  currentSessionId.current = sessionId;
  const session = snapshot.sessions.find((s) => s.id === sessionId);
  const tasks = snapshot.tasks.filter((t) => t.session_id === sessionId);
  const plans = (snapshot.plans ?? []).filter((p) => p.session_id === sessionId);
  const selectedPlan = selectedSessionPlan(snapshot.plans, sessionId, planIds);
  const planId = selectedPlan?.id;
  const selectPlan = (id: string) => {
    if (sessionId) setPlanIds((current) => ({ ...current, [sessionId]: id }));
  };
  const selectedTask = tasks.find((t) => t.id === taskId) ?? tasks[0];
  const running = snapshot.tasks.filter(
    (t) => t.task_kind !== "plan" && ["queued", "running"].includes(t.status),
  );
  const draftKey = sessionId ?? "new";
  const draft = drafts[draftKey] ?? "";
  const executionDraft = executionDrafts[draftKey];
  const sessionRequests = (snapshot.execution_requests ?? []).filter(request => request.session_id === sessionId);
  const pendingRequestCount = sessionRequests.filter(request => request.status === "pending").length;
  const inputTarget = inputTargets[draftKey] ?? { mode: "queue" as const };
  const steerableTasks = tasks.filter(canSteer);
  const currentRunningTask = tasks.find(task => task.task_kind === "main" && task.status === "running");
  const steeringTask = inputTarget.mode === "steer" ? tasks.find(task => task.id === inputTarget.taskId) : undefined;
  const steeringUnavailable = inputTarget.mode === "steer" && !canSteer(steeringTask);
  const events = snapshot.events.filter(
    (e) => !sessionId || e.session_id === sessionId,
  );
  const remoteSelection =
    snapshot.selections.find((s) => sameSource(s.source, remoteSource))
      ?.session_id ?? "";
  const remoteKey = `${remoteSource.channel_id}:${remoteSource.actor_id}:${remoteSource.conversation_id}:${remoteSelection}`;
  const remotePlans = snapshot.plans.filter((item) => item.session_id === remoteSelection);
  const remotePlan = remotePlans.find((item) => item.id === remotePlanIds[remoteKey]) ?? remotePlans[0];
  const remoteMode = inputModes[remoteKey] ?? "goal";
  const currentRemoteText = remoteDrafts[remoteKey] ?? "";
  const changeable = (status?: string) => !!status && ["draft", "running", "paused", "reviewing", "waiting_input"].includes(status);

  useEffect(() => {
    if (connected)
      api<Settings>("GET", "/settings")
        .then((value) => setSettings(normalizeSettings(value)))
        .catch(() => {});
  }, [connected]);
  const previousModal = useRef<Modal>(null);
  useEffect(() => {
    if (modal === "settings" && previousModal.current !== "providers") {
      setSettingsForm(settings);
      setApiKey("");
      setCodexStatus("");
    }
    previousModal.current = modal;
  }, [modal]);
  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    localStorage.setItem("harness-theme", theme);
    void syncNativeTheme(theme).catch(() => {
      setNotice("原生窗口主题同步失败，请重启应用后重试。");
    });
  }, [theme]);
  useEffect(() => {
    try {
      saveUiScale(uiScale);
    } catch {
      setNotice("界面缩放已调整，但本机偏好保存失败，重启后可能恢复默认值。");
    }
    void applyUiScale(uiScale).catch(() => {
      setNotice("界面缩放未能应用，请重新打开应用后重试。");
    });
  }, [uiScale]);
  useEffect(() => {
    const desktop = window.matchMedia("(min-width: 761px)");
    const update = (event: MediaQueryListEvent) => setSidebar(event.matches);
    desktop.addEventListener("change", update);
    return () => desktop.removeEventListener("change", update);
  }, []);
  useEffect(() => {
    if (sessionId)
      scrollRef.current?.scrollTo({
        top: scrollRef.current.scrollHeight,
        behavior: "smooth",
      });
  }, [sessionId, session?.messages.length, tasks.length]);
  const liveTextLength = tasks.filter(task => activeTask(task.status)).reduce((length, task) => length + (task.stream_output?.length ?? 0), 0);
  useEffect(() => {
    if (followScroll.current && scrollRef.current) scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
  }, [liveTextLength, pendingRequestCount]);
  useEffect(() => {
    const shortcut = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.key === "n") {
        event.preventDefault();
        setModal("new");
      }
      if (event.key === "Escape") {
        setPanel(null);
      }
    };
    window.addEventListener("keydown", shortcut);
    return () => window.removeEventListener("keydown", shortcut);
  }, []);

  async function command(value: Command, source = DESKTOP_SOURCE) {
    const applyingChange = value.type === "apply_plan_change" ? snapshot.plan_changes.find((item) => item.id === value.change_id) : undefined;
    const applyingPlan = applyingChange ? snapshot.plans.find((item) => item.id === applyingChange.plan_id) : undefined;
    if (applyingPlan && hasUnsavedPlanDraft(applyingPlan, planDrafts)) {
      setNotice("该计划在桌面还有未保存的结构修改，请先保存或重新加载草稿，再应用调整建议。");
      return undefined;
    }
    return commandGate.current.run(async () => {
      setBusy(true);
      setNotice("");
      try {
        const receipt = await dispatch(source, value);
        if (applyingPlan) {
          setPlanDrafts((current) => discardCleanPlanDraft(applyingPlan, current));
          setPlanLayouts((current) => updatePlanLayouts(applyingPlan, applyingPlan.nodes, current, current[applyingPlan.id]));
        }
        await refresh();
        return receipt;
      } catch (error) {
        setNotice(errorMessage(error));
        return undefined;
      } finally {
        setBusy(false);
      }
    });
  }
  async function createSession(event: FormEvent) {
    event.preventDefault();
    const receipt = await command({
      type: "create_session",
      title: createTitle.trim(),
      project_path: createPath.trim(),
    });
    if (receipt?.session_id) {
      await command({ type: "select_session", session_id: receipt.session_id });
      setModal(null);
      setCreateTitle("");
      setCreatePath("");
    }
  }
  async function send() {
    if (!session || !draft.trim() || busy || !connected) return;
    const key = draftKey;
    let value: Command;
    try {
      value = inputCommand(session.id, inputTarget, draft, tasks);
      if (value.type === "run_agent" && executionDraft) value = { ...value, controls: controlsForTask(executionDraft, settings.main_executor ?? "native") };
    }
    catch (error) { setNotice(errorMessage(error)); return; }
    const receipt = await command(value);
    if (receipt) {
      setDrafts((old) => old[key] === draft ? { ...old, [key]: "" } : old);
      if (value.type === "run_agent" && executionDraft) setExecutionDrafts(current => current[key] === executionDraft ? { ...current, [key]: undefined } : current);
      if (receipt.task_id && currentSessionId.current === session.id) setTaskId(receipt.task_id);
      if (receipt.supplement_id) setNotice(`补充已接收 · ${receipt.supplement_id.slice(0, 8)}，等待执行器应用；接收不代表已生效。`);
    }
  }
  async function saveSettings(event: FormEvent) {
    event.preventDefault();
    const controlsError = settingsExecutionError(settingsForm);
    if (controlsError) { setNotice(controlsError); return; }
    setBusy(true);
    setNotice("");
    try {
      const value = await api<Settings>("PUT", "/settings", {
        base_url: settingsForm.base_url,
        provider_source: settingsForm.provider_source ?? "manual",
        provider_id: settingsForm.provider_id ?? null,
        model: settingsForm.model,
        execution_mode: settingsForm.execution_mode,
        agents: settingsForm.agents,
        default_agent_id: settingsForm.default_agent_id,
        codex_binary: settingsForm.codex_binary,
        main_executor: settingsForm.main_executor ?? "native",
        models_endpoint: settingsForm.models_endpoint ?? "",
        model_contexts: settingsForm.model_contexts ?? {},
        execution_controls: settingsForm.execution_controls ?? defaultExecutionControls(),
        ...(apiKey && settingsForm.provider_source !== "named" ? { api_key: apiKey } : {}),
      });
      setSettings(normalizeSettings(value));
      setApiKey("");
      quickModels.refresh();
      setModal(null);
    } catch (error) {
      setNotice(errorMessage(error));
    } finally {
      setBusy(false);
    }
  }
  async function selectModel(model: string) {
    if (!model || busy) return;
    setBusy(true);
    setNotice("");
    try {
      const value = await api<Settings>("PUT", "/settings", { model });
      setSettings(normalizeSettings(value));
    } catch (error) {
      setNotice(errorMessage(error));
    } finally {
      setBusy(false);
    }
  }
  async function remoteSend(run: boolean) {
    if (!currentRemoteText.trim() || !remoteSelection) return;
    if (!run && remoteMode === "change" && (!remotePlan || !changeable(remotePlan.status))) return;
    const key = remoteKey;
    const receipt = await command(
      run
        ? { type: "start_task", session_id: remoteSelection, title: currentRemoteText.trim() }
        : remoteMode === "change" && remotePlan ? { type: "request_plan_change", plan_id: remotePlan.id, expected_revision: remotePlan.revision, content: currentRemoteText.trim() }
        : { type: "send_message", session_id: remoteSelection, content: currentRemoteText.trim() },
      remoteSource,
    );
    if (receipt) {
      setRemoteReceipt(
        `已接收 · ${receipt.command_id.slice(0, 8)}${receipt.task_id ? ` · 任务 ${receipt.task_id.slice(0, 8)}` : ""}`,
      );
      setRemoteDrafts((current) => current[key] === currentRemoteText ? { ...current, [key]: "" } : current);
    }
  }
  async function checkCodex() {
    setCheckingCodex(true);
    setCodexStatus("");
    try {
      const result = await api<{ available: boolean; version: string | null; message: string }>("GET", "/codex/status");
      setCodexStatus(`${result.available ? "检测通过" : "暂不可用"} · ${result.message}${result.version ? ` · ${result.version}` : ""}`);
    } catch (error) {
      setCodexStatus(errorMessage(error));
    } finally {
      setCheckingCodex(false);
    }
  }
  const togglePanel = (value: Panel) =>
    setPanel((current) => (current === value ? null : value));
  const groups = [...new Set(snapshot.sessions.map((s) => s.project_path))];

  return (
    <div
      className={`workbench ${sidebar ? "" : "sidebar-hidden"} ${panel ? "panel-open" : ""}`}
    >
      <aside className="sidebar" aria-label="会话导航">
        <div className="sidebar-brand">
          <span className="brand-mark">
            <Waypoints size={20} strokeWidth={1.8} />
          </span>
          <strong>
            harness<span className="brand-period">.</span>
          </strong>
          <span className="local-label">本地</span>
          <Button
            variant="ghost"
            size="icon"
            aria-label="折叠导航"
            onClick={() => setSidebar(false)}
          >
            <PanelLeft size={16} />
          </Button>
        </div>
        <Button
          variant="secondary"
          className="new-session"
          onClick={() => setModal("new")}
        >
          <Plus size={17} />
          <span>新建会话</span>
          <kbd>⌘ N</kbd>
        </Button>
        <div className="sidebar-section">
          <span>工作空间</span>
          <span className="count-label">{snapshot.sessions.length}</span>
        </div>
        <nav className="session-list">
          {groups.map((path) => (
            <div className="project-group" key={path}>
              <div className="project-heading">
                <ChevronDown size={12} />
                <FolderOpen size={14} />
                <span title={path}>{projectName(path)}</span>
              </div>
              {snapshot.sessions
                .filter((s) => s.project_path === path)
                .map((s) => {
                  const pending = snapshot.tasks.some(
                    (t) => t.session_id === s.id && activeTask(t.status),
                  );
                  return (
                    <button
                      key={s.id}
                      className={`session-item ${sessionId === s.id ? "session-active" : ""}`}
                      onClick={() => {
                        void command({
                          type: "select_session",
                          session_id: s.id,
                        });
                        setTaskId(undefined);
                        if (window.innerWidth < 760) setSidebar(false);
                      }}
                    >
                      <MessageSquare size={14} />
                      <span>{s.title}</span>
                      {pending && (
                        <span
                          className="running-dot"
                          aria-label="有任务运行或排队"
                        />
                      )}
                    </button>
                  );
                })}
            </div>
          ))}
          {!groups.length && (
            <div className="sidebar-empty">
              <Folder size={24} strokeWidth={1.3} />
              <span>
                你的下一个想法，
                <br />
                从这里开始。
              </span>
            </div>
          )}
        </nav>
        <div className="sidebar-footer">
          <button
            className="sidebar-link"
            onClick={() => setModal("capabilities")}
          >
            <Blocks size={16} />
            <span>能力目录</span>
            <span className="tiny-count">
              {snapshot.capabilities.filter((c) => c.enabled).length}
            </span>
          </button>
          <button className="sidebar-link" onClick={() => setModal("reliability")}><ShieldCheck size={16}/><span>备份与诊断</span></button>
          <button className="sidebar-link" onClick={() => setModal("gateway")}><Radio size={16}/><span>自托管网关 / H5</span></button>
          <button className="sidebar-link" onClick={() => setModal("qq")}><Radio size={16}/><span>QQ 官方应用</span></button>
          <button className="sidebar-link" onClick={() => setModal("remote")}>
            <Radio size={16} />
            <span>远程入口</span>
            <span className="beta-tag">实验</span>
          </button>
          <div className="sidebar-bottom">
            <button className="profile" onClick={() => setModal("settings")}>
              <span className="avatar">Y</span>
              <span>
                <strong>个人工作空间</strong>
                <small>本地优先 · v0.6.0</small>
              </span>
              <Settings2 size={15} />
            </button>
            <Button
              variant="ghost"
              size="icon"
              aria-label={theme === "light" ? "切换深色主题" : "切换浅色主题"}
              onClick={() => setTheme(theme === "light" ? "dark" : "light")}
            >
              {theme === "light" ? <Moon size={15} /> : <Sun size={15} />}
            </Button>
          </div>
        </div>
      </aside>

      <main className="main-area">
        <header className="topbar">
          <div className="topbar-left">
            {!sidebar && (
              <Button
                variant="ghost"
                size="icon"
                aria-label="展开导航"
                onClick={() => setSidebar(true)}
              >
                <PanelLeft size={17} />
              </Button>
            )}
            <span className="breadcrumb">工作空间</span>
            <ChevronRight size={12} />
            <strong>{session?.title ?? "新的开始"}</strong>
            {session && (
              <span className="project-chip">
                <Folder size={12} />
                {projectName(session.project_path)}
              </span>
            )}
          </div>
          <div className="topbar-actions">
            <span
              className={`connection ${connected ? "online" : ""}`}
              title={connectionError || "本机后台已连接"}
            >
              <span />
              {connected ? "后台在线" : checked ? "后台离线" : "连接中"}
            </span>
            <Button
              variant={panel === "plan" ? "secondary" : "ghost"}
              size="icon"
              aria-label="查看计划"
              onClick={() => togglePanel("plan")}
            >
              <PanelRight size={17} />
            </Button>
          </div>
        </header>
        {notice && (
          <div className="notice" role="alert">
            <CircleDot size={15} />
            <span>{notice}</span>
            <button onClick={() => setNotice("")} aria-label="关闭提示">
              <X size={14} />
            </button>
          </div>
        )}
        {checked && !connected && (
          <div className="offline-banner" role="status">
            <WifiOff size={14} />
            <span>
              {connectionError || "连接后台失败。重新连接后可以继续操作。"}
            </span>
            <button onClick={() => void refresh()}>
              <RefreshCw size={12} />
              重试
            </button>
          </div>
        )}
        <div className="main-content">
          <div className="conversation">
            <div className="conversation-scroll" ref={scrollRef} onScroll={() => {
              const area = scrollRef.current;
              if (area) followScroll.current = area.scrollHeight - area.scrollTop - area.clientHeight < 100;
            }}>
              {!session ? (
                <section className="welcome">
                  <div className="welcome-emblem">
                    <Waypoints size={36} strokeWidth={1.3} />
                    <span />
                  </div>
                  <div className="eyebrow">你的智能体，你的工作流。</div>
                  <h1>
                    想法在前，
                    <br />
                    <span>智能体随行。</span>
                  </h1>
                  <p>
                    把目标交给主智能体，让执行在后台继续。
                    <br />
                    由你掌控每一段工作流。
                  </p>
                  <Button
                    className="welcome-start"
                    onClick={() => setModal("new")}
                  >
                    <Plus size={16} />
                    开启一个会话
                    <ArrowUpRight size={16} />
                  </Button>
                  <div className="welcome-features">
                    <div>
                      <div className="feature-icon">
                        <GitBranch size={19} />
                      </div>
                      <strong>直接工作</strong>
                      <span>对话中完成编码和验证</span>
                    </div>
                    <div>
                      <div className="feature-icon">
                        <Layers3 size={19} />
                      </div>
                      <strong>持续推进</strong>
                      <span>后台执行，会话随时可用</span>
                    </div>
                    <div>
                      <div className="feature-icon">
                        <Radio size={19} />
                      </div>
                      <strong>远程指令</strong>
                      <span>跨入口管理同一组会话</span>
                    </div>
                  </div>
                  <div className="version-note">
                    <span className="status-dot" />
                    本地优先 · 数据保存在你的设备
                  </div>
                </section>
              ) : (
                <div className="message-column">
                  <div className="session-intro">
                    <span className="intro-icon">
                      <FolderOpen size={20} />
                    </span>
                    <h2>{session.title}</h2>
                    <p title={session.project_path}>{session.project_path}</p>
                    <span className="session-date">
                      {new Date(session.created_at).toLocaleDateString(
                        "zh-CN",
                        { month: "long", day: "numeric" },
                      )}{" "}
                      · 主智能体 会话
                    </span>
                  </div>
                  {!session.messages.length && !tasks.length && (
                    <div className="conversation-empty">
                      <Sparkles size={20} />
                      <p>准备好了，告诉我你的目标。</p>
                      <span>
                        主智能体会直接阅读项目、修改文件并验证结果。远程入口可按需编排子任务。
                      </span>
                    </div>
                  )}
                  {session.messages.map((message) => (
                    <article
                      className={`message message-${message.role}`}
                      key={message.id}
                    >
                      <div className="message-avatar">
                        {message.role === "user" ? (
                          "Y"
                        ) : (
                          <Waypoints size={17} />
                        )}
                      </div>
                      <div className="message-body">
                        <div className="message-author">
                          {message.role === "user"
                            ? "你"
                            : message.role === "assistant"
                              ? "主智能体"
                              : "系统"}
                          <time>{time(message.created_at)}</time>
                          {message.task_id && (
                            <span className="inline-tag">任务更新</span>
                          )}
                        </div>
                        <div className="message-text">{message.role === "user" ? message.content : localizeSystemText(message.content)}</div>
                        {message.role === "assistant" && message.task_id && <ToolActivity task={tasks.find(task => task.id === message.task_id)} events={snapshot.events} />}
                      </div>
                    </article>
                  ))}
                  {tasks.filter(task => task.task_kind === "main" && (task.stream_output || task.status === "running")
                    && !session.messages.some(message => message.role === "assistant" && message.task_id === task.id))
                    .slice().reverse().map(task => <LiveTaskMessage key={`stream-${task.id}`} task={task} events={snapshot.events} />)}
                  <ExecutionRequests requests={sessionRequests} tasks={tasks} disabled={busy || !connected} onCommand={command} />
                  {plans.length > 0 && (
                    <section className="tasks-section">
                      <div className="section-label"><Network size={13} />编排计划<span>{plans.length}</span></div>
                      {plans.map((plan) => (
                        <button key={plan.id} className={`task-card ${planId === plan.id && panel === "plan" ? "task-selected" : ""}`} onClick={() => { selectPlan(plan.id); setPanel("plan"); }}>
                          <div className="task-icon"><Network size={18} /></div>
                          <div className="task-card-content">
                            <div className="task-card-top"><span>主智能体编排</span><Status status={plan.status} /></div>
                            <strong>{plan.title}</strong>
                            <div className="task-card-meta"><span>{plan.nodes.length} 个任务 · 版本 {plan.revision}</span><span>{plan.status === "draft" ? "审阅后启动" : `回顾 ${plan.round}/${plan.max_rounds}`}</span></div>
                          </div><ChevronRight size={16} className="muted" />
                        </button>
                      ))}
                    </section>
                  )}
                  {tasks.length > 0 && (
                    <section className="tasks-section">
                      <div className="section-label">
                        <Waypoints size={13} />
                        后台任务<span>{tasks.length}</span>
                      </div>
                      {tasks.map((task) => (
                        <TaskCard
                          key={task.id}
                          task={task}
                          selected={selectedTask?.id === task.id && !!panel}
                          onClick={() => {
                            setTaskId(task.id);
                            setPanel("task");
                          }}
                        />
                      ))}
                    </section>
                  )}
                </div>
              )}
            </div>
            <div className="composer-area">
              <div className="composer-context">
                <span>
                  <span
                    className={running.length ? "running-dot" : "idle-dot"}
                  />
                  {running.length
                    ? `${running.length} 个后台任务尚未完成`
                    : "准备就绪，随时开始"}
                </span>
                {running.length > 0 && <span>主智能体 仍可接收指令</span>}
                {connected && <span className={`event-connection event-connection-${eventStatus}`} title={eventStatus === "connected" ? "通过持久事件实时更新；断线后自动补发。" : "实时事件暂未连接，仍定期读取权威快照。"}>{eventStatus === "connected" ? "实时更新" : eventStatus === "error" ? "快照更新 · 事件桥暂不可用" : "快照更新 · 正在连接事件"}</span>}
              </div>
              {session && <div className="input-routing">
                <div className="input-route-options" role="group" aria-label="指令作用方式">
                  <Button size="small" variant={inputTarget.mode === "queue" ? "secondary" : "ghost"} disabled={busy} onClick={() => setInputTargets(current => ({ ...current, [draftKey]: { mode: "queue" } }))}>排队下一条</Button>
                  <Button size="small" variant={inputTarget.mode === "steer" ? "secondary" : "ghost"} disabled={busy || (!steerableTasks.length && inputTarget.mode !== "steer")} onClick={() => {
                    if (steerableTasks[0]) setInputTargets(current => ({ ...current, [draftKey]: { mode: "steer", taskId: steerableTasks[0].id } }));
                  }}>补充当前任务</Button>
                </div>
                {inputTarget.mode === "steer" ? <>
                  <Select aria-label="补充目标任务" value={inputTarget.taskId} disabled={busy}
                    options={[...(steeringUnavailable ? [{ value: inputTarget.taskId, label: `原目标 ${inputTarget.taskId.slice(0, 8)} 已结束或不可用`, disabled: true }] : []),
                      ...steerableTasks.map(task => ({ value: task.id, label: `${task.status === "running" ? "执行中" : "排队中"} · ${task.id.slice(0, 8)} · ${task.title}` }))]}
                    onValueChange={taskId => setInputTargets(current => ({ ...current, [draftKey]: { mode: "steer", taskId } }))} />
                  <p className="help-text">目标会话：{session.title} · 任务：{inputTarget.taskId}。{steeringUnavailable ? "请重新选择目标，或改为排队下一条；不会自动转投新任务。" : "接收后在执行器支持的安全边界应用，不保证立即生效。"}</p>
                </> : <p className="help-text">目标会话：{session.title} · {session.id.slice(0, 8)}。创建独立主任务，已有任务运行时顺序排队；不会补充当前执行。</p>}
              </div>}
              {session && inputTarget.mode === "queue" && <details className="next-execution-controls">
                <summary>下一条执行预算与权限{executionDraft ? " · 已设置额外限制" : " · 使用后台默认值"}</summary>
                <label className="execution-checkbox"><input type="checkbox" disabled={busy} checked={!!executionDraft}
                  onChange={event => setExecutionDrafts(current => ({ ...current, [draftKey]: event.target.checked ? structuredClone(settings.execution_controls ?? defaultExecutionControls()) : undefined }))} />为下一条任务指定更严格限制</label>
                {executionDraft && <ExecutionSettings registry={registry} value={executionDraft} executor={settings.main_executor ?? "native"} scope="task" disabled={busy}
                  onChange={value => setExecutionDrafts(current => ({ ...current, [draftKey]: value }))} />}
                {!executionDraft && <CapabilityScopeSummary registry={registry} value={(settings.execution_controls ?? defaultExecutionControls()).capabilities} label="下一条任务继承的能力范围" />}
              </details>}
              <div
                className={`composer ${!session ? "composer-inactive" : ""}`}
              >
                <textarea
                  aria-label="向主智能体 发送指令"
                  placeholder={
                    session
                      ? inputTarget.mode === "steer" ? "补充所选任务的要求，不会创建下一条任务…" : "输入下一条独立任务，按会话顺序执行…"
                      : "先创建一个会话，开始你的工作…"
                  }
                  value={draft}
                  onChange={(e) =>
                    setDrafts((old) => ({ ...old, [draftKey]: e.target.value }))
                  }
                  disabled={!session}
                  rows={3}
                  onKeyDown={(e) => {
                    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
                      e.preventDefault();
                      void send();
                    }
                  }}
                />
                <div className="composer-toolbar">
                  <div className="composer-options">
                    <QuickModelSelect value={settings.main_executor !== "codex" && settings.provider_source === "named" ? providers.providers.find(provider=>provider.id===settings.provider_id)?.definition.model ?? "" : settings.model} catalog={quickModels} disabled={busy || !connected || (settings.main_executor !== "codex" && settings.provider_source === "named")}
                      emptyLabel={settings.main_executor === "codex" ? "Codex 本机默认模型" : settings.provider_source === "named" ? "在命名服务中设置默认模型" : "请选择模型"}
                      onChange={(model) => void selectModel(model)} onSettings={() => setModal("settings")} />
                    <span
                      className={`mode-pill ${settings.execution_mode === "demo" ? "mode-demo" : ""}`}
                    >
                      {settings.execution_mode === "demo" ? "演示" : "真实执行"}
                    </span>
                  </div>
                  <div className="composer-submit">
                    <Button
                      size="small"
                      disabled={!connected || !session || !draft.trim() || busy || steeringUnavailable || (inputTarget.mode === "queue" && !!executionDraft && !!executionControlsError(executionDraft, settings.main_executor ?? "native"))}
                      onClick={() => void send()}
                      title="主智能体直接处理（⌘/Ctrl + Enter）"
                    >
                      <ArrowUp size={14} />
                      <span>{inputTarget.mode === "steer" ? "提交补充" : "排队下一条"}</span>
                    </Button>
                    {currentRunningTask && <Button size="small" variant="danger" disabled={busy || !connected}
                      onClick={() => void command({ type: "cancel_task", task_id: currentRunningTask.id })} title={`停止任务 ${currentRunningTask.id}`}><Square size={12} />停止当前 · {currentRunningTask.id.slice(0, 8)}</Button>}
                  </div>
                </div>
              </div>
              <div className="composer-footnote">
                <span>{settings.main_executor === "codex" ? "Codex CLI" : "原生工具循环"} · 主智能体直接工作</span>
                <span>⌘ ↵ 发送</span>
              </div>
            </div>
          </div>
          {panel && (
            <aside className="work-panel" aria-label="工作详情">
              <div className="panel-tabs">
                {(
                  [
                    { id: "plan", label: "计划", icon: <Network size={14} /> },
                    {
                      id: "changes",
                      label: "更改",
                      icon: <FileCode2 size={14} />,
                    },
                    {
                      id: "artifacts",
                      label: "产物",
                      icon: <Layers3 size={14} />,
                    },
                    {
                      id: "task",
                      label: "任务",
                      icon: <Waypoints size={14} />,
                    },
                  ] as const
                ).map((tab) => (
                  <button
                    key={tab.id}
                    className={panel === tab.id ? "tab-active" : ""}
                    onClick={() => setPanel(tab.id)}
                  >
                    {tab.icon}
                    {tab.label}
                  </button>
                ))}
                <button
                  className="panel-close"
                  aria-label="关闭工作面板"
                  onClick={() => setPanel(null)}
                >
                  <X size={15} />
                </button>
              </div>
              {panel === "plan" ? (
                <PlanPanel
                  plans={plans} selectedId={planId} onSelect={selectPlan}
                  connected={connected} busy={busy} onCommand={command}
                  changes={snapshot.plan_changes}
                  drafts={planDrafts} setDrafts={setPlanDrafts}
                  layouts={planLayouts} setLayouts={setPlanLayouts}
                />
              ) : panel === "changes" ? (
                selectedTask ? <TaskChanges key={selectedTask.id} task={selectedTask} connected={connected} /> : <EmptyPanel icon={<FileCode2 />} title="代码更改" detail="选择一个任务后读取其只读改动报告。" />
              ) : panel === "artifacts" ? (
                selectedTask?.result ? (
                  <div className="task-detail">
                    <div className="section-label">任务结果</div>
                    <pre className="result-text">
                      {typeof selectedTask.result === "string"
                        ? localizeSystemText(selectedTask.result)
                        : JSON.stringify(selectedTask.result, null, 2)}
                    </pre>
                  </div>
                ) : (
                  <EmptyPanel
                    icon={<Layers3 />}
                    title="产物将在这里汇集"
                    detail="任务完成后，在这里查看执行器返回的结果。"
                  />
                )
              ) : selectedTask ? (
                <div className="task-detail">
                  <div className="task-detail-heading">
                    <span className="detail-icon">
                      <Waypoints size={22} />
                    </span>
                    <Status status={selectedTask.status} />
                  </div>
                  <h2>{selectedTask.title}</h2>
                  <div className="detail-labels">
                    <span>
                      {selectedTask.execution_mode === "demo"
                        ? "演示模式 · 模拟任务"
                        : selectedTask.agent ? `${selectedTask.agent.name} · ${selectedTask.agent.executor === "codex" ? "Codex CLI" : "原生工具循环"}` : "真实执行"}
                    </span>
                    <code>{selectedTask.id.slice(0, 8)}</code>
                  </div>
                  {selectedTask.agent && <p className="task-model-preset">模型预设：{agentModelLabel(selectedTask.agent)}</p>}
                  {(snapshot.context_states ?? []).filter(item => item.session_id === selectedTask.session_id).map(item => (
                    <div className="context-state" key={item.executor}>
                      <strong>{item.executor === "native" ? "Native 摘要" : "Codex 会话"} · {({empty:"尚无摘要",available:"可复用",invalidated:"已失效",ready:"已关联",inflight:"回合执行中",idle:"可续接"} as Record<string,string>)[item.state] ?? "状态待确认"}</strong>
                      {item.reason && <p>{item.reason}</p>}
                      {item.executor === "native" && item.covered_messages != null && <p>覆盖 {item.covered_messages} 条历史消息；这不是精确 token 计数。</p>}
                    </div>
                  ))}
                  <div className="detail-divider" />
                  <div className="section-label">执行步骤</div>
                  <div className="step-list">
                    {selectedTask.steps.map((step, i) => (
                      <div className={`step step-${step.status}`} key={step.id}>
                        <span className="step-circle">
                          {step.status === "completed" ? (
                            <Check size={12} />
                          ) : step.status === "running" ? (
                            <Loader2 size={12} className="spin" />
                          ) : (
                            i + 1
                          )}
                        </span>
                        <div>
                          <strong>{localizeSystemText(step.title)}</strong>
                          {step.detail && <p>{localizeSystemText(step.detail)}</p>}
                        </div>
                      </div>
                    ))}
                  </div>
                  {selectedTask.error && (
                    <div className="task-error" role="alert">
                      {localizeSystemText(selectedTask.error)}
                    </div>
                  )}
                  {selectedTask.result && (
                    <div className="task-result">
                      <div className="section-label">执行结果</div>
                      <pre>
                        {typeof selectedTask.result === "string"
                          ? localizeSystemText(selectedTask.result)
                          : JSON.stringify(selectedTask.result, null, 2)}
                      </pre>
                    </div>
                  )}
                  {selectedTask.stream_output && (
                    <details className="task-stream-detail" open={activeTask(selectedTask.status)}>
                      <summary>{activeTask(selectedTask.status) ? "实时回复" : "执行过程回复"}</summary>
                      <pre>{selectedTask.stream_output}</pre>
                      {selectedTask.stream_output_truncated && <p className="help-text">过程回复已达到保留上限；最终结果与真实命令记录仍可查看。</p>}
                    </details>
                  )}
                  <ToolActivity task={selectedTask} events={snapshot.events} />
                  <TaskUsage taskId={selectedTask.id} events={snapshot.events} />
                  <TaskContext task={selectedTask} events={snapshot.events} />
                  <TaskSupplements task={selectedTask} />
                  {selectedTask.managed_provider && <section className="task-execution-details"><h4>任务固定模型服务</h4><p>{selectedTask.managed_provider.name} · 修订 {selectedTask.managed_provider.revision}</p><p>模型 {selectedTask.managed_provider.model}</p><p className="help-text">服务选择不证明请求成功；不会因修改配置静默转投其他服务。</p></section>}
                  <TaskExecutionDetails task={selectedTask} onCleanupResolved={() => void refresh()} disabled={busy || !connected} />
                  <ExecutionRequests requests={sessionRequests.filter(request => request.task_id === selectedTask.id)} tasks={[selectedTask]} disabled={busy || !connected} onCommand={command} />
                  {selectedTask.continued_from && <p className="help-text">继续自任务 {selectedTask.continued_from}；这是新的执行，不是原任务重放。</p>}
                  {canContinue(selectedTask) && <ContinueTask key={selectedTask.id} task={selectedTask} disabled={busy || !connected} onCommand={command} onCreated={id => { if (currentSessionId.current === selectedTask.session_id) setTaskId(id); }} />}
                  <ExecutionEvidence evidence={selectedTask.evidence} />
                  <div className="detail-divider" />
                  <div className="section-label">指令来源 / 回传位置</div>
                  <div className="source-card">
                    <Radio size={16} />
                    <div>
                      <strong>{selectedTask.source.channel_id}</strong>
                      <span>
                        {selectedTask.source.actor_id} /{" "}
                        {selectedTask.source.conversation_id}
                      </span>
                    </div>
                    <ShieldCheck size={14} />
                  </div>
                  <p className="help-text">
                    派发时固定回传位置，后续切换会话不会改变。
                  </p>
                  {activeTask(selectedTask.status) && (
                    <Button
                      variant="danger"
                      className="cancel-task"
                      disabled={busy || !connected}
                      onClick={() =>
                        void command(
                          selectedTask.task_kind === "plan" && selectedTask.plan_id
                            ? { type: "cancel_plan", plan_id: selectedTask.plan_id }
                            : { type: "cancel_task", task_id: selectedTask.id },
                        )
                      }
                    >
                      <Square size={13} />
                      取消此任务
                    </Button>
                  )}
                </div>
              ) : (
                <EmptyPanel
                  icon={<Waypoints />}
                  title="没有选中的任务"
                  detail="从会话中的任务卡片打开执行详情。"
                />
              )}
            </aside>
          )}
        </div>
        {activity && (
          <section className="activity-panel" aria-label="执行活动">
            <div className="activity-heading">
              <span>
                <Terminal size={14} />
                执行活动<span className="tiny-count">{events.length}</span>
              </span>
              <Button
                variant="ghost"
                size="icon"
                aria-label="收起执行活动"
                onClick={() => setActivity(false)}
              >
                <X size={14} />
              </Button>
            </div>
            <div className="activity-lines">
              {events.length ? (
                events.slice(-80).map((event) => (
                  <div className="activity-line" key={event.id}>
                    <time>{time(event.created_at)}</time>
                    <span className="event-kind" title={event.kind}>{eventLabel(event.kind)}</span>
                    <span>{localizeSystemText(event.message)}</span>
                  </div>
                ))
              ) : (
                <div className="activity-empty">
                  尚无执行记录。任务启动后会在这里显示真实事件。
                </div>
              )}
            </div>
          </section>
        )}
        <footer className="statusbar">
          <button
            onClick={() => setActivity(!activity)}
            className={activity ? "statusbar-selected" : ""}
          >
            <PanelBottom size={13} />
            活动{events.length > 0 && <span>{events.length}</span>}
          </button>
          <div className="statusbar-right">
            <span>
              <ShieldCheck size={12} />
              本机工作空间
            </span>
            <span className="statusbar-divider" />
            <span>
              {settings.execution_mode === "demo"
                ? "模拟执行模式"
                : "真实执行模式"}
            </span>
            <button onClick={() => setModal("settings")} aria-label="打开设置">
              <Settings2 size={12} />
            </button>
          </div>
        </footer>
      </main>

      <ModalFrame
        error={notice}
        open={modal === "new"}
        onClose={() => setModal(null)}
        title="开启新的工作"
        description="选择项目目录，所有工作围绕这个会话展开。"
      >
        <form onSubmit={createSession} className="form-body">
          <label>
            会话名称
            <input
              required
              autoFocus
              placeholder="例如：搭建个人网站"
              value={createTitle}
              onChange={(e) => setCreateTitle(e.target.value)}
            />
          </label>
          <label>
            本地项目路径
            <div className="input-with-icon">
              <Folder size={16} />
              <input
                required
                placeholder="/Users/you/projects/my-project"
                value={createPath}
                onChange={(e) => setCreatePath(e.target.value)}
              />
            </div>
            <small>输入已存在的目录绝对路径，后台会验证实际可访问性。</small>
          </label>
          <div className="form-info">
            <ShieldCheck size={16} />
            <p>
              任务在本机执行。工作目录限定为此项目，桌面窗口关闭后后台继续运行。
            </p>
          </div>
          <div className="modal-actions">
            <Button
              type="button"
              variant="ghost"
              onClick={() => setModal(null)}
            >
              取消
            </Button>
            <Button
              disabled={
                !connected || busy || !createTitle.trim() || !createPath.trim()
              }
              type="submit"
            >
              {busy ? (
                <Loader2 size={14} className="spin" />
              ) : (
                <Plus size={14} />
              )}
              创建会话
            </Button>
          </div>
        </form>
      </ModalFrame>

      <ModalFrame
        error={notice}
        open={modal === "settings"}
        onClose={() => setModal(null)}
        title="偏好与模型"
        description="调整本机界面、主智能体和远程子任务的执行配置。"
        wide
      >
        <form onSubmit={saveSettings} className="form-body">
          <section className="appearance-settings" aria-label="本机界面设置">
            <div className="settings-section-heading"><Settings2 size={18} /><h3>界面缩放</h3><span>仅此设备</span></div>
            <div className="ui-scale-controls">
              <label>缩放比例<Select aria-label="界面缩放比例" value={String(uiScale)} onValueChange={(value) => setUiScale(Number(value))}
                options={UI_SCALE_OPTIONS.map((value) => ({ value: String(value), label: `${value}%${value === DEFAULT_UI_SCALE ? " · 默认" : ""}` }))} /></label>
              <Button type="button" size="small" variant="secondary" onClick={() => setUiScale(DEFAULT_UI_SCALE)}>恢复默认</Button>
            </div>
            <p className="help-text">立即生效并自动保存在本机，重启后保持；不随模型配置同步。</p>
          </section>
          <div className="mode-selector">
            <button
              type="button"
              className={
                settingsForm.execution_mode === "native" ? "mode-selected" : ""
              }
              onClick={() =>
                setSettingsForm((s) => ({ ...s, execution_mode: "native" }))
              }
            >
              <Code2 size={19} />
              <strong>真实执行</strong>
              <span>主智能体直接工作</span>
              {settingsForm.execution_mode === "native" && (
                <CheckCircle2 size={15} />
              )}
            </button>
            <button
              type="button"
              className={
                settingsForm.execution_mode === "demo" ? "mode-selected" : ""
              }
              onClick={() =>
                setSettingsForm((s) => ({ ...s, execution_mode: "demo" }))
              }
            >
              <Sparkles size={19} />
              <strong>演示模式</strong>
              <span>仅模拟步骤与回传</span>
              {settingsForm.execution_mode === "demo" && (
                <CheckCircle2 size={15} />
              )}
            </button>
          </div>
          <ProviderSettings providers={providers.providers} onManageProviders={()=>{setApiKey("");setModal("providers");}} settings={settingsForm} saved={settings} onChange={setSettingsForm} apiKey={apiKey}
            onKeyChange={setApiKey} catalog={settingsForm.main_executor === "codex" ? codexModels : nativeModels} disabled={busy || !connected} />
          <ExecutionSettings registry={registry} value={settingsForm.execution_controls ?? defaultExecutionControls()} executor={settingsForm.main_executor ?? "native"} disabled={busy}
            onChange={execution_controls => setSettingsForm(current => ({ ...current, execution_controls }))} />
          <ModelContextSettings providers={providers.providers} settings={settingsForm} onChange={setSettingsForm} disabled={busy} catalogs={[nativeModels, codexModels]} />
          <AgentSettings providers={providers.providers} registry={registry} settings={settingsForm} onChange={setSettingsForm} disabled={busy}
            nativeModels={nativeModels} codexModels={codexModels} codexPathChanged={settingsForm.codex_binary !== settings.codex_binary} />
          <div className="codex-check">
            <Button type="button" size="small" variant="secondary" disabled={busy || checkingCodex || !connected || settingsForm.codex_binary !== settings.codex_binary} onClick={() => void checkCodex()}>
              {checkingCodex ? <Loader2 size={13} className="spin" /> : <Terminal size={13} />}检测 Codex
            </Button>
            <span>{settingsForm.codex_binary !== settings.codex_binary ? "路径已修改，请先保存设置再检测。" : "检测已保存的 CLI 路径，不验证模型连接或登录状态。"}</span>
          </div>
          {codexStatus && settingsForm.codex_binary === settings.codex_binary && <p className="codex-check-result" role="status">{codexStatus}</p>}
          {settingsForm.execution_mode === "demo" && (
            <div className="demo-warning">
              演示模式只验证编排和回传链路，不修改项目，也不运行真实测试。
            </div>
          )}
          <div className="modal-actions">
            <Button
              variant="ghost"
              type="button"
              onClick={() => setModal(null)}
            >
              取消
            </Button>
            <Button disabled={busy || !connected || !!settingsExecutionError(settingsForm)} type="submit">
              保存设置
            </Button>
          </div>
        </form>
      </ModalFrame>

      <ModalFrame error="" open={modal === "reliability"} onClose={()=>setModal(null)} title="备份与诊断" description="本机所有者维护入口；私有历史备份与固定字段诊断严格分开。" wide>
        {modal === "reliability" && <ReliabilitySettings client={reliabilityClient} disabled={!connected}/>}
      </ModalFrame>

      <ModalFrame error="" open={modal === "gateway"} onClose={()=>setModal(null)} title="自托管网关 / H5" description="出站设备连接、一次性配对与会话级授权；不是本机管理 API 的公网代理。" wide>
        {modal === "gateway" && <GatewaySettings initialSnapshot={EMPTY_GATEWAY} client={gatewayClient} credentialClient={gatewayCredentialClient} registry={registry} sessions={snapshot.sessions} executor={settings.main_executor==="codex"?"codex":"native"} disabled={!connected}/>}
      </ModalFrame>

      <ModalFrame error="" open={modal === "qq"} onClose={()=>setModal(null)} title="QQ 官方应用" description="独立应用凭据、精确身份授权与被动回复；不是任意远端管理代理。" wide>
        {modal === "qq" && <QqSettings initialSnapshot={EMPTY_QQ} client={qqClient} credentialClient={qqCredentialClient} sessions={snapshot.sessions} executor={settings.main_executor==="codex"?"codex":"native"} disabled={!connected}/>}
      </ModalFrame>

      <ModalFrame error="" open={modal === "providers"} onClose={()=>setModal("settings")} title="命名服务与系统凭据" description="仅本机所有者管理；关闭页面不撤回已经接受的系统操作。" wide>
        {modal === "providers" && <ProvidersSettings registry={providers} client={providersClient} disabled={!connected} onRegistryChange={()=>void refresh()}/>}
        <div className="modal-actions"><Button type="button" variant="secondary" onClick={()=>setModal("settings")}>返回设置（保留元数据草稿）</Button></div>
      </ModalFrame>

      <ModalFrame
        error={notice}
        open={modal === "capabilities"}
        onClose={() => setModal(null)}
        title="能力目录"
        description="查看后台能力并管理 MCP / Skills。配置、发现、授权和实际使用彼此独立。"
        wide
      >
        {modal === "capabilities" && <CapabilitiesSettings registry={registry} client={capabilityClient} credentialClient={mcpCredentialClient} disabled={busy || !connected} onRegistryChange={() => void refresh()} />}
        <div className="capability-list">
          {snapshot.capabilities.length ? (
            snapshot.capabilities.map((capability) => (
              <div className="capability" key={capability.id}>
                <span className="capability-icon">
                  {capability.kind === "executor" ? (
                    <Zap size={19} />
                  ) : capability.kind === "agent" ? (
                    <Waypoints size={19} />
                  ) : (
                    <Blocks size={19} />
                  )}
                </span>
                <div>
                  <strong>
                    {capabilityLabel(capability).name}
                    <span className="inline-tag">{kindLabel(capability.kind)}</span>
                  </strong>
                  <p>{capabilityLabel(capability).description}</p>
                </div>
                <span
                  className={`availability ${capability.enabled ? "available" : ""}`}
                >
                  <span className="status-dot" />
                  {capability.enabled ? "可用" : "未启用"}
                </span>
              </div>
            ))
          ) : (
            <EmptyPanel
              icon={<Blocks />}
              title="等待能力发现"
              detail="连接后台后，在这里查看工具与执行器。"
            />
          )}
        </div>
      </ModalFrame>

      <ModalFrame
        error={notice}
        open={modal === "remote"}
        onClose={() => setModal(null)}
        title="远程指令实验室"
        description="模拟通讯入口，验证独立会话选择、异步派发和原位置回传。不会向真实通讯工具发送消息。"
        wide
      >
        <div className="remote-body">
          <div className="remote-controls">
            <div className="section-label">
              <Radio size={13} />
              模拟输入入口
            </div>
            <div className="remote-source">
              <span className="source-icon">
                <Globe2 size={20} />
              </span>
              <div>
                <strong>本地模拟器 / 所有者</strong>
                <Select
                  aria-label="远程对话"
                  value={remoteSource.conversation_id}
                  onValueChange={(value) => {
                    setRemoteSource({
                      ...REMOTE_SOURCE,
                      conversation_id: value,
                    });
                    setRemoteReceipt("");
                  }}
                  options={[{ value: "remote-1", label: "远程对话 1" }, { value: "remote-2", label: "远程对话 2" }]}
                />
              </div>
              <span className="beta-tag">本地</span>
            </div>
            <label className="remote-label">
              此入口选中的会话
              <Select
                aria-label="此入口选中的会话"
                value={remoteSelection}
                disabled={busy || !connected}
                onValueChange={(value) =>
                  value &&
                  void command(
                    { type: "select_session", session_id: value },
                    remoteSource,
                  )
                }
                options={[{ value: "", label: "请选择会话" }, ...snapshot.sessions.map((item) => ({ value: item.id, label: item.title }))]}
              />
            </label>
            <p className="help-text">
              独立于桌面会话选择。任务保存派发时的回传位置。
            </p>
            <div className="instruction-scope remote-instruction-scope">
              <label>指令用途<Select aria-label="远程指令用途" value={remoteMode} onValueChange={(value) => setInputModes((current) => ({ ...current, [remoteKey]: value as "goal" | "change" }))}
                options={[{ value: "goal", label: "新目标" }, { value: "change", label: "补充当前计划" }]}
              /></label>
              <label>目标计划<Select aria-label="远程目标计划" value={remotePlan?.id ?? ""} onValueChange={(value) => setRemotePlanIds((current) => ({ ...current, [remoteKey]: value }))}
                options={remotePlans.length ? remotePlans.map((item) => ({ value: item.id, label: `${item.title} · v${item.revision}` })) : [{ value: "", label: "暂无计划" }]}
              /></label>
            </div>
            {remotePlan && <div className="remote-plan-progress">
              <strong>{remotePlan.title}</strong><Status status={remotePlan.status} />
              <p>已完成 {remotePlan.nodes.filter((node) => node.status === "completed").length}/{remotePlan.nodes.length} 个任务 · 版本 {remotePlan.revision}</p>
              <div className="remote-actions">
                <Button size="small" variant="secondary" disabled={busy || !connected} onClick={() => void refresh()}><RefreshCw size={12} />查询进度</Button>
                {["running", "reviewing", "waiting_input", "paused"].includes(remotePlan.status) && <Button size="small" variant="secondary" disabled={busy || !connected} onClick={() => void command({ type: remotePlan.status === "paused" ? "resume_plan" : "pause_plan", plan_id: remotePlan.id, expected_revision: remotePlan.revision }, remoteSource)}>{remotePlan.status === "paused" ? "恢复派发" : "暂停派发"}</Button>}
              </div>
            </div>}
            <textarea
              className="remote-input"
              aria-label="远程指令内容"
              value={currentRemoteText}
              onChange={(e) => setRemoteDrafts((current) => ({ ...current, [remoteKey]: e.target.value }))}
              placeholder={remoteMode === "change" ? "补充对此计划的要求，生成调整建议后审阅应用…" : "模拟手机或通讯工具发来的新目标…"}
              rows={4}
            />
            <div className="remote-actions">
              <Button
                size="small"
                disabled={
                  !connected || busy || !remoteSelection || !currentRemoteText.trim() || (remoteMode === "change" && !changeable(remotePlan?.status))
                }
                onClick={() => void remoteSend(false)}
              >
                <Send size={13} />
                {remoteMode === "change" ? "生成调整建议" : "发送目标"}
              </Button>
              <Button
                variant="secondary"
                size="small"
                disabled={
                  !connected || busy || !remoteSelection || !currentRemoteText.trim() || remoteMode === "change"
                }
                onClick={() => void remoteSend(true)}
              >
                <ArrowUp size={13} />
                直接执行
              </Button>
            </div>
            {remoteReceipt && (
              <p className="receipt">
                <CheckCircle2 size={13} />
                {remoteReceipt}
              </p>
            )}
            {remotePlan && <PlanChanges plan={remotePlan} changes={snapshot.plan_changes} busy={busy} connected={connected} localDirty={hasUnsavedPlanDraft(remotePlan, planDrafts)} onCommand={(value) => command(value, remoteSource)} />}
          </div>
          <div className="delivery-section">
            <div className="section-label">
              <ArrowDown size={13} />
              回传记录
              <span>
                {
                  snapshot.deliveries.filter((d) =>
                    sameSource(d.target, remoteSource),
                  ).length
                }
              </span>
            </div>
            <div className="delivery-list">
              {snapshot.deliveries.filter((d) =>
                sameSource(d.target, remoteSource),
              ).length ? (
                [...snapshot.deliveries]
                  .reverse()
                  .filter((d) => sameSource(d.target, remoteSource))
                  .map((delivery) => (
                    <article className="delivery" key={delivery.id}>
                      <div className="delivery-top">
                        <Status status={delivery.status} />
                        <time>{time(delivery.created_at)}</time>
                      </div>
                      <strong>
                        {snapshot.sessions.find(
                          (s) => s.id === delivery.session_id,
                        )?.title ?? delivery.session_id.slice(0, 8)}
                      </strong>
                      <pre>
                        {typeof delivery.payload === "string"
                          ? delivery.payload
                          : JSON.stringify(delivery.payload, null, 2)}
                      </pre>
                      <div className="delivery-meta">
                        <code>任务 {delivery.task_id.slice(0, 8)}</code>
                        <span>尝试 {delivery.attempts} 次</span>
                      </div>
                      {delivery.error && (
                        <p className="error-text">{delivery.error}</p>
                      )}
                      {delivery.status === "failed" && (
                        <Button
                          variant="secondary"
                          size="small"
                          disabled={busy || !connected}
                          onClick={() =>
                            void command(
                              {
                                type: "retry_delivery",
                                delivery_id: delivery.id,
                              },
                              remoteSource,
                            )
                          }
                        >
                          <RefreshCw size={12} />
                          重试投递
                        </Button>
                      )}
                    </article>
                  ))
              ) : (
                <EmptyPanel
                  icon={<Radio />}
                  title="等待第一条回传"
                  detail="从此入口派发任务，完成后查看后台实际投递结果。"
                />
              )}
            </div>
          </div>
        </div>
      </ModalFrame>
    </div>
  );
}
