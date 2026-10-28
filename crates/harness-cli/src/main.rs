use std::{io::Read, path::PathBuf, time::Duration};

use anyhow::{bail, Context, Result};
use harness_core::{data_dir, new_id, DaemonInfo};
use serde_json::{json, Value};

const HELP: &str = r#"Self Agent 命令行客户端

用法：self-agent-cli [--data-dir 目录] [--conversation 来源会话] [--command-id 幂等ID] [--wait 秒] 命令 [参数]

  sessions                         列出会话
  create <标题> <项目绝对路径>       创建并选择会话
  select <会话ID>                   选择此命令来源的默认会话
  run <会话ID> <内容|->             主智能体直接执行；- 从标准输入读取
  send <会话ID> <内容|->            远程编排：生成计划或讨论
  status [任务ID]                   查询完整状态或单个任务
  cancel <任务ID>                   取消任务
  settings [JSON文件|-]             查看公开设置或提交设置补丁
  models [JSON文件|-]               从当前或草稿Endpoint获取模型
  command <JSON文件|->              发送完整指令对象，支持所有管理命令

先独立启动 harness-daemon。客户端读取同一数据目录的私有 daemon.json，
自动进行本机认证；不需要桌面窗口。所有成功输出为JSON，错误写入标准错误。
--wait 仅适用于返回任务ID的命令；终态失败或超时会返回非零退出码。
输入密钥时请使用受保护的JSON文件或标准输入，避免写入命令参数。
"#;

#[derive(Debug)]
struct Options {
    directory: PathBuf,
    conversation: String,
    command_id: String,
    wait: Option<u64>,
    command: String,
    args: Vec<String>,
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Option<Options>> {
    let mut options = Options {
        directory: data_dir(),
        conversation: "default".into(),
        command_id: new_id(),
        wait: None,
        command: String::new(),
        args: Vec::new(),
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
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
        "sessions" | "status" | "settings" | "models" => return Ok(None),
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

async fn run(options: Options) -> Result<()> {
    let client = Client::open(&options.directory)?;
    let response = if let Some(command) = command(&options)? {
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
}
