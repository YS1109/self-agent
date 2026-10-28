//! Explicit OS-vault access. This module never reads environment/file credentials.
//!
//! All OS operations are serial and lazy: constructing the worker does not open a
//! credential store. A timed-out mutation may still finish on the worker. Callers
//! must persist an intent before submitting it and must not retry an Unknown
//! result. Fresh references are consumed by `set`; rotation creates a new entry.

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot, Notify};
use uuid::Uuid;
use zeroize::Zeroizing;

#[path = "vault/system.rs"]
mod system;
#[cfg(test)]
#[path = "vault/test_support.rs"]
pub(crate) mod test_support;
#[cfg(test)]
#[path = "vault/tests.rs"]
mod tests;

const NAMESPACE: &str = "self-agent.vault.v1";
const MAX_SECRET_BYTES: usize = 2048;
const MAX_QUEUE: usize = 32;
const MAX_WAIT: Duration = Duration::from_secs(30);
const PENDING: u8 = 0;
const STARTED: u8 = 1;
const ABANDONED: u8 = 2;
const COMPLETE: u8 = 3;
const RUNNING: u8 = 0;
const CLOSING: u8 = 1;
const STOPPED: u8 = 2;
const FAILED: u8 = 3;

/// Private bytes with explicit exposure only; deliberately not Debug/Serialize/Clone.
pub(crate) struct SecretBytes(Zeroizing<Vec<u8>>);

impl SecretBytes {
    pub(crate) fn new(bytes: Vec<u8>) -> Result<Self, VaultError> {
        let bytes = Zeroizing::new(bytes);
        if bytes.is_empty() || bytes.len() > MAX_SECRET_BYTES {
            return Err(VaultError::not_attempted(VaultReason::InvalidSecret));
        }
        Ok(Self(bytes))
    }

    pub(crate) fn expose_secret(&self) -> &[u8] {
        &self.0
    }
}

/// No endpoint, purpose, or identifier is stored in OS entry attributes verbatim.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct VaultBinding(String);

impl VaultBinding {
    pub(crate) fn provider(owner: &str, purpose: &str, endpoint: &str) -> Result<Self, VaultError> {
        if endpoint.len() > 2048
            || endpoint.trim() != endpoint
            || endpoint.chars().any(char::is_control)
        {
            return Err(VaultError::not_attempted(VaultReason::InvalidBinding));
        }
        let mut url = reqwest::Url::parse(endpoint)
            .map_err(|_| VaultError::not_attempted(VaultReason::InvalidBinding))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(VaultError::not_attempted(VaultReason::InvalidBinding));
        }
        // A trailing slash is not an origin/service change. Other path differences
        // remain distinct, even if a remote server might happen to alias them.
        let path = url.path().trim_end_matches('/').to_owned();
        url.set_path(if path.is_empty() { "/" } else { &path });
        Self::build("provider", owner, purpose, url.as_str())
    }

    /// Bind stdio credentials to the approved complete server-config fingerprint,
    /// including executable, args, protocol, cwd and environment variable names
    /// (computed by caller). Exclude secret values and credential-reference IDs
    /// from that fingerprint, otherwise reference creation would be circular.
    pub(crate) fn capability(
        owner: &str,
        purpose: &str,
        config_sha256: &str,
    ) -> Result<Self, VaultError> {
        if !is_digest(config_sha256) {
            return Err(VaultError::not_attempted(VaultReason::InvalidBinding));
        }
        Self::build("capability", owner, purpose, config_sha256)
    }

    fn build(kind: &str, owner: &str, purpose: &str, endpoint: &str) -> Result<Self, VaultError> {
        if !canonical_uuid(owner)
            || purpose.is_empty()
            || purpose.len() > 64
            || !purpose
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_:-.".contains(&b))
        {
            return Err(VaultError::not_attempted(VaultReason::InvalidBinding));
        }
        let mut hash = Sha256::new();
        for part in [NAMESPACE, kind, owner, purpose, endpoint] {
            hash.update((part.len() as u64).to_be_bytes());
            hash.update(part.as_bytes());
        }
        Ok(Self(format!("{:x}", hash.finalize())))
    }
}

