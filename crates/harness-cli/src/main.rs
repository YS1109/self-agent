use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use harness_core::{data_dir, new_id, DaemonInfo};
use serde_json::{json, Value};

const HELP: &str = r#"Self Agent 命令行客户端

用法：self-agent-cli [--data-dir 目录] [--conversation 来源会话] [--command-id 幂等ID] [--wait 秒] [--follow] [--controls JSON文件|-] 命令 [参数]

  sessions                         列出会话
  create <标题> <项目绝对路径>       创建并选择会话
  select <会话ID>                   选择此命令来源的默认会话
  run <会话ID> <内容|->             主智能体直接执行；- 从标准输入读取
  send <会话ID> <内容|->            远程编排：生成计划或讨论
  status [任务ID]                   查询完整状态或单个任务
  cancel <任务ID>                   取消任务
  steer <任务ID> <内容|->           补充指定任务（接收不代表已应用）
  continue <任务ID> [内容|-]        从终态主任务创建新的继续任务
  changes <任务ID>                  查看只读执行区间改动报告
  requests [任务ID]                 列出持久化问题与逐次授权请求
  providers [list]                 查看命名服务与系统凭据操作
  providers save <JSON文件|->       保存服务元数据（不接受密钥）
  providers enable <ID> <修订> <true|false>  显式启停命名服务
  providers delete <ID> <修订>      删除服务，遗留凭据需显式清理
  mcp-credential set <SERVER> <SLOT> <修订> --stdin  写入MCP专用凭据
  mcp-credential delete <SERVER> <SLOT> <修订>  解绑并删除
  mcp-credential operation <操作ID> | cleanup <操作ID>  查询/显式清理
  provider-credential set <ID> <修订> --stdin  仅从标准输入读取新密钥
  provider-credential delete <ID> <修订>     解绑并删除系统凭据
  provider-credential cleanup <操作ID>       清理指定遗留凭据
  capabilities                     查看托管 MCP / Skills 配置与发现状态
  resolve-cleanup <任务ID> <JSON文件|-> 本机所有者确认解除未知清理隔离
  capability-command <JSON文件|->   管理能力，幂等ID用--command-id指定
  answer <请求ID> <修订> <回答|->   回答问题（不授予执行权限）
  approve <请求ID> <修订> <摘要>    仅批准此请求绑定的具体操作
  deny <请求ID> <修订> <摘要>       拒绝此操作
  follow <任务ID>                   跟随已有任务的持久化事件与终态
  settings [JSON文件|-]             查看公开设置或提交设置补丁
  models [JSON文件|-]               从当前或草稿Endpoint获取模型
  command <JSON文件|->              发送完整指令对象，支持所有管理命令

先独立启动 harness-daemon。客户端读取同一数据目录的私有 daemon.json，
自动进行本机认证；不需要桌面窗口。所有成功输出为JSON，错误写入标准错误。
--wait 适用于返回任务ID的命令及follow；终态失败或超时会返回非零退出码。
--follow 用于run、steer、continue、answer、approve、deny或返回任务ID的command，逐行输出JSON收据、事件和任务快照。
follow先输出当前快照再跟随新事件；自动重连，恢复快照带reset:true。
跟随默认等待3600秒，可用--wait覆盖；超时不会取消后台任务。
跟随按任务终态退出；execution_cleanup为pending/unknown时仍需检查在途文件操作，
不能把终态当作副作用已停止。task_snapshot会附带cleanup_notice，之后可用status查询。
--controls 仅用于run，读取任务预算/权限JSON；只能收紧全局与角色限制。
Codex仅支持runtime/output限制，内部请求/修复及逐工具权限不可伪装成受控。
批准/拒绝必须先核对requests中的任务、操作、修订与完整action_digest；回答不是授权。
MCP 配置、启用、发现目录不等于实际调用或任务授权。probe 必须 confirm_start:true，
它会启动可信未沙箱化本机进程，可能写文件或联网；是否可用以后台真实发现状态为准。
能力命令只保存元数据；MCP 环境凭据通过 mcp-credential --stdin 专用入口写入系统凭据库。
该入口保留 UTF-8 换行，不复用 Provider header 限制。
capability-command 不重试POST，也不支持--follow/--wait；用capabilities刷新发现状态。
resolve-cleanup 须先在操作系统确认进程全部停止并检查改动，JSON包含快照中的
expected_updated_at 和 confirm_processes_stopped:true；只记录所有者确认，不改写原任务/调用结果。
命名服务凭据只接受--stdin，不接受密钥参数或文件；移除单个末尾换行，禁止内部换行。
凭据POST绝不自动重试；结果未知请用providers只读刷新，新尝试使用新的--command-id。
no_secret_applied:true表示新输入没有写入，不能当作凭据保存成功。
旧版临时模型密钥可使用受保护的JSON文件或标准输入，避免写入命令参数。
"#;

