//! One launch attempt, bounded readiness observation, no automatic replacement.
//! Timeout does not prove the child stopped; future observers only probe it.
use std::{
    future::Future,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::time::Instant;
pub(crate) const READY_BUDGET: Duration = Duration::from_secs(30);
#[derive(Default)]
pub(crate) struct Startup {
    attempt: Option<(Instant, Arc<AtomicBool>)>,
    observed_peer: bool,
}
impl Startup {
    /// Attaching to an existing verified peer also consumes automatic launch
    /// authority: a later owner shutdown must not cause replacement by this UI.
    pub(crate) fn observed_peer(&mut self) {
        self.observed_peer = true;
    }

    pub(crate) fn start_once(
        &mut self,
        launch: impl FnOnce() -> Result<Arc<AtomicBool>, String>,
    ) -> Result<bool, String> {
        if self.observed_peer {
            return Err("已连接过的后台当前不可用或已由本机所有者关闭；不会自动重启。请由所有者明确启动后台后再只读刷新。".into());
        }
        if self.attempt.is_some() {
            return Ok(false);
        }
        let began = Instant::now();
        match launch() {
            Ok(exited) => {
                self.attempt = Some((began, exited));
                Ok(true)
            }
            Err(_) => {
                self.attempt = Some((began, Arc::new(AtomicBool::new(true))));
                Err("后台进程启动失败；未自动重试，请检查安装文件和本机诊断。".into())
            }
        }
    }
    pub(crate) async fn observe<T, F: Future<Output = Result<Option<T>, String>>>(
        &self,
        mut probe: impl FnMut() -> F,
        budget: Duration,
        mut progress: impl FnMut(u64),
    ) -> Result<T, String> {
        let (began, exited) = self.attempt.as_ref().ok_or("尚未登记后台启动观察")?;
        let deadline = *began + budget;
        // A later observer can discover readiness after an earlier timeout, but
        // cannot reset the original waiting budget or launch another process.
        if Instant::now() >= deadline {
            return tokio::time::timeout(Duration::from_secs(2),probe()).await.map_err(|_|"后台只读就绪探测超时；未启动替代进程".to_string())??.ok_or_else(||"后台启动等待已超过30秒；进程可能仍在加载，未停止或重新启动。可只读刷新，或检查后台日志/数据目录与磁盘空间。".into());
        }
        loop {
            progress(began.elapsed().as_secs());
            match tokio::time::timeout_at(deadline, probe()).await {
                Ok(Ok(Some(value))) => return Ok(value),
                Ok(Err(error)) => return Err(error),
                Ok(Ok(None)) => {}
                Err(_) => break,
            }
            if exited.load(Ordering::Acquire) {
                return Err(
                    "后台进程已退出且未就绪；未自动重启，请检查安装文件、数据目录锁和后台日志。"
                        .into(),
                );
            }
            if Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep_until((Instant::now() + Duration::from_millis(100)).min(deadline))
                .await;
        }
        Err("后台启动等待已超过30秒；未停止或重新启动进程。可只读刷新以发现稍后就绪，或检查后台日志/数据目录与磁盘空间。".into())
    }
}
#[derive(Default)]
pub(crate) struct Observations(std::sync::Mutex<(u64, serde_json::Value)>);
impl Observations {
    pub(crate) fn record(&self, mut value: serde_json::Value) -> serde_json::Value {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.0 = state.0.saturating_add(1);
        value["revision"] = state.0.into();
        state.1 = value.clone();
        value
    }
    pub(crate) fn snapshot(&self) -> serde_json::Value {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .1
            .clone()
    }
}

pub(crate) fn result_event<T>(result: &Result<T, String>) -> serde_json::Value {
    match result {
        Ok(_) => serde_json::json!({"status":"connected"}),
        Err(message) => {
            serde_json::json!({"status":"startup_error","message":if message.len() <= 1024 {message.as_str()} else {"后台就绪未确认；未停止或替换进程，请检查本机诊断。"}})
        }
    }
}

/// This preflight is separately bounded; an unreadable/nonregular metadata
/// entry is uncertainty, never evidence that another daemon should be started.
pub(crate) async fn read_info(
    path: &std::path::Path,
) -> Result<Option<harness_core::DaemonInfo>, String> {
    use tokio::io::AsyncReadExt;
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut options = tokio::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
        #[cfg(windows)]
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
        let file = match options.open(path).await {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err("无法读取本机后台信息，未启动替代进程".into()),
        };
        let metadata = file
            .metadata()
            .await
            .map_err(|_| "无法确认本机后台信息类型".to_string())?;
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                // FILE_ATTRIBUTE_REPARSE_POINT
                return Err("本机后台信息是重解析点，未启动替代进程".into());
            }
        }
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err("本机后台信息不是普通文件，未启动替代进程".into());
        }
        let mut bytes = Vec::new();
        file.take(16 * 1024 + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| "无法读取本机后台信息，未启动替代进程".to_string())?;
        if bytes.len() > 16 * 1024 {
            return Err("后台信息超过限制，未启动替代进程".into());
        }
        let info: harness_core::DaemonInfo =
            serde_json::from_slice(&bytes).map_err(|_| "本机后台服务信息格式无效".to_string())?;
        crate::event_stream::validate_base_url(&info.base_url)?;
        if info.token.is_empty() {
            return Err("缺少本机后台服务凭据".into());
        }
        Ok(Some(info))
    })
    .await
    .map_err(|_| "本机后台信息读取超过2秒，未启动替代进程".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;
    fn process(root: &std::path::Path, delay: u64) -> Arc<AtomicBool> {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "startup::tests::fixture_child_process",
                "--ignored",
                "--nocapture",
            ])
            .env("SELF_AGENT_STARTUP_FIXTURE_ROOT", root)
            .env("SELF_AGENT_STARTUP_FIXTURE_MS", delay.to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let exited = Arc::new(AtomicBool::new(false));
        let marker = exited.clone();
        std::thread::spawn(move || {
            let _ = child.wait();
            marker.store(true, Ordering::Release);
        });
        exited
    }
    #[test]
    #[ignore = "Only an explicitly selected trusted test subprocess calls this fixture"]
    fn fixture_child_process() {
        let root =
            std::path::PathBuf::from(std::env::var_os("SELF_AGENT_STARTUP_FIXTURE_ROOT").unwrap());
        let ms = std::env::var("SELF_AGENT_STARTUP_FIXTURE_MS")
            .unwrap()
            .parse::<u64>()
            .unwrap();
        assert!(ms <= 8000);
        std::thread::sleep(Duration::from_millis(ms));
        std::fs::write(root.join("ready"), b"fixture-ready").unwrap();
    }
    #[tokio::test]
    async fn real_slow_child_exceeding_old_six_seconds_reports_progress_and_becomes_ready() {
        let root = std::env::temp_dir().join(format!(
            "self-agent-startup-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let mut state = Startup::default();
        assert!(state.start_once(|| Ok(process(&root, 6500))).unwrap());
        let mut progress = Vec::new();
        let ready = state
            .observe(
                || async { Ok(root.join("ready").is_file().then_some("ready")) },
                Duration::from_secs(15),
                |seconds| progress.push(seconds),
            )
            .await
            .unwrap();
        assert_eq!(ready, "ready");
        assert!(progress.iter().any(|n| *n >= 6));
        assert!(!state.start_once(|| panic!("second launch")).unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn deadline_keeps_original_attempt_and_later_readiness_never_relaunches() {
        let root = std::env::temp_dir().join(format!(
            "self-agent-startup-timeout-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let mut state = Startup::default();
        state.start_once(|| Ok(process(&root, 400))).unwrap();
        assert!(state
            .observe(
                || async { Ok::<Option<()>, String>(None) },
                Duration::from_millis(50),
                |_| {}
            )
            .await
            .unwrap_err()
            .contains("未停止"));
        assert!(!state
            .start_once(|| panic!("timeout must not relaunch"))
            .unwrap());
        tokio::time::timeout(Duration::from_secs(5), async {
            while !root.join("ready").is_file() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            state
                .observe(
                    || async { Ok(root.join("ready").is_file().then_some(7)) },
                    Duration::from_millis(50),
                    |_| {}
                )
                .await
                .unwrap(),
            7
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn observed_existing_peer_consumes_autostart_without_creating_an_attempt() {
        let mut state = Startup::default();
        state.observed_peer();
        for _ in 0..3 {
            assert!(state
                .start_once(|| panic!("owner shutdown must not relaunch"))
                .unwrap_err()
                .contains("不会自动重启"));
        }
        assert!(state.attempt.is_none());
        // No mutation or reset is needed to discover an explicitly restarted
        // peer: Bridge always performs its bounded read-only probe first.
        state.observed_peer();
        assert!(state.attempt.is_none());
    }

    #[tokio::test]
    async fn failed_launch_and_responding_incompatible_peer_never_trigger_replacement() {
        let mut state = Startup::default();
        assert!(state.start_once(|| Err("private-canary".into())).is_err());
        assert!(!state.start_once(|| panic!("retry")).unwrap());
        let result = state
            .observe(
                || async { Err::<Option<()>, String>("incompatible".into()) },
                READY_BUDGET,
                |_| {},
            )
            .await;
        assert_eq!(result.unwrap_err(), "incompatible");
    }
}

#[cfg(test)]
mod metadata_tests {
    use super::*;
    #[test]
    fn early_observation_is_retained_and_later_readiness_has_new_revision() {
        let state = Observations::default();
        assert!(state.snapshot().is_null());
        let failure = state.record(result_event(&Err::<(), String>("未启动替代进程".into())));
        assert_eq!(state.snapshot(), failure);
        assert_eq!(failure["revision"], 1);
        let ready = state.record(result_event(&Ok::<_, String>(())));
        assert_eq!(ready["revision"], 2);
        assert_eq!(state.snapshot(), ready);
    }
    #[test]
    fn every_early_failure_has_bounded_actionable_status_without_replacement() {
        for message in [
            "后台协议不兼容；未停止或替换后台",
            "后台进程启动失败；未自动重试",
        ] {
            let event = result_event(&Err::<(), String>(message.into()));
            assert_eq!(event["status"], "startup_error");
            assert_eq!(event["message"], message);
        }
        let event = result_event(&Err::<(), String>("x".repeat(1025)));
        assert!(event.to_string().len() < 1024);
        assert_eq!(
            result_event(&Ok::<_, String>(())),
            serde_json::json!({"status":"connected"})
        );
    }
    #[tokio::test]
    async fn metadata_rejects_nonregular_oversized_and_invalid_without_launch() {
        let root = std::env::temp_dir().join(format!("startup-meta-{}", harness_core::new_id()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("daemon.json");
        assert!(read_info(&path).await.unwrap().is_none());
        assert!(read_info(&root).await.is_err());
        for bytes in [vec![b' '; 16 * 1024 + 1], b"private-canary".to_vec()] {
            std::fs::write(&path, bytes).unwrap();
            let error = read_info(&path).await.unwrap_err();
            assert!(!error.contains("private-canary"));
        }
        std::fs::write(
            &path,
            serde_json::json!({"base_url":"http://127.0.0.1:1234","token":"fixture","pid":1})
                .to_string(),
        )
        .unwrap();
        assert_eq!(read_info(&path).await.unwrap().unwrap().pid, 1);
        #[cfg(unix)]
        {
            std::fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink(root.join("target"), &path).unwrap();
            // Dangling links are also uncertainty, not absence/offline evidence.
            assert!(read_info(&path).await.is_err());
            std::fs::remove_file(&path).unwrap();
            use std::os::unix::ffi::OsStrExt;
            let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            let started = Instant::now();
            assert!(read_info(&path).await.is_err());
            assert!(started.elapsed() < Duration::from_secs(1));
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