/// An opaque, non-secret database reference; never a raw OS lookup selector.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct CredentialRef(String);

impl CredentialRef {
    pub(crate) fn parse(value: &str, binding: &VaultBinding) -> Result<Self, VaultError> {
        if value.len() != NAMESPACE.len() + 1 + 64 + 1 + 36 {
            return Err(VaultError::not_attempted(VaultReason::BindingMismatch));
        }
        let parts: Vec<_> = value.split(':').collect();
        if parts.len() != 3
            || parts[0] != NAMESPACE
            || parts[1] != binding.0
            || !canonical_uuid(parts[2])
        {
            return Err(VaultError::not_attempted(VaultReason::BindingMismatch));
        }
        Ok(Self(value.to_owned()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    fn matches(&self, binding: &VaultBinding) -> bool {
        Self::parse(&self.0, binding).is_ok()
    }

    fn account(&self) -> String {
        // Fixed service plus fixed-format digest/UUID avoids Windows delimiter
        // collisions. Attributes are public metadata, never credential contents.
        self.0[NAMESPACE.len() + 1..].to_owned()
    }
}

/// One-shot write material. Cannot be constructed from a persisted reference.
pub(crate) struct FreshCredential {
    reference: CredentialRef,
    secret: SecretBytes,
}

impl FreshCredential {
    pub(crate) fn new(binding: &VaultBinding, secret: SecretBytes) -> Self {
        Self {
            reference: CredentialRef(format!("{NAMESPACE}:{}:{}", binding.0, Uuid::new_v4())),
            secret,
        }
    }
    pub(crate) fn reference(&self) -> &CredentialRef {
        &self.reference
    }
}

fn canonical_uuid(value: &str) -> bool {
    value.len() == 36 && Uuid::parse_str(value).is_ok_and(|id| id.hyphenated().to_string() == value)
}
fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VaultReason {
    InvalidBinding,
    BindingMismatch,
    InvalidSecret,
    InvalidQueue,
    Busy,
    Unavailable,
    AccessDeniedOrLocked,
    Missing,
    Ambiguous,
    InvalidStoredData,
    Unexpected,
    Timeout,
    Cancelled,
    ShuttingDown,
    WorkerFailed,
}

impl VaultReason {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::InvalidBinding => "invalid_binding",
            Self::BindingMismatch => "binding_mismatch",
            Self::InvalidSecret => "invalid_secret",
            Self::InvalidQueue => "invalid_queue",
            Self::Busy => "busy",
            Self::Unavailable => "unavailable",
            Self::AccessDeniedOrLocked => "access_denied_or_locked",
            Self::Missing => "missing",
            Self::Ambiguous => "ambiguous",
            Self::InvalidStoredData => "invalid_stored_data",
            Self::Unexpected => "unexpected",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::ShuttingDown => "shutting_down",
            Self::WorkerFailed => "worker_failed",
        }
    }
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::InvalidBinding | Self::BindingMismatch => "凭据与服务绑定不匹配",
            Self::InvalidSecret => "凭据必须为 1 至 2048 字节",
            Self::InvalidQueue => "凭据工作队列配置无效",
            Self::Busy => "凭据工作队列已满，请稍后显式重试未执行的操作",
            Self::Unavailable => "系统凭据存储不可用，未使用替代存储",
            Self::AccessDeniedOrLocked => "系统凭据存储已锁定或访问被拒绝",
            Self::Missing => "系统凭据不存在",
            Self::Ambiguous => "系统凭据记录不唯一，已拒绝访问",
            Self::InvalidStoredData => "系统凭据内容无效，未回显内容",
            Self::Unexpected => "系统凭据操作异常，未回显原始错误",
            Self::Timeout => "等待系统凭据操作超时",
            Self::Cancelled => "已停止等待系统凭据操作",
            Self::ShuttingDown => "凭据工作线程正在关闭",
            Self::WorkerFailed => "凭据工作线程已终止",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VaultCertainty {
    /// No OS operation was started (safe to submit a fresh explicit operation).
    NotAttempted,
    /// A read failed or a platform explicitly rejected the mutation.
    Rejected,
    /// A mutation entered the OS and its final effect was not confirmed.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VaultError {
    pub(crate) reason: VaultReason,
    pub(crate) certainty: VaultCertainty,
}
impl VaultError {
    fn not_attempted(reason: VaultReason) -> Self {
        Self {
            reason,
            certainty: VaultCertainty::NotAttempted,
        }
    }
}
impl std::fmt::Display for VaultError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.reason.message())
    }
}
impl std::error::Error for VaultError {}