#[derive(Debug)]
struct Options {
    directory: PathBuf,
    conversation: String,
    command_id: String,
    wait: Option<u64>,
    follow: bool,
    controls: Option<String>,
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
        controls: None,
        command: String::new(),
        args: Vec::new(),
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--follow" => options.follow = true,
            "--data-dir" | "--conversation" | "--command-id" | "--wait" | "--controls" => {
                let value = args.next().context("选项缺少参数")?;
                match arg.as_str() {
                    "--data-dir" => options.directory = PathBuf::from(value),
                    "--conversation" => options.conversation = value,
                    "--command-id" => options.command_id = value,
                    "--controls" => {
                        if options.controls.replace(value).is_some() {
                            bail!("--controls 不能重复指定");
                        }
                    }
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
    if options.controls.is_some() && options.command != "run" {
        bail!("--controls 仅用于run；完整命令请在command JSON内指定controls");
    }
    if options.controls.as_deref() == Some("-") && args.get(1).map(String::as_str) == Some("-") {
        bail!("任务内容与controls不能同时读取标准输入");
    }
    let mut value = match options.command.as_str() {
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
        "steer" => {
            arity(options, 2, 2)?;
            json!({"type":"steer_task","task_id":args[0],"content":input(&args[1],false)?})
        }
        "continue" => {
            arity(options, 1, 2)?;
            let mut value = json!({"type":"continue_task","task_id":args[0]});
            if let Some(content) = args.get(1) {
                value["content"] = json!(input(content, false)?);
            }
            value
        }
        "answer" => {
            arity(options, 3, 3)?;
            let revision: u64 = args[1].parse().context("修订必须是非负整数")?;
            json!({"type":"answer_request","request_id":args[0],"expected_revision":revision,"answer":input(&args[2],false)?})
        }
        "approve" | "deny" => {
            arity(options, 3, 3)?;
            let revision: u64 = args[1].parse().context("修订必须是非负整数")?;
            let digest = &args[2];
            if digest.len() != 64
                || !digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                bail!("请提供requests中的完整小写SHA256操作摘要，不可省略或使用通配符");
            }
            json!({"type":"decide_request","request_id":args[0],"expected_revision":revision,"approve":options.command=="approve","action_digest":digest})
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
        "sessions"
        | "status"
        | "settings"
        | "models"
        | "follow"
        | "changes"
        | "requests"
        | "capabilities"
        | "capability-command"
        | "resolve-cleanup"
        | "providers"
        | "provider-credential"
        | "mcp-credential" => return Ok(None),
        _ => bail!("未知命令，使用 --help 查看用法"),
    };
    if let Some(path) = &options.controls {
        let controls: Value =
            serde_json::from_str(&input(path, true)?).context("执行控制JSON格式无效")?;
        if !controls.is_object() {
            bail!("执行控制必须为JSON对象");
        }
        value["controls"] = controls;
    }
    Ok(Some(value))
}

fn capability_body(options: &Options) -> Result<Option<Value>> {
    if !matches!(
        options.command.as_str(),
        "capabilities" | "capability-command"
    ) {
        return Ok(None);
    }
    if options.wait.is_some() || options.follow {
        bail!("能力管理不支持--wait或--follow；使用capabilities读取最新状态");
    }
    if options.command == "capabilities" {
        arity(options, 0, 0)?;
        return Ok(None);
    }
    arity(options, 1, 1)?;
    let content = input(&options.args[0], true)?;
    if content.len() > 256 * 1024 {
        bail!("能力命令超过256 KiB限制");
    }
    let value: Value = serde_json::from_str(&content).context("能力命令JSON格式无效")?;
    if !value.is_object()
        || !matches!(
            value["type"].as_str(),
            Some(
                "save_mcp_server"
                    | "set_capability_enabled"
                    | "probe_mcp_server"
                    | "save_skill"
                    | "import_skill"
                    | "delete_capability"
            )
        )
    {
        bail!("不支持的能力管理命令，请使用已定义的type对象");
    }
    if value["type"] == "probe_mcp_server" && value["confirm_start"] != true {
        bail!("发现工具会启动未沙箱化本机进程，必须显式confirm_start:true");
    }
    if value["type"] == "save_mcp_server"
        && value["definition"]
            .get("credential_refs")
            .is_some_and(|refs| !refs.as_object().is_some_and(|refs| refs.is_empty()))
    {
        bail!("本切片不支持凭据绑定；不要在能力配置或命令参数输入原始密钥");
    }
    let envelope = json!({"id":options.command_id,"command":value});
    if serde_json::to_vec(&envelope)?.len() > 256 * 1024 {
        bail!("能力管理信封超过256 KiB限制");
    }
    Ok(Some(envelope))
}

fn cleanup_resolution_request(options: &Options) -> Result<Option<(String, Value)>> {
    if options.command != "resolve-cleanup" {
        return Ok(None);
    }
    arity(options, 2, 2)?;
    if options.wait.is_some() || options.follow {
        bail!("解除隔离不支持--wait或--follow，也不会重启任务");
    }
    let id = &options.args[0];
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    {
        bail!("任务ID无效");
    }
    let body: Value =
        serde_json::from_str(&input(&options.args[1], true)?).context("所有者确认JSON格式无效")?;
    if body.as_object().is_none_or(|map| map.len() != 2)
        || body["confirm_processes_stopped"] != true
        || !body["expected_updated_at"]
            .as_str()
            .is_some_and(|value| !value.is_empty() && value.len() <= 128)
    {
        bail!("先确认相关进程全部停止并检查项目改动；仅提交expected_updated_at和confirm_processes_stopped:true");
    }
    Ok(Some((
        format!("/api/v1/tasks/{id}/cleanup-resolution"),
        body,
    )))
}

fn provider_uuid(value: &str) -> Result<()> {
    if value.len() != 36
        || !value.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
            }
        })
    {
        bail!("服务或操作标识必须为规范UUID");
    }
    Ok(())
}

