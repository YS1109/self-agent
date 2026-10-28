use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use harness_core::{data_dir, new_id, DaemonInfo};
use serde_json::{json, Value};

const HELP: &str = r#"Self Agent 命令行客户端

用法：self-agent-cli [--data-dir 目录] [--conversation 来源会话] [--command-id 幂等ID] [--wait 秒] [--follow] 命令 [参数]

  sessions                         列出会话
  create <标题> <项目绝对路径>       创建并选择会话
  select <会话ID>                   选择此命令来源的默认会话
  run <会话ID> <内容|->             主智能体直接执行；- 从标准输入读取
  send <会话ID> <内容|->            远程编排：生成计划或讨论
  status [任务ID]                   查询完整状态或单个任务
  cancel <任务ID>                   取消任务
  follow <任务ID>                   跟随已有任务的持久化事件与终态
  settings [JSON文件|-]             查看公开设置或提交设置补丁
  models [JSON文件|-]               从当前或草稿Endpoint获取模型
  command <JSON文件|->              发送完整指令对象，支持所有管理命令

先独立启动 harness-daemon。客户端读取同一数据目录的私有 daemon.json，
自动进行本机认证；不需要桌面窗口。所有成功输出为JSON，错误写入标准错误。
--wait 适用于返回任务ID的命令及follow；终态失败或超时会返回非零退出码。
--follow 用于run或返回任务ID的command，逐行输出JSON收据、事件和任务快照。
follow先输出当前快照再跟随新事件；自动重连，恢复快照带reset:true。
跟随默认等待3600秒，可用--wait覆盖；超时不会取消后台任务。
输入密钥时请使用受保护的JSON文件或标准输入，避免写入命令参数。
"#;

#[derive(Debug)]
struct Options {
    directory: PathBuf,
    conversation: String,
    command_id: String,
    wait: Option<u64>,
    follow: bool,
    command: String,
    args: Vec<String>,
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Option<Options>> {
    let mut options = Options {
        directory: data_dir(),
        conversation: "default".into(),
        command_id: new_id(),
        wait: None,
        follow: false,
        command: String::new(),
        args: Vec::new(),
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--follow" => options.follow = true,
            "--data-dir" | "--conversation" | "--command-id" | "--wait" => {
                let value = args.next().context("选项缺少参数")?;
                match arg.as_str() {
                    "--data-dir" => options.directory = PathBuf::from(value),
                    "--conversation" => options.conversation = value,
                    "--command-id" => options.command_id = value,
                    "--wait" => {
                        let seconds: u64 = value.parse().context("等待秒数必须是正整数")?;
                        if !(1..=86_400).contains(&seconds) {
                            bail!("等待时间范围为1至86400秒");
                        }
                        options.wait = Some(seconds);
                    }
                    _ => unreachable!(),
                }
            }
            _ if arg.starts_with('-') => bail!("未知选项：{arg}"),
            _ => {
                options.command = arg;
                options.args = args.collect();
                return Ok(Some(options));
            }
        }
    }
    Ok(None)
}

fn input(argument: &str, file: bool) -> Result<String> {
    if argument == "-" {
        let mut buffer = Vec::new();
        std::io::stdin()
            .take(2 * 1024 * 1024 + 1)
            .read_to_end(&mut buffer)?;
        if buffer.len() > 2 * 1024 * 1024 {
            bail!("标准输入超过2 MiB限制");
        }
        Ok(String::from_utf8(buffer).context("标准输入必须为UTF-8文本")?)
    } else if file {
        let bytes = std::fs::read(argument).context("无法读取JSON文件")?;
        if bytes.len() > 2 * 1024 * 1024 {
            bail!("JSON文件超过2 MiB限制");
        }
        Ok(String::from_utf8(bytes).context("JSON文件必须为UTF-8文本")?)
    } else {
        Ok(argument.to_owned())
    }
}

fn arity(options: &Options, min: usize, max: usize) -> Result<()> {
    if !(min..=max).contains(&options.args.len()) {
        bail!("命令参数数量无效，使用 --help 查看用法");
    }
    Ok(())
}

fn loopback_url(base: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(base).context("后台元数据地址无效")?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        bail!("后台元数据必须指向本机127.0.0.1 HTTP服务");
    }
    Ok(url)
}

struct Client {
    http: reqwest::Client,
    base: reqwest::Url,
    token: String,
}

impl Client {
    fn open(directory: &std::path::Path) -> Result<Self> {
        let metadata = std::fs::read(directory.join("daemon.json"))
            .context("未找到后台元数据，请先启动同一数据目录的 harness-daemon")?;
        let info: DaemonInfo = serde_json::from_slice(&metadata).context("后台元数据格式无效")?;
        Ok(Self {
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .timeout(Duration::from_secs(40))
                .build()?,
            base: loopback_url(&info.base_url)?,
            token: info.token,
        })
    }

