use super::*;

pub(super) async fn run(options: &Options) -> Result<bool> {
    if !matches!(
        options.command.as_str(),
        "diagnostics" | "backups" | "backup" | "backup-status" | "backup-inspect" | "restore"
    ) {
        return Ok(false);
    }
    if options.follow
        || options.controls.is_some()
        || (options.wait.is_some() && options.command != "backup")
    {
        bail!("维护命令不支持follow/controls；只有backup支持wait");
    }
    let value = match options.command.as_str() {
        "backup-inspect" => {
            arity(options, 1, 1)?;
            let bundle = PathBuf::from(&options.args[0]);
            tokio::task::spawn_blocking(move || {
                harness_daemon::maintenance::inspect_backup(&bundle)
            })
            .await??
        }
        "restore" => {
            arity(options, 3, 3)?;
            if options.args[1] != "--confirm-sha256" || !digest(&options.args[2]) {
                bail!("恢复必须显式提供--confirm-sha256和64位小写摘要；未停止后台或修改数据库");
            }
            let directory = options.directory.clone();
            let bundle = PathBuf::from(&options.args[0]);
            let expected = options.args[2].clone();
            tokio::task::spawn_blocking(move || {
                harness_daemon::maintenance::restore_backup(&directory, &bundle, &expected)
            })
            .await??
        }
        _ => {
            let (method, path, body) = route(options)?;
            let client = Client::open(&options.directory)?;
            if method == "POST" {
                eprintln!(
                    "备份操作 ID：{}；结果未知时仅用backup-status查询，不重发。",
                    options.command_id
                );
            }
            let mut response = client.request(method, &path, body).await.map_err(|_| {
                anyhow::anyhow!(
                    "维护请求未确认；未回显路径或底层错误。请查询原备份操作，不要自动重发。"
                )
            })?;
            if options.command == "backup" {
                if let Some(seconds) = options.wait {
                    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
                    loop {
                        match response["state"].as_str() {
                            Some("completed") => break,
                            Some("failed" | "unknown") => {
                                println!("{}", serde_json::to_string_pretty(&response)?);
                                bail!("备份未确认成功；保留原操作记录，请检查目标目录");
                            }
                            Some("pending" | "running") => {}
                            _ => bail!("备份回执无效，未重发操作"),
                        }
                        if tokio::time::Instant::now() >= deadline {
                            bail!(
                                "等待备份超时；没有取消或重新发送后台操作，请用backup-status查询"
                            );
                        }
                        response = tokio::time::timeout_at(deadline, async {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            client
                                .request(
                                    "GET",
                                    &format!("/api/v1/reliability/backups/{}", options.command_id),
                                    None,
                                )
                                .await
                        })
                        .await
                        .map_err(|_| {
                            anyhow::anyhow!(
                                "等待备份超时；没有取消或重新发送后台操作，请用backup-status查询"
                            )
                        })??;
                    }
                }
            }
            response
        }
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(true)
}
fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn route(options: &Options) -> Result<(&'static str, String, Option<Value>)> {
    match options.command.as_str() {
        "diagnostics" | "backups" => {
            arity(options, 0, 0)?;
            Ok((
                "GET",
                if options.command == "diagnostics" {
                    "/api/v1/reliability/diagnostics"
                } else {
                    "/api/v1/reliability"
                }
                .into(),
                None,
            ))
        }
        "backup" => {
            arity(options, 2, 2)?;
            Ok((
                "POST",
                "/api/v1/reliability/backups".into(),
                Some(
                    json!({"id":options.command_id,"parent":options.args[0],"name":options.args[1]}),
                ),
            ))
        }
        "backup-status" => {
            arity(options, 1, 1)?;
            let id = &options.args[0];
            if id.len() != 36 || !id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
                bail!("备份操作ID格式无效");
            }
            Ok(("GET", format!("/api/v1/reliability/backups/{id}"), None))
        }
        _ => bail!("维护命令无效"),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn options(args: &[&str]) -> Options {
        parse(args.iter().map(|s| s.to_string())).unwrap().unwrap()
    }
    #[test]
    fn maintenance_routes_are_explicit_and_private_paths_only_in_the_owner_post() {
        let value = options(&["backup", "/private/fixture", "backup-1"]);
        let (method, path, body) = route(&value).unwrap();
        assert_eq!(method, "POST");
        assert_eq!(path, "/api/v1/reliability/backups");
        assert_eq!(body.unwrap()["parent"], "/private/fixture");
        assert_eq!(route(&options(&["diagnostics"])).unwrap().0, "GET");
        assert!(route(&options(&["backup-status", "../../credential"])).is_err());
        assert!(route(&options(&["backups", "unexpected"])).is_err());
        assert!(digest(&"a".repeat(64)));
        assert!(!digest(&"A".repeat(64)));
    }

    #[tokio::test]
    async fn backup_wait_deadline_bounds_a_stalled_status_get_without_reposting() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let root = std::env::temp_dir().join(format!("sa-backup-cli-{}", new_id()));
        std::fs::create_dir(&root).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        std::fs::write(root.join("daemon.json"), json!({"base_url":format!("http://{}",listener.local_addr().unwrap()),"token":"fixture","pid":1}).to_string()).unwrap();
        let mut options = options(&["--wait", "1", "backup", "/fixture", "new"]);
        options.directory = root.clone();
        let id = options.command_id.clone();
        let server = tokio::spawn(async move {
            for method in ["POST", "GET"] {
                let (mut stream, _) = crate::tests::accept_application(&listener).await.unwrap();
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).await.unwrap();
                    header.push(byte[0]);
                    assert!(header.len() < 16384);
                }
                let header = String::from_utf8(header).unwrap();
                assert!(header.starts_with(&format!("{method} /api/v1/reliability/backups")));
                let length = header
                    .to_ascii_lowercase()
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("content-length: ")
                            .map(|s| s.parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                let mut body = vec![0; length];
                stream.read_exact(&mut body).await.unwrap();
                if method == "POST" {
                    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap()["id"], id);
                    let body = json!({"id":id,"state":"pending","reason":null,"manifest":null})
                        .to_string();
                    stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
                } else {
                    // No response: cancellation must close this read-only GET.
                    let mut byte = [0];
                    assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
                }
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(150), listener.accept())
                    .await
                    .is_err()
            );
        });
        let started = std::time::Instant::now();
        let error = tokio::time::timeout(Duration::from_secs(4), run(&options))
            .await
            .expect("wait deadline must bound stalled GET")
            .unwrap_err();
        assert!(error.to_string().contains("等待备份超时"));
        assert!(started.elapsed() < Duration::from_secs(4));
        server.await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
