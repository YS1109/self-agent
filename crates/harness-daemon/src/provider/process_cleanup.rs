//! Cleanup ownership for the existing Native/Codex process launchers.
//! Unix groups do not contain a child deliberately escaping with setsid. Windows
//! uses suspended-root Job assignment before any child code runs. A root exit
//! alone never proves that its descendants stopped.
use anyhow::Result;
use std::fmt;
#[cfg(not(windows))]
use std::time::Duration;
#[cfg(not(windows))]
pub(crate) use tokio::process::Child;
#[cfg(windows)]
#[path = "process_cleanup/windows.rs"]
mod windows;
#[cfg(windows)]
pub(crate) use windows::{spawn, Child};
#[cfg(windows)]
pub(crate) type PipeReader = tokio::net::windows::named_pipe::NamedPipeServer;
#[cfg(windows)]
pub(crate) type PipeWriter = tokio::net::windows::named_pipe::NamedPipeServer;

#[derive(Debug)]
pub(crate) struct CleanupUnconfirmed;

impl fmt::Display for CleanupUnconfirmed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("进程树清理尚未确认；项目保持隔离，不自动重放")
    }
}
impl std::error::Error for CleanupUnconfirmed {}

#[cfg(not(windows))]
const OBSERVATION: Duration = Duration::from_secs(2);

#[cfg(not(windows))]
struct Report<'a, F: Fn() -> Result<()>> {
    unknown: bool,
    notify: &'a F,
}
#[cfg(not(windows))]
impl<F: Fn() -> Result<()>> Report<'_, F> {
    fn uncertain(&mut self) {
        if !self.unknown {
            self.unknown = true;
            // Notification failure must never abandon the actual child wait.
            // The typed final error independently preserves the uncertainty.
            let _ = (self.notify)();
        }
    }
}

