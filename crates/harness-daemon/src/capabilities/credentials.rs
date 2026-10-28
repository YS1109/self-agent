//! Private immutable MCP environment snapshots. No host environment or Provider
//! credential fallback is possible. Each declared slot is read once per task.
use super::vault::{SecretBytes, VaultCancellation};
use crate::{
    store::capability_credentials::{mcp_binding, McpCredentialSnapshot},
    AppState,
};
use anyhow::{bail, Context, Result};
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};
use zeroize::Zeroize;

pub(crate) struct ResolvedServerCredentials {
    snapshot: McpCredentialSnapshot,
    environment: BTreeMap<String, String>,
}
impl Drop for ResolvedServerCredentials {
    fn drop(&mut self) {
        for value in self.environment.values_mut() {
            value.zeroize();
        }
    }
}
impl ResolvedServerCredentials {
    pub(crate) fn environment(&self) -> BTreeMap<String, String> {
        self.environment.clone()
    }
    pub(crate) fn secrets(&self) -> Vec<String> {
        self.environment.values().cloned().collect()
    }
    pub(crate) fn check(&self, state: &Arc<AppState>) -> Result<()> {
        state
            .store
            .lock()
            .unwrap()
            .check_mcp_credential_snapshot(&self.snapshot)
    }
}
pub(crate) struct ResolvedTaskCredentials {
    servers: BTreeMap<String, ResolvedServerCredentials>,
}
impl ResolvedTaskCredentials {
    pub(crate) fn environment(&self, id: &str, revision: u64) -> Result<BTreeMap<String, String>> {
        let server = self
            .servers
            .get(id)
            .context("任务没有此服务的私有凭据快照")?;
        if server.snapshot.revision != revision {
            bail!("MCP 服务修订与凭据快照不一致");
        }
        Ok(server.environment())
    }
    pub(crate) fn secrets(&self) -> Vec<String> {
        self.servers
            .values()
            .flat_map(ResolvedServerCredentials::secrets)
            .collect()
    }
}
struct CancelOnDrop(VaultCancellation);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
async fn read(
    state: &Arc<AppState>,
    snapshot: &McpCredentialSnapshot,
    slot: &str,
    deadline: Instant,
    stopped: &(impl Fn() -> bool + Sync),
) -> Result<SecretBytes> {
    state
        .store
        .lock()
        .unwrap()
        .check_mcp_credential_snapshot(snapshot)?;
    if stopped() || Instant::now() >= deadline {
        bail!("MCP 凭据读取已取消或到期");
    }
    let reference = snapshot
        .bindings
        .get(slot)
        .context("MCP 声明的凭据槽未绑定")?;
    let binding = mcp_binding(&snapshot.server_id, slot, &snapshot.config_digest)?;
    let cancellation = CancelOnDrop(VaultCancellation::default());
    let result = state.vault.get(
        reference,
        &binding,
        deadline.min(Instant::now() + Duration::from_secs(15)),
        &cancellation.0,
    );
    tokio::pin!(result);
    loop {
        if stopped()
            || state
                .store
                .lock()
                .unwrap()
                .check_mcp_credential_snapshot(snapshot)
                .is_err()
        {
            cancellation.0.cancel();
        }
        tokio::select! {
            value=&mut result=> {
                let value=value.context("MCP 系统凭据不可用；未使用其它服务凭据")?;
                if stopped() { bail!("MCP 凭据读取期间已撤销或取消"); }
                state.store.lock().unwrap().check_mcp_credential_snapshot(snapshot)?;
                return Ok(value)
            },
            _=tokio::time::sleep(Duration::from_millis(25))=>{}
        }
    }
}
async fn resolve(
    state: &Arc<AppState>,
    snapshot: McpCredentialSnapshot,
    deadline: Instant,
    stopped: &(impl Fn() -> bool + Sync),
) -> Result<ResolvedServerCredentials> {
    let mut result = ResolvedServerCredentials {
        snapshot,
        environment: BTreeMap::new(),
    };
    for slot in &result.snapshot.declared_slots {
        let bytes = read(state, &result.snapshot, slot, deadline, stopped).await?;
        let value =
            std::str::from_utf8(bytes.expose_secret()).context("MCP 系统凭据不是有效 UTF-8")?;
        if value.is_empty() || value.contains('\0') {
            bail!("MCP 系统凭据不适合环境变量，未启动服务");
        }
        result.environment.insert(slot.clone(), value.to_owned());
    }
    if stopped() {
        bail!("MCP 凭据解析已取消或撤销");
    }
    result.check(state)?;
    Ok(result)
}
pub(crate) async fn task(
    state: &Arc<AppState>,
    task_id: &str,
    deadline: tokio::time::Instant,
    stopped: impl Fn() -> bool + Sync,
) -> Result<ResolvedTaskCredentials> {
    let snapshots = state.store.lock().unwrap().task_mcp_credentials(task_id)?;
    let mut servers = BTreeMap::new();
    for snapshot in snapshots {
        let id = snapshot.server_id.clone();
        let resolved = resolve(state, snapshot, deadline.into_std(), &stopped).await?;
        if servers.insert(id, resolved).is_some() {
            bail!("任务 MCP 凭据快照重复");
        }
    }
    state
        .store
        .lock()
        .unwrap()
        .check_task_mcp_credentials(task_id)?;
    Ok(ResolvedTaskCredentials { servers })
}
pub(crate) async fn probe(
    state: &Arc<AppState>,
    server: &str,
    revision: u64,
    deadline: tokio::time::Instant,
    stopped: impl Fn() -> bool + Sync,
) -> Result<ResolvedServerCredentials> {
    let snapshot = state
        .store
        .lock()
        .unwrap()
        .probe_mcp_credentials(server, revision)?;
    resolve(state, snapshot, deadline.into_std(), &stopped).await
}
