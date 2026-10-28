//! Process ownership, not a sandbox. Unix process groups cannot contain a
//! malicious child that deliberately detaches with setsid or equivalent.
#[cfg(not(windows))]
use super::{Control, McpError, ServerConfig};
#[cfg(unix)]
use cap_fs_ext::DirExt;
#[cfg(not(windows))]
use std::{path::Path, time::Duration};
#[cfg(unix)]
use std::{
    path::{Component, PathBuf},
    process::Stdio,
};
#[cfg(not(windows))]
use tokio::{process::Child, time::timeout};

#[cfg(windows)]
#[path = "windows.rs"]
mod windows;
#[cfg(windows)]
pub(super) use windows::Process;
#[cfg(windows)]
pub(super) type PipeReader = tokio::net::windows::named_pipe::NamedPipeServer;
#[cfg(windows)]
pub(super) type PipeWriter = tokio::net::windows::named_pipe::NamedPipeServer;
#[cfg(not(windows))]
pub(super) type PipeReader = tokio::process::ChildStdout;
#[cfg(not(windows))]
pub(super) type PipeWriter = tokio::process::ChildStdin;

#[cfg(unix)]
use tokio::{
    process::Command,
    time::{sleep, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CleanupStatus {
    #[cfg_attr(not(unix), allow(dead_code))]
    ProcessGroupStopped,
    #[cfg_attr(not(windows), allow(dead_code))]
    JobStopped,
    Unknown,
    NotStarted,
}

#[cfg(not(windows))]
pub(super) struct Process {
    pub child: Child,
    #[cfg(unix)]
    group: Option<u32>,
    cleanup: Option<CleanupStatus>,
    _root: cap_std::fs::Dir,
}
#[cfg(not(windows))]
impl Process {
    #[cfg(not(unix))]
    pub async fn spawn(_: &ServerConfig, _: &Path, _: Control<'_>) -> Result<Self, McpError> {
        Err(McpError {
            reason: "此平台尚无经过验证的 MCP 进程树管理，未启动服务",
            cleanup: Some(CleanupStatus::NotStarted),
        })
    }

    #[cfg(unix)]
    pub async fn spawn(
        config: &ServerConfig,
        root: &Path,
        control: Control<'_>,
    ) -> Result<Self, McpError> {
        control.check()?;
        Self::spawn_pinned(config, root, || {})
    }

    #[cfg(all(test, unix))]
    pub fn spawn_with_root_hook(
        config: &ServerConfig,
        root: &Path,
        hook: impl FnOnce(),
    ) -> Result<Self, McpError> {
        Self::spawn_pinned(config, root, hook)
    }

    #[cfg(unix)]
    fn spawn_pinned(
        config: &ServerConfig,
        root: &Path,
        before_spawn: impl FnOnce(),
    ) -> Result<Self, McpError> {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;
        let root_handle = root_nofollow(root)?;
        if !config.program.is_file() {
            return Err(McpError::new("MCP 可执行文件不可用"));
        }
        let mut command = Command::new(&config.program);
        command
            .args(&config.args)
            .env_clear()
            .envs(&config.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command.as_std_mut().process_group(0);
        let root_fd = root_handle.as_raw_fd();
        // The owned Dir outlives spawn, so its CLOEXEC fd remains valid until
        // exec. Do not reopen the ambient pathname after checking it. Only
        // async-signal-safe fchdir and a raw OS error value run after fork.
        unsafe {
            command.as_std_mut().pre_exec(move || {
                if libc::fchdir(root_fd) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
        before_spawn();
        let child = command
            .spawn()
            .map_err(|_| McpError::new("MCP 服务启动失败"))?;
        Ok(Self {
            group: child.id(),
            child,
            cleanup: None,
            _root: root_handle,
        })
    }

    pub async fn close(&mut self, control: Control<'_>) -> CleanupStatus {
        if let Some(status) = self.cleanup {
            return status;
        }
        #[cfg(unix)]
        if let Some(group) = self.group {
            // Allow cooperative EOF cleanup briefly, unless task cancellation
            // or its deadline already requires immediate termination.
            if !(control.cancelled)() && Instant::now() < control.deadline {
                let grace = control
                    .deadline
                    .min(Instant::now() + Duration::from_millis(100));
                tokio::select! { _ = self.child.wait() => {}, _ = tokio::time::sleep_until(grace) => {}, _ = async { while !(control.cancelled)() { sleep(Duration::from_millis(10)).await; } } => {} }
            }
            signal(group, libc::SIGTERM);
            let until = Instant::now() + Duration::from_millis(100);
            while Instant::now() < until && group_exists(group) != Some(false) {
                let _ = self.child.try_wait();
                if (control.cancelled)() {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
            signal(group, libc::SIGKILL);
            let reaped = timeout(Duration::from_millis(500), self.child.wait())
                .await
                .is_ok_and(|result| result.is_ok());
            let until = Instant::now() + Duration::from_millis(300);
            while Instant::now() < until && group_exists(group) != Some(false) {
                sleep(Duration::from_millis(10)).await;
            }
            let status = if reaped && group_exists(group) == Some(false) {
                CleanupStatus::ProcessGroupStopped
            } else {
                CleanupStatus::Unknown
            };
            self.cleanup = Some(status);
            return status;
        }
        let _ = control;
        let _ = self.child.start_kill();
        let _ = timeout(Duration::from_millis(500), self.child.wait()).await;
        self.cleanup = Some(CleanupStatus::Unknown);
        CleanupStatus::Unknown
    }
}
#[cfg(not(windows))]
impl Drop for Process {
    fn drop(&mut self) {
        if self.cleanup.is_none() {
            #[cfg(unix)]
            if let Some(group) = self.group {
                signal(group, libc::SIGKILL);
            }
            let _ = self.child.start_kill();
        }
    }
}

#[cfg(unix)]
fn signal(group: u32, signal: i32) {
    if group > 0 && group <= i32::MAX as u32 {
        unsafe {
            libc::kill(-(group as i32), signal);
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

/// Reject links component-by-component and retain a capability for Unix fchdir.
/// This pins only the child's initial cwd, not the trusted executable pathname,
/// later server filesystem access, network access or detached descendants.
#[cfg(unix)]
fn root_nofollow(root: &Path) -> Result<cap_std::fs::Dir, McpError> {
    if !root.is_absolute() {
        return Err(McpError::new("MCP 项目根目录必须为绝对路径"));
    }
    let mut anchor = PathBuf::new();
    let mut names = Vec::new();
    for component in root.components() {
        match component {
            Component::Prefix(prefix) => anchor.push(prefix.as_os_str()),
            Component::RootDir => anchor.push(component.as_os_str()),
            Component::Normal(name) => names.push(name),
            Component::CurDir => {}
            Component::ParentDir => return Err(McpError::new("MCP 项目根目录不能包含上级路径")),
        }
    }
    let mut dir = cap_std::fs::Dir::open_ambient_dir(anchor, cap_std::ambient_authority())
        .map_err(|_| McpError::new("MCP 项目文件系统不可访问"))?;
    for name in names {
        dir = dir
            .open_dir_nofollow(name)
            .map_err(|_| McpError::new("MCP 项目路径不可用或包含符号链接"))?;
    }
    Ok(dir)
}
