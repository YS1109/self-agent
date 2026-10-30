#[cfg(unix)]
use super::*;

#[cfg(unix)]
mod unix {
    use super::*;
    use std::sync::{atomic::AtomicBool, Mutex};

    struct Project(PathBuf);
    impl Project {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("mcp-fixture-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }
        fn text(&self, path: &str) -> String {
            std::fs::read_to_string(self.0.join(path)).unwrap()
        }
    }
    impl Drop for Project {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn config(mode: ProtocolMode, fixture_mode: &str) -> ServerConfig {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/mcp_fixture.py");
        // Standalone harness sets this compile-time path; the integrated crate
        // uses its repository fixture without runtime environment discovery.
        let script = option_env!("MCP_FIXTURE_PATH")
            .map(PathBuf::from)
            .unwrap_or(script);
        // Respect the explicit CI/developer Python toolchain. On macOS,
        // /usr/bin/python3 can be an Xcode launcher with cold-start behavior;
        // bypassing actions/setup-python makes bounded fixture startup flaky.
        let search = std::env::var_os("PATH").unwrap_or_default();
        let python = std::env::split_paths(&search)
            .filter(|directory| directory.is_absolute())
            .map(|directory| directory.join("python3"))
            .chain([
                PathBuf::from("/usr/bin/python3"),
                PathBuf::from("/usr/local/bin/python3"),
            ])
            .find(|path| path.is_file())
            .expect("MCP fixture requires an installed Python 3 interpreter")
            .canonicalize()
            .unwrap();
        ServerConfig {
            program: python,
            args: vec![
                "-I".into(),
                script.to_string_lossy().into_owned(),
                fixture_mode.into(),
            ],
            env: BTreeMap::new(),
            mode,
            limits: Limits {
                setup_timeout: Duration::from_secs(10),
                call_timeout: Duration::from_millis(250),
            },
        }
    }
    fn control() -> Control<'static> {
        Control {
            deadline: Instant::now() + Duration::from_secs(15),
            cancelled: &|| false,
        }
    }
    async fn ready(project: &Project, config: &ServerConfig) -> McpSession {
        let mut session = McpSession::open(config, &project.0, control())
            .await
            .unwrap_or_else(|error| panic!("fixture open failed: {}", error.reason));
        let discovery = session
            .discover(control())
            .await
            .unwrap_or_else(|error| panic!("fixture discovery failed: {}", error.reason));
        assert_eq!(discovery.protocol, config.mode);
        assert!(discovery.tools && discovery.metadata.is_object());
        let tools = session
            .list_tools(control())
            .await
            .unwrap_or_else(|error| panic!("fixture list failed: {}", error.reason));
        assert!(!tools.is_empty());
        assert_eq!(tools[0].description.as_deref(), Some("本地模拟工具"));
        assert!(tools[0].input_schema.is_object());
        assert!(tools[0].output_schema.is_none());
        assert!(tools[0].annotations.is_some());
        session
    }
    fn assert_dead(pid: &str) {
        let pid = pid.trim().parse::<i32>().unwrap();
        assert!(
            unsafe { libc::kill(pid, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH),
            "fixture process still exists"
        );
    }

    async fn wait_for_pid_record(path: &Path, pid: u32, timeout: Duration) -> bool {
        let expected = pid.to_string();
        tokio::time::timeout(timeout, async {
            // Creation is not publication: Python write_text opens/truncates
            // before writing. Match the live child identity, not mere existence
            // or a parseable prefix of a partially written PID.
            loop {
                if std::fs::read_to_string(path).is_ok_and(|value| value == expected) {
                    return;
                }
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .is_ok()
    }

    #[tokio::test]
    async fn pid_readiness_requires_complete_matching_child_identity() {
        let project = Project::new();
        let path = project.0.join("pid-publication");
        std::fs::write(&path, "").unwrap();
        let waiting = wait_for_pid_record(&path, 12345, Duration::from_secs(10));
        tokio::pin!(waiting);
        for value in ["", "123", "12346"] {
            std::fs::write(&path, value).unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(20), &mut waiting)
                    .await
                    .is_err()
            );
        }
        std::fs::write(&path, "12345").unwrap();
        assert!(waiting.await);
    }

    #[tokio::test]
    async fn precancelled_open_is_definitely_not_started() {
        let project = Project::new();
        let error = McpSession::open(
            &config(ProtocolMode::Modern, "success"),
            &project.0,
            Control {
                deadline: control().deadline,
                cancelled: &|| true,
            },
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.cleanup, Some(CleanupStatus::NotStarted));
        assert!(!project.0.join("mcp-pid").exists());
    }

    #[tokio::test]
    async fn both_explicit_protocol_eras_discover_list_and_call_without_fallback() {
        for mode in [ProtocolMode::Modern, ProtocolMode::Legacy] {
            let project = Project::new();
            let mut session = ready(&project, &config(mode, "multi-page")).await;
            let result = session
                .call_once("echo", &json!({"text":"中文结果"}), control())
                .await;
            assert!(
                matches!(result,CallOutcome::Completed(value) if value["content"][0]["text"]=="中文结果")
            );
            assert_eq!(project.text("mcp-calls"), "1");
            assert_eq!(
                session.close(control()).await,
                CleanupStatus::ProcessGroupStopped
            );
            assert_dead(&project.text("mcp-pid"));
            let trace = project.text("mcp-trace.jsonl");
            assert_eq!(trace.matches("tools/list").count(), 2);
            if mode == ProtocolMode::Modern {
                assert!(trace.contains("server/discover") && !trace.contains("initialize"));
            } else {
                assert!(
                    !trace.contains("server/discover")
                        && trace.contains("notifications/initialized")
                );
            }
        }
    }

    #[tokio::test]
    async fn explicit_errors_are_terminal_but_unknown_outcomes_never_retry() {
        for mode in [
            "tool-error",
            "rpc-error",
            "missing-result-type",
            "valid-media",
            "structured-scalar",
        ] {
            let project = Project::new();
            let mut session = ready(&project, &config(ProtocolMode::Modern, mode)).await;
            let result = session.call_once("echo", &json!({}), control()).await;
            if matches!(
                mode,
                "missing-result-type" | "valid-media" | "structured-scalar"
            ) {
                assert!(matches!(result, CallOutcome::Completed(_)));
            } else {
                assert!(matches!(result,CallOutcome::ToolError(value) if value.is_object()));
            }
            assert_eq!(project.text("mcp-calls"), "1");
            session.close(control()).await;
        }
        for mode in [
            "eof",
            "call-stall",
            "malformed",
            "oversize",
            "no-newline",
            "notifications",
            "total-large",
            "reverse-request",
            "mrtr",
            "task-result",
            "wrong-id",
            "both-result-error",
            "stderr-large",
            "bad-block-null",
            "bad-block-text",
            "bad-block-resource",
        ] {
            let project = Project::new();
            let mut session = ready(&project, &config(ProtocolMode::Modern, mode)).await;
            let outcome = session
                .call_once(
                    "echo",
                    &json!({"text":"private-argument-canary"}),
                    control(),
                )
                .await;
            match outcome {
                CallOutcome::Unknown { reason, cleanup } => {
                    assert!(!reason.contains("private-"));
                    assert_eq!(cleanup, CleanupStatus::ProcessGroupStopped, "{mode}");
                }
                _ => panic!("expected unknown for fixture {mode}"),
            }
            assert_eq!(project.text("mcp-calls"), "1", "{mode}");
            assert!(matches!(
                session.call_once("echo", &json!({}), control()).await,
                CallOutcome::NotSent { .. }
            ));
            assert_dead(&project.text("mcp-pid"));
        }
    }

    #[tokio::test]
    async fn pagination_schema_and_protocol_bounds_close_without_calling_tools() {
        for mode in [
            "repeat-cursor",
            "too-many-pages",
            "duplicate-tool",
            "too-many-tools",
            "schema-large",
        ] {
            let project = Project::new();
            let mut session =
                McpSession::open(&config(ProtocolMode::Modern, mode), &project.0, control())
                    .await
                    .ok()
                    .unwrap();
            session.discover(control()).await.ok().unwrap();
            let error = session.list_tools(control()).await.err().unwrap();
            assert!(error.cleanup.is_some());
            assert!(!project.0.join("mcp-calls").exists());
            assert_dead(&project.text("mcp-pid"));
        }
        for mode in [ProtocolMode::Modern, ProtocolMode::Legacy] {
            let project = Project::new();
            let mut session =
                McpSession::open(&config(mode, "wrong-version"), &project.0, control())
                    .await
                    .ok()
                    .unwrap();
            assert!(session.discover(control()).await.is_err());
            assert_eq!(project.text("mcp-trace.jsonl").lines().count(), 1);
        }
    }

    #[tokio::test]
    async fn cancellation_before_send_is_not_sent_and_after_send_is_unknown_and_reaped() {
        let project = Project::new();
        let mut session = ready(&project, &config(ProtocolMode::Modern, "call-stall")).await;
        let cancelled = AtomicBool::new(true);
        let check = || cancelled.load(Ordering::SeqCst);
        let control = Control {
            deadline: Instant::now() + Duration::from_secs(5),
            cancelled: &check,
        };
        assert!(matches!(
            session.call_once("echo", &json!({}), control).await,
            CallOutcome::NotSent { .. }
        ));
        assert!(!project.0.join("mcp-calls").exists());
        cancelled.store(false, Ordering::SeqCst);
        let cancel = async {
            while !project.0.join("mcp-call-received").exists() {
                sleep(Duration::from_millis(5)).await;
            }
            cancelled.store(true, Ordering::SeqCst);
        };
        let arguments = json!({});
        let (outcome, ()) = tokio::join!(session.call_once("echo", &arguments, control), cancel);
        assert!(matches!(outcome, CallOutcome::Unknown { .. }));
        assert_eq!(project.text("mcp-calls"), "1");
        assert_dead(&project.text("mcp-pid"));
    }

    #[tokio::test]
    async fn launch_and_arguments_fail_before_send_with_no_inherited_environment() {
        let project = Project::new();
        for key in [
            "HOME",
            "CODEX_HOME",
            "HTTPS_PROXY",
            "LD_PRELOAD",
            "DYLD_INSERT_LIBRARIES",
            "PYTHONPATH",
            "NODE_OPTIONS",
            "ENV",
        ] {
            let mut cfg = config(ProtocolMode::Modern, "success");
            cfg.env.insert(key.into(), "private-canary".into());
            let error = McpSession::open(&cfg, &project.0, control())
                .await
                .err()
                .unwrap();
            assert!(!error.to_string().contains("private-canary"));
            assert!(!project.0.join("mcp-pid").exists());
        }
        let mut cfg = config(ProtocolMode::Modern, "env");
        cfg.env
            .insert("MCP_TOKEN".into(), "explicit-private-canary".into());
        let mut session = ready(&project, &cfg).await;
        match session.call_once("echo", &json!({}), control()).await {
            CallOutcome::Completed(value) => {
                let env = &value["structuredContent"];
                assert_eq!(env["explicit"], "explicit-private-canary");
                for key in ["host", "home", "proxy", "codex"] {
                    assert!(env[key].is_null());
                }
            }
            _ => panic!("expected private raw fixture result"),
        }
        assert!(matches!(
            session.call_once("unknown", &json!({}), control()).await,
            CallOutcome::NotSent { .. }
        ));
        assert!(matches!(
            session
                .call_once("echo", &json!({"x":"x".repeat(65536)}), control())
                .await,
            CallOutcome::NotSent { .. }
        ));
        assert_eq!(project.text("mcp-calls"), "1");
        session.close(control()).await;
    }

    #[tokio::test]
    async fn symlink_root_refused_and_descendant_process_group_cleanup_is_verified() {
        let project = Project::new();
        let outside = Project::new();
        let link = project.0.join("linked");
        std::os::unix::fs::symlink(&outside.0, &link).unwrap();
        assert!(
            McpSession::open(&config(ProtocolMode::Modern, "success"), &link, control())
                .await
                .is_err()
        );
        assert!(!outside.0.join("mcp-pid").exists());
        let mut cfg = config(ProtocolMode::Modern, "descendant");
        cfg.limits.call_timeout = Duration::from_millis(100);
        let mut session = ready(&project, &cfg).await;
        assert!(matches!(
            session.call_once("echo", &json!({}), control()).await,
            CallOutcome::Unknown {
                cleanup: CleanupStatus::ProcessGroupStopped,
                ..
            }
        ));
        assert_dead(&project.text("mcp-pid"));
        assert_dead(&project.text("mcp-child-pid"));
    }

    #[tokio::test]
    async fn pinned_cwd_survives_root_or_ancestor_replacement_before_spawn() {
        for ancestor in [false, true] {
            let project = Project::new();
            let outside = Project::new();
            let selected = project.0.join("ancestor/selected");
            std::fs::create_dir_all(&selected).unwrap();
            std::fs::create_dir_all(outside.0.join("selected")).unwrap();
            let moved = project.0.join("moved");
            let actual = if ancestor {
                moved.join("selected")
            } else {
                moved.clone()
            };
            let cfg = config(ProtocolMode::Modern, "success");
            let mut process = process::Process::spawn_with_root_hook(&cfg, &selected, || {
                if ancestor {
                    std::fs::rename(selected.parent().unwrap(), &moved).unwrap();
                    std::os::unix::fs::symlink(&outside.0, selected.parent().unwrap()).unwrap();
                } else {
                    std::fs::rename(&selected, &moved).unwrap();
                    std::os::unix::fs::symlink(&outside.0, &selected).unwrap();
                }
            })
            .ok()
            .unwrap();
            let pid = process.child.id().unwrap();
            let published =
                wait_for_pid_record(&actual.join("mcp-pid"), pid, cfg.limits.setup_timeout).await;
            process.child.stdin.take();
            let cleanup = process.close(control()).await;
            // Even a failed readiness observation must join the original child
            // before the fixture reports an assertion failure.
            assert!(published, "fixture did not publish its complete child PID");
            assert!(!outside.0.join("mcp-pid").exists());
            assert!(!outside.0.join("selected/mcp-pid").exists());
            assert_eq!(cleanup, CleanupStatus::ProcessGroupStopped);
            assert_dead(&pid.to_string());
        }
    }

    #[tokio::test]
    async fn secret_canaries_are_never_traced_even_with_trace_subscriber() {
        struct Capture(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Capture {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let logs = Arc::new(Mutex::new(Vec::new()));
        let captured = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || Capture(captured.clone()))
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let project = Project::new();
        let mut cfg = config(ProtocolMode::Modern, "secret-logs");
        cfg.env
            .insert("MCP_TOKEN".into(), "private-mcp-log-canary".into());
        let mut session = ready(&project, &cfg).await;
        assert!(
            matches!(session.call_once("echo",&json!({"text":"private-argument-canary"}),control()).await,CallOutcome::Completed(value) if value["content"][0]["text"]=="private-mcp-log-canary")
        );
        session.close(control()).await;
        let output = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        for needle in [
            "private-mcp-log-canary",
            "private-argument-canary",
            "private-server-info",
        ] {
            assert!(!output.contains(needle));
        }
    }
    #[tokio::test]
    async fn blocked_stdin_write_and_setup_wait_obey_task_deadline_and_cancel() {
        let project = Project::new();
        let mut cfg = config(ProtocolMode::Modern, "write-stall");
        cfg.limits.call_timeout = Duration::from_secs(30);
        let mut session = ready(&project, &cfg).await;
        let short = Control {
            deadline: Instant::now() + Duration::from_millis(100),
            cancelled: &|| false,
        };
        let started = Instant::now();
        assert!(matches!(
            session
                .call_once("echo", &json!({"text":"x".repeat(64000)}), short)
                .await,
            CallOutcome::Unknown { .. }
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(!project.0.join("mcp-calls").exists());
        assert_dead(&project.text("mcp-pid"));
        for cancel in [false, true] {
            let project = Project::new();
            let mut session = McpSession::open(
                &config(ProtocolMode::Modern, "discover-stall"),
                &project.0,
                control(),
            )
            .await
            .ok()
            .unwrap();
            let pid = session.process.child.id().unwrap().to_string();
            let start = Instant::now();
            let cancelled = || cancel && start.elapsed() > Duration::from_millis(100);
            let control = Control {
                deadline: Instant::now() + Duration::from_millis(200),
                cancelled: &cancelled,
            };
            let error = session.discover(control).await.err().unwrap();
            assert!(error.cleanup.is_some());
            assert!(start.elapsed() < Duration::from_secs(2));
            assert_dead(&pid);
        }
    }
}

#[cfg(windows)]
mod windows {
    use super::super::*;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::sync::Mutex;
    use windows_sys::Win32::{
        Foundation::{GetLastError, ERROR_INVALID_PARAMETER, WAIT_OBJECT_0},
        System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
    };

    struct Project(PathBuf);
    impl Project {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("mcp-windows-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }
        fn text(&self, file: &str) -> String {
            std::fs::read_to_string(self.0.join(file)).unwrap()
        }
    }
    impl Drop for Project {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn config(mode: ProtocolMode, fixture_mode: &str) -> ServerConfig {
        let python = std::env::split_paths(
            &std::env::var_os("PATH").expect("Windows fixture requires Python on PATH"),
        )
        .map(|p| p.join("python.exe"))
        .find(|p| p.is_file())
        .expect("Set up official Python before cargo test on Windows")
        .canonicalize()
        .unwrap();
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/mcp_fixture.py");
        let script = option_env!("MCP_FIXTURE_PATH")
            .map(PathBuf::from)
            .unwrap_or(script)
            .canonicalize()
            .unwrap();
        let mut env = BTreeMap::new();
        // Explicit, non-secret OS runtime requirement, not ambient inheritance.
        if let Ok(root) = std::env::var("SystemRoot") {
            env.insert("SystemRoot".into(), root);
        }
        ServerConfig {
            program: python,
            args: vec![
                "-I".into(),
                "-X".into(),
                "utf8".into(),
                script.to_string_lossy().into_owned(),
                fixture_mode.into(),
            ],
            env,
            mode,
            limits: Limits {
                setup_timeout: Duration::from_secs(5),
                call_timeout: Duration::from_millis(500),
            },
        }
    }
    fn control() -> Control<'static> {
        Control {
            deadline: Instant::now() + Duration::from_secs(10),
            cancelled: &|| false,
        }
    }
    async fn ready(project: &Project, cfg: &ServerConfig) -> McpSession {
        let mut session = McpSession::open(cfg, &project.0, control())
            .await
            .unwrap_or_else(|e| panic!("{}", e.reason));
        assert!(
            session
                .discover(control())
                .await
                .unwrap_or_else(|e| panic!("{}", e.reason))
                .tools
        );
        assert!(!session
            .list_tools(control())
            .await
            .unwrap_or_else(|e| panic!("{}", e.reason))
            .is_empty());
        session
    }
    fn process_handle(pid: u32) -> Option<OwnedHandle> {
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            assert_eq!(unsafe { GetLastError() }, ERROR_INVALID_PARAMETER);
            None
        } else {
            Some(unsafe { OwnedHandle::from_raw_handle(handle) })
        }
    }
    fn assert_dead(pid: u32) {
        if let Some(handle) = process_handle(pid) {
            assert_eq!(
                unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) },
                WAIT_OBJECT_0
            );
        }
    }

    #[tokio::test]
    async fn windows_dual_lifecycle_env_and_exact_argv() {
        for mode in [ProtocolMode::Modern, ProtocolMode::Legacy] {
            let project = Project::new();
            let mut cfg = config(mode, "env");
            cfg.env
                .insert("MCP_TOKEN".into(), "private-explicit-token".into());
            let mut session = ready(&project, &cfg).await;
            let CallOutcome::Completed(value) = session
                .call_once("echo", &json!({"text":"中文"}), control())
                .await
            else {
                panic!("expected completed");
            };
            assert_eq!(value["content"][0]["text"], "中文");
            assert_eq!(
                value["structuredContent"]["explicit"],
                "private-explicit-token"
            );
            for key in ["host", "home", "proxy", "codex"] {
                assert!(value["structuredContent"][key].is_null());
            }
            assert_eq!(session.close(control()).await, CleanupStatus::JobStopped);
            assert_dead(project.text("mcp-pid").parse().unwrap());
            let trace = project.text("mcp-trace.jsonl");
            assert_eq!(trace.matches("tools/call").count(), 1);
            assert_eq!(
                trace.contains("server/discover"),
                mode == ProtocolMode::Modern
            );
        }
        let project = Project::new();
        let mut cfg = config(ProtocolMode::Modern, "argv");
        let args = vec![
            "",
            "空 格",
            "quote\" inside",
            "tail\\",
            "slash\\\"quoted",
            "&|%NOT_EXPANDED%",
        ];
        cfg.args.extend(args.iter().map(|s| s.to_string()));
        let mut session = ready(&project, &cfg).await;
        let CallOutcome::Completed(value) = session.call_once("echo", &json!({}), control()).await
        else {
            panic!("argv fixture failed");
        };
        assert_eq!(value["structuredContent"]["args"], json!(args));
        assert_eq!(session.close(control()).await, CleanupStatus::JobStopped);
    }

    #[tokio::test]
    async fn windows_unknown_outcomes_are_never_retried_and_job_is_stopped() {
        for mode in [
            "eof",
            "call-stall",
            "malformed",
            "oversize",
            "no-newline",
            "notifications",
            "total-large",
            "reverse-request",
            "mrtr",
            "task-result",
            "wrong-id",
            "both-result-error",
            "stderr-large",
            "bad-block-null",
            "bad-block-text",
            "bad-block-resource",
        ] {
            let project = Project::new();
            let mut session = ready(&project, &config(ProtocolMode::Modern, mode)).await;
            assert!(
                matches!(
                    session.call_once("echo", &json!({}), control()).await,
                    CallOutcome::Unknown {
                        cleanup: CleanupStatus::JobStopped,
                        ..
                    }
                ),
                "{mode}"
            );
            assert_eq!(project.text("mcp-calls"), "1");
            assert!(matches!(
                session.call_once("echo", &json!({}), control()).await,
                CallOutcome::NotSent { .. }
            ));
            assert_dead(project.text("mcp-pid").parse().unwrap());
        }
    }

    #[tokio::test]
    async fn windows_cancellation_deadline_and_blocked_stdin_clean_entire_job() {
        for mode in ["descendant", "write-stall"] {
            let project = Project::new();
            let mut cfg = config(ProtocolMode::Modern, mode);
            cfg.limits.call_timeout = Duration::from_secs(30);
            let mut session = ready(&project, &cfg).await;
            let root = session.process.child.id().unwrap();
            let child = (mode == "descendant")
                .then(|| project.text("mcp-child-pid").parse::<u32>().unwrap());
            let child_handle = child.and_then(process_handle);
            let start = Instant::now();
            let cancelled = || start.elapsed() > Duration::from_millis(100);
            let ctl = Control {
                deadline: Instant::now() + Duration::from_millis(250),
                cancelled: if mode == "descendant" {
                    &cancelled
                } else {
                    &|| false
                },
            };
            assert!(matches!(
                session
                    .call_once("echo", &json!({"text":"x".repeat(64000)}), ctl)
                    .await,
                CallOutcome::Unknown {
                    cleanup: CleanupStatus::JobStopped,
                    ..
                }
            ));
            assert!(start.elapsed() < Duration::from_secs(3));
            assert_dead(root);
            if let Some(handle) = child_handle {
                assert_eq!(
                    unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) },
                    WAIT_OBJECT_0
                );
            }
            if mode == "write-stall" {
                assert!(!project.0.join("mcp-calls").exists());
            }
        }
        let project = Project::new();
        let mut session = McpSession::open(
            &config(ProtocolMode::Modern, "discover-stall"),
            &project.0,
            control(),
        )
        .await
        .ok()
        .unwrap();
        let root = session.process.child.id().unwrap();
        let error = session
            .discover(Control {
                deadline: Instant::now() + Duration::from_millis(100),
                cancelled: &|| false,
            })
            .await
            .err()
            .unwrap();
        assert_eq!(error.cleanup, Some(CleanupStatus::JobStopped));
        assert_dead(root);
        let project = Project::new();
        let error = McpSession::open(
            &config(ProtocolMode::Modern, "success"),
            &project.0,
            Control {
                deadline: control().deadline,
                cancelled: &|| true,
            },
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.cleanup, Some(CleanupStatus::NotStarted));
        assert!(!project.0.join("mcp-pid").exists());
    }

    #[tokio::test]
    async fn windows_suspended_job_failures_never_run_fixture_code() {
        for assign_failure in [true, false] {
            let project = Project::new();
            let error = process::Process::spawn_with_fault(
                &config(ProtocolMode::Modern, "success"),
                &project.0,
                control(),
                assign_failure,
            )
            .await
            .err()
            .unwrap();
            assert_eq!(error.cleanup, Some(CleanupStatus::JobStopped));
            assert!(!project.0.join("mcp-pid").exists());
        }
    }

    #[tokio::test]
    async fn windows_directory_locks_prevent_root_and_ancestor_replacement() {
        let project = Project::new();
        let ancestor = project.0.join("ancestor");
        let root = ancestor.join("selected");
        std::fs::create_dir_all(&root).unwrap();
        let mut process = process::Process::spawn_with_root_hook(
            &config(ProtocolMode::Modern, "success"),
            &root,
            control(),
            || {
                assert!(std::fs::rename(&root, ancestor.join("moved-root")).is_err());
                assert!(std::fs::rename(&ancestor, project.0.join("moved-ancestor")).is_err());
            },
        )
        .await
        .ok()
        .unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while !root.join("mcp-pid").exists() && Instant::now() < until {
            sleep(Duration::from_millis(10)).await;
        }
        assert!(root.join("mcp-pid").exists());
        assert_eq!(process.close(control()).await, CleanupStatus::JobStopped);
    }

    #[tokio::test]
    async fn windows_secret_canaries_are_never_traced_even_with_trace_subscriber() {
        struct Capture(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Capture {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let logs = Arc::new(Mutex::new(Vec::new()));
        let captured = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || Capture(captured.clone()))
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let project = Project::new();
        let mut cfg = config(ProtocolMode::Modern, "secret-logs");
        cfg.env
            .insert("MCP_TOKEN".into(), "private-mcp-log-canary".into());
        let mut session = ready(&project, &cfg).await;
        assert!(
            matches!(session.call_once("echo",&json!({"text":"private-argument-canary"}),control()).await,CallOutcome::Completed(value) if value["content"][0]["text"]=="private-mcp-log-canary")
        );
        session.close(control()).await;
        let output = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        for needle in [
            "private-mcp-log-canary",
            "private-argument-canary",
            "private-server-info",
        ] {
            assert!(!output.contains(needle));
        }
    }
    #[tokio::test]
    async fn windows_catalog_bounds_and_environment_collisions_fail_closed() {
        for mode in [
            "repeat-cursor",
            "too-many-pages",
            "duplicate-tool",
            "too-many-tools",
            "schema-large",
        ] {
            let project = Project::new();
            let mut session =
                McpSession::open(&config(ProtocolMode::Modern, mode), &project.0, control())
                    .await
                    .ok()
                    .unwrap();
            session.discover(control()).await.ok().unwrap();
            let error = session.list_tools(control()).await.err().unwrap();
            assert_eq!(error.cleanup, Some(CleanupStatus::JobStopped));
            assert!(!project.0.join("mcp-calls").exists());
            assert_dead(project.text("mcp-pid").parse().unwrap());
        }
        let project = Project::new();
        let mut cfg = config(ProtocolMode::Modern, "success");
        cfg.env.insert("MCP_TOKEN".into(), "one".into());
        cfg.env.insert("mcp_token".into(), "two".into());
        assert!(McpSession::open(&cfg, &project.0, control()).await.is_err());
        assert!(!project.0.join("mcp-pid").exists());
    }

    // NTFS mount-point fixture: no privilege adjustment or elevation is attempted.
    // Only private directories are touched; unsupported filesystems fail this
    // gate rather than silently skipping, escalating, or invoking a shell.
    fn junction(link: &Path, target: &Path) {
        use std::{
            os::windows::ffi::OsStrExt,
            ptr::{null, null_mut},
        };
        use windows_sys::Win32::{
            Foundation::{GENERIC_WRITE, INVALID_HANDLE_VALUE},
            Storage::FileSystem::{
                CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
                OPEN_EXISTING,
            },
            System::{
                Ioctl::FSCTL_SET_REPARSE_POINT, SystemServices::IO_REPARSE_TAG_MOUNT_POINT,
                IO::DeviceIoControl,
            },
        };
        std::fs::create_dir(link).unwrap();
        let target = target.to_str().unwrap();
        let target = target.strip_prefix(r"\\?\").unwrap_or(target);
        let substitute: Vec<u16> = format!(r"\??\{target}").encode_utf16().collect();
        let print: Vec<u16> = target.encode_utf16().collect();
        let mut path = substitute.clone();
        path.push(0);
        path.extend(&print);
        path.push(0);
        let mut data = Vec::new();
        data.extend(IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
        data.extend(u16::try_from(8 + path.len() * 2).unwrap().to_le_bytes());
        data.extend(0u16.to_le_bytes());
        data.extend(0u16.to_le_bytes()); // SubstituteNameOffset
        data.extend(u16::try_from(substitute.len() * 2).unwrap().to_le_bytes());
        data.extend(
            u16::try_from((substitute.len() + 1) * 2)
                .unwrap()
                .to_le_bytes(),
        );
        data.extend(u16::try_from(print.len() * 2).unwrap().to_le_bytes());
        for value in path {
            data.extend(value.to_le_bytes());
        }
        let path: Vec<u16> = link.as_os_str().encode_wide().chain(Some(0)).collect();
        let raw = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_WRITE,
                0,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                null_mut(),
            )
        };
        assert_ne!(
            raw, INVALID_HANDLE_VALUE,
            "temporary junction handle failed"
        );
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut returned = 0;
        assert_ne!(
            unsafe {
                DeviceIoControl(
                    handle.as_raw_handle(),
                    FSCTL_SET_REPARSE_POINT,
                    data.as_ptr().cast(),
                    data.len() as u32,
                    null_mut(),
                    0,
                    &mut returned,
                    null_mut(),
                )
            },
            0,
            "temporary NTFS junction creation failed"
        );
    }

    #[tokio::test]
    async fn windows_junction_root_and_program_ancestors_are_rejected_before_spawn() {
        let project = Project::new();
        let outside = Project::new();
        let nested = outside.0.join("nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(outside.0.join("fake.exe"), b"not an executable").unwrap();
        let link = project.0.join("junction");
        junction(&link, &outside.0);
        let cfg = config(ProtocolMode::Modern, "success");
        for root in [&link, &link.join("nested")] {
            let error = McpSession::open(&cfg, root, control()).await.err().unwrap();
            assert_eq!(error.cleanup, Some(CleanupStatus::NotStarted));
            assert_eq!(error.reason, "MCP 项目路径不可用或包含重解析点");
        }
        let mut cfg = cfg;
        cfg.program = link.join("fake.exe");
        let error = McpSession::open(&cfg, &project.0, control())
            .await
            .err()
            .unwrap();
        assert_eq!(error.cleanup, Some(CleanupStatus::NotStarted));
        assert_eq!(error.reason, "MCP 项目路径不可用或包含重解析点");
        assert!(!outside.0.join("mcp-pid").exists());
        assert!(!nested.join("mcp-pid").exists());
        std::fs::remove_dir(&link).unwrap();
        assert_eq!(
            std::fs::read(outside.0.join("fake.exe")).unwrap(),
            b"not an executable"
        );
    }

    #[test]
    fn windows_open_future_remains_send_for_daemon_workers() {
        fn assert_send<T: Send>(_: T) {}
        let cfg = config(ProtocolMode::Modern, "success");
        let project = Project::new();
        assert_send(McpSession::open(&cfg, &project.0, control()));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn stderr_cancellation_waits_for_actual_task_drop_not_a_detached_timeout() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::time::Duration;
    struct Cleanup {
        entered: Arc<AtomicBool>,
        release: std::sync::mpsc::Receiver<()>,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            self.entered.store(true, Ordering::SeqCst);
            let _ = self.release.recv_timeout(Duration::from_secs(5));
        }
    }
    let entered = Arc::new(AtomicBool::new(false));
    let (release, blocked) = std::sync::mpsc::channel();
    let (ready, started) = tokio::sync::oneshot::channel();
    let cleanup = Cleanup {
        entered: entered.clone(),
        release: blocked,
    };
    let task = tokio::spawn(async move {
        let _cleanup = cleanup;
        let _ = ready.send(());
        std::future::pending::<()>().await;
    });
    started.await.unwrap();
    let mut closing = tokio::spawn(super::finish_stderr(Some(task)));
    tokio::time::timeout(Duration::from_secs(2), async {
        while !entered.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    // Former 100 ms timeout would drop the JoinHandle and falsely finish here.
    assert!(
        tokio::time::timeout(Duration::from_millis(180), &mut closing)
            .await
            .is_err()
    );
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), closing)
        .await
        .unwrap()
        .unwrap();
}