#[derive(Clone, Default)]
pub(crate) struct VaultCancellation {
    cancelled: Arc<AtomicBool>,
    wake: Arc<Notify>,
}
impl VaultCancellation {
    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.wake.notify_waiters();
    }
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
    async fn cancelled(&self) {
        loop {
            let wake = self.wake.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            wake.await;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShutdownStatus {
    Stopped,
    Draining,
    Failed,
}

pub(crate) struct Vault {
    sender: mpsc::Sender<Job>,
    state: Arc<AtomicU8>,
    done: Arc<Notify>,
}

impl Vault {
    /// Lazy construction: does not call any OS credential APIs.
    pub(crate) fn system(queue_capacity: usize) -> Result<Self, VaultError> {
        Self::start(system::SystemBackend::default(), queue_capacity)
    }

    fn start(backend: impl Backend, queue_capacity: usize) -> Result<Self, VaultError> {
        if !(1..=MAX_QUEUE).contains(&queue_capacity) {
            return Err(VaultError::not_attempted(VaultReason::InvalidQueue));
        }
        let (sender, receiver) = mpsc::channel(queue_capacity);
        let state = Arc::new(AtomicU8::new(RUNNING));
        let done = Arc::new(Notify::new());
        let worker_state = state.clone();
        let worker_done = done.clone();
        std::thread::Builder::new()
            .name("self-agent-vault".into())
            .spawn(move || worker(backend, receiver, worker_state, worker_done))
            .map_err(|_| VaultError::not_attempted(VaultReason::Unavailable))?;
        Ok(Self {
            sender,
            state,
            done,
        })
    }

    pub(crate) async fn set(
        &self,
        fresh: FreshCredential,
        binding: &VaultBinding,
        deadline: Instant,
        cancel: &VaultCancellation,
    ) -> Result<(), VaultError> {
        match self
            .submit(
                Operation::Set(fresh.secret),
                fresh.reference,
                binding,
                deadline,
                cancel,
            )
            .await?
        {
            Reply::Done => Ok(()),
            Reply::Secret(_) => unreachable!("typed vault operation"),
        }
    }

    pub(crate) async fn get(
        &self,
        reference: &CredentialRef,
        binding: &VaultBinding,
        deadline: Instant,
        cancel: &VaultCancellation,
    ) -> Result<SecretBytes, VaultError> {
        match self
            .submit(Operation::Get, reference.clone(), binding, deadline, cancel)
            .await?
        {
            Reply::Secret(secret) => Ok(secret),
            Reply::Done => unreachable!("typed vault operation"),
        }
    }

    pub(crate) async fn delete(
        &self,
        reference: &CredentialRef,
        binding: &VaultBinding,
        deadline: Instant,
        cancel: &VaultCancellation,
    ) -> Result<(), VaultError> {
        match self
            .submit(
                Operation::Delete,
                reference.clone(),
                binding,
                deadline,
                cancel,
            )
            .await?
        {
            Reply::Done => Ok(()),
            Reply::Secret(_) => unreachable!("typed vault operation"),
        }
    }

    async fn submit(
        &self,
        operation: Operation,
        reference: CredentialRef,
        binding: &VaultBinding,
        deadline: Instant,
        cancel: &VaultCancellation,
    ) -> Result<Reply, VaultError> {
        if !reference.matches(binding) {
            return Err(VaultError::not_attempted(VaultReason::BindingMismatch));
        }
        if cancel.is_cancelled() {
            return Err(VaultError::not_attempted(VaultReason::Cancelled));
        }
        if Instant::now() >= deadline {
            return Err(VaultError::not_attempted(VaultReason::Timeout));
        }
        if self.state.load(Ordering::SeqCst) != RUNNING {
            return Err(self.closed_error());
        }
        let mutation = operation.is_mutation();
        let state = Arc::new(AtomicU8::new(PENDING));
        let (reply, mut receive) = oneshot::channel();
        let deadline = deadline.min(Instant::now() + MAX_WAIT);
        let job = Job {
            operation,
            reference,
            state: state.clone(),
            reply,
            deadline,
        };
        self.sender.try_send(job).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => VaultError::not_attempted(VaultReason::Busy),
            mpsc::error::TrySendError::Closed(_) => self.closed_error(),
        })?;
        let _guard = AbandonOnDrop(state.clone());
        tokio::select! {
            biased;
            result = &mut receive => result.unwrap_or_else(|_| Err(interrupted(&state, mutation, VaultReason::WorkerFailed))),
            _ = cancel.cancelled() => Err(interrupted(&state, mutation, VaultReason::Cancelled)),
            _ = tokio::time::sleep_until(deadline.into()) => Err(interrupted(&state, mutation, VaultReason::Timeout)),
        }
    }

    fn closed_error(&self) -> VaultError {
        VaultError::not_attempted(
            if matches!(self.state.load(Ordering::SeqCst), FAILED | RUNNING) {
                VaultReason::WorkerFailed
            } else {
                VaultReason::ShuttingDown
            },
        )
    }

    /// A deadline does not kill a blocked OS call. Draining means this worker is
    /// still alive, owns its queued secrets, and accepts no additional operations.
    pub(crate) async fn shutdown(&self, deadline: Instant) -> ShutdownStatus {
        self.begin_shutdown();
        loop {
            let done = self.done.notified();
            tokio::pin!(done);
            done.as_mut().enable();
            match self.state.load(Ordering::SeqCst) {
                STOPPED => return ShutdownStatus::Stopped,
                FAILED => return ShutdownStatus::Failed,
                _ => {}
            }
            tokio::select! {
                _ = done => {},
                _ = tokio::time::sleep_until(deadline.into()) => return ShutdownStatus::Draining,
            }
        }
    }

    fn begin_shutdown(&self) {
        if self
            .state
            .compare_exchange(RUNNING, CLOSING, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            // Wake an idle receiver. If full, the existing work will wake it.
            let (reply, _) = oneshot::channel();
            let _ = self.sender.try_send(Job {
                operation: Operation::Stop,
                reference: CredentialRef(String::new()),
                state: Arc::new(AtomicU8::new(ABANDONED)),
                reply,
                deadline: Instant::now(),
            });
        }
    }
}
impl Drop for Vault {
    fn drop(&mut self) {
        self.begin_shutdown();
    }
}

