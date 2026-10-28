use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use anyhow::{bail, Context, Result};
use harness_core::{data_dir, new_id, DaemonInfo};
use harness_daemon::{router, spawn_workers, AppState};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "harness_daemon=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let bind: SocketAddr = std::env::var("HARNESS_BIND")
        .unwrap_or_else(|_| "127.0.0.1:47371".into())
        .parse()
        .context("HARNESS_BIND 监听地址无效")?;
    if bind.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) {
        bail!("HARNESS_BIND 必须使用 127.0.0.1，当前版本不支持公网监听");
    }
    let directory = data_dir();
    let lease = harness_daemon::maintenance::DataLease::acquire(&directory)?;
    // Bind before opening SQLite so a second process cannot run crash recovery
    // against a still-active daemon using the usual address.
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .context("无法绑定本机服务端口，可能已有实例正在运行")?;
    let address = listener.local_addr()?;
    let token = format!("{}{}", new_id(), new_id());
    let state = AppState::open_managed(lease.clone(), token.clone())?;
    let info = DaemonInfo {
        base_url: format!("http://{address}"),
        token,
        pid: std::process::id(),
    };
    lease.write_daemon_metadata(&info)?;
    spawn_workers(state.clone());
    tracing::info!(base_url=%info.base_url, "本机后台服务已就绪");
    let result = axum::serve(listener, router(state.clone())).await;
    if !state.shutdown_credentials().await {
        tracing::warn!("凭据工作线程未确认停止；未完成操作将在下次启动保守恢复，不能自动重放");
    }
    // Do not remove metadata after an abrupt exit; clients verify health and PID.
    result?;
    Ok(())
}
