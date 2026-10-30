use super::*;
use crate::capabilities::mcp::{Limits, ProtocolMode};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("mcp-process-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn control() -> Control<'static> {
    Control {
        deadline: Instant::now() + Duration::from_secs(5),
        cancelled: &|| false,
    }
}
fn cancelled() -> Control<'static> {
    Control {
        cancelled: &|| true,
        ..control()
    }
}
async fn process(root: &Root, script: &str) -> Process {
    Process::spawn(
        &ServerConfig {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), script.into()],
            env: BTreeMap::new(),
            mode: ProtocolMode::Modern,
            limits: Limits::default(),
        },
        &root.0,
        control(),
    )
    .await
    .unwrap()
}

// Independent kernel check, not Tokio's cached child ID. On Darwin a zombie
// may be excluded from killpg(0), so use the actual waitable child identity.
fn waitable_exit(pid: u32) -> Option<i32> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result == 0 && unsafe { info.si_pid() } == pid as libc::pid_t {
        Some(unsafe { info.si_status() })
    } else {
        None
    }
}

#[tokio::test]
async fn cooperative_exit_stays_kernel_waitable_through_final_group_signal() {
    let root = Root::new();
    let mut process = process(&root, "exit 7").await;
    let pid = process.child.id().unwrap();
    assert_eq!(
        crate::provider::process_cleanup::observe_exit(&mut process.child)
            .await
            .unwrap()
            .code(),
        Some(7)
    );
    let observations = Arc::new(Mutex::new(Vec::new()));
    let record = observations.clone();
    process.signal_hook = Some(Box::new(move |group, signum| {
        let observed = waitable_exit(group);
        record.lock().unwrap().push((group, signum, observed));
        // Never issue a real signal if a regression already released identity.
        if observed == Some(7) {
            signal(group, signum);
        }
    }));
    assert_eq!(
        process.close(control()).await,
        CleanupStatus::ProcessGroupStopped
    );
    assert_eq!(
        *observations.lock().unwrap(),
        vec![(pid, libc::SIGTERM, Some(7)), (pid, libc::SIGKILL, Some(7))]
    );
    assert!(process.child.id().is_none());
    assert!(process.signal_authority.is_none());
    assert_eq!(process.child.wait().await.unwrap().code(), Some(7));
}

#[tokio::test]
async fn already_reaped_root_never_signals_its_saved_group_in_close_or_drop() {
    for close_first in [false, true] {
        let root = Root::new();
        let mut process = process(&root, "exit 0").await;
        let pid = process.child.id();
        process.child.wait().await.unwrap();
        assert!(process.child.id().is_none());
        assert_eq!(process.signal_authority, pid);
        let signals = Arc::new(Mutex::new(Vec::new()));
        let record = signals.clone();
        // Suppress all real signals: this deliberately models an already stale
        // saved number without ever risking an unrelated process group.
        process.signal_hook = Some(Box::new(move |group, signum| {
            record.lock().unwrap().push((group, signum));
        }));
        if close_first {
            let _ = process.close(control()).await;
        }
        drop(process);
        assert!(signals.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn mismatched_saved_group_is_not_signal_authority() {
    let root = Root::new();
    let mut process = process(&root, "exec /bin/sleep 30").await;
    let pid = process.child.id().unwrap();
    let signals = Arc::new(Mutex::new(Vec::new()));
    let record = signals.clone();
    process.signal_hook = Some(Box::new(move |group, signum| {
        record.lock().unwrap().push((group, signum));
    }));
    process.group = Some(pid + 1);
    process.signal_owned_group(libc::SIGTERM, false);
    process.signal_owned_group(libc::SIGKILL, true);
    assert!(signals.lock().unwrap().is_empty());
    assert!(process.signal_authority.is_none());
    // Clean only our still-owned root handle, never the fabricated group.
    process.child.start_kill().unwrap();
    process.child.wait().await.unwrap();
}

#[tokio::test]
async fn cancelled_close_after_final_signal_cannot_rearm_group_kill() {
    let root = Root::new();
    let mut process = process(&root, "exec /bin/sleep 30").await;
    let signals = Arc::new(Mutex::new(Vec::new()));
    let record = signals.clone();
    let final_signal = Arc::new(tokio::sync::Notify::new());
    let reached = final_signal.clone();
    process.signal_hook = Some(Box::new(move |_, signum| {
        record.lock().unwrap().push(signum);
        // Intentionally suppress delivery to hold the real wait pending. This
        // tests cancellation authority, not successful process-tree cleanup.
        if signum == libc::SIGKILL {
            reached.notify_one();
        }
    }));
    {
        let close = process.close(cancelled());
        tokio::pin!(close);
        tokio::select! {
            _ = final_signal.notified() => {},
            _ = &mut close => panic!("fixture must pause at the unreaped root wait"),
        }
    }
    let pid = process.child.id().unwrap();
    assert!(process.signal_authority.is_none());
    // Drop during the still-pending real wait, not after manually reaping it.
    // Only the original Child's root kill/reaper remains; no numeric group
    // signal authority may be reconstructed by this cancellation path.
    drop(process);
    timeout(Duration::from_secs(3), async {
        while group_exists(pid) != Some(false) {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(*signals.lock().unwrap(), vec![libc::SIGTERM, libc::SIGKILL]);
}

#[tokio::test]
async fn cancelled_close_before_final_signal_retains_original_cleanup_authority() {
    let root = Root::new();
    let mut process = process(&root, "exec /bin/sleep 30").await;
    let pid = process.child.id();
    let term = Arc::new(tokio::sync::Notify::new());
    let reached = term.clone();
    process.signal_hook = Some(Box::new(move |_, signum| {
        if signum == libc::SIGTERM {
            reached.notify_one();
        }
    }));
    {
        let close = process.close(control());
        tokio::pin!(close);
        tokio::select! {
            _ = term.notified() => {},
            _ = &mut close => panic!("fixture must pause during TERM observation"),
        }
    }
    assert_eq!(process.child.id(), pid);
    assert_eq!(process.signal_authority, pid);
    process.signal_hook = None;
    assert_eq!(
        process.close(cancelled()).await,
        CleanupStatus::ProcessGroupStopped
    );
    assert!(process.child.id().is_none());
    assert!(process.signal_authority.is_none());
}

#[tokio::test]
async fn drop_only_signals_the_still_owned_root_group_once_without_claiming_cleanup() {
    let root = Root::new();
    let mut process = process(&root, "exec /bin/sleep 30").await;
    let pid = process.child.id().unwrap();
    let signals = Arc::new(Mutex::new(Vec::new()));
    let record = signals.clone();
    process.signal_hook = Some(Box::new(move |group, signum| {
        record.lock().unwrap().push((group, signum));
        signal(group, signum);
    }));
    drop(process);
    // Tokio owns reaping its dropped child. No public cleanup result was
    // fabricated by Drop; this is solely a bounded local fixture leak check.
    timeout(Duration::from_secs(3), async {
        while group_exists(pid) != Some(false) {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(*signals.lock().unwrap(), vec![(pid, libc::SIGKILL)]);
}