struct AbandonOnDrop(Arc<AtomicU8>);
impl Drop for AbandonOnDrop {
    fn drop(&mut self) {
        let _ = self
            .0
            .compare_exchange(PENDING, ABANDONED, Ordering::SeqCst, Ordering::SeqCst);
    }
}
fn interrupted(state: &AtomicU8, mutation: bool, reason: VaultReason) -> VaultError {
    let previous = state.compare_exchange(PENDING, ABANDONED, Ordering::SeqCst, Ordering::SeqCst);
    let not_started = matches!(previous, Ok(_) | Err(ABANDONED));
    VaultError {
        reason,
        certainty: if not_started {
            VaultCertainty::NotAttempted
        } else if mutation {
            VaultCertainty::Unknown
        } else {
            VaultCertainty::Rejected
        },
    }
}

enum Operation {
    Set(SecretBytes),
    Get,
    Delete,
    Stop,
}
impl Operation {
    fn is_mutation(&self) -> bool {
        matches!(self, Self::Set(_) | Self::Delete)
    }
}
enum Reply {
    Done,
    Secret(SecretBytes),
}
struct Job {
    operation: Operation,
    reference: CredentialRef,
    state: Arc<AtomicU8>,
    reply: oneshot::Sender<Result<Reply, VaultError>>,
    deadline: Instant,
}

