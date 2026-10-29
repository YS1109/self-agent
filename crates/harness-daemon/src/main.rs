use std::{
    future::IntoFuture,
    net::{IpAddr, Ipv4Addr, SocketAddr},
};

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
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .context("无法绑定本机服务端口，可能已有实例正在运行")?;
    let address = listener.local_addr()?;
    // Install Unix termination handlers before creating the Vault or execution
    // workers. Keep the streams alive through the original cleanup owner: a
    // second signal must not introduce an untracked forced-exit path.
    let mut shutdown_signal = ShutdownSignal::new().context("无法监听本机关闭信号")?;
    let token = format!("{}{}", new_id(), new_id());
    let state = AppState::open_managed(lease.clone(), token.clone())?;
    let info = DaemonInfo {
        base_url: format!("http://{address}"),
        token,
        pid: std::process::id(),
    };
    if let Err(error) = lease.write_daemon_metadata(&info) {
        // State already owns the real Vault thread. A metadata failure is not
        // permission to release the lease or runtime before that thread joins.
        if state.request_shutdown().is_err() {
            tracing::error!("启动失败且无法启动关闭监督器；保持数据目录所有权，未确认停止");
        }
        state.wait_stopped().await;
        return Err(error);
    }
    if let Err(error) = spawn_workers(state.clone()) {
        // A partial startup may already own OS/async workers. Do not drop the
        // runtime/data lease merely because the remaining registration failed.
        if state.request_shutdown().is_err() {
            tracing::error!("启动失败且关闭控制不可用；保持原工作所有权");
        }
        state.wait_stopped().await;
        return Err(error);
    }
    tracing::info!(base_url=%info.base_url, "本机后台服务已就绪");
    let (stop_http, mut http_stop) = tokio::sync::watch::channel(false);
    let server = axum::serve(listener, router(state.clone()))
        .with_graceful_shutdown(async move {
            while !*http_stop.borrow_and_update() {
                if http_stop.changed().await.is_err() {
                    break;
                }
            }
        })
        .into_future();
    tokio::pin!(server);
    // This owner observer is continuously driven, not a worker in the set it
    // drains. A failure is detected even with zero HTTP clients or polls.
    let serving_result = tokio::select! {
        result = &mut server => Some(result),
        _ = state.monitor_shutdown() => None,
        result = shutdown_signal.wait() => {
            if result.is_err() { tracing::error!("无法监听本机关闭信号；启动安全排空"); }
            None
        }
    };
    if state.request_shutdown().is_err() {
        tracing::error!("关闭控制不可用；保持原工作和数据目录所有权，未确认停止");
    }
    // Pending/Unknown retains actual ownership indefinitely; repeated HTTP
    // observation/connection loss never aborts cleanup or claims Stopped.
    // Keep polling the server while draining so status remains available.
    if let Some(result) = serving_result {
        state.wait_stopped().await;
        result?;
    } else {
        tokio::select! {
            _ = state.wait_stopped() => {},
            result = &mut server => {
                state.wait_stopped().await;
                result?;
                return Ok(());
            }
        }
        stop_http.send_replace(true);
        server.await?;
    }
    // Stale metadata is not a stop proof; clients already check health/identity.
    // The directory lease lives until this confirmed cleanup/graceful exit.
    Ok(())
}

/// Signal observation only. Every accepted signal enters the same retained
/// supervisor as HTTP and worker failure; this type never stops a worker.
struct ShutdownSignal {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
}

impl ShutdownSignal {
    fn new() -> std::io::Result<Self> {
        Ok(Self {
            #[cfg(unix)]
            interrupt: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?,
            #[cfg(unix)]
            terminate: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
        })
    }

    async fn wait(&mut self) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            let received = tokio::select! {
                received = self.interrupt.recv() => received,
                received = self.terminate.recv() => received,
            };
            received.ok_or_else(|| std::io::Error::other("关闭信号流已结束"))
        }
        #[cfg(not(unix))]
        {
            tokio::signal::ctrl_c().await
        }
    }
}