/// Observe a deadline without dropping the original wait or its child handle.
/// An OS wait error is not proof of termination: keep the original custodian
/// alive and retry only the wait, never the command or its side effects.
#[cfg(not(windows))]
async fn reap(
    child: &mut Child,
    report: &mut Report<'_, impl Fn() -> Result<()>>,
) -> std::process::ExitStatus {
    let until = tokio::time::Instant::now() + OBSERVATION;
    loop {
        let result = if report.unknown {
            child.wait().await
        } else {
            match tokio::time::timeout_at(until, child.wait()).await {
                Ok(result) => result,
                Err(_) => {
                    report.uncertain();
                    continue;
                }
            }
        };
        match result {
            Ok(status) => return status,
            Err(_) => {
                report.uncertain();
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

#[cfg(unix)]
fn group_exists(group: u32) -> Option<bool> {
    if group == 0 || group > i32::MAX as u32 {
        return None;
    }
    if unsafe { libc::kill(-(group as i32), 0) } == 0 {
        Some(true)
    } else if std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        Some(false)
    } else {
        None
    }
}

/// Best-effort synchronous emergency signal only. No detached helper thread or
/// process is created by Drop; it is never evidence of confirmed cleanup.
pub(crate) fn emergency_group_kill(group: Option<u32>) {
    #[cfg(unix)]
    if let Some(group) = group.filter(|group| *group > 0 && *group <= i32::MAX as u32) {
        unsafe {
            libc::kill(-(group as i32), libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = group;
}

/// Observe exit without releasing Unix PID/PGID identity. Normal execution must
/// not reap the root before the final group signal: its numeric ID could then
/// be reused by an unrelated process group. Only stop() performs the real reap.
pub(crate) async fn observe_exit(child: &mut Child) -> std::io::Result<std::process::ExitStatus> {
    #[cfg(unix)]
    {
        let Some(pid) = child.id() else {
            return child.wait().await;
        };
        loop {
            if let Some(status) = observed_status(pid)? {
                return Ok(status);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    #[cfg(not(unix))]
    child.wait().await
}

#[cfg(unix)]
fn observed_status(pid: u32) -> std::io::Result<Option<std::process::ExitStatus>> {
    use std::os::unix::process::ExitStatusExt;
    // siginfo_t can contain raw pointers on some targets. Keep it wholly within
    // this synchronous frame, never across an await or registry transfer.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::EINTR) {
            Ok(None)
        } else {
            Err(error)
        };
    }
    if unsafe { info.si_pid() } == 0 {
        return Ok(None);
    }
    let status = unsafe { info.si_status() };
    let raw = match info.si_code {
        libc::CLD_EXITED => status << 8,
        libc::CLD_KILLED => status,
        libc::CLD_DUMPED => status | 0x80,
        _ => return Err(std::io::Error::other("进程退出观察无效")),
    };
    Ok(Some(std::process::ExitStatus::from_raw(raw)))
}

pub(crate) async fn stop(
    child: &mut Child,
    group: Option<u32>,
    cleanup_unknown: &impl Fn() -> Result<()>,
) -> Result<()> {
    #[cfg(windows)]
    {
        let _ = group;
        child.close(cleanup_unknown).await
    }
    #[cfg(not(windows))]
    {
        let mut report = Report {
            unknown: false,
            notify: cleanup_unknown,
        };
        #[cfg(unix)]
        // A caller that already reaped the root no longer owns numeric signal
        // authority. It may observe an absent group, but must not kill a reused ID.
        if child.id() == group && group.is_some() {
            emergency_group_kill(group);
        }

        // start_kill failure may simply race with a natural exit. Only the actual
        // wait plus tree observation decides cleanup, never this return value.
        let _ = child.start_kill();
        reap(child, &mut report).await;

        #[cfg(unix)]
        {
            let until = tokio::time::Instant::now() + OBSERVATION;
            while group.is_some_and(|group| group_exists(group) == Some(true))
                && tokio::time::Instant::now() < until
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            if group.is_none_or(|group| group_exists(group) != Some(false)) {
                report.uncertain();
            }
        }
        #[cfg(not(any(unix, windows)))]
        report.uncertain();

        if report.unknown {
            Err(CleanupUnconfirmed.into())
        } else {
            Ok(())
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn process(script: &str) -> Child {
        let mut command = tokio::process::Command::new("/bin/sh");
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
        command
            .args(["-c", script])
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    }

    #[test]
    fn exit_observation_future_can_be_owned_by_the_multithread_registry() {
        fn requires_send(_: impl Send) {}
        fn check(child: &mut Child) {
            requires_send(observe_exit(child));
        }
        let _ = check;
    }

    #[tokio::test]
    async fn live_root_is_reaped_and_its_group_is_confirmed_absent() {
        let mut child = process("exec sleep 30");
        let pid = child.id();
        stop(&mut child, pid, &|| {
            panic!("confirmed cleanup must not notify unknown")
        })
        .await
        .unwrap();
        assert!(child.try_wait().unwrap().is_some());
        assert_eq!(group_exists(pid.unwrap()), Some(false));
    }

    #[tokio::test]
    async fn already_exited_root_is_not_a_failed_cleanup_when_unix_group_is_absent() {
        let mut child = process("exit 7");
        let pid = child.id();
        assert_eq!(child.wait().await.unwrap().code(), Some(7));
        stop(&mut child, pid, &|| Ok(())).await.unwrap();
    }

    #[tokio::test]
    async fn observed_exit_keeps_root_identity_until_group_cleanup() {
        let mut child = process("exit 7");
        let pid = child.id();
        assert_eq!(observe_exit(&mut child).await.unwrap().code(), Some(7));
        assert_eq!(child.id(), pid, "normal observation must not reap");
        // Re-observe the actual kernel child, not merely Tokio's cached ID.
        // Darwin killpg1 filters SZOMB from the group and can return EPERM for
        // a zombie-only group. Thus kill(-pgid, 0) is not a portable proof that
        // our exited root is still waitable; production must keep EPERM unknown.
        // XNU: bsd/kern/kern_sig.c killpg1; bsd/kern/kern_exit.c waitid_nocancel.
        assert_eq!(
            observed_status(pid.unwrap()).unwrap().unwrap().code(),
            Some(7),
            "WNOWAIT must leave the same kernel child waitable"
        );
        stop(&mut child, pid, &|| panic!("owned root should settle"))
            .await
            .unwrap();
        assert!(child.id().is_none());
        assert_eq!(child.wait().await.unwrap().code(), Some(7));
        assert_eq!(group_exists(pid.unwrap()), Some(false));
    }

    #[tokio::test]
    async fn reaped_root_cannot_signal_another_saved_group_identity() {
        let mut completed = process("exit 0");
        completed.wait().await.unwrap();
        let mut unrelated = process("exec sleep 30");
        let other = unrelated.id();
        let error = stop(&mut completed, other, &|| Ok(())).await.unwrap_err();
        assert!(error.is::<CleanupUnconfirmed>());
        assert!(
            unrelated.try_wait().unwrap().is_none(),
            "stale ID must not be signalled"
        );
        stop(&mut unrelated, other, &|| Ok(())).await.unwrap();
    }

    #[tokio::test]
    async fn missing_group_identity_propagates_safe_error_after_actual_root_wait() {
        let mut child = process("exec sleep 30");
        let notifications = AtomicUsize::new(0);
        let error = stop(&mut child, None, &|| {
            notifications.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("fixture notification failure with private detail")
        })
        .await
        .unwrap_err();
        assert!(error.is::<CleanupUnconfirmed>());
        assert!(!error.to_string().contains("private"));
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
        assert!(child.try_wait().unwrap().is_some());
    }

    #[tokio::test]
    async fn observation_timeout_does_not_drop_the_original_child_wait() {
        let mut child = process("exec sleep 3");
        let pid = child.id().unwrap();
        let notified = tokio::sync::Notify::new();
        let notify = || {
            notified.notify_one();
            Ok(())
        };
        let mut report = Report {
            unknown: false,
            notify: &notify,
        };
        let wait = reap(&mut child, &mut report);
        tokio::pin!(wait);
        tokio::select! {
            _ = notified.notified() => {},
            _ = &mut wait => panic!("root should still be owned at observation deadline"),
        }
        assert_eq!(group_exists(pid), Some(true));
        assert!(tokio::time::timeout(Duration::from_millis(30), &mut wait)
            .await
            .is_err());
        assert!(wait.await.success());
        assert_eq!(group_exists(pid), Some(false));
    }
}