fn provider_stdin_secret(reader: impl Read) -> Result<String> {
    let mut bytes = Vec::new();
    reader
        .take(2051)
        .read_to_end(&mut bytes)
        .context("无法读取凭据标准输入")?;
    if bytes.ends_with(b"\n") {
        bytes.pop();
        if bytes.ends_with(b"\r") {
            bytes.pop();
        }
    }
    if bytes.is_empty() || bytes.len() > 2048 || !bytes.iter().all(u8::is_ascii_graphic) {
        bail!("凭据必须为1至2048字节的可见ASCII字符，不能包含空格或内部换行；内容未回显");
    }
    String::from_utf8(bytes).context("凭据格式无效，内容未回显")
}

fn mcp_stdin_secret(reader: impl Read) -> Result<String> {
    let mut bytes = Vec::new();
    reader
        .take(2049)
        .read_to_end(&mut bytes)
        .context("无法读取 MCP 凭据标准输入")?;
    if bytes.is_empty() || bytes.len() > 2048 || bytes.contains(&0) {
        bail!("MCP 凭据须为1至2048字节UTF-8，不能含空字符；内容未回显");
    }
    String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("MCP 凭据须为UTF-8；内容未回显"))
}
fn mcp_credential_request(options: &Options) -> Result<Option<(String, String, Option<Value>)>> {
    if options.command != "mcp-credential" {
        return Ok(None);
    }
    if options.follow || options.wait.is_some() {
        bail!("MCP 凭据只支持一次提交或只读查询，不支持follow/wait");
    }
    provider_uuid(&options.command_id)?;
    let a = &options.args;
    match a.first().map(String::as_str){
 Some("operation") if a.len()==2=>{provider_uuid(&a[1])?;Ok(Some(("GET".into(),format!("/api/v1/capability-credential-operations/{}",a[1]),None)))},
 Some("cleanup") if a.len()==2=>{provider_uuid(&a[1])?;Ok(Some(("POST".into(),format!("/api/v1/capability-credential-operations/{}/cleanup",a[1]),Some(json!({"id":options.command_id})))))},
 Some(kind @ ("set"|"delete")) if (kind=="set"&&a.len()==5&&a[4]=="--stdin")||(kind=="delete"&&a.len()==4)=>{
 provider_uuid(&a[1])?;let slot=&a[2];harness_core::validate_mcp_credential_slot(slot).map_err(anyhow::Error::msg)?;
 let rev=a[3].parse::<u64>().ok().filter(|v|*v<=9_007_199_254_740_991).context("MCP 修订无效")?;
 let mut body=json!({"id":options.command_id,"expected_revision":rev});if kind=="set"{body["secret"]=json!(mcp_stdin_secret(std::io::stdin())?);}
 Ok(Some(("POST".into(),format!("/api/v1/capabilities/mcp/{}/credentials/{}{}",a[1],slot,if kind=="delete"{"/delete"}else{""}),Some(body))))},
 _=>bail!("使用 mcp-credential set SERVER SLOT REV --stdin、delete SERVER SLOT REV、operation ID 或cleanup ID；不要在参数中填写凭据"),
 }
}

fn provider_request(options: &Options) -> Result<Option<(String, String, Option<Value>)>> {
    if !matches!(
        options.command.as_str(),
        "providers" | "provider-credential"
    ) {
        return Ok(None);
    }
    if options.follow || options.wait.is_some() {
        bail!("服务管理不支持--follow/--wait；请用providers刷新持久记录");
    }
    let args = &options.args;
    if options.command == "providers" && (args.is_empty() || args.as_slice() == ["list"]) {
        return Ok(Some(("GET".into(), "/api/v1/providers".into(), None)));
    }
    provider_uuid(&options.command_id)?;
    if options.command == "providers" {
        let command = match args.first().map(String::as_str) {
            Some("save") => {
                arity(options, 2, 2)?;
                let value: Value = serde_json::from_str(&input(&args[1], true)?)
                    .context("服务元数据JSON格式无效")?;
                if value["type"] != "save_provider"
                    || value.get("secret").is_some()
                    || value.get("api_key").is_some()
                    || !value["definition"].as_object().is_some_and(|map| {
                        map.keys().all(|key| {
                            matches!(
                                key.as_str(),
                                "name"
                                    | "base_url"
                                    | "models_endpoint"
                                    | "model"
                                    | "allow_insecure_http"
                            )
                        })
                    })
                {
                    bail!("仅接受save_provider元数据对象；凭据必须通过provider-credential set --stdin提交");
                }
                value
            }
            Some("enable" | "delete") => {
                let enable = args[0] == "enable";
                arity(
                    options,
                    if enable { 4 } else { 3 },
                    if enable { 4 } else { 3 },
                )?;
                provider_uuid(&args[1])?;
                let revision: u64 = args[2].parse().context("修订必须是正整数")?;
                if revision == 0 || revision > 9_007_199_254_740_991 {
                    bail!("服务修订无效");
                }
                if enable {
                    let enabled = args[3].parse::<bool>().context("启用值只能为true或false")?;
                    json!({"type":"set_provider_enabled","id":args[1],"expected_revision":revision,"enabled":enabled})
                } else {
                    json!({"type":"delete_provider","id":args[1],"expected_revision":revision})
                }
            }
            _ => bail!("使用providers list/save/enable/delete；不要在命令参数中填写密钥"),
        };
        let envelope = json!({"id":options.command_id,"command":command});
        if serde_json::to_vec(&envelope)?.len() > 256 * 1024 {
            bail!("服务元数据请求超过256 KiB");
        }
        return Ok(Some((
            "POST".into(),
            "/api/v1/providers/commands".into(),
            Some(envelope),
        )));
    }
    match args.first().map(String::as_str) {
        Some("set" | "delete") => {
            let set = args[0] == "set";
            arity(options, if set { 4 } else { 3 }, if set { 4 } else { 3 })?;
            if set && args[3] != "--stdin" {
                bail!("凭据只接受--stdin；不接受密钥参数或文件，未回显输入");
            }
            provider_uuid(&args[1])?;
            let revision: u64 = args[2].parse().context("修订必须是正整数")?;
            if revision == 0 || revision > 9_007_199_254_740_991 {
                bail!("服务修订无效");
            }
            let mut body = json!({"id":options.command_id,"expected_revision":revision});
            if set {
                body["secret"] = json!(provider_stdin_secret(std::io::stdin())?);
            }
            Ok(Some((
                "POST".into(),
                format!(
                    "/api/v1/providers/{}/{}",
                    args[1],
                    if set {
                        "credential"
                    } else {
                        "credential-delete"
                    }
                ),
                Some(body),
            )))
        }
        Some("cleanup") => {
            arity(options, 2, 2)?;
            provider_uuid(&args[1])?;
            Ok(Some((
                "POST".into(),
                format!("/api/v1/credential-operations/{}/cleanup", args[1]),
                Some(json!({"id":options.command_id})),
            )))
        }
        _ => bail!("使用provider-credential set/delete/cleanup；凭据只能从标准输入读取"),
    }
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
    if !matches!(
        options.command.as_str(),
        "run" | "steer" | "continue" | "answer" | "approve" | "deny" | "command"
    ) || !matches!(
        command.and_then(|value| value["type"].as_str()),
        Some(
            "run_agent"
                | "steer_task"
                | "continue_task"
                | "start_task"
                | "start_plan"
                | "resume_plan"
                | "cancel_task"
                | "answer_request"
                | "decide_request"
        )
    ) {
        bail!(
            "--follow 仅适用于返回任务ID的run、steer、continue、answer、approve、deny或command指令；已有任务请使用follow"
        )
    }
    Ok(())
}