    async fn request(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        let mut request = self
            .http
            .request(method.parse()?, self.base.join(path)?)
            .bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let mut response = request.send().await.context("无法连接本机后台服务")?;
        let status = response.status();
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.context("无法读取后台响应")? {
            if bytes.len() + chunk.len() > 16 * 1024 * 1024 {
                bail!("后台响应超过16 MiB限制");
            }
            bytes.extend_from_slice(&chunk);
        }
        let value: Value = serde_json::from_slice(&bytes).context("后台返回无效JSON")?;
        if !status.is_success() {
            bail!("{}", value["error"].as_str().unwrap_or("后台请求失败"));
        }
        Ok(value)
    }
}

fn command(options: &Options) -> Result<Option<Value>> {
    let args = &options.args;
    let value = match options.command.as_str() {
        "create" => {
            arity(options, 2, 2)?;
            json!({"type":"create_session","title":args[0],"project_path":args[1]})
        }
        "select" => {
            arity(options, 1, 1)?;
            json!({"type":"select_session","session_id":args[0]})
        }
        "run" | "send" => {
            arity(options, 2, 2)?;
            json!({"type":if options.command=="run" {"run_agent"} else {"send_message"},
                "session_id":args[0], "content":input(&args[1],false)?})
        }
        "cancel" => {
            arity(options, 1, 1)?;
            json!({"type":"cancel_task","task_id":args[0]})
        }
        "command" => {
            arity(options, 1, 1)?;
            let value: Value =
                serde_json::from_str(&input(&args[0], true)?).context("指令JSON格式无效")?;
            if !value.is_object() || !value["type"].is_string() {
                bail!("指令必须是包含type的JSON对象");
            }
            value
        }
        "sessions" | "status" | "settings" | "models" | "follow" => return Ok(None),
        _ => bail!("未知命令，使用 --help 查看用法"),
    };
    Ok(Some(value))
}

fn task<'a>(snapshot: &'a Value, id: &str) -> Result<&'a Value> {
    snapshot["tasks"]
        .as_array()
        .and_then(|tasks| tasks.iter().find(|task| task["id"] == id))
        .context("未找到指定任务")
}

fn validate_follow(options: &Options, command: Option<&Value>) -> Result<()> {
    if !options.follow || options.command == "follow" {
        return Ok(());
    }
    if !matches!(options.command.as_str(), "run" | "command")
        || !matches!(
            command.and_then(|value| value["type"].as_str()),
            Some("run_agent" | "start_task" | "start_plan" | "resume_plan" | "cancel_task")
        )
    {
        bail!("--follow 仅适用于返回任务ID的run或command指令；已有任务请使用follow")
    }
    Ok(())
}

fn snapshot_cursor(snapshot: &Value) -> Result<u64> {
    snapshot["event_cursor"]
        .as_u64()
        .context("后台不支持持久化事件游标，请升级后台后再使用跟随功能")
}

fn is_terminal(task: &Value) -> bool {
    matches!(
        task["status"].as_str(),
        Some("completed" | "failed" | "cancelled")
    )
}

fn terminal_result(task: &Value) -> Result<()> {
    if task["status"] != "completed" {
        bail!("任务未成功完成");
    }
    Ok(())
}

fn write_ndjson(output: &mut impl Write, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *output, value)?;
    output.write_all(b"\n")?;
    // Pipes must receive increments immediately rather than only at process exit.
    output.flush()?;
    Ok(())
}

fn print_task_snapshot(
    output: &mut impl Write,
    task: &Value,
    cursor: u64,
    reset: bool,
) -> Result<()> {
    write_ndjson(
        output,
        &json!({"type":"task_snapshot", "task":task, "event_cursor":cursor, "reset":reset}),
    )
}

const MAX_SSE_FRAME_BYTES: usize = 512 * 1024;

#[derive(Debug, Default)]
struct SseFrame {
    event: String,
    id: Option<String>,
    data: String,
}

#[derive(Default)]
struct SseDecoder {
    line: Vec<u8>,
    frame: SseFrame,
    frame_bytes: usize,
    skip_lf: bool,
    has_data: bool,
}

