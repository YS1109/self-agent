//! Explicit, bounded MCP stdio only. No SDK logs, automatic retry, HTTP, resource
//! reads, MRTR continuation, sampling, elicitation or background tasks.
//! Modern wire: https://modelcontextprotocol.io/specification/2026-07-28/basic
//! Legacy wire: https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle
//! Raw server metadata/results and resolved credentials are private, untrusted
//! values: deliberately no Debug/Serialize implementations on those containers.

use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashSet},
    fmt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    task::JoinHandle,
    time::{sleep, sleep_until, Instant},
};

#[path = "mcp/process.rs"]
mod process;
pub(crate) use process::CleanupStatus;
use process::{PipeReader, PipeWriter};

const MAX_LINE: usize = 1024 * 1024;
const MAX_OPERATION: usize = 4 * 1024 * 1024;
const MAX_STDERR: usize = 64 * 1024;
// Reserve the entire bounded stderr lifetime so even a concurrent burst cannot
// make one operation exceed the combined traffic allowance (plus one sentinel).
const MAX_PROTOCOL_OPERATION: usize = MAX_OPERATION - MAX_STDERR;
const MAX_NOTIFICATIONS: usize = 256;
const MAX_SCHEMA: usize = 32 * 1024;
const MAX_ARGUMENTS: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProtocolMode {
    Modern,
    Legacy,
}
impl ProtocolMode {
    pub fn version(self) -> &'static str {
        match self {
            Self::Modern => "2026-07-28",
            Self::Legacy => "2025-11-25",
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Limits {
    pub setup_timeout: Duration,
    pub call_timeout: Duration,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            setup_timeout: Duration::from_secs(10),
            call_timeout: Duration::from_secs(30),
        }
    }
}

pub(crate) struct ServerConfig {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub mode: ProtocolMode,
    pub limits: Limits,
}

