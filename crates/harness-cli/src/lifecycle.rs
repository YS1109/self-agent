//! One owner shutdown request. A lost response is never a Stopped proof and
//! never causes a second POST or an automatic daemon replacement.
use super::*;

pub(super) async fn run(options: &Options) -> Result<bool> {
    if !matches!(options.command.as_str(), "daemon-status" | "daemon-stop") {
        return Ok(false);
    }
    if options.follow || options.controls.is_some() {
        bail!("后台生命周期命令不支持 follow/controls");
    }
    let stop = options.command == "daemon-stop";
    if stop {
        if options.args != ["--confirm"] {
            bail!("关闭后台须显式 daemon-stop --confirm；运行、排队及暂停任务/计划会取消，QQ/H5入口同时停止，已有文件不撤销");
        }
        if options.wait.is_some_and(|n| n > 30) {
            bail!("关闭观察等待范围为1至30秒；超时不取消清理");
        }
    } else if !options.args.is_empty() || options.wait.is_some() {
        bail!("daemon-status仅只读查询，不接受参数或wait");
    }
    let client = Client::open(&options.directory)?;
    client
        .compatible()
        .await
        .map_err(|_| anyhow::anyhow!("后台兼容性尚未确认；未发送关闭请求"))?;
    if stop {
        eprintln!("仅发送一次后台关闭请求；运行、排队及暂停任务/计划会取消，重启不自动恢复。清理可能继续，连接丢失不等于已停止，不自动重发或启动替代后台。");
    }
    let initial = observe(&client, None, None).await?;
    let value = if stop {
        let instance = initial["instance_id"].as_str().unwrap();
        eprintln!("固定关闭实例：{instance}");
        observe(&client, Some(options.wait.unwrap_or(30)), Some(instance)).await?
    } else {
        initial
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    if stop && value["outcome"] != "stopped" {
        bail!("后台停止尚未确认；未取消原清理、未重发请求。使用 daemon-status 只读观察；离线或新实例不能证明原实例已停止");
    }
    Ok(true)
}

async fn observe(client: &Client, wait: Option<u64>, expected: Option<&str>) -> Result<Value> {
    let (method, path, body, timeout) = match wait {
        Some(seconds) => (
            "POST",
            format!("/api/v1/lifecycle/shutdown?wait_ms={}", seconds * 1000),
            Some(json!({"expected_instance":expected})),
            Duration::from_secs(seconds + 1),
        ),
        None => (
            "GET",
            "/api/v1/lifecycle".into(),
            None,
            Duration::from_secs(3),
        ),
    };
    let value = tokio::time::timeout(timeout, client.request(method, &path, body)).await
        .map_err(|_| anyhow::anyhow!("后台生命周期观察超时；原清理可能仍在运行，没有重发或启动后台"))?
        .map_err(|_| anyhow::anyhow!("后台生命周期响应未确认；连接中断、接口不可用或鉴权失败均不代表已停止，没有重发请求"))?;
    validate(&value)?;
    if expected.is_some_and(|id| value["instance_id"].as_str() != Some(id)) {
        bail!("后台实例已变化；原实例停止未确认，没有重发请求");
    }
    Ok(value)
}

fn validate(value: &Value) -> Result<()> {
    let invalid = || anyhow::anyhow!("后台生命周期响应格式或一致性无效；未确认停止");
    let object = value.as_object().ok_or_else(invalid)?;
    if object.len() != 5
        || ["instance_id", "state", "phase", "outcome", "workers"]
            .iter()
            .any(|key| !object.contains_key(*key))
        || value["instance_id"]
            .as_str()
            .is_none_or(|id| provider_uuid(id).is_err())
        || !object.contains_key("workers")
        || !matches!(
            value["state"].as_str(),
            Some("running" | "draining" | "stopped")
        )
        || !matches!(
            value["phase"].as_str(),
            Some("not_requested" | "initial_quiesce" | "workers" | "final_quiesce" | "finished")
        )
        || !(value["outcome"].is_null()
            || matches!(value["outcome"].as_str(), Some("stopped" | "unknown")))
    {
        return Err(invalid());
    }
    let workers = value["workers"].as_object().ok_or_else(invalid)?;
    if workers.len() != 7
        || !matches!(
            value["workers"]["state"].as_str(),
            Some("running" | "draining" | "stopped")
        )
        || !value["workers"]["accepting_work"].is_boolean()
    {
        return Err(invalid());
    }
    for key in [
        "active_admissions",
        "pending_workers",
        "settled_workers",
        "unknown_workers",
        "unknown_admissions",
    ] {
        if value["workers"][key].as_u64().is_none() {
            return Err(invalid());
        }
    }
    let stopped = value["state"] == "stopped" || value["outcome"] == "stopped";
    if stopped
        && (value["state"] != "stopped"
            || value["outcome"] != "stopped"
            || value["phase"] != "finished"
            || value["workers"]["state"] != "stopped"
            || value["workers"]["accepting_work"] != false
            || [
                "active_admissions",
                "pending_workers",
                "unknown_workers",
                "unknown_admissions",
            ]
            .iter()
            .any(|key| value["workers"][*key] != 0))
    {
        return Err(invalid());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn stopped() -> Value {
        json!({"instance_id":"8f48d8ac-0d5f-49aa-b2b3-3a1149ab748c","state":"stopped","phase":"finished","outcome":"stopped","workers":{"state":"stopped","accepting_work":false,"active_admissions":0,"pending_workers":0,"settled_workers":2,"unknown_workers":0,"unknown_admissions":0}})
    }
    #[test]
    fn cannot_infer_stopped_from_partial_or_inconsistent_state() {
        validate(&stopped()).unwrap();
        for key in [
            "pending_workers",
            "unknown_workers",
            "unknown_admissions",
            "active_admissions",
        ] {
            let mut v = stopped();
            v["workers"][key] = json!(1);
            assert!(validate(&v).is_err());
        }
        let mut v = stopped();
        v["outcome"] = Value::Null;
        assert!(validate(&v).is_err());
        assert!(validate(&json!({"state":"stopped"})).is_err());
        let mut v = stopped();
        v["private"] = json!("must not print");
        assert!(validate(&v).is_err());
        let mut v = stopped();
        v["state"] = json!("running");
        v["phase"] = json!("not_requested");
        v.as_object_mut().unwrap().remove("outcome");
        v["private"] = json!("must not print");
        assert!(validate(&v).is_err());
    }
    #[tokio::test]
    async fn shutdown_requires_explicit_confirmation_before_opening_metadata() {
        let options = parse(["daemon-stop"].into_iter().map(str::to_owned))
            .unwrap()
            .unwrap();
        assert!(run(&options)
            .await
            .unwrap_err()
            .to_string()
            .contains("--confirm"));
    }
    #[tokio::test]
    async fn lost_shutdown_response_is_one_post_and_never_stopped() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut connection, _) = listener.accept().await.unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                connection.read_exact(&mut byte).await.unwrap();
                header.push(byte[0]);
                assert!(header.len() < 8192);
            }
            let header = String::from_utf8(header).unwrap();
            assert!(header.starts_with("POST /api/v1/lifecycle/shutdown?wait_ms=1000 "));
            let length: usize = header
                .to_ascii_lowercase()
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .unwrap()
                .parse()
                .unwrap();
            let mut body = vec![0; length];
            connection.read_exact(&mut body).await.unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&body).unwrap(),
                json!({"expected_instance":"8f48d8ac-0d5f-49aa-b2b3-3a1149ab748c"})
            );
            connection.shutdown().await.unwrap();
            drop(connection);
            assert!(
                tokio::time::timeout(Duration::from_millis(200), listener.accept())
                    .await
                    .is_err()
            );
        });
        let client = Client {
            http: reqwest::Client::builder().no_proxy().build().unwrap(),
            base: base.parse().unwrap(),
            token: "fixture-lifecycle-owner".into(),
            protocol: tokio::sync::OnceCell::new(),
        };
        client.protocol.set(1).unwrap();
        let result = observe(
            &client,
            Some(1),
            Some("8f48d8ac-0d5f-49aa-b2b3-3a1149ab748c"),
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("未确认"));
        server.await.unwrap();
    }
}