impl SseDecoder {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseFrame>> {
        let mut frames = Vec::new();
        for &byte in bytes {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            self.frame_bytes += 1;
            if self.frame_bytes > MAX_SSE_FRAME_BYTES {
                bail!("后台事件超过512 KiB限制");
            }
            if byte == b'\n' || byte == b'\r' {
                self.skip_lf = byte == b'\r';
                if self.line.is_empty() {
                    let frame = std::mem::take(&mut self.frame);
                    if self.has_data || !frame.event.is_empty() {
                        frames.push(frame);
                    }
                    self.has_data = false;
                    self.frame_bytes = 0;
                } else {
                    self.consume_line()?;
                }
            } else {
                self.line.push(byte);
            }
        }
        Ok(frames)
    }

    fn consume_line(&mut self) -> Result<()> {
        let bytes = std::mem::take(&mut self.line);
        let line = std::str::from_utf8(&bytes).context("后台事件不是有效UTF-8文本")?;
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.frame.event = value.to_owned(),
            "id" if !value.contains('\0') => self.frame.id = Some(value.to_owned()),
            "data" => {
                if self.has_data {
                    self.frame.data.push('\n');
                }
                self.frame.data.push_str(value);
                self.has_data = true;
            }
            _ => {} // Comments, retry and future fields do not contain task data.
        }
        Ok(())
    }
}

/// Write first, then commit the local cursor. Duplicate replay and unrelated
/// tasks cannot duplicate text or trap a reconnect behind another task's events.
fn accept_event(
    output: &mut impl Write,
    frame: SseFrame,
    task_id: &str,
    cursor: &mut u64,
) -> Result<bool> {
    let sequence: u64 = frame
        .id
        .as_deref()
        .context("后台事件缺少持久化游标")?
        .parse()
        .context("后台事件游标无效")?;
    if sequence == 0 {
        bail!("后台事件游标无效");
    }
    let event: Value = serde_json::from_str(&frame.data).context("后台事件JSON无效")?;
    if event["sequence"].as_u64() != Some(sequence) {
        bail!("后台事件游标与内容不一致");
    }
    if sequence <= *cursor {
        return Ok(false);
    }
    let selected = event["task_id"].as_str() == Some(task_id);
    let terminal = selected
        && matches!(
            event["kind"].as_str(),
            Some("task_completed" | "task_failed" | "task_cancelled")
        );
    if selected {
        write_ndjson(output, &json!({"type":"event", "event":event}))?;
    }
    *cursor = sequence;
    Ok(terminal)
}

async fn follow_task(
    directory: &Path,
    task_id: &str,
    cursor: u64,
    seconds: u64,
    output: &mut impl Write,
) -> Result<()> {
    tokio::time::timeout(
        Duration::from_secs(seconds),
        follow_loop(directory, task_id, cursor, output),
    )
    .await
    .context("跟随超时；后台任务继续运行，可使用status、follow或cancel")?
}

