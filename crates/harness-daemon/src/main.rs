use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::Path,
};

use anyhow::{bail, Context, Result};
use fs2::FileExt;
use harness_core::{data_dir, new_id, DaemonInfo};
use harness_daemon::{router, spawn_workers, AppState};

fn private_file(path: &Path, content: &[u8]) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    use std::io::Write;
    file.write_all(content)?;
    file.sync_all()?;
    Ok(())
}

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
        .context("Invalid HARNESS_BIND")?;
    if bind.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) {
        bail!("HARNESS_BIND must use 127.0.0.1; public listeners are not supported");
    }
    let directory = data_dir();
    std::fs::create_dir_all(&directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    }
    let mut lock_options = std::fs::OpenOptions::new();
    lock_options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        lock_options.mode(0o600);
    }
    let _lock_file = lock_options.open(directory.join("daemon.lock"))?;
    _lock_file
        .try_lock_exclusive()
        .context("Another daemon already owns this data directory")?;
    // Bind before opening SQLite so a second process cannot run crash recovery
    // against a still-active daemon using the usual address.
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .context("Could not bind local daemon; another instance may be running")?;
    let address = listener.local_addr()?;
    let token = format!("{}{}", new_id(), new_id());
    let state = AppState::open(&directory.join("harness.sqlite"), token.clone())?;
    let info = DaemonInfo {
        base_url: format!("http://{address}"),
        token,
        pid: std::process::id(),
    };
    let temporary = directory.join(format!("daemon-{}.tmp", new_id()));
    private_file(&temporary, &serde_json::to_vec(&info)?)?;
    let metadata = directory.join("daemon.json");
    #[cfg(windows)]
    if metadata.exists() {
        std::fs::remove_file(&metadata)?;
    }
    std::fs::rename(&temporary, &metadata)?;
    spawn_workers(state.clone());
    tracing::info!(base_url=%info.base_url, "Local daemon ready");
    let result = axum::serve(listener, router(state)).await;
    // Do not remove metadata after an abrupt exit; clients verify health and PID.
    result?;
    Ok(())
}