#[derive(Clone, Copy)]
pub(crate) struct Control<'a> {
    pub deadline: Instant,
    pub cancelled: &'a (dyn Fn() -> bool + Sync),
}
impl Control<'_> {
    fn check(self) -> Result<(), McpError> {
        if (self.cancelled)() {
            return Err(McpError::new("MCP 操作已取消"));
        }
        if Instant::now() >= self.deadline {
            return Err(McpError::new("MCP 操作已达到任务截止时间"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct McpError {
    pub reason: &'static str,
    pub cleanup: Option<CleanupStatus>,
}
impl McpError {
    fn new(reason: &'static str) -> Self {
        Self {
            reason,
            cleanup: None,
        }
    }
}
impl fmt::Display for McpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason)
    }
}
impl std::error::Error for McpError {}

pub(crate) struct Discovery {
    pub protocol: ProtocolMode,
    pub tools: bool,
    pub metadata: Value,
}
pub(crate) struct Tool {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    pub output_schema: Option<Value>,
    /// Hints are untrusted; never infer permission or absence of side effects.
    pub annotations: Option<Value>,
}

pub(crate) enum CallOutcome {
    NotSent {
        reason: &'static str,
    },
    Completed(Value),
    /// Explicit terminal tool/RPC error; does not imply the tool had no effects.
    ToolError(Value),
    /// Includes partial write, EOF, timeout, cancellation, unsupported input or
    /// task continuation. A write may have happened: no automatic replay.
    Unknown {
        reason: &'static str,
        cleanup: CleanupStatus,
    },
}

enum Reply {
    Result(Value),
    Error(Value),
}
#[derive(Default)]
struct Budget {
    bytes: usize,
    notifications: usize,
}

pub(crate) struct McpSession {
    process: process::Process,
    stdin: Option<PipeWriter>,
    stdout: BufReader<PipeReader>,
    stderr_task: Option<JoinHandle<()>>,
    stderr_overflow: Arc<AtomicBool>,
    mode: ProtocolMode,
    limits: Limits,
    next_id: u64,
    ready: bool,
    tools_capability: bool,
    valid: bool,
    tools: HashSet<String>,
}

impl McpSession {
    /// Launch is itself side-effectful and unsandboxed. Caller must authorize
    /// the executable/config before calling this, separately from tools/call.
    pub async fn open(
        config: &ServerConfig,
        root: &Path,
        control: Control<'_>,
    ) -> Result<Self, McpError> {
        control.check().map_err(not_started)?;
        validate_config(config).map_err(not_started)?;
        let mut process = process::Process::spawn(config, root, control)
            .await
            .map_err(not_started)?;
        let stdin = process
            .child
            .stdin
            .take()
            .ok_or_else(|| McpError::new("MCP 标准输入不可用"))?;
        let stdout = process
            .child
            .stdout
            .take()
            .ok_or_else(|| McpError::new("MCP 标准输出不可用"))?;
        let mut stderr = process
            .child
            .stderr
            .take()
            .ok_or_else(|| McpError::new("MCP 标准错误不可用"))?;
        let stderr_overflow = Arc::new(AtomicBool::new(false));
        let overflow = stderr_overflow.clone();
        let stderr_task = tokio::spawn(async move {
            let mut buffer = [0u8; 4096];
            let mut bytes = 0usize;
            loop {
                let remaining = (MAX_STDERR + 1).saturating_sub(bytes).min(buffer.len());
                match stderr.read(&mut buffer[..remaining]).await {
                    Ok(0) => break,
                    Ok(count) => {
                        bytes = bytes.saturating_add(count);
                        if bytes > MAX_STDERR {
                            overflow.store(true, Ordering::SeqCst);
                            break;
                        }
                    }
                    Err(_) => {
                        overflow.store(true, Ordering::SeqCst);
                        break;
                    }
                }
            }
        });
        let mut session = Self {
            process,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            stderr_task: Some(stderr_task),
            stderr_overflow,
            mode: config.mode,
            limits: config.limits,
            next_id: 1,
            ready: false,
            tools_capability: false,
            valid: true,
            tools: HashSet::new(),
        };
        if let Err(mut error) = control.check() {
            error.cleanup = Some(session.close(control).await);
            return Err(error);
        }
        Ok(session)
    }

    pub async fn discover(&mut self, control: Control<'_>) -> Result<Discovery, McpError> {
        if self.ready || !self.valid {
            return Err(McpError::new("MCP 会话状态不允许重复发现"));
        }
        let deadline = self.operation_deadline(control, self.limits.setup_timeout);
        let mut budget = Budget::default();
        let (method, params) = match self.mode {
            ProtocolMode::Modern => ("server/discover", json!({})),
            ProtocolMode::Legacy => (
                "initialize",
                json!({"protocolVersion":self.mode.version(), "capabilities":{}, "clientInfo":{"name":"self_agent","version":env!("CARGO_PKG_VERSION")}}),
            ),
        };
        let mut sent = false;
        let result = self
            .exchange(method, params, control, deadline, &mut budget, &mut sent)
            .await;
        let parsed = match result {
            Ok(Reply::Result(value)) => {
                let version_ok = match self.mode {
                    ProtocolMode::Modern => {
                        value["supportedVersions"]
                            .as_array()
                            .is_some_and(|versions| {
                                versions.len() <= 16
                                    && versions
                                        .iter()
                                        .any(|version| version == self.mode.version())
                            })
                    }
                    ProtocolMode::Legacy => value["protocolVersion"] == self.mode.version(),
                };
                if !version_ok || !value["capabilities"].is_object() {
                    Err(McpError::new("MCP 服务协议版本或能力响应无效"))
                } else {
                    Ok(value)
                }
            }
            Ok(Reply::Error(_)) => Err(McpError::new("MCP 服务拒绝所选协议；不会自动降级或重试")),
            Err(error) => Err(error),
        };
        let value = match parsed {
            Ok(value) => value,
            Err(error) => return Err(self.invalidate(error, control).await),
        };
        if self.mode == ProtocolMode::Legacy {
            let bytes = b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n";
            if let Err(error) = self.write(bytes, control, deadline).await {
                return Err(self.invalidate(error, control).await);
            }
        }
        self.ready = true;
        self.tools_capability = value["capabilities"]["tools"].is_object();
        Ok(Discovery {
            protocol: self.mode,
            tools: self.tools_capability,
            metadata: value,
        })
    }

    pub async fn list_tools(&mut self, control: Control<'_>) -> Result<Vec<Tool>, McpError> {
        if !self.ready || !self.valid || !self.tools_capability {
            return Err(McpError::new("MCP 服务尚未确认工具能力"));
        }
        let result = self.list_inner(control).await;
        match result {
            Ok(tools) => {
                self.tools = tools.iter().map(|tool| tool.name.clone()).collect();
                Ok(tools)
            }
            Err(error) => Err(self.invalidate(error, control).await),
        }
    }

    async fn list_inner(&mut self, control: Control<'_>) -> Result<Vec<Tool>, McpError> {
        let deadline = self.operation_deadline(control, self.limits.setup_timeout);
        let mut budget = Budget::default();
        let mut cursor = None;
        let mut cursors = HashSet::new();
        let mut names = HashSet::new();
        let mut tools = Vec::new();
        for _ in 0..8 {
            let params = cursor
                .as_ref()
                .map_or_else(|| json!({}), |cursor| json!({"cursor":cursor}));
            let result = match self
                .exchange(
                    "tools/list",
                    params,
                    control,
                    deadline,
                    &mut budget,
                    &mut false,
                )
                .await?
            {
                Reply::Result(result) => result,
                Reply::Error(_) => return Err(McpError::new("MCP 工具列表请求失败")),
            };
            let page = result["tools"]
                .as_array()
                .ok_or_else(|| McpError::new("MCP 工具列表格式无效"))?;
            if tools.len() + page.len() > 128 {
                return Err(McpError::new("MCP 工具数量超过 128 项限制"));
            }
            for item in page {
                let tool = parse_tool(item)?;
                if !names.insert(tool.name.clone()) {
                    return Err(McpError::new("MCP 工具列表含重复名称"));
                }
                tools.push(tool);
            }
            cursor = match result.get("nextCursor") {
                None | Some(Value::Null) => return Ok(tools),
                Some(Value::String(value))
                    if !value.is_empty()
                        && value.len() <= 4096
                        && cursors.insert(value.clone()) =>
                {
                    Some(value.clone())
                }
                _ => return Err(McpError::new("MCP 工具分页游标无效或重复")),
            };
        }
        Err(McpError::new("MCP 工具列表超过 8 页限制"))
    }

    /// Integration validates JSON Schema and persists call-start BEFORE invoking
    /// this method. This client never interprets annotations as authorization.
    pub async fn call_once(
        &mut self,
        name: &str,
        arguments: &Value,
        control: Control<'_>,
    ) -> CallOutcome {
        if let Err(error) = control.check() {
            return CallOutcome::NotSent {
                reason: error.reason,
            };
        }
        if !self.ready || !self.valid || !self.tools.contains(name) {
            return CallOutcome::NotSent {
                reason: "MCP 工具未经当前会话发现或会话已关闭",
            };
        }
        if !arguments.is_object() || !within_bytes(arguments, MAX_ARGUMENTS) {
            return CallOutcome::NotSent {
                reason: "MCP 工具参数格式无效或超过 64 KiB",
            };
        }
        let mut sent = false;
        let deadline = self.operation_deadline(control, self.limits.call_timeout);
        let result = self
            .exchange(
                "tools/call",
                json!({"name":name,"arguments":arguments}),
                control,
                deadline,
                &mut Budget::default(),
                &mut sent,
            )
            .await;
        match result {
            Ok(Reply::Error(value)) => CallOutcome::ToolError(json!({"error":value})),
            Ok(Reply::Result(value)) => {
                let content = value.get("content");
                let structured = value.get("structuredContent");
                let valid_content = (content.is_some() || structured.is_some())
                    && content.is_none_or(|v| {
                        v.as_array()
                            .is_some_and(|items| items.iter().all(valid_content_block))
                    })
                    && structured
                        .is_none_or(|v| self.mode == ProtocolMode::Modern || v.is_object());
                let valid_error = value.get("isError").is_none_or(Value::is_boolean);
                if !valid_content
                    || !valid_error
                    || value.get("task").is_some()
                    || value.get("taskId").is_some()
                {
                    let cleanup = self.close(control).await;
                    return CallOutcome::Unknown {
                        reason: "MCP 工具结果格式无效或要求不支持的异步任务",
                        cleanup,
                    };
                }
                if value["isError"] == true {
                    CallOutcome::ToolError(value)
                } else {
                    CallOutcome::Completed(value)
                }
            }
            Err(error) => {
                let cleanup = self.close(control).await;
                if sent {
                    CallOutcome::Unknown {
                        reason: error.reason,
                        cleanup,
                    }
                } else {
                    CallOutcome::NotSent {
                        reason: error.reason,
                    }
                }
            }
        }
    }

    fn operation_deadline(&self, control: Control<'_>, maximum: Duration) -> Instant {
        control.deadline.min(Instant::now() + maximum)
    }

    async fn exchange(
        &mut self,
        method: &str,
        mut params: Value,
        control: Control<'_>,
        deadline: Instant,
        budget: &mut Budget,
        sent: &mut bool,
    ) -> Result<Reply, McpError> {
        control.check()?;
        if !self.valid || self.stdin.is_none() {
            return Err(McpError::new("MCP 会话已关闭"));
        }
        if self.mode == ProtocolMode::Modern {
            params["_meta"] = json!({"io.modelcontextprotocol/protocolVersion":self.mode.version(),"io.modelcontextprotocol/clientCapabilities":{},"io.modelcontextprotocol/clientInfo":{"name":"self_agent","version":env!("CARGO_PKG_VERSION")}});
        }
        let id = self.next_id;
        self.next_id = id
            .checked_add(1)
            .ok_or_else(|| McpError::new("MCP 请求编号已耗尽"))?;
        let mut bytes =
            serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
                .map_err(|_| McpError::new("MCP 请求无法编码"))?;
        bytes.push(b'\n');
        if bytes.len() > MAX_LINE {
            return Err(McpError::new("MCP 请求超过单行预算"));
        }
        budget.bytes = budget.bytes.saturating_add(bytes.len());
        if budget.bytes > MAX_PROTOCOL_OPERATION {
            return Err(McpError::new("MCP 操作总字节数超过安全预算"));
        }
        *sent = true; // Any attempted/partial write is ambiguous: never retry.
        self.write(&bytes, control, deadline).await?;
        loop {
            let mut line = Vec::new();
            let overflow = self.stderr_overflow.clone();
            let remaining = (MAX_PROTOCOL_OPERATION + 1).saturating_sub(budget.bytes);
            let mut reader = (&mut self.stdout).take((MAX_LINE + 1).min(remaining) as u64);
            let count = tokio::select! {
                value = reader.read_until(b'\n', &mut line) => value.map_err(|_|McpError::new("MCP 标准输出读取失败"))?,
                error = stopped(control, deadline, &overflow) => return Err(error),
            };
            self.check_operation(control, deadline)?;
            budget.bytes = budget.bytes.saturating_add(count);
            if count == 0 {
                return Err(McpError::new("MCP 在确认结果前关闭连接"));
            }
            if count > MAX_LINE || budget.bytes > MAX_PROTOCOL_OPERATION {
                return Err(McpError::new("MCP 响应超过安全预算"));
            }
            if line.last() != Some(&b'\n') {
                return Err(McpError::new("MCP 响应缺少完整换行边界"));
            }
            let message: Value = serde_json::from_slice(&line)
                .map_err(|_| McpError::new("MCP 响应不是有效 UTF-8 JSON"))?;
            if !message.is_object() || message["jsonrpc"] != "2.0" {
                return Err(McpError::new("MCP JSON-RPC 信封无效"));
            }
            if message.get("method").is_some() {
                if message.get("id").is_some() {
                    return Err(McpError::new("MCP 服务请求了不支持的反向调用或交互"));
                }
                if !message["method"]
                    .as_str()
                    .is_some_and(|method| !method.is_empty() && method.len() <= 128)
                    || message.get("result").is_some()
                    || message.get("error").is_some()
                {
                    return Err(McpError::new("MCP 通知格式无效"));
                }
                budget.notifications += 1;
                if budget.notifications > MAX_NOTIFICATIONS {
                    return Err(McpError::new("MCP 通知数量超过安全预算"));
                }
                continue; // No raw notification is logged, rendered or executed.
            }
            if message["id"].as_u64() != Some(id) {
                return Err(McpError::new("MCP 响应编号不匹配"));
            }
            match (message.get("result"), message.get("error")) {
                (Some(value), None) if value.is_object() => {
                    // The specification requires missing resultType to mean
                    // complete for backward compatibility, even in modern mode.
                    if !matches!(value.get("resultType"), None | Some(Value::String(_))) {
                        return Err(McpError::new("MCP 结果类型无效"));
                    }
                    if value
                        .get("resultType")
                        .is_some_and(|kind| kind != "complete")
                        || value.get("inputRequests").is_some()
                        || value.get("requestState").is_some()
                    {
                        return Err(McpError::new(
                            "MCP 请求需要不支持的多轮交互或扩展；不会自动重试",
                        ));
                    }
                    return Ok(Reply::Result(value.clone()));
                }
                (None, Some(error))
                    if error["code"].as_i64().is_some() && error["message"].is_string() =>
                {
                    return Ok(Reply::Error(error.clone()))
                }
                _ => return Err(McpError::new("MCP 响应缺少唯一有效结果")),
            }
        }
    }

    async fn write(
        &mut self,
        bytes: &[u8],
        control: Control<'_>,
        deadline: Instant,
    ) -> Result<(), McpError> {
        self.check_operation(control, deadline)?;
        let overflow = self.stderr_overflow.clone();
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| McpError::new("MCP 标准输入已关闭"))?;
        tokio::select! {
            result = async { stdin.write_all(bytes).await?; stdin.flush().await } => result.map_err(|_:std::io::Error|McpError::new("MCP 请求写入失败，结果可能未知")),
            error = stopped(control, deadline, &overflow) => Err(error),
        }
    }

    fn check_operation(&self, control: Control<'_>, deadline: Instant) -> Result<(), McpError> {
        control.check()?;
        if Instant::now() >= deadline {
            return Err(McpError::new("MCP 操作超时"));
        }
        if self.stderr_overflow.load(Ordering::SeqCst) {
            return Err(McpError::new("MCP 标准错误输出超过安全预算"));
        }
        Ok(())
    }

    async fn invalidate(&mut self, mut error: McpError, control: Control<'_>) -> McpError {
        error.cleanup = Some(self.close(control).await);
        error
    }

    /// No MCP shutdown method exists. Closing stdin and bounded process cleanup
    /// are used. Cleanup still runs after cancellation/deadline to avoid leaks.
    pub async fn close(&mut self, control: Control<'_>) -> CleanupStatus {
        self.valid = false;
        self.stdin.take();
        let status = self.process.close(control).await;
        finish_stderr(self.stderr_task.take()).await;
        status
    }
}
async fn finish_stderr(task: Option<JoinHandle<()>>) {
    if let Some(task) = task {
        task.abort();
        // Abort only requests cancellation. Retain and await the actual
        // pipe-reader task; a timeout must not detach it and report cleanup.
        let _ = task.await;
    }
}
/// Validate only wire shape, never decode media, open URIs or infer trust.
fn valid_content_block(block: &Value) -> bool {
    let Some(object) = block.as_object() else {
        return false;
    };
    let string = |key: &str| object.get(key).is_some_and(Value::is_string);
    let optional_string = |key: &str| object.get(key).is_none_or(Value::is_string);
    if object.get("_meta").is_some_and(|v| !v.is_object())
        || object.get("annotations").is_some_and(|v| !v.is_object())
    {
        return false;
    }
    match object.get("type").and_then(Value::as_str) {
        Some("text") => string("text"),
        Some("image" | "audio") => string("data") && string("mimeType"),
        Some("resource_link") => {
            string("uri")
                && string("name")
                && optional_string("description")
                && optional_string("mimeType")
                && optional_string("title")
        }
        Some("resource") => object
            .get("resource")
            .and_then(Value::as_object)
            .is_some_and(|resource| {
                resource.get("uri").is_some_and(Value::is_string)
                    && resource.get("mimeType").is_none_or(Value::is_string)
                    && resource.get("_meta").is_none_or(Value::is_object)
                    && ((resource.get("text").is_some_and(Value::is_string)
                        && resource.get("blob").is_none_or(Value::is_string))
                        || (resource.get("blob").is_some_and(Value::is_string)
                            && resource.get("text").is_none_or(Value::is_string)))
            }),
        // Neither supported protocol era defines arbitrary extension blocks.
        _ => false,
    }
}