async fn follow_loop(
    directory: &Path,
    task_id: &str,
    mut cursor: u64,
    output: &mut impl Write,
) -> Result<()> {
    let mut rejected_auth = false;
    loop {
        // The daemon rotates its token (and may change port) on restart. Reread
        // the private metadata, never forward this token outside loopback.
        let client = match Client::open(directory) {
            Ok(client) => client,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                // Windows replaces daemon.json with remove + rename. Only this
                // transient absence is retryable; invalid or nonlocal metadata
                // must still fail closed. follow_task bounds the entire wait.
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
            Err(error) => return Err(error),
        };
        let request = client
            .http
            .get(
                client
                    .base
                    .join(&format!("/api/v1/events?after={cursor}"))?,
            )
            .bearer_auth(&client.token);
        let response = request.send().await;
        let mut response = match response {
            Ok(response) => response,
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
        };
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            if rejected_auth {
                bail!("本机后台认证失败，请检查所选数据目录并重新运行");
            }
            rejected_auth = true;
            tokio::time::sleep(Duration::from_millis(250)).await;
            continue;
        }
        rejected_auth = false;
        if status == reqwest::StatusCode::CONFLICT {
            let snapshot = match client.request("GET", "/api/v1/snapshot", None).await {
                Ok(snapshot) => snapshot,
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    continue;
                }
            };
            cursor = snapshot_cursor(&snapshot)?;
            let current = task(&snapshot, task_id)?;
            print_task_snapshot(output, current, cursor, true)?;
            if is_terminal(current) {
                return terminal_result(current);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
            continue;
        }
        if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        }
        if !status.is_success() {
            bail!("后台拒绝事件订阅（HTTP {}）", status.as_u16());
        }
        if response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::trim)
            != Some("text/event-stream")
        {
            bail!("后台未返回SSE事件流，请升级后台后再使用跟随功能");
        }
        let mut decoder = SseDecoder::default();
        let mut poll = tokio::time::interval(Duration::from_secs(2));
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // The initial tick is immediate; wait a full interval so buffered replay
        // is consumed before an authoritative terminal snapshot short-circuits it.
        poll.tick().await;
        let mut reset = false;
        'stream: loop {
            tokio::select! {
                chunk = response.chunk() => {
                    let bytes = match chunk {
                        Ok(Some(bytes)) => bytes,
                        Ok(None) | Err(_) => break,
                    };
                    for frame in decoder.push(&bytes)? {
                        if frame.event == "resync" {
                            reset = true;
                            break 'stream;
                        }
                        if frame.event != "run_event" {
                            continue; // Keepalives and future control events.
                        }
                        if !accept_event(output, frame, task_id, &mut cursor)? {
                            continue;
                        }
                        let snapshot = match client.request("GET", "/api/v1/snapshot", None).await {
                            Ok(snapshot) => snapshot,
                            Err(_) => break 'stream,
                        };
                        let current = task(&snapshot, task_id)?;
                        print_task_snapshot(output, current, snapshot_cursor(&snapshot)?, false)?;
                        if !is_terminal(current) {
                            bail!("后台终态事件与任务快照不一致");
                        }
                        return terminal_result(current);
                    }
                }
                _ = poll.tick() => {
                    // A snapshot is the authoritative fallback if a terminal
                    // event was pruned or the live stream became idle.
                    if let Ok(snapshot) = client.request("GET", "/api/v1/snapshot", None).await {
                        let current = task(&snapshot, task_id)?;
                        if is_terminal(current) {
                            print_task_snapshot(output, current, snapshot_cursor(&snapshot)?, true)?;
                            return terminal_result(current);
                        }
                    }
                }
            }
        }
        if reset {
            let snapshot = match client.request("GET", "/api/v1/snapshot", None).await {
                Ok(snapshot) => snapshot,
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    continue;
                }
            };
            cursor = snapshot_cursor(&snapshot)?;
            let current = task(&snapshot, task_id)?;
            print_task_snapshot(output, current, cursor, true)?;
            if is_terminal(current) {
                return terminal_result(current);
            }
        }
        // Network EOF and request timeout only retry the read-only subscription;
        // commands and tools are never replayed. Resume at the last printed ID.
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn run(options: Options) -> Result<()> {
    let command = command(&options)?;
    validate_follow(&options, command.as_ref())?;
    let client = Client::open(&options.directory)?;
    if options.command == "follow" {
        arity(&options, 1, 1)?;
        let snapshot = client.request("GET", "/api/v1/snapshot", None).await?;
        let mut output = std::io::stdout().lock();
        let cursor = snapshot_cursor(&snapshot)?;
        let current = task(&snapshot, &options.args[0])?;
        print_task_snapshot(&mut output, current, cursor, true)?;
        if is_terminal(current) {
            return terminal_result(current);
        }
        return follow_task(
            &options.directory,
            &options.args[0],
            cursor,
            options.wait.unwrap_or(3600),
            &mut output,
        )
        .await;
    }
    // Establish the cursor before submitting: even a very fast task cannot race
    // ahead of our subscription. The POST is never retried by follow mode.
    let cursor = if options.follow {
        Some(snapshot_cursor(
            &client.request("GET", "/api/v1/snapshot", None).await?,
        )?)
    } else {
        None
    };
    let response = if let Some(command) = command {
        client
            .request(
                "POST",
                "/api/v1/commands",
                Some(json!({"command_id":options.command_id,
                    "source":{"channel_id":"cli","actor_id":"local","conversation_id":options.conversation},
                    "command":command})),
            )
            .await?
    } else {
        match options.command.as_str() {
            "sessions" | "status" => {
                arity(&options, 0, usize::from(options.command == "status"))?;
                let snapshot = client.request("GET", "/api/v1/snapshot", None).await?;
                if options.command == "sessions" {
                    snapshot["sessions"].clone()
                } else if let Some(id) = options.args.first() {
                    task(&snapshot, id)?.clone()
                } else {
                    snapshot
                }
            }
            "settings" | "models" => {
                arity(&options, 0, 1)?;
                let body = options
                    .args
                    .first()
                    .map(|path| -> Result<Value> {
                        let value: Value = serde_json::from_str(&input(path, true)?)
                            .context("配置JSON格式无效")?;
                        if !value.is_object() {
                            bail!("配置必须为JSON对象");
                        }
                        Ok(value)
                    })
                    .transpose()?;
                let (method, path, body) = if options.command == "models" {
                    ("POST", "/api/v1/models", Some(body.unwrap_or(json!({}))))
                } else if body.is_some() {
                    ("PUT", "/api/v1/settings", body)
                } else {
                    ("GET", "/api/v1/settings", None)
                };
                client.request(method, path, body).await?
            }
            _ => unreachable!(),
        }
    };
    if let Some(cursor) = cursor {
        let id = response["task_id"]
            .as_str()
            .context("该指令没有返回任务ID，无法跟随")?;
        let mut output = std::io::stdout().lock();
        write_ndjson(&mut output, &json!({"type":"receipt", "receipt":response}))?;
        return follow_task(
            &options.directory,
            id,
            cursor,
            options.wait.unwrap_or(3600),
            &mut output,
        )
        .await;
    }
    if let Some(seconds) = options.wait {
        let id = response["task_id"]
            .as_str()
            .context("该指令没有返回任务ID，无法等待；send生成计划请使用status查询")?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
        loop {
            let snapshot = client.request("GET", "/api/v1/snapshot", None).await?;
            let current = task(&snapshot, id)?;
            if matches!(
                current["status"].as_str(),
                Some("completed" | "failed" | "cancelled")
            ) {
                println!("{}", serde_json::to_string_pretty(current)?);
                if current["status"] != "completed" {
                    bail!("任务未成功完成");
                }
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                println!("{}", serde_json::to_string_pretty(current)?);
                bail!("等待超时；后台任务继续运行，可使用status或cancel");
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}

#[tokio::main]
async fn main() {
    let result = match parse(std::env::args().skip(1)) {
        Ok(Some(options)) => run(options).await,
        Ok(None) => {
            println!("{HELP}");
            Ok(())
        }
        Err(error) => Err(error),
    };
    if let Err(error) = result {
        eprintln!("错误：{error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_nonlocal_metadata_and_credentials() {
        for url in [
            "https://127.0.0.1:99",
            "http://localhost:99",
            "http://example.com",
            "http://token@127.0.0.1",
            "http://127.0.0.1/?token=x",
            "http://127.0.0.1/path",
        ] {
            assert!(loopback_url(url).is_err());
        }
        assert!(loopback_url("http://127.0.0.1:47371").is_ok());
    }

    #[test]
    fn direct_and_remote_commands_are_distinct() {
        let parsed = |name: &str| {
            parse([name, "session", "目标"].into_iter().map(String::from))
                .unwrap()
                .unwrap()
        };
        assert_eq!(
            command(&parsed("run")).unwrap().unwrap()["type"],
            "run_agent"
        );
        assert_eq!(
            command(&parsed("send")).unwrap().unwrap()["type"],
            "send_message"
        );
        assert!(parse(["--wait", "0", "run"].into_iter().map(String::from)).is_err());
    }

    #[test]
    fn follow_is_opt_in_and_rejects_commands_without_task_ids() {
        let parsed = |args: &[&str]| {
            parse(args.iter().map(|value| (*value).to_owned()))
                .unwrap()
                .unwrap()
        };
        let plain = parsed(&["run", "session", "目标"]);
        assert!(!plain.follow);
        let following = parsed(&["--follow", "--wait", "10", "run", "session", "目标"]);
        assert!(following.follow);
        assert_eq!(following.wait, Some(10));
        assert!(validate_follow(&following, command(&following).unwrap().as_ref()).is_ok());
        let unsupported = parsed(&["--follow", "settings"]);
        assert!(validate_follow(&unsupported, None).is_err());
        let remote = parsed(&["--follow", "send", "session", "目标"]);
        assert!(validate_follow(&remote, command(&remote).unwrap().as_ref()).is_err());
        let attach = parsed(&["--wait", "10", "follow", "task"]);
        assert_eq!(attach.command, "follow");
        assert!(command(&attach).unwrap().is_none());
    }

    #[test]
    fn decoder_handles_utf8_crlf_and_multiline_data_at_every_byte_boundary() {
        let bytes = "\u{3a} keepalive\r\n\r\nevent: run_event\r\nid: 9\r\ndata: {\"sequence\":9,\r\ndata: \"text\":\"中文\"}\r\n\r\n".as_bytes();
        for split in 0..=bytes.len() {
            let mut decoder = SseDecoder::default();
            let mut frames = decoder.push(&bytes[..split]).unwrap();
            frames.extend(decoder.push(&bytes[split..]).unwrap());
            assert_eq!(frames.len(), 1);
            assert_eq!(frames[0].id.as_deref(), Some("9"));
            assert_eq!(frames[0].event, "run_event");
            let value: Value = serde_json::from_str(&frames[0].data).unwrap();
            assert_eq!(value["text"], "中文");
        }
        let mut decoder = SseDecoder::default();
        let mut frames = Vec::new();
        for byte in bytes {
            frames.extend(decoder.push(&[*byte]).unwrap());
        }
        assert_eq!(frames.len(), 1);
    }

    #[test]
    fn decoder_supports_cr_only_and_does_not_dispatch_incomplete_frames() {
        let mut decoder = SseDecoder::default();
        assert!(decoder
            .push(b"event: run_event\rid: 1\rdata: {}")
            .unwrap()
            .is_empty());
        assert_eq!(decoder.push(b"\r\r").unwrap().len(), 1);
        assert!(decoder.push(b"data: unfinished\n").unwrap().is_empty());
    }

    #[test]
    fn decoder_bounds_frames_and_rejects_invalid_utf8() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(&vec![b'x'; MAX_SSE_FRAME_BYTES + 1]).is_err());
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(b"data: \xff\n\n").is_err());
        let mut decoder = SseDecoder::default();
        for _ in 0..20 {
            assert!(decoder.push(b": keepalive\n\n").unwrap().is_empty());
        }
        assert_eq!(decoder.frame_bytes, 0);
    }

    fn frame(sequence: u64, task_id: &str, kind: &str) -> SseFrame {
        SseFrame {
            event: "run_event".into(),
            id: Some(sequence.to_string()),
            data: json!({"sequence":sequence,"task_id":task_id,"kind":kind}).to_string(),
        }
    }

    #[test]
    fn event_acceptance_deduplicates_and_advances_past_other_tasks() {
        let mut output = Vec::new();
        let mut cursor = 1;
        assert!(!accept_event(
            &mut output,
            frame(1, "a", "assistant_delta"),
            "a",
            &mut cursor
        )
        .unwrap());
        assert!(output.is_empty());
        assert!(!accept_event(
            &mut output,
            frame(2, "b", "assistant_delta"),
            "a",
            &mut cursor
        )
        .unwrap());
        assert_eq!(cursor, 2);
        assert!(output.is_empty());
        assert!(!accept_event(
            &mut output,
            frame(3, "a", "assistant_delta"),
            "a",
            &mut cursor
        )
        .unwrap());
        assert!(accept_event(
            &mut output,
            frame(4, "a", "task_completed"),
            "a",
            &mut cursor
        )
        .unwrap());
        assert_eq!(cursor, 4);
        assert_eq!(std::str::from_utf8(&output).unwrap().lines().count(), 2);
        let mut invalid = frame(5, "a", "assistant_delta");
        invalid.id = Some("6".into());
        assert!(accept_event(&mut output, invalid, "a", &mut cursor).is_err());
        assert_eq!(cursor, 4);
    }

    #[test]
    fn failed_output_does_not_advance_cursor() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut cursor = 1;
        assert!(accept_event(
            &mut Broken,
            frame(2, "a", "assistant_delta"),
            "a",
            &mut cursor
        )
        .is_err());
        assert_eq!(cursor, 1);
    }

    #[test]
    fn snapshots_are_single_line_replacements_and_terminal_errors_are_nonzero() {
        let mut output = Vec::new();
        let task = json!({"id":"t","status":"completed","stream_output":"第一行\n第二行"});
        print_task_snapshot(&mut output, &task, 42, true).unwrap();
        let lines: Vec<_> = std::str::from_utf8(&output).unwrap().lines().collect();
        assert_eq!(lines.len(), 1);
        let printed: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(printed["reset"], true);
        assert_eq!(printed["event_cursor"], 42);
        assert!(terminal_result(&task).is_ok());
        assert!(terminal_result(&json!({"status":"failed"})).is_err());
        assert!(terminal_result(&json!({"status":"cancelled"})).is_err());
        assert!(snapshot_cursor(&json!({})).is_err());
    }

    struct MockReply {
        path: &'static str,
        status: u16,
        content_type: &'static str,
        body: String,
        token: &'static str,
        rotate_token: Option<&'static str>,
    }

    fn event_body(sequence: u64, kind: &str) -> String {
        format!(
            "event: run_event\nid: {sequence}\ndata: {}\n\n",
            json!({"sequence":sequence,"task_id":"t","kind":kind,"data":{"text":"中文"}})
        )
    }

    fn snapshot_body(sequence: u64, status: &str) -> String {
        json!({"event_cursor":sequence,"tasks":[{"id":"t","status":status,"stream_output":"中文"}]})
            .to_string()
    }

    async fn mock_follow(replies: Vec<MockReply>) -> Vec<Value> {
        mock_follow_with_metadata_gap(replies, false).await
    }

    async fn mock_follow_with_metadata_gap(
        replies: Vec<MockReply>,
        metadata_gap: bool,
    ) -> Vec<Value> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let directory = std::env::temp_dir().join(format!("self-agent-cli-follow-{}", new_id()));
        std::fs::create_dir_all(&directory).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let metadata = directory.join("daemon.json");
        let write_metadata = |token: &str| {
            std::fs::write(
                &metadata,
                json!({"base_url":base,"token":token,"pid":1}).to_string(),
            )
            .unwrap();
        };
        write_metadata("token-a");
        let server = tokio::spawn(async move {
            for reply in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    socket.read_exact(&mut byte).await.unwrap();
                    request.push(byte[0]);
                    assert!(request.len() < 16 * 1024);
                }
                let request = String::from_utf8(request).unwrap();
                assert!(
                    request.starts_with(&format!("GET {} HTTP/1.1\r\n", reply.path)),
                    "{request}"
                );
                assert!(request
                    .to_ascii_lowercase()
                    .contains(&format!("authorization: bearer {}\r\n", reply.token)));
                if let Some(token) = reply.rotate_token {
                    if metadata_gap {
                        std::fs::remove_file(&metadata).unwrap();
                    } else {
                        std::fs::write(
                            &metadata,
                            json!({"base_url":base,"token":token,"pid":2}).to_string(),
                        )
                        .unwrap();
                    }
                }
                let headers = format!(
                    "HTTP/1.1 {} OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    reply.status, reply.content_type, reply.body.len()
                );
                socket.write_all(headers.as_bytes()).await.unwrap();
                socket.write_all(reply.body.as_bytes()).await.unwrap();
                socket.shutdown().await.unwrap();
                if let Some(token) = reply.rotate_token.filter(|_| metadata_gap) {
                    // Keep the file absent across at least two reconnect
                    // attempts, then emulate the completed daemon replacement.
                    tokio::time::sleep(Duration::from_millis(750)).await;
                    std::fs::write(
                        &metadata,
                        json!({"base_url":base,"token":token,"pid":2}).to_string(),
                    )
                    .unwrap();
                }
            }
        });
        let mut output = Vec::new();
        let result = follow_task(&directory, "t", 0, 5, &mut output).await;
        if result.is_err() {
            server.abort();
        }
        result.unwrap();
        server.await.unwrap();
        std::fs::remove_dir_all(directory).unwrap();
        std::str::from_utf8(&output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn follows_disconnect_replay_and_daemon_token_rotation_without_duplicate_text() {
        let output = mock_follow(vec![
            MockReply {
                path: "/api/v1/events?after=0",
                status: 200,
                content_type: "text/event-stream",
                body: event_body(1, "assistant_delta"),
                token: "token-a",
                rotate_token: Some("token-b"),
            },
            MockReply {
                path: "/api/v1/events?after=1",
                status: 200,
                content_type: "text/event-stream",
                body: format!(
                    "{}{}{}",
                    event_body(1, "assistant_delta"),
                    event_body(2, "assistant_delta"),
                    event_body(3, "task_completed")
                ),
                token: "token-b",
                rotate_token: None,
            },
            MockReply {
                path: "/api/v1/snapshot",
                status: 200,
                content_type: "application/json",
                body: snapshot_body(3, "completed"),
                token: "token-b",
                rotate_token: None,
            },
        ])
        .await;
        let sequences: Vec<_> = output
            .iter()
            .filter_map(|line| line["event"]["sequence"].as_u64())
            .collect();
        assert_eq!(sequences, [1, 2, 3]);
        assert_eq!(output.last().unwrap()["task"]["status"], "completed");
        assert_eq!(output.last().unwrap()["reset"], false);
    }

    #[tokio::test]
    async fn cursor_conflict_and_resync_replace_snapshot_before_resuming() {
        for conflict in [true, false] {
            let output = mock_follow(vec![
                MockReply {
                    path: "/api/v1/events?after=0",
                    status: if conflict { 409 } else { 200 },
                    content_type: if conflict {
                        "application/json"
                    } else {
                        "text/event-stream"
                    },
                    body: if conflict {
                        "{\"code\":\"snapshot_required\"}".into()
                    } else {
                        "event: resync\ndata: snapshot_required\n\n".into()
                    },
                    token: "token-a",
                    rotate_token: None,
                },
                MockReply {
                    path: "/api/v1/snapshot",
                    status: 200,
                    content_type: "application/json",
                    body: snapshot_body(8, "running"),
                    token: "token-a",
                    rotate_token: None,
                },
                MockReply {
                    path: "/api/v1/events?after=8",
                    status: 200,
                    content_type: "text/event-stream",
                    body: format!(
                        "{}{}",
                        event_body(9, "assistant_delta"),
                        event_body(10, "task_completed")
                    ),
                    token: "token-a",
                    rotate_token: None,
                },
                MockReply {
                    path: "/api/v1/snapshot",
                    status: 200,
                    content_type: "application/json",
                    body: snapshot_body(10, "completed"),
                    token: "token-a",
                    rotate_token: None,
                },
            ])
            .await;
            assert_eq!(output[0]["type"], "task_snapshot");
            assert_eq!(output[0]["reset"], true);
            assert_eq!(output[0]["event_cursor"], 8);
            assert_eq!(output[1]["event"]["sequence"], 9);
            assert_eq!(output.last().unwrap()["reset"], false);
        }
    }

    #[tokio::test]
    async fn a_failed_terminal_snapshot_is_retried_read_only() {
        let output = mock_follow(vec![
            MockReply {
                path: "/api/v1/events?after=0",
                status: 200,
                content_type: "text/event-stream",
                body: event_body(1, "task_completed"),
                token: "token-a",
                rotate_token: None,
            },
            MockReply {
                path: "/api/v1/snapshot",
                status: 500,
                content_type: "application/json",
                body: "{\"error\":\"暂时不可用\"}".into(),
                token: "token-a",
                rotate_token: None,
            },
            MockReply {
                path: "/api/v1/events?after=1",
                status: 409,
                content_type: "application/json",
                body: "{\"code\":\"snapshot_required\"}".into(),
                token: "token-a",
                rotate_token: None,
            },
            MockReply {
                path: "/api/v1/snapshot",
                status: 200,
                content_type: "application/json",
                body: snapshot_body(2, "completed"),
                token: "token-a",
                rotate_token: None,
            },
        ])
        .await;
        assert_eq!(output.len(), 2);
        assert_eq!(output[0]["event"]["sequence"], 1);
        assert_eq!(output[1]["task"]["status"], "completed");
        assert_eq!(output[1]["reset"], true);
    }

    #[tokio::test]
    async fn unavailable_daemon_stops_at_follow_deadline_without_reissuing_commands() {
        let directory = std::env::temp_dir().join(format!("self-agent-cli-timeout-{}", new_id()));
        std::fs::create_dir_all(&directory).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        std::fs::write(
            directory.join("daemon.json"),
            json!({"base_url":base,"token":"test","pid":1}).to_string(),
        )
        .unwrap();
        let started = std::time::Instant::now();
        let mut output = Vec::new();
        let result = follow_task(&directory, "t", 0, 1, &mut output).await;
        std::fs::remove_dir_all(directory).unwrap();
        assert!(result.unwrap_err().to_string().contains("跟随超时"));
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(output.is_empty());
    }

    #[tokio::test]
    async fn reconnect_survives_metadata_removal_then_rotated_replacement() {
        let output = mock_follow_with_metadata_gap(
            vec![
                MockReply {
                    path: "/api/v1/events?after=0",
                    status: 200,
                    content_type: "text/event-stream",
                    body: event_body(1, "assistant_delta"),
                    token: "token-a",
                    rotate_token: Some("token-b"),
                },
                MockReply {
                    path: "/api/v1/events?after=1",
                    status: 200,
                    content_type: "text/event-stream",
                    body: event_body(2, "task_completed"),
                    token: "token-b",
                    rotate_token: None,
                },
                MockReply {
                    path: "/api/v1/snapshot",
                    status: 200,
                    content_type: "application/json",
                    body: snapshot_body(2, "completed"),
                    token: "token-b",
                    rotate_token: None,
                },
            ],
            true,
        )
        .await;
        assert_eq!(output.len(), 3);
        assert_eq!(output[0]["event"]["sequence"], 1);
        assert_eq!(output[1]["event"]["sequence"], 2);
        assert_eq!(output[2]["task"]["status"], "completed");
    }

    #[tokio::test]
    async fn malformed_or_nonlocal_follow_metadata_is_not_retried() {
        let directory = std::env::temp_dir().join(format!("self-agent-cli-invalid-{}", new_id()));
        std::fs::create_dir_all(&directory).unwrap();
        let cases = [
            ("{bad".to_owned(), "后台元数据格式无效"),
            (
                json!({"base_url":"http://example.com","token":"test","pid":1}).to_string(),
                "后台元数据必须指向本机",
            ),
        ];
        for (body, expected) in cases {
            std::fs::write(directory.join("daemon.json"), body).unwrap();
            let error = follow_task(&directory, "t", 0, 1, &mut Vec::new())
                .await
                .unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
        }
        std::fs::remove_dir_all(directory).unwrap();
    }
}
