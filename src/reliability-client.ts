/** Local-owner reliability operations. Diagnostics are a fixed export, never a raw snapshot. */
import { providerUuid } from "./providers-client";
export type BackupState = "pending" | "running" | "completed" | "failed" | "unknown";
export type BackupReason = "export_failed" | "cancelled" | "deadline" | "restarted" | "outcome_uncertain";
export interface BackupManifest { format: 1; schema: number; bytes: number; sha256: string }
export interface BackupReceipt { id: string; state: BackupState; reason: BackupReason | null; manifest: BackupManifest | null }
export interface ReliabilityStatus { schema_version: number; backup_available: boolean; restore_mode: "offline_only"; updates: "disabled_no_trusted_release_configuration"; backup_jobs: BackupReceipt[] }
export interface BackupRequest { id: string; parent: string; name: string }
export interface ReliabilityClient {
  status(signal: AbortSignal): Promise<unknown>;
  backup(body: BackupRequest, signal: AbortSignal): Promise<unknown>;
  receipt(id: string, signal: AbortSignal): Promise<unknown>;
  diagnostics(signal: AbortSignal): Promise<unknown>;
}
export const DIAGNOSTIC_COUNTS = ["sessions", "tasks_pending", "tasks_running", "tasks_completed", "tasks_failed", "tasks_cancelled", "tasks_unknown", "events_retained", "providers_configured", "providers_connection_verified", "provider_tasks_succeeded", "credentials_unknown", "external_calls_unknown"] as const;
export const DIAGNOSTIC_EXCLUDES = ["credentials", "private_references", "private_digests", "identifiers", "prompts", "tool_output", "paths", "urls", "logs"] as const;
export interface SafeDiagnostics {
  diagnostic_schema: 1; application: "self-agent"; application_version: string; platform: "windows" | "macos" | "linux" | "other";
  database_schema: number; connection_check_history: "not_recorded";
  storage: "not_checked" | "compatible" | "unsupported" | "recovery_required";
  vault: "not_checked" | "available" | "unavailable" | "locked";
  updates: "disabled_no_trusted_release_configuration" | "verification_only";
  counts: Record<(typeof DIAGNOSTIC_COUNTS)[number], number>; excludes: string[];
}
const object = (value: unknown): value is Record<string, unknown> => !!value && typeof value === "object" && !Array.isArray(value);
const keys = (value: Record<string, unknown>, expected: readonly string[]) => Object.keys(value).length === expected.length && expected.every(key => Object.hasOwn(value, key));
const integer = (value: unknown, max = Number.MAX_SAFE_INTEGER): value is number => Number.isSafeInteger(value) && (value as number) >= 0 && (value as number) <= max;
const bytes = (value: string) => new TextEncoder().encode(value).length;
const badResponse = () => new Error("可靠性记录格式不受支持；没有导出原始数据或自动重发。");
export function parseBackupReceipt(value: unknown): BackupReceipt {
  if (!object(value) || !keys(value, ["id", "state", "reason", "manifest"]) || typeof value.id !== "string" || !providerUuid(value.id) || typeof value.state !== "string" || (value.reason !== null && typeof value.reason !== "string")) throw badResponse();
  if (value.state === "completed") {
    const manifest = value.manifest;
    if (value.reason !== null || !object(manifest) || !keys(manifest, ["format", "schema", "bytes", "sha256"]) || manifest.format !== 1 || !integer(manifest.schema, 0xffffffff) || !integer(manifest.bytes, 128 * 1024 * 1024) || manifest.bytes < 100 || typeof manifest.sha256 !== "string" || !/^[a-f0-9]{64}$/.test(manifest.sha256)) throw badResponse();
  } else {
    if (value.manifest !== null || !(["pending", "running"].includes(String(value.state)) ? value.reason === null : value.state === "failed" ? ["export_failed", "cancelled", "deadline"].includes(String(value.reason)) : value.state === "unknown" && ["restarted", "outcome_uncertain"].includes(String(value.reason)))) throw badResponse();
  }
  return structuredClone(value) as unknown as BackupReceipt;
}
export function parseReliabilityStatus(value: unknown): ReliabilityStatus {
  if (!object(value) || !keys(value, ["schema_version", "backup_available", "restore_mode", "updates", "backup_jobs"]) || !integer(value.schema_version, 0xffffffff) || typeof value.backup_available !== "boolean" || value.restore_mode !== "offline_only" || value.updates !== "disabled_no_trusted_release_configuration" || !Array.isArray(value.backup_jobs) || value.backup_jobs.length > 256) throw badResponse();
  const jobs = value.backup_jobs.map(parseBackupReceipt);
  if (new Set(jobs.map(job => job.id)).size !== jobs.length || jobs.some(job => job.manifest && job.manifest.schema > (value.schema_version as number))) throw badResponse();
  return { schema_version: value.schema_version, backup_available: value.backup_available, restore_mode: "offline_only", updates: "disabled_no_trusted_release_configuration", backup_jobs: jobs };
}
export function parseSafeDiagnostics(value: unknown): SafeDiagnostics {
  if (!object(value) || !keys(value, ["diagnostic_schema", "application", "application_version", "platform", "database_schema", "connection_check_history", "storage", "vault", "updates", "counts", "excludes"]) || bytes(JSON.stringify(value)) > 16 * 1024) throw badResponse();
  if (!["platform", "storage", "vault", "updates"].every(key => typeof value[key] === "string")) throw badResponse();
  if (value.diagnostic_schema !== 1 || value.application !== "self-agent" || typeof value.application_version !== "string" || value.application_version.length > 80 || !/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/.test(value.application_version) || !["windows", "macos", "linux", "other"].includes(String(value.platform)) || !integer(value.database_schema, 0xffffffff) || value.connection_check_history !== "not_recorded" || !["not_checked", "compatible", "unsupported", "recovery_required"].includes(String(value.storage)) || !["not_checked", "available", "unavailable", "locked"].includes(String(value.vault)) || !["disabled_no_trusted_release_configuration", "verification_only"].includes(String(value.updates))) throw badResponse();
  if (!object(value.counts) || !keys(value.counts, DIAGNOSTIC_COUNTS) || !Object.values(value.counts).every(count => integer(count)) || !Array.isArray(value.excludes) || value.excludes.length !== DIAGNOSTIC_EXCLUDES.length || !DIAGNOSTIC_EXCLUDES.every((entry, index) => (value.excludes as unknown[])[index] === entry)) throw badResponse();
  return structuredClone(value) as unknown as SafeDiagnostics;
}
export function diagnosticJson(value: unknown): string { return JSON.stringify(parseSafeDiagnostics(value), null, 2); }
export function backupRequestError(parent: string, name: string): string | null {
  if (!/^(?:\/|[A-Za-z]:[\\/]|\\\\[^\\]+\\[^\\]+)/.test(parent) || bytes(parent) > 4096 || parent.includes("\0") || parent.split(/[\\/]/).some(part => part === "." || part === "..")) return "请选择后台机器上已存在的绝对父目录（最多 4096 UTF-8 字节，不能含 . 或 .. 路径段）。";
  if (!/^[A-Za-z0-9_-]{1,80}$/.test(name)) return "新备份名称须为 1–80 位 ASCII 字母、数字、短横线或下划线。";
  if (bytes(JSON.stringify({ id: "0".repeat(36), parent, name })) > 8192) return "备份请求编码后超过 8 KiB，请缩短路径或名称。";
  return null;
}
export interface BackupRecovery { load(): string | null; save(id: string): void; clear(id: string): void; history(): string[]; archive(id: string): void }
const RECOVERY_KEY = "self-agent.backup-recovery-id.v1";
const HISTORY_KEY = "self-agent.backup-observation-history.v1";
export const MAX_BACKUP_OBSERVATIONS = 64;
function observationHistory(): string[] {
  const raw = localStorage.getItem(HISTORY_KEY); const values: unknown = raw === null ? [] : JSON.parse(raw);
  if (!Array.isArray(values) || values.length > MAX_BACKUP_OBSERVATIONS || !values.every(id => typeof id === "string" && providerUuid(id)) || new Set(values).size !== values.length) throw new Error("invalid_observation_history");
  return values;
}
export const localBackupRecovery: BackupRecovery = {
  load() { const id = localStorage.getItem(RECOVERY_KEY); if (id !== null && !providerUuid(id)) throw new Error("invalid_recovery_id"); return id; },
  save(id) { const current = localStorage.getItem(RECOVERY_KEY); if (!providerUuid(id) || (current !== null && current !== id) || observationHistory().length >= MAX_BACKUP_OBSERVATIONS) throw new Error("invalid_or_full_recovery_history"); localStorage.setItem(RECOVERY_KEY, id); },
  clear(id) { if (localStorage.getItem(RECOVERY_KEY) === id) localStorage.removeItem(RECOVERY_KEY); },
  history: observationHistory,
  archive(id) {
    if (!providerUuid(id)) throw new Error("invalid_recovery_id");
    const history = observationHistory();
    if (!history.includes(id)) {
      if (history.length >= MAX_BACKUP_OBSERVATIONS) throw new Error("observation_history_full");
      // Write history first: a failed clear must never lose the unresolved UUID.
      localStorage.setItem(HISTORY_KEY, JSON.stringify([...history, id]));
    }
    if (localStorage.getItem(RECOVERY_KEY) === id) localStorage.removeItem(RECOVERY_KEY);
  },
};
const activeJob = (job: BackupReceipt) => job.state === "pending" || job.state === "running";
function advance(previous: BackupReceipt | undefined, next: BackupReceipt): BackupReceipt {
  if (!previous || activeJob(previous) && !(previous.state === "running" && next.state === "pending")) return next;
  return previous;
}
export interface ReliabilityState { status: ReliabilityStatus | null; writing: boolean; loading: boolean; inspecting: boolean; diagnosticLoading: boolean; notice: string; diagnosticNotice: string; recoveryId: string | null; observationHistory: string[]; selected: BackupReceipt | null; diagnostic: SafeDiagnostics | null }
export function createReliabilityController(client: ReliabilityClient, changed: (state: ReliabilityState) => void, recovery: BackupRecovery, options: { uuid?: () => string; pollMs?: number } = {}) {
  let state: ReliabilityState = { status: null, writing: false, loading: false, inspecting: false, diagnosticLoading: false, notice: "", diagnosticNotice: "", recoveryId: null, observationHistory: [], selected: null, diagnostic: null };
  try { const id = recovery.load(); if (id !== null && !providerUuid(id)) throw new Error("invalid_recovery_id"); state.recoveryId = id; state.observationHistory = recovery.history(); } catch { state.notice = "本机操作恢复标识无法读取；请先核对历史记录，未发送任何备份请求。"; }
  let active = true, version = 0, inspectVersion = 0, diagnosticVersion = 0, write: AbortController | undefined, read: AbortController | undefined, inspectRead: AbortController | undefined, diagnosticRead: AbortController | undefined, flight: Promise<unknown> | undefined, timer: ReturnType<typeof setTimeout> | undefined;
  const publish = (patch: Partial<ReliabilityState>) => { if (active) { state = { ...state, ...patch }; changed(state); } };
  const schedule = () => { clearTimeout(timer); if (active && (state.recoveryId || state.status?.backup_jobs.some(job => activeJob(job) && !state.observationHistory.includes(job.id)))) timer = setTimeout(() => { void refresh(); if (state.recoveryId) void inspect(state.recoveryId); }, options.pollMs ?? 1500); };
  function accept(job: BackupReceipt) {
    const previous = state.status?.backup_jobs.find(item => item.id === job.id) ?? (state.selected?.id === job.id ? state.selected : undefined);
    job = advance(previous, job);
    publish({ selected: job, ...(state.status ? { status: { ...state.status, backup_jobs: [...state.status.backup_jobs.filter(item => item.id !== job.id), job].slice(-256) } } : {}) });
    if (state.recoveryId === job.id && !activeJob(job)) {
      try { recovery.clear(job.id); publish({ recoveryId: null }); } catch { publish({ notice: "原操作终态已确认，但本机恢复标识未清除；重开只会查询原记录。" }); }
    }
  }
  async function refresh() {
    if (!active || state.loading) return; const current = ++version; read?.abort(); read = new AbortController(); publish({ loading: true });
    try {
      const snapshot = parseReliabilityStatus(await client.status(read.signal)); if (!active || current !== version) return;
      snapshot.backup_jobs = snapshot.backup_jobs.map(job => advance(state.status?.backup_jobs.find(previous => previous.id === job.id) ?? (state.selected?.id === job.id ? state.selected : undefined), job));
      publish({ status: snapshot });
      const known = snapshot.backup_jobs.find(job => job.id === state.recoveryId); if (known) accept(known);
    } catch { if (active && current === version) publish({ notice: "可靠性状态暂不可读或格式不受支持；没有以空历史代替，也没有重发备份。" }); }
    finally { if (active && current === version) { publish({ loading: false }); schedule(); } }
  }
  async function inspect(id: string) {
    if (!active || state.writing || state.inspecting || !providerUuid(id)) return;
    const current = ++inspectVersion; inspectRead?.abort(); inspectRead = new AbortController(); publish({ inspecting: true });
    try {
      const job = parseBackupReceipt(await client.receipt(id, inspectRead.signal)); if (job.id !== id) throw badResponse(); if (!active || current !== inspectVersion) return;
      accept(job); publish({ notice: "已只读查询原备份记录；没有再次发起导出。" });
    } catch { if (active && current === inspectVersion) publish({ notice: "原备份操作暂不可读或不存在，结果仍未确认；找不到记录不代表没有写入，请核对原操作及目标目录。" }); }
    finally { if (active && current === inspectVersion) { publish({ inspecting: false }); schedule(); } }
  }
  function backup(parent: string, name: string, confirmed: boolean): Promise<unknown> {
    if (!active || flight || state.writing) return Promise.resolve(undefined);
    const error = backupRequestError(parent, name);
    if (!confirmed || error || !state.status?.backup_available || state.recoveryId || state.status.backup_jobs.some(activeJob) || state.status.backup_jobs.length >= 256 || state.observationHistory.length >= MAX_BACKUP_OBSERVATIONS) { publish({ notice: error ?? "请先核对备份可用性、未决记录、容量并确认私有数据风险；没有发送新备份。" }); return Promise.resolve(undefined); }
    const id = (options.uuid ?? (() => crypto.randomUUID()))();
    try { if (!providerUuid(id)) throw new Error("invalid_id"); recovery.save(id); } catch { publish({ notice: "无法保存操作恢复标识，未发送备份；请检查本机存储权限、观察容量或其他窗口的未决操作。" }); return Promise.resolve(undefined); }
    ++version; read?.abort(); write = new AbortController(); const signal = write.signal; publish({ writing: true, loading: false, recoveryId: id, selected: null, notice: "备份请求正在提交；接受不等于完成。" });
    flight = (async () => {
      try {
        const value = await Promise.resolve().then(() => active ? client.backup({ id, parent, name }, signal) : undefined); if (!active) return;
        const job = parseBackupReceipt(value); if (job.id !== id) throw badResponse(); accept(job);
        publish({ notice: activeJob(job) ? "备份意图已登记；等待后台持久回执确认完成，不会重复提交。" : "已取得备份操作回执，请以其实际终态和清单为准。" });
        await refresh(); return job;
      } catch { publish({ notice: "提交结果未知。已保留操作 ID，只读查询恢复；关闭页面不会撤销后台导出，不自动重发。" }); }
      finally { flight = undefined; publish({ writing: false }); schedule(); }
    })(); return flight;
  }
  async function diagnostics() {
    if (!active) return; const current = ++diagnosticVersion; diagnosticRead?.abort(); diagnosticRead = new AbortController(); publish({ diagnosticLoading: true, diagnostic: null, diagnosticNotice: "" });
    try { const diagnostic = parseSafeDiagnostics(await client.diagnostics(diagnosticRead.signal)); if (active && current === diagnosticVersion) publish({ diagnostic }); }
    catch { if (active && current === diagnosticVersion) publish({ diagnosticNotice: "诊断暂不可用或包含未允许字段；未展示、下载原始回应，也不会改用全量快照。" }); }
    finally { if (active && current === diagnosticVersion) publish({ diagnosticLoading: false }); }
  }
  return { getState: () => state, refresh, inspect, backup, diagnostics,
    finishObservation(confirmed: boolean) {
      if (!active || !confirmed || !state.recoveryId || state.writing || state.inspecting) return false;
      try {
        recovery.archive(state.recoveryId);
        publish({ recoveryId: null, observationHistory: recovery.history(), notice: "仅结束本机主动观察，旧操作可能仍在运行或已写入；UUID 已保留。请检查旧目标，后续备份必须显式选择不同的新名称；没有撤销、删除后台记录或重发旧请求。" });
        schedule(); return true;
      } catch { publish({ notice: "观察历史已满或无法保存；保留原操作 ID，不接受新的观察，也未删除任何后台记录。" }); return false; }
    },
    start() { publish({}); void refresh(); if (state.recoveryId) void inspect(state.recoveryId); },
    dispose() { active = false; ++version; ++inspectVersion; ++diagnosticVersion; read?.abort(); inspectRead?.abort(); diagnosticRead?.abort(); write?.abort(); clearTimeout(timer); },
  };
}