fn not_started(mut error: McpError) -> McpError {
    if error.cleanup.is_none() {
        error.cleanup = Some(CleanupStatus::NotStarted);
    }
    error
}
impl Drop for McpSession {
    fn drop(&mut self) {
        self.stdin.take();
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
    }
}

async fn stopped(control: Control<'_>, deadline: Instant, overflow: &AtomicBool) -> McpError {
    loop {
        if let Err(error) = control.check() {
            return error;
        }
        if overflow.load(Ordering::SeqCst) {
            return McpError::new("MCP 标准错误输出超过安全预算");
        }
        tokio::select! { _ = sleep_until(deadline) => return McpError::new("MCP 操作超时"), _ = sleep(Duration::from_millis(25)) => {} }
    }
}

fn parse_tool(item: &Value) -> Result<Tool, McpError> {
    let name = item["name"]
        .as_str()
        .filter(|value| valid_name(value))
        .ok_or_else(|| McpError::new("MCP 工具名称无效"))?;
    let input_schema = &item["inputSchema"];
    if !input_schema.is_object() || !within_bytes(input_schema, MAX_SCHEMA) {
        return Err(McpError::new("MCP 输入 Schema 无效或超过 32 KiB"));
    }
    let output_schema = match item.get("outputSchema") {
        None => None,
        Some(value) if value.is_object() && within_bytes(value, MAX_SCHEMA) => Some(value.clone()),
        _ => return Err(McpError::new("MCP 输出 Schema 无效或超过 32 KiB")),
    };
    let description = match item.get("description") {
        None => None,
        Some(Value::String(value)) if value.len() <= 8192 => Some(value.clone()),
        _ => return Err(McpError::new("MCP 工具说明无效或过长")),
    };
    let annotations = match item.get("annotations") {
        None => None,
        Some(value) if value.is_object() && within_bytes(value, 8192) => Some(value.clone()),
        _ => return Err(McpError::new("MCP 工具注释无效或过长")),
    };
    Ok(Tool {
        name: name.into(),
        description,
        input_schema: input_schema.clone(),
        output_schema,
        annotations,
    })
}
fn within_bytes(value: &Value, limit: usize) -> bool {
    struct Counter {
        used: usize,
        limit: usize,
    }
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.used) {
                return Err(std::io::Error::from(std::io::ErrorKind::InvalidData));
            }
            self.used += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Counter { used: 0, limit }, value).is_ok()
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn validate_config(config: &ServerConfig) -> Result<(), McpError> {
    if !config.program.is_absolute() || config.program.as_os_str().len() > 4096 {
        return Err(McpError::new("MCP 可执行文件必须是已授权的绝对路径"));
    }
    if config.args.len() > 64
        || config
            .args
            .iter()
            .any(|arg| arg.len() > 8192 || arg.contains('\0'))
        || config.args.iter().map(String::len).sum::<usize>() > 64 * 1024
    {
        return Err(McpError::new("MCP 启动参数超过安全预算"));
    }
    let name = config
        .program
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(
        name.as_str(),
        "sh" | "bash"
            | "dash"
            | "zsh"
            | "cmd"
            | "cmd.exe"
            | "powershell.exe"
            | "pwsh"
            | "pwsh.exe"
            | "npx"
            | "npx.cmd"
            | "npm"
            | "npm.cmd"
            | "yarn"
            | "pnpm"
    ) {
        return Err(McpError::new(
            "MCP 必须直接启动已安装服务，不能通过 shell 或包安装器启动",
        ));
    }
    if config.limits.setup_timeout.is_zero()
        || config.limits.setup_timeout > Duration::from_secs(10)
        || config.limits.call_timeout.is_zero()
        || config.limits.call_timeout > Duration::from_secs(30)
    {
        return Err(McpError::new("MCP 超时配置超过安全上限"));
    }
    if config.env.len() > 64
        || config
            .env
            .iter()
            .map(|(key, value)| key.len() + value.len())
            .sum::<usize>()
            > 64 * 1024
    {
        return Err(McpError::new("MCP 显式环境超过安全预算"));
    }
    for (key, value) in &config.env {
        let upper = key.to_ascii_uppercase();
        if key.is_empty()
            || key.len() > 128
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            || key.as_bytes()[0].is_ascii_digit()
            || value.len() > 16384
            || value.contains('\0')
        {
            return Err(McpError::new("MCP 显式环境变量格式无效"));
        }
        if upper.starts_with("LD_")
            || upper.starts_with("DYLD_")
            || upper.starts_with("PYTHON")
            || upper.starts_with("PERL")
            || upper.starts_with("RUBY")
            || upper.starts_with("XDG_")
            || upper.starts_with("DOTNET_")
            || upper.starts_with("CORECLR_")
            || upper.starts_with("COR_")
            || upper.ends_with("PROXY")
            || matches!(
                upper.as_str(),
                "HOME"
                    | "USERPROFILE"
                    | "APPDATA"
                    | "LOCALAPPDATA"
                    | "CODEX_HOME"
                    | "BASH_ENV"
                    | "ENV"
                    | "SHELLOPTS"
                    | "BASHOPTS"
                    | "COMSPEC"
                    | "PSMODULEPATH"
                    | "NODE_OPTIONS"
                    | "NODE_PATH"
                    | "JAVA_TOOL_OPTIONS"
                    | "JDK_JAVA_OPTIONS"
                    | "_JAVA_OPTIONS"
                    | "CLASSPATH"
                    | "GCONV_PATH"
                    | "GLIBC_TUNABLES"
                    | "LOCPATH"
                    | "NLSPATH"
                    | "GIO_EXTRA_MODULES"
                    | "GTK_PATH"
                    | "ZDOTDIR"
                    | "BASH_XTRACEFD"
                    | "PROMPT_COMMAND"
                    | "CDPATH"
                    | "IFS"
                    | "RUSTC_WRAPPER"
                    | "RUSTC_WORKSPACE_WRAPPER"
            )
        {
            return Err(McpError::new(
                "MCP 环境变量可能注入运行时或读取宿主配置，已拒绝启动",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "mcp/tests.rs"]
mod tests;
