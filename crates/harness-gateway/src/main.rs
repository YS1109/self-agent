use anyhow::{bail, Context, Result};
use harness_gateway::{directory::Directory, Config, Gateway};
#[tokio::main]
async fn main() {
    if run().await.is_err() {
        eprintln!("网关操作失败：请检查本地参数、目录权限与配置；不输出凭据或请求正文。");
        std::process::exit(1)
    }
}
async fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("--db") {
        bail!("用法：--db PATH enroll LABEL | list | revoke UUID | serve --origin ORIGIN --bind LOOPBACK:PORT [--fixture]")
    }
    let path = args.next().context("缺少数据库路径")?;
    let directory = Directory::open(path)?;
    match args.next().as_deref() {
        Some("enroll") => {
            let label = args.next().context("缺少设备名称")?;
            if args.next().is_some() {
                bail!("多余参数")
            };
            let (device, token) = directory.enroll(&label)?;
            println!(
                "{}",
                serde_json::json!({"device":device,"transport_token":token})
            );
        }
        Some("list") => {
            if args.next().is_some() {
                bail!("多余参数")
            };
            println!("{}", serde_json::to_string(&directory.list()?)?);
        }
        Some("revoke") => {
            let id = args.next().context("缺少设备标识")?;
            if args.next().is_some() {
                bail!("多余参数")
            };
            directory.revoke(&id)?;
            println!("{}", serde_json::json!({"revoked":true,"device_id":id}));
        }
        Some("serve") => {
            let mut origin = None;
            let mut bind = None;
            let mut fixture = false;
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--origin" if origin.is_none() => origin = args.next(),
                    "--bind" if bind.is_none() => bind = args.next(),
                    "--fixture" if !fixture => fixture = true,
                    _ => bail!("参数无效"),
                }
            }
            let config = Config {
                origin: origin.context("缺少公开 Origin")?,
                bind: bind.context("缺少 loopback 地址")?.parse()?,
                fixture,
            };
            let addr = config.bind;
            let gateway = Gateway::new(config, directory)?;
            let listener = tokio::net::TcpListener::bind(addr).await?;
            eprintln!("网关已启动（在线请求，不缓存离线命令）");
            axum::serve(listener, gateway.router())
                .with_graceful_shutdown(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await?;
        }
        _ => bail!("命令无效"),
    }
    Ok(())
}