/// Private injection seam: production only constructs the explicit OS backend.
trait Backend: Send + 'static {
    fn set(&mut self, reference: &CredentialRef, secret: &SecretBytes) -> Result<(), BackendError>;
    fn get(&mut self, reference: &CredentialRef) -> Result<SecretBytes, BackendError>;
    fn delete(&mut self, reference: &CredentialRef) -> Result<(), BackendError>;
}
#[derive(Clone, Copy)]
struct BackendError {
    reason: VaultReason,
    uncertain_mutation: bool,
}

fn worker(
    mut backend: impl Backend,
    mut receiver: mpsc::Receiver<Job>,
    state: Arc<AtomicU8>,
    done: Arc<Notify>,
) {
    let mut exit = WorkerExit {
        state: state.clone(),
        done: done.clone(),
        normal: false,
    };
    // Catch backend panics without exposing their payload. Rust's process panic
    // hook is outside this module; production backends must not panic with secrets.
    while state.load(Ordering::SeqCst) == RUNNING {
        let Some(job) = receiver.blocking_recv() else {
            break;
        };
        if state.load(Ordering::SeqCst) != RUNNING {
            reject_job(job, VaultReason::ShuttingDown);
            break;
        }
        if Instant::now() >= job.deadline {
            reject_job(job, VaultReason::Timeout);
            continue;
        }
        if job.reply.is_closed()
            || job
                .state
                .compare_exchange(PENDING, STARTED, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            reject_job(job, VaultReason::Cancelled);
            continue;
        }
        let mutation = job.operation.is_mutation();
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match job.operation {
                Operation::Set(secret) => {
                    backend.set(&job.reference, &secret).map(|()| Reply::Done)
                }
                Operation::Get => backend.get(&job.reference).map(Reply::Secret),
                Operation::Delete => backend.delete(&job.reference).map(|()| Reply::Done),
                Operation::Stop => Ok(Reply::Done),
            }));
        let failed = result.is_err();
        let result = match result {
            Ok(result) => result.map_err(|error| VaultError {
                reason: error.reason,
                certainty: if mutation && error.uncertain_mutation {
                    VaultCertainty::Unknown
                } else {
                    VaultCertainty::Rejected
                },
            }),
            Err(_) => Err(VaultError {
                reason: VaultReason::WorkerFailed,
                certainty: if mutation {
                    VaultCertainty::Unknown
                } else {
                    VaultCertainty::Rejected
                },
            }),
        };
        job.state.store(COMPLETE, Ordering::SeqCst);
        let _ = job.reply.send(result);
        if failed {
            state.store(FAILED, Ordering::SeqCst);
            break;
        }
    }
    receiver.close();
    while let Ok(job) = receiver.try_recv() {
        reject_job(
            job,
            if state.load(Ordering::SeqCst) == FAILED {
                VaultReason::WorkerFailed
            } else {
                VaultReason::ShuttingDown
            },
        );
    }
    if state.load(Ordering::SeqCst) != FAILED {
        state.store(STOPPED, Ordering::SeqCst);
    }
    exit.normal = true;
    done.notify_waiters();
}

struct WorkerExit {
    state: Arc<AtomicU8>,
    done: Arc<Notify>,
    normal: bool,
}
impl Drop for WorkerExit {
    fn drop(&mut self) {
        if !self.normal {
            self.state.store(FAILED, Ordering::SeqCst);
            self.done.notify_waiters();
        }
    }
}

fn reject_job(job: Job, reason: VaultReason) {
    job.state.store(ABANDONED, Ordering::SeqCst);
    let _ = job.reply.send(Err(VaultError::not_attempted(reason)));
}
