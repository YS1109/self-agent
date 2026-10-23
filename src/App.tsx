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
import { api, dispatch } from "./api";
import { PlanPanel } from "./components/plan-panel";
import { ExecutionEvidence } from "./components/execution-evidence";
import { Status } from "./components/status";
import { AgentSettings, DEFAULT_AGENT, normalizeSettings } from "./components/agent-settings";
import { syncNativeTheme, type AppTheme } from "./theme";
import { capabilityLabel, eventLabel, kindLabel, localizeSystemText } from "./labels";
import { Button } from "./components/ui/button";
import {
  activeTask,
  DESKTOP_SOURCE,
  EMPTY_SNAPSHOT,
  REMOTE_SOURCE,
  sameSource,
  type Command,
  type Settings,
  type Snapshot,
  type SourceContext,
  type TaskRun,
} from "./types";

type Panel = "plan" | "changes" | "artifacts" | "task";
type Modal = "new" | "settings" | "remote" | "capabilities" | null;
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
          <span>{task.task_kind === "plan" ? "计划执行" : task.plan_id ? "计划子任务" : "直接执行"}</span>
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
  const [snapshot, setSnapshot] = useState<Snapshot>(EMPTY_SNAPSHOT);
  const [connected, setConnected] = useState(false);
  const [checked, setChecked] = useState(false);
  const [connectionError, setConnectionError] = useState("");
  const [notice, setNotice] = useState("");
  const [busy, setBusy] = useState(false);
  const [modal, setModal] = useState<Modal>(null);
  const [sidebar, setSidebar] = useState(() => window.innerWidth > 760);
  const [panel, setPanel] = useState<Panel | null>(null);
  const [activity, setActivity] = useState(false);
  const [taskId, setTaskId] = useState<string>();
  const [planId, setPlanId] = useState<string>();
  const [drafts, setDrafts] = useState<Record<string, string>>({});
  const [settings, setSettings] = useState<Settings>({
    base_url: "",
    model: "",
    configured: false,
    execution_mode: "native",
    agents: [{ ...DEFAULT_AGENT }],
    default_agent_id: DEFAULT_AGENT.id,
    codex_binary: "codex",
  });
  const [settingsForm, setSettingsForm] = useState<Settings>(settings);
  const [theme, setTheme] = useState<AppTheme>(
    () => localStorage.getItem("harness-theme") === "dark" ? "dark" : "light",
  );
  const [createTitle, setCreateTitle] = useState("");
  const [createPath, setCreatePath] = useState("");
  const [apiKey, setApiKey] = useState("");
  const [codexStatus, setCodexStatus] = useState("");
  const [checkingCodex, setCheckingCodex] = useState(false);
  const [remoteText, setRemoteText] = useState("");
  const [remoteSource, setRemoteSource] =
    useState<SourceContext>(REMOTE_SOURCE);
  const [remoteReceipt, setRemoteReceipt] = useState("");
  const scrollRef = useRef<HTMLDivElement>(null);
  const sessionId = snapshot.selections.find((s) =>
    sameSource(s.source, DESKTOP_SOURCE),
  )?.session_id;
  const session = snapshot.sessions.find((s) => s.id === sessionId);
  const tasks = snapshot.tasks.filter((t) => t.session_id === sessionId);
  const plans = (snapshot.plans ?? []).filter((p) => p.session_id === sessionId);
  const selectedTask = tasks.find((t) => t.id === taskId) ?? tasks[0];
  const running = snapshot.tasks.filter(
    (t) => t.task_kind !== "plan" && ["queued", "running"].includes(t.status),
  );
  const draftKey = sessionId ?? "new";
  const draft = drafts[draftKey] ?? "";
  const events = snapshot.events.filter(
    (e) => !sessionId || e.session_id === sessionId,
  );
  const remoteSelection =
    snapshot.selections.find((s) => sameSource(s.source, remoteSource))
      ?.session_id ?? "";

  async function refresh() {
    try {
      const value = await api<Snapshot>("GET", "/snapshot");
      setSnapshot({ ...value, plans: value.plans ?? [] });
      setConnected(true);
      setConnectionError("");
    } catch (error) {
      setConnected(false);
      setConnectionError(errorMessage(error));
    } finally {
      setChecked(true);
    }
  }
  useEffect(() => {
    let alive = true;
    let timer: ReturnType<typeof setTimeout>;
    const poll = async () => {
      if (!alive) return;
      await refresh();
      if (alive) timer = setTimeout(poll, 1500);
    };
    void poll();
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, []);
  useEffect(() => {
    if (connected)
      api<Settings>("GET", "/settings")
        .then((value) => setSettings(normalizeSettings(value)))
        .catch(() => {});
  }, [connected]);
  useEffect(() => {
    if (modal === "settings") {
      setSettingsForm(settings);
      setApiKey("");
      setCodexStatus("");
    }
  }, [modal]);
  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    localStorage.setItem("harness-theme", theme);
    void syncNativeTheme(theme).catch(() => {
      setNotice("原生窗口主题同步失败，请重启应用后重试。");
    });
  }, [theme]);
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
    setBusy(true);
    setNotice("");
    try {
      const receipt = await dispatch(source, value);
      await refresh();
      return receipt;
    } catch (error) {
      setNotice(errorMessage(error));
      return undefined;
    } finally {
      setBusy(false);
    }
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
  async function send(run: boolean) {
    if (!session || !draft.trim()) return;
    const key = draftKey;
    const receipt = await command(
      run
        ? { type: "start_task", session_id: session.id, title: draft.trim() }
        : {
            type: "send_message",
            session_id: session.id,
            content: draft.trim(),
          },
    );
    if (receipt) {
      setDrafts((old) => old[key] === draft ? { ...old, [key]: "" } : old);
      if (receipt.task_id) setTaskId(receipt.task_id);
      if (!run) {
        setPanel("plan");
        setPlanId(receipt.plan_id);
        setNotice(settings.execution_mode !== "demo" && !settings.configured
          ? "请先在设置中配置模型服务。指令已保存，主智能体会提示配置要求。"
          : "目标已接收，主智能体正在后台处理；生成计划后可在右侧审阅。" );
      }
    }
  }
  async function saveSettings(event: FormEvent) {
    event.preventDefault();
    setBusy(true);
    setNotice("");
    try {
      const value = await api<Settings>("PUT", "/settings", {
        base_url: settingsForm.base_url,
        model: settingsForm.model,
        execution_mode: settingsForm.execution_mode,
        agents: settingsForm.agents,
        default_agent_id: settingsForm.default_agent_id,
        codex_binary: settingsForm.codex_binary,
        ...(apiKey ? { api_key: apiKey } : {}),
      });
      setSettings(normalizeSettings(value));
      setApiKey("");
      setModal(null);
    } catch (error) {
      setNotice(errorMessage(error));
    } finally {
      setBusy(false);
    }
  }
  async function remoteSend(run: boolean) {
    if (!remoteText.trim()) return;
    const receipt = await command(
      run
        ? { type: "start_task", title: remoteText.trim() }
        : { type: "send_message", content: remoteText.trim() },
      remoteSource,
    );
    if (receipt) {
      setRemoteReceipt(
        `已接收 · ${receipt.command_id.slice(0, 8)}${receipt.task_id ? ` · 任务 ${receipt.task_id.slice(0, 8)}` : ""}`,
      );
      setRemoteText((current) => current === remoteText ? "" : current);
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
                        setPlanId(undefined);
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
              <small>本地优先 · v0.4.1</small>
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
            <div className="conversation-scroll" ref={scrollRef}>
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
                      <strong>自主编排</strong>
                      <span>把目标拆成可追踪的步骤</span>
                    </div>
                    <div>
                      <div className="feature-icon">
                        <Layers3 size={19} />
                      </div>
                      <strong>并行执行</strong>
                      <span>任务继续，你也继续</span>
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
                        发送目标生成可编辑计划，或选择「直接执行」运行单个任务。
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
                      </div>
                    </article>
                  ))}
                  {plans.length > 0 && (
                    <section className="tasks-section">
                      <div className="section-label"><Network size={13} />编排计划<span>{plans.length}</span></div>
                      {plans.map((plan) => (
                        <button key={plan.id} className={`task-card ${planId === plan.id && panel === "plan" ? "task-selected" : ""}`} onClick={() => { setPlanId(plan.id); setPanel("plan"); }}>
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
              </div>
              <div
                className={`composer ${!session ? "composer-inactive" : ""}`}
              >
                <textarea
                  aria-label="向主智能体 发送指令"
                  placeholder={
                    session
                      ? "告诉主智能体你的目标，或补充新的指令…"
                      : "先创建一个会话，开始编排你的工作…"
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
                      void send(false);
                    }
                  }}
                />
                <div className="composer-toolbar">
                  <div className="composer-options">
                    <button
                      className="model-picker"
                      onClick={() => setModal("settings")}
                    >
                      <Sparkles size={13} />
                      <span>{settings.model || "配置模型"}</span>
                      <ChevronDown size={12} />
                    </button>
                    <span
                      className={`mode-pill ${settings.execution_mode === "demo" ? "mode-demo" : ""}`}
                    >
                      {settings.execution_mode === "demo" ? "演示" : "真实执行"}
                    </span>
                  </div>
                  <div className="composer-submit">
                    <Button
                      size="small"
                      disabled={!connected || !session || !draft.trim() || busy}
                      onClick={() => void send(false)}
                      title="发送目标给主智能体（⌘/Ctrl + Enter）"
                    >
                      <MessageSquare size={13} />
                      <span>发送目标</span>
                    </Button>
                    <Button
                      variant="ghost"
                      size="small"
                      disabled={!connected || !session || !draft.trim() || busy}
                      onClick={() => void send(true)}
                      title={`由默认角色「${settings.agents.find((agent) => agent.id === settings.default_agent_id)?.name ?? "编码助手"}」直接执行`}
                    >
                      <ArrowUp size={14} />
                      <span>直接执行</span>
                    </Button>
                  </div>
                </div>
              </div>
              <div className="composer-footnote">
                <span>发送目标 → 审阅计划 → 启动执行</span>
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
                  plans={plans} selectedId={planId} onSelect={setPlanId}
                  connected={connected} busy={busy} onCommand={command}
                />
              ) : panel === "changes" ? (
                <EmptyPanel
                  icon={<FileCode2 />}
                  title="代码更改"
                  detail="此版本尚未接入结构化差异视图。任务的真实执行记录可在活动面板中查看。"
                />
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
        title="模型与执行"
        description="配置主智能体的模型网关，以及执行子任务的角色和执行器。"
        wide
      >
        <form onSubmit={saveSettings} className="form-body">
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
              <span>按角色选择执行器</span>
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
          <label>
            主智能体 · 模型服务地址
            <input
              type="url"
              placeholder="https://api.example.com/v1"
              value={settingsForm.base_url}
              onChange={(e) =>
                setSettingsForm((s) => ({ ...s, base_url: e.target.value }))
              }
            />
          </label>
          <label>
            主智能体 · 模型名称
            <input
              placeholder="输入网关支持的模型 ID"
              value={settingsForm.model}
              onChange={(e) =>
                setSettingsForm((s) => ({ ...s, model: e.target.value }))
              }
            />
          </label>
          <label>
            访问密钥{" "}
            <span className="field-note">
              {settings.api_key_configured ? "已配置 · 留空保留" : "未配置"}
            </span>
            <input
              type="password"
              autoComplete="off"
              placeholder={
                settings.api_key_configured ? "留空保留现有密钥" : "输入访问密钥"
              }
              value={apiKey}
              onChange={(e) => setApiKey(e.target.value)}
            />
          </label>
          <div className="form-info">
            <ShieldCheck size={16} />
            <p>
              访问密钥仅保存在后台进程内存中，重启后需重新填写，也可通过环境变量配置。模型协议为
              Chat Completions。
            </p>
          </div>
          <AgentSettings settings={settingsForm} onChange={setSettingsForm} disabled={busy} />
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
            <Button disabled={busy || !connected} type="submit">
              保存设置
            </Button>
          </div>
        </form>
      </ModalFrame>

      <ModalFrame
        error={notice}
        open={modal === "capabilities"}
        onClose={() => setModal(null)}
        title="能力目录"
        description="由后台报告的真实可用能力。更多执行器、MCP 工具和技能将在后续接入。"
        wide
      >
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
                <select
                  aria-label="远程对话"
                  value={remoteSource.conversation_id}
                  onChange={(e) => {
                    setRemoteSource({
                      ...REMOTE_SOURCE,
                      conversation_id: e.target.value,
                    });
                    setRemoteReceipt("");
                  }}
                >
                  <option value="remote-1">远程对话 1</option>
                  <option value="remote-2">远程对话 2</option>
                </select>
              </div>
              <span className="beta-tag">本地</span>
            </div>
            <label className="remote-label">
              此入口选中的会话
              <select
                value={remoteSelection}
                disabled={busy || !connected}
                onChange={(e) =>
                  e.target.value &&
                  void command(
                    { type: "select_session", session_id: e.target.value },
                    remoteSource,
                  )
                }
              >
                <option value="">请选择会话</option>
                {snapshot.sessions.map((s) => (
                  <option key={s.id} value={s.id}>
                    {s.title}
                  </option>
                ))}
              </select>
            </label>
            <p className="help-text">
              独立于桌面会话选择。任务保存派发时的回传位置。
            </p>
            <textarea
              className="remote-input"
              aria-label="远程指令内容"
              value={remoteText}
              onChange={(e) => setRemoteText(e.target.value)}
              placeholder="模拟手机或通讯工具发来的指令…"
              rows={4}
            />
            <div className="remote-actions">
              <Button
                size="small"
                disabled={
                  !connected || busy || !remoteSelection || !remoteText.trim()
                }
                onClick={() => void remoteSend(false)}
              >
                <Send size={13} />
                发送目标
              </Button>
              <Button
                variant="secondary"
                size="small"
                disabled={
                  !connected || busy || !remoteSelection || !remoteText.trim()
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