fn changes_route(options: &Options) -> Result<String> {
    arity(options, 1, 1)?;
    if options.wait.is_some() || options.follow {
        bail!("changes 为只读查询，不支持 --wait 或 --follow");
    }
    let id = &options.args[0];
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        bail!("任务ID格式无效");
    }
    Ok(format!("/api/v1/tasks/{id}/changes"))
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

fn cleanup_notice(task: &Value) -> Option<&'static str> {
    match task["execution_cleanup"].as_str() {
        Some("pending") => Some("在途文件操作仍在清理；即使任务已进入终态，也不能断言副作用已停止，请使用status查询并检查实际改动。"),
        Some("unknown") => Some("执行清理结果未知，同项目后续执行已隔离；请在操作系统核对相关进程及实际改动，使用resolve-cleanup显式记录所有者确认，不会自动重放。"),
        _ => None,
    }
}

fn terminal_result(task: &Value) -> Result<()> {
    if let Some(notice) = cleanup_notice(task) {
        eprintln!("注意：{notice}");
    }
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
    let mut envelope =
        json!({"type":"task_snapshot", "task":task, "event_cursor":cursor, "reset":reset});
    if let Some(notice) = cleanup_notice(task) {
        envelope["cleanup_notice"] = json!(notice);
    }
    write_ndjson(output, &envelope)
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
    let capability_request = capability_body(&options)?;
    let cleanup_request = cleanup_resolution_request(&options)?;
    let provider_request = provider_request(&options)?;
    let mcp_credential_request = mcp_credential_request(&options)?;
    validate_follow(&options, command.as_ref())?;
    if options.command == "requests" {
        arity(&options, 0, 1)?;
        if options.wait.is_some() || options.follow {
            bail!("requests为只读查询，不支持--wait或--follow");
        }
    }
    let changes_path = (options.command == "changes")
        .then(|| changes_route(&options))
        .transpose()?;
    let client = Client::open(&options.directory)?;
    if let Some((method, path, body)) = mcp_credential_request {
        if method != "GET" {
            eprintln!(
                "MCP 凭据操作 ID：{}；结果未知时只用 operation 查询，不会重发。",
                options.command_id
            );
        }
        let response = client.request(&method, &path, body).await.map_err(|_| {
            anyhow::anyhow!("MCP 凭据请求未确认，请只读查询原操作；未回显凭据或底层错误")
        })?;
        println!("{}", serde_json::to_string_pretty(&response)?);
        return Ok(());
    }
    if let Some((method, path, body)) = provider_request {
        let response = client.request(&method, &path, body).await?;
        println!("{}", serde_json::to_string_pretty(&response)?);
        return Ok(());
    }
    if let Some((path, body)) = cleanup_request {
        let response = client.request("POST", &path, Some(body)).await?;
        println!("{}", serde_json::to_string_pretty(&response)?);
        return Ok(());
    }
    if options.command == "capabilities" || capability_request.is_some() {
        let response = if let Some(body) = capability_request {
            client
                .request("POST", "/api/v1/capabilities/commands", Some(body))
                .await?
        } else {
            client.request("GET", "/api/v1/capabilities", None).await?
        };
        println!("{}", serde_json::to_string_pretty(&response)?);
        return Ok(());
    }
    if let Some(path) = changes_path {
        let response = client.request("GET", &path, None).await?;
        println!("{}", serde_json::to_string_pretty(&response)?);
        return Ok(());
    }
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
            "requests" => {
                let snapshot = client.request("GET", "/api/v1/snapshot", None).await?;
                let requests = snapshot["execution_requests"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                json!(requests
                    .into_iter()
                    .filter(|request| options
                        .args
                        .first()
                        .is_none_or(|id| request["task_id"] == *id))
                    .collect::<Vec<_>>())
            }
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
    fn supplements_and_continuations_keep_explicit_task_and_optional_content() {
        let parsed = |args: &[&str]| {
            parse(args.iter().map(|value| (*value).to_owned()))
                .unwrap()
                .unwrap()
        };
        let steer = parsed(&["--follow", "steer", "old-task", "补充"]);
        assert_eq!(
            command(&steer).unwrap().unwrap(),
            json!({"type":"steer_task","task_id":"old-task","content":"补充"})
        );
        assert!(validate_follow(&steer, command(&steer).unwrap().as_ref()).is_ok());
        let next = parsed(&["--follow", "continue", "terminal"]);
        assert_eq!(
            command(&next).unwrap().unwrap(),
            json!({"type":"continue_task","task_id":"terminal"})
        );
        assert!(validate_follow(&next, command(&next).unwrap().as_ref()).is_ok());
        assert_eq!(
            command(&parsed(&["continue", "terminal", "修复"]))
                .unwrap()
                .unwrap()["content"],
            "修复"
        );
        assert!(command(&parsed(&["steer", "task"])).is_err());
        assert!(command(&parsed(&["continue", "task", "one", "two"])).is_err());
    }

    #[test]
    fn changes_is_read_only_and_rejects_path_injection_and_follow_options() {
        let parsed = |args: &[&str]| {
            parse(args.iter().map(|value| (*value).to_owned()))
                .unwrap()
                .unwrap()
        };
        let options = parsed(&["changes", "task-1"]);
        assert!(command(&options).unwrap().is_none());
        assert_eq!(
            changes_route(&options).unwrap(),
            "/api/v1/tasks/task-1/changes"
        );
        for id in ["", "../snapshot", "a?target=x", "a%2fb", "a/b", "#x"] {
            assert!(changes_route(&parsed(&["changes", id])).is_err());
        }
        assert!(changes_route(&parsed(&["changes"])).is_err());
        assert!(changes_route(&parsed(&["--wait", "1", "changes", "t"])).is_err());
        assert!(changes_route(&parsed(&["--follow", "changes", "t"])).is_err());
    }

    #[test]
    fn decisions_bind_revision_and_exact_digest_and_do_not_confuse_answers() {
        let parsed = |args: &[&str]| {
            parse(args.iter().map(|value| (*value).to_owned()))
                .unwrap()
                .unwrap()
        };
        let digest = "a".repeat(64);
        let answer = parsed(&["--follow", "answer", "request", "2", "保留原名"]);
        assert_eq!(
            command(&answer).unwrap().unwrap(),
            json!({"type":"answer_request","request_id":"request","expected_revision":2,"answer":"保留原名"})
        );
        assert!(validate_follow(&answer, command(&answer).unwrap().as_ref()).is_ok());
        for name in ["approve", "deny"] {
            let options = parsed(&["--follow", name, "request", "0", &digest]);
            assert_eq!(
                command(&options).unwrap().unwrap(),
                json!({"type":"decide_request","request_id":"request","expected_revision":0,"approve":name=="approve","action_digest":digest})
            );
            assert!(validate_follow(&options, command(&options).unwrap().as_ref()).is_ok());
        }
        for args in [
            vec!["approve", "r", "0"],
            vec!["approve", "r", "-1", &digest],
            vec!["deny", "r", "0", "*"],
            vec!["answer", "r", "not-a-revision", "yes"],
        ] {
            assert!(command(&parsed(&args)).is_err());
        }
    }

    #[test]
    fn controls_are_run_only_json_and_cannot_consume_stdin_twice() {
        let parsed = |args: &[&str]| {
            parse(args.iter().map(|value| (*value).to_owned()))
                .unwrap()
                .unwrap()
        };
        let file = std::env::temp_dir().join(format!("cli-controls-{}.json", new_id()));
        std::fs::write(
            &file,
            r#"{"budget":{"max_runtime_seconds":12},"permissions":{"run_commands":"ask"}}"#,
        )
        .unwrap();
        let options = parsed(&["--controls", file.to_str().unwrap(), "run", "s", "目标"]);
        assert_eq!(
            command(&options).unwrap().unwrap()["controls"],
            json!({"budget":{"max_runtime_seconds":12},"permissions":{"run_commands":"ask"}})
        );
        assert!(command(&parsed(&[
            "--controls",
            file.to_str().unwrap(),
            "send",
            "s",
            "目标"
        ]))
        .is_err());
        assert!(command(&parsed(&["--controls", "-", "run", "s", "-"])).is_err());
        std::fs::write(&file, "[]").unwrap();
        assert!(command(&options).is_err());
        std::fs::remove_file(file).unwrap();
        assert!(parse(
            ["--controls", "a", "--controls", "b", "run"]
                .into_iter()
                .map(String::from)
        )
        .is_err());
    }

    #[test]
    fn terminal_follow_exposes_unsettled_cleanup_without_changing_exit_semantics() {
        for state in ["pending", "unknown"] {
            for status in ["completed", "cancelled", "failed"] {
                let task = json!({"id":"t","status":status,"execution_cleanup":state});
                let mut output = Vec::new();
                print_task_snapshot(&mut output, &task, 3, false).unwrap();
                let envelope: Value = serde_json::from_slice(&output).unwrap();
                assert_eq!(envelope["task"]["execution_cleanup"], state);
                assert!(envelope["cleanup_notice"].as_str().is_some());
                assert_eq!(terminal_result(&task).is_ok(), status == "completed");
            }
        }
        for task in [
            json!({"status":"completed"}),
            json!({"status":"completed","execution_cleanup":"settled"}),
        ] {
            let mut output = Vec::new();
            print_task_snapshot(&mut output, &task, 4, true).unwrap();
            assert!(serde_json::from_slice::<Value>(&output)
                .unwrap()
                .get("cleanup_notice")
                .is_none());
        }
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

    async fn cli_fixture(
        args: &[&str],
        replies: Vec<(&str, &str, Option<Value>, Option<String>)>,
    ) -> Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let directory = std::env::temp_dir().join(format!("self-agent-cli-command-{}", new_id()));
        std::fs::create_dir_all(&directory).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        std::fs::write(directory.join("daemon.json"), json!({
            "base_url":format!("http://{}",listener.local_addr().unwrap()), "token":"fixture-private", "pid":1
        }).to_string()).unwrap();
        let replies: Vec<_> = replies
            .into_iter()
            .map(|(method, path, command, body)| {
                (method.to_owned(), path.to_owned(), command, body)
            })
            .collect();
        let server = tokio::spawn(async move {
            for (method, path, command, body) in replies {
                let (mut socket, _) =
                    tokio::time::timeout(Duration::from_secs(4), listener.accept())
                        .await
                        .unwrap()
                        .unwrap();
                let mut bytes = Vec::new();
                while !bytes.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    socket.read_exact(&mut byte).await.unwrap();
                    bytes.push(byte[0]);
                    assert!(bytes.len() < 16384);
                }
                let headers = String::from_utf8(bytes).unwrap();
                assert!(
                    headers.starts_with(&format!("{method} {path} HTTP/1.1\r\n")),
                    "{headers}"
                );
                let lower = headers.to_ascii_lowercase();
                assert!(lower.contains("authorization: bearer fixture-private\r\n"));
                let length = lower
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .map(|value| value.parse::<usize>().unwrap())
                    .unwrap_or(0);
                let mut bytes = vec![0; length];
                socket.read_exact(&mut bytes).await.unwrap();
                if let Some(command) = command {
                    let envelope: Value = serde_json::from_slice(&bytes).unwrap();
                    if path.ends_with("/cleanup-resolution")
                        || path.ends_with("/credential")
                        || path.ends_with("/credential-delete")
                        || path.starts_with("/api/v1/credential-operations/")
                    {
                        assert_eq!(envelope, command);
                    } else {
                        assert_eq!(envelope["command"], command);
                        assert!(envelope[if path == "/api/v1/capabilities/commands"
                            || path == "/api/v1/providers/commands"
                        {
                            "id"
                        } else {
                            "command_id"
                        }]
                        .as_str()
                        .is_some());
                    }
                } else {
                    assert!(bytes.is_empty());
                }
                if let Some(body) = body {
                    let content_type = if path.starts_with("/api/v1/events?") {
                        "text/event-stream"
                    } else {
                        "application/json"
                    };
                    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
                }
                // None deliberately drops the connection after reading a POST.
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(250), listener.accept())
                    .await
                    .is_err(),
                "unexpected retry or extra request"
            );
        });
        let mut options = parse(args.iter().map(|arg| (*arg).to_owned()))
            .unwrap()
            .unwrap();
        options.directory = directory.clone();
        let result = run(options).await;
        server.await.unwrap();
        std::fs::remove_dir_all(directory).unwrap();
        result
    }

    #[tokio::test]
    async fn steer_and_continue_follow_post_once_then_only_read_events() {
        for (args, command) in [
            (
                vec!["--follow", "--wait", "3", "steer", "t", "补充"],
                json!({"type":"steer_task","task_id":"t","content":"补充"}),
            ),
            (
                vec!["--follow", "--wait", "3", "continue", "old", "继续"],
                json!({"type":"continue_task","task_id":"old","content":"继续"}),
            ),
        ] {
            cli_fixture(&args,vec![
                ("GET","/api/v1/snapshot",None,Some(snapshot_body(0,"running"))),
                ("POST","/api/v1/commands",Some(command),Some(json!({"status":"accepted","task_id":"t"}).to_string())),
                ("GET","/api/v1/events?after=0",None,Some("id: 1\nevent: run_event\ndata: {\"sequence\":1,\"task_id\":\"t\",\"kind\":\"task_completed\"}\n\n".into())),
                ("GET","/api/v1/snapshot",None,Some(snapshot_body(1,"completed"))),
            ]).await.unwrap();
        }
    }

    #[tokio::test]
    async fn changes_uses_only_get_and_uncertain_continuation_post_is_not_retried() {
        cli_fixture(
            &["changes", "t"],
            vec![(
                "GET",
                "/api/v1/tasks/t/changes",
                None,
                Some(json!({"state":"expired","reason":"已过期"}).to_string()),
            )],
        )
        .await
        .unwrap();
        let error = cli_fixture(
            &["continue", "old"],
            vec![(
                "POST",
                "/api/v1/commands",
                Some(json!({"type":"continue_task","task_id":"old"})),
                None,
            )],
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("无法连接本机后台服务"));
    }

    #[tokio::test]
    async fn decision_follow_submits_once_then_reads_and_request_list_only_gets() {
        let digest = "a".repeat(64);
        for (args, expected) in [
            (
                vec!["--follow", "--wait", "3", "answer", "r", "0", "答案"],
                json!({"type":"answer_request","request_id":"r","expected_revision":0,"answer":"答案"}),
            ),
            (
                vec!["--follow", "--wait", "3", "approve", "r", "0", &digest],
                json!({"type":"decide_request","request_id":"r","expected_revision":0,"approve":true,"action_digest":digest}),
            ),
        ] {
            cli_fixture(&args,vec![
                ("GET","/api/v1/snapshot",None,Some(snapshot_body(0,"waiting_input"))),
                ("POST","/api/v1/commands",Some(expected),Some(json!({"status":"accepted","task_id":"t"}).to_string())),
                ("GET","/api/v1/events?after=0",None,Some("id: 1\nevent: run_event\ndata: {\"sequence\":1,\"task_id\":\"t\",\"kind\":\"task_completed\"}\n\n".into())),
                ("GET","/api/v1/snapshot",None,Some(snapshot_body(1,"completed"))),
            ]).await.unwrap();
        }
        cli_fixture(&["requests","t"],vec![("GET","/api/v1/snapshot",None,Some(json!({"execution_requests":[{"id":"r","task_id":"t","kind":"approval","action_digest":digest}]}).to_string()))]).await.unwrap();
        assert!(cli_fixture(&["deny","r","0",&digest],vec![("POST","/api/v1/commands",Some(json!({"type":"decide_request","request_id":"r","expected_revision":0,"approve":false,"action_digest":digest})),None)]).await.is_err());
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
    #[test]
    fn capability_and_cleanup_input_are_explicit_and_bounded() {
        let parsed = |args: &[&str]| {
            parse(args.iter().map(|arg| (*arg).to_owned()))
                .unwrap()
                .unwrap()
        };
        let path = std::env::temp_dir().join(format!("harness-cap-cli-{}.json", new_id()));
        let file = path.to_str().unwrap();
        let command =
            json!({"type":"probe_mcp_server","id":"m","expected_revision":2,"confirm_start":true});
        std::fs::write(&path, command.to_string()).unwrap();
        let options = parsed(&["--command-id", "stable", "capability-command", file]);
        assert_eq!(
            capability_body(&options).unwrap().unwrap(),
            json!({"id":"stable","command":command})
        );
        assert!(capability_body(&parsed(&["--wait", "1", "capability-command", file])).is_err());
        assert!(capability_body(&parsed(&["--follow", "capabilities"])).is_err());
        std::fs::write(
            &path,
            json!({"type":"probe_mcp_server","confirm_start":false}).to_string(),
        )
        .unwrap();
        assert!(capability_body(&options).is_err());
        std::fs::write(&path,json!({"type":"save_mcp_server","definition":{"credential_refs":{"TOKEN":"raw-secret"}}}).to_string()).unwrap();
        assert!(capability_body(&options).is_err());
        std::fs::write(
            &path,
            json!({"expected_updated_at":"stamp","confirm_processes_stopped":true}).to_string(),
        )
        .unwrap();
        assert_eq!(
            cleanup_resolution_request(&parsed(&["resolve-cleanup", "task-1", file]))
                .unwrap()
                .unwrap()
                .0,
            "/api/v1/tasks/task-1/cleanup-resolution"
        );
        for id in ["..", "t/x", "t?token=a", ""] {
            assert!(cleanup_resolution_request(&parsed(&["resolve-cleanup", id, file])).is_err());
        }
        assert!(
            cleanup_resolution_request(&parsed(&["--follow", "resolve-cleanup", "t", file]))
                .is_err()
        );
        std::fs::write(
            &path,
            json!({"expected_updated_at":"stamp","confirm_processes_stopped":false}).to_string(),
        )
        .unwrap();
        assert!(cleanup_resolution_request(&parsed(&["resolve-cleanup", "t", file])).is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn capability_management_and_owner_cleanup_use_dedicated_routes_without_post_retry() {
        cli_fixture(
            &["capabilities"],
            vec![(
                "GET",
                "/api/v1/capabilities",
                None,
                Some(json!({"mcp_servers":[],"skills":[]}).to_string()),
            )],
        )
        .await
        .unwrap();
        let path = std::env::temp_dir().join(format!("harness-cap-cli-http-{}.json", new_id()));
        let file = path.to_str().unwrap();
        let command = json!({"type":"save_skill","name":"fixture","content":"local fixture"});
        std::fs::write(&path, command.to_string()).unwrap();
        cli_fixture(
            &["--command-id", "stable", "capability-command", file],
            vec![(
                "POST",
                "/api/v1/capabilities/commands",
                Some(command.clone()),
                Some(json!({"command_id":"stable","status":"completed"}).to_string()),
            )],
        )
        .await
        .unwrap();
        assert!(cli_fixture(
            &["capability-command", file],
            vec![("POST", "/api/v1/capabilities/commands", Some(command), None)]
        )
        .await
        .is_err());
        let body = json!({"expected_updated_at":"stamp","confirm_processes_stopped":true});
        std::fs::write(&path, body.to_string()).unwrap();
        cli_fixture(
            &["resolve-cleanup", "t", file],
            vec![(
                "POST",
                "/api/v1/tasks/t/cleanup-resolution",
                Some(body.clone()),
                Some(json!({"task_id":"t","resolved":true}).to_string()),
            )],
        )
        .await
        .unwrap();
        assert!(cli_fixture(
            &["resolve-cleanup", "t", file],
            vec![(
                "POST",
                "/api/v1/tasks/t/cleanup-resolution",
                Some(body),
                None
            )]
        )
        .await
        .is_err());
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn named_provider_inputs_reject_secret_arguments_and_bind_revisions() {
        let parsed = |args: &[&str]| {
            parse(args.iter().map(|arg| (*arg).to_owned()))
                .unwrap()
                .unwrap()
        };
        let id = "00000000-0000-0000-0000-000000000001";
        assert_eq!(
            provider_stdin_secret(&b"fixture-secret\r\n"[..]).unwrap(),
            "fixture-secret"
        );
        assert_eq!(
            provider_stdin_secret("x".repeat(2048).as_bytes())
                .unwrap()
                .len(),
            2048
        );
        for value in [
            "".to_owned(),
            "a\nb".to_owned(),
            "x".repeat(2049),
            "秘密".to_owned(),
        ] {
            assert!(provider_stdin_secret(value.as_bytes()).is_err());
        }
        let error = provider_request(&parsed(&[
            "provider-credential",
            "set",
            id,
            "1",
            "secret-on-argv",
        ]))
        .unwrap_err()
        .to_string();
        assert!(!error.contains("secret-on-argv"));
        assert!(error.contains("--stdin"));
        for value in [
            "../provider",
            "not-an-id",
            "00000000-0000-0000-0000-00000000000X",
        ] {
            assert!(
                provider_request(&parsed(&["providers", "enable", value, "1", "true"])).is_err()
            );
        }
        assert!(provider_request(&parsed(&["--follow", "providers", "list"])).is_err());
        let (_, path, body) = provider_request(&parsed(&["providers", "enable", id, "2", "false"]))
            .unwrap()
            .unwrap();
        assert_eq!(path, "/api/v1/providers/commands");
        assert_eq!(
            body.unwrap()["command"],
            json!({"type":"set_provider_enabled","id":id,"expected_revision":2,"enabled":false})
        );
        let (_, path, body) =
            provider_request(&parsed(&["provider-credential", "delete", id, "2"]))
                .unwrap()
                .unwrap();
        assert_eq!(path, format!("/api/v1/providers/{id}/credential-delete"));
        assert_eq!(body.unwrap()["expected_revision"], 2);
    }

    #[tokio::test]
    async fn named_provider_routes_are_owner_only_and_uncertain_mutations_do_not_retry() {
        let id = "00000000-0000-0000-0000-000000000001";
        let op = "00000000-0000-0000-0000-000000000002";
        cli_fixture(
            &["providers", "list"],
            vec![(
                "GET",
                "/api/v1/providers",
                None,
                Some(json!({"providers":[],"credential_operations":[]}).to_string()),
            )],
        )
        .await
        .unwrap();
        cli_fixture(&["providers","enable",id,"1","true"],vec![("POST","/api/v1/providers/commands",Some(json!({"type":"set_provider_enabled","id":id,"expected_revision":1,"enabled":true})),None)]).await.unwrap_err();
        let path = format!("/api/v1/providers/{id}/credential-delete");
        cli_fixture(&["--command-id",op,"provider-credential","delete",id,"1"],vec![("POST",&path,Some(json!({"id":op,"expected_revision":1})),Some(json!({"operation":{"id":op,"status":"unknown"},"replayed":false,"no_secret_applied":false}).to_string()))]).await.unwrap();
        let cleanup = format!("/api/v1/credential-operations/{id}/cleanup");
        cli_fixture(
            &["--command-id", op, "provider-credential", "cleanup", id],
            vec![("POST", &cleanup, Some(json!({"id":op})), None)],
        )
        .await
        .unwrap_err();
    }
}

#[cfg(test)]
mod mcp_credential_cli_tests {
    use super::*;
    #[test]
    fn dangerous_slots_are_rejected_before_reading_stdin_or_network() {
        let id = "11111111-1111-4111-8111-111111111111";
        for slot in [
            "PATH",
            "PSMODULEPATH",
            "CORECLR_ENABLE_PROFILING",
            "GLIBC_TUNABLES",
            "LOCPATH",
            "NLSPATH",
            "BASH_XTRACEFD",
            "PROMPT_COMMAND",
            "CUSTOMPROXY",
        ] {
            for kind in ["set", "delete"] {
                let mut args = vec!["mcp-credential", kind, id, slot, "1"];
                if kind == "set" {
                    args.push("--stdin");
                }
                let options = parse(args.into_iter().map(str::to_owned)).unwrap().unwrap();
                assert!(mcp_credential_request(&options).is_err(), "{slot}");
            }
        }
    }

    #[test]
    fn stdin_utf8_bounded_and_routes_are_separate() {
        assert_eq!(
            mcp_stdin_secret("多行\nfixture".as_bytes()).unwrap(),
            "多行\nfixture"
        );
        for v in [Vec::new(), vec![b'x'; 2049], vec![0], vec![255]] {
            assert!(mcp_stdin_secret(v.as_slice()).is_err());
        }
    }
}
