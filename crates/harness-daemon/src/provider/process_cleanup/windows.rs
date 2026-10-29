//! Race-free Windows Native/Codex launch: suspended root -> non-breakaway Job
//! -> ResumeThread. Adapted from the independently owned MCP launcher; MCP is
//! unchanged. Job ownership is not a filesystem/network sandbox.
use super::CleanupUnconfirmed;
use anyhow::{bail, Context, Result};
use std::{
    collections::BTreeMap,
    ffi::{c_void, OsStr, OsString},
    fmt,
    mem::{size_of, zeroed},
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::ExitStatusExt,
    },
    path::{Component, Path, PathBuf, Prefix},
    process::ExitStatus,
    ptr::{null, null_mut},
    time::Duration,
};
use tokio::{
    net::windows::named_pipe::{NamedPipeServer, ServerOptions},
    time::{sleep, Instant},
};
use windows_sys::Win32::{
    Foundation::{
        GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Security::{
        Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG},
        SECURITY_ATTRIBUTES,
    },
    Storage::FileSystem::{
        CreateFileW, GetDriveTypeW, GetFileInformationByHandle, GetFinalPathNameByHandleW,
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    },
    System::{
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
            JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
            TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        },
        Pipes::GetNamedPipeClientProcessId,
        SystemInformation::GetSystemDirectoryW,
        Threading::{
            CreateProcessW, DeleteProcThreadAttributeList, GetCurrentProcessId, GetExitCodeProcess,
            InitializeProcThreadAttributeList, ResumeThread, TerminateProcess,
            UpdateProcThreadAttribute, WaitForSingleObject, CREATE_NO_WINDOW, CREATE_SUSPENDED,
            CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST,
            PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, STARTF_USESTDHANDLES,
            STARTUPINFOEXW,
        },
    },
};

#[derive(Debug)]
struct SpawnError {
    reason: &'static str,
}
impl SpawnError {
    fn new(reason: &'static str) -> Self {
        Self { reason }
    }
}
impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason)
    }
}
impl std::error::Error for SpawnError {}
struct Control<'a, F: Fn() -> bool> {
    deadline: Instant,
    cancelled: &'a F,
}
impl<F: Fn() -> bool> Copy for Control<'_, F> {}
impl<F: Fn() -> bool> Clone for Control<'_, F> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<F: Fn() -> bool> Control<'_, F> {
    fn check(&self) -> std::result::Result<(), SpawnError> {
        if (self.cancelled)() || Instant::now() >= self.deadline {
            Err(SpawnError::new("执行器启动已取消或超时"))
        } else {
            Ok(())
        }
    }
}

pub(crate) struct Child {
    pub stdin: Option<NamedPipeServer>,
    pub stdout: Option<NamedPipeServer>,
    pub stderr: Option<NamedPipeServer>,
    root: OwnedHandle,
    job: OwnedHandle,
    pid: u32,
    status: Option<ExitStatus>,
    _roots: Vec<OwnedHandle>,
}
impl Child {
    pub(crate) fn id(&self) -> Option<u32> {
        Some(self.pid)
    }
    pub(crate) fn start_kill(&mut self) -> std::io::Result<()> {
        let job_ok = unsafe { TerminateJobObject(self.job.as_raw_handle(), 1) } != 0;
        let root_ok = unsafe { TerminateProcess(self.root.as_raw_handle(), 1) } != 0;
        if job_ok || root_ok {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    pub(crate) fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        if self.status.is_some() {
            return Ok(self.status);
        }
        match unsafe { WaitForSingleObject(self.root.as_raw_handle(), 0) } {
            WAIT_OBJECT_0 => {
                let mut code = 0;
                if unsafe { GetExitCodeProcess(self.root.as_raw_handle(), &mut code) } == 0 {
                    return Err(std::io::Error::last_os_error());
                }
                self.status = Some(ExitStatus::from_raw(code));
                Ok(self.status)
            }
            WAIT_TIMEOUT => Ok(None),
            _ => Err(std::io::Error::last_os_error()),
        }
    }
    pub(crate) async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            sleep(Duration::from_millis(10)).await;
        }
    }
    pub(crate) async fn close(&mut self, notify: &impl Fn() -> Result<()>) -> Result<()> {
        self.stdin.take();
        let _ = self.start_kill();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut unknown = false;
        loop {
            if self.try_wait().is_ok_and(|value| value.is_some())
                && job_empty(&self.job) == Some(true)
            {
                return if unknown {
                    Err(CleanupUnconfirmed.into())
                } else {
                    Ok(())
                };
            }
            if !unknown && Instant::now() >= deadline {
                unknown = true;
                // Persist/quarantine errors cannot release the physical owner.
                let _ = notify();
            }
            // Keep root, Job, cwd and executable handles while observation is
            // uncertain. Never detach a waiter or drop this live custodian.
            sleep(Duration::from_millis(20)).await;
        }
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.start_kill();
    }
}

/// `stdio` explicitly describes the callsite; all unused ends are real NUL
/// handles, not unread pipes. Environment and arguments never become Debug,
/// serialized state or raw error text.
pub(crate) async fn spawn(
    command: &tokio::process::Command,
    stdio: [bool; 3],
    cancelled: &impl Fn() -> bool,
    cleanup_unknown: &impl Fn() -> Result<()>,
) -> Result<Child> {
    spawn_inner(command, stdio, cancelled, cleanup_unknown, Fault::None).await
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    None,
    #[cfg(test)]
    Assign,
    #[cfg(test)]
    Resume,
}
async fn spawn_inner(
    command: &tokio::process::Command,
    stdio: [bool; 3],
    cancelled: &impl Fn() -> bool,
    cleanup_unknown: &impl Fn() -> Result<()>,
    fault: Fault,
) -> Result<Child> {
    let control = Control {
        deadline: Instant::now() + Duration::from_secs(10),
        cancelled,
    };
    control.check()?;
    let command = command.as_std();
    let root = command
        .get_current_dir()
        .map(|path| Ok(path.to_path_buf()))
        .unwrap_or_else(std::env::current_dir)
        .context("执行器当前目录不可用")?;
    let (mut roots, mut root_wide) = lock_path(&root, true, control)?;
    let env = effective_environment(command)?;
    let (selected, selected_roots) = resolve_program(command.get_program(), &root, &env, control)?;
    roots.extend(selected_roots);
    let args: Vec<_> = command.get_args().map(OsStr::to_os_string).collect();
    let (program, mut line) = launch_line(&selected, &args)?;
    if selected
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat"))
    {
        // CMD's shell namespace is not the full extended-length Win32 namespace.
        // Convert only ordinary DOS names whose interpretation cannot change;
        // the existing ancestor handles continue to pin the exact same objects.
        root_wide = OsStr::new(&batch_path(&root)?)
            .encode_wide()
            .chain(Some(0))
            .collect();
    }
    let (program_roots, program_wide) = lock_path(&program, false, control)?;
    roots.extend(program_roots);
    let mut environment = environment(&env)?;
    let job = owned(unsafe { CreateJobObjectW(null(), null()) })?;
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    } == 0
    {
        bail!("执行器 Windows Job 安全策略不可用；未启动进程");
    }
    let (stdin, input_child) = stdio_end(false, stdio[0], control).await?;
    let (stdout, output_child) = stdio_end(true, stdio[1], control).await?;
    let (stderr, error_child) = stdio_end(true, stdio[2], control).await?;
    let (root_handle, thread, pid) = {
        let inherited = [
            input_child.as_raw_handle(),
            output_child.as_raw_handle(),
            error_child.as_raw_handle(),
        ];
        let attributes = Attributes::new(&inherited)?;
        let mut startup: STARTUPINFOEXW = unsafe { zeroed() };
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = inherited[0];
        startup.StartupInfo.hStdOutput = inherited[1];
        startup.StartupInfo.hStdError = inherited[2];
        startup.lpAttributeList = attributes.ptr();
        control.check()?;
        let mut info: PROCESS_INFORMATION = unsafe { zeroed() };
        if unsafe {
            CreateProcessW(
                program_wide.as_ptr(),
                line.as_mut_ptr(),
                null(),
                null(),
                1,
                CREATE_SUSPENDED
                    | CREATE_NO_WINDOW
                    | CREATE_UNICODE_ENVIRONMENT
                    | EXTENDED_STARTUPINFO_PRESENT,
                environment.as_mut_ptr().cast(),
                root_wide.as_ptr(),
                &startup.StartupInfo,
                &mut info,
            )
        } == 0
        {
            bail!("执行器 Windows 进程创建失败；未执行用户代码");
        }
        (
            unsafe { OwnedHandle::from_raw_handle(info.hProcess) },
            unsafe { OwnedHandle::from_raw_handle(info.hThread) },
            info.dwProcessId,
        )
    };
    let mut child = Child {
        stdin,
        stdout,
        stderr,
        root: root_handle,
        job,
        pid,
        status: None,
        _roots: roots,
    };
    #[cfg(test)]
    let assign_allowed = fault != Fault::Assign;
    #[cfg(not(test))]
    let assign_allowed = {
        let _ = fault;
        true
    };
    if !assign_allowed
        || unsafe {
            AssignProcessToJobObject(child.job.as_raw_handle(), child.root.as_raw_handle())
        } == 0
    {
        child.close(cleanup_unknown).await?;
        bail!("执行器 Windows Job 绑定失败；未恢复用户代码");
    }
    if control.check().is_err() {
        child.close(cleanup_unknown).await?;
        bail!("执行器启动已取消；挂起进程已回收");
    }
    #[cfg(test)]
    let resume_allowed = fault != Fault::Resume;
    #[cfg(not(test))]
    let resume_allowed = true;
    if !resume_allowed || unsafe { ResumeThread(thread.as_raw_handle()) } != 1 {
        child.close(cleanup_unknown).await?;
        bail!("执行器 Windows 恢复失败；进程已回收");
    }
    drop((input_child, output_child, error_child, thread));
    if control.check().is_err() {
        child.close(cleanup_unknown).await?;
        bail!("执行器启动已取消；进程已回收");
    }
    Ok(child)
}

fn effective_environment(command: &std::process::Command) -> Result<BTreeMap<String, OsString>> {
    // The Native/Codex callsites intentionally inherit the owner environment.
    // Apply their explicit env_remove/env overrides case-insensitively. MCP's
    // env_clear policy is separate and unchanged.
    let mut env = BTreeMap::new();
    for (name, value) in std::env::vars_os() {
        let name = name.to_str().context("执行器环境名称无法安全编码")?;
        if name.starts_with('=') {
            continue;
        } // Per-drive legacy cwd is not inherited.
        env.insert(name.to_ascii_uppercase(), value);
    }
    for (name, value) in command.get_envs() {
        let name = name
            .to_str()
            .context("执行器环境名称无法安全编码")?
            .to_ascii_uppercase();
        if let Some(value) = value {
            env.insert(name, value.to_os_string());
        } else {
            env.remove(&name);
        }
    }
    Ok(env)
}
fn resolve_program(
    program: &OsStr,
    root: &Path,
    env: &BTreeMap<String, OsString>,
    control: Control<'_, impl Fn() -> bool>,
) -> Result<(PathBuf, Vec<OwnedHandle>)> {
    let selected = Path::new(program);
    let bases = if selected.is_absolute() {
        vec![selected.to_path_buf()]
    } else if selected.components().count() > 1 {
        vec![root.join(selected)]
    } else {
        env.get("PATH")
            .map(|path| {
                std::env::split_paths(path)
                    .map(|dir| dir.join(selected))
                    .collect()
            })
            .unwrap_or_default()
    };
    for base in bases {
        let variants = if base.extension().is_some() {
            vec![base]
        } else {
            vec![
                base.with_extension("exe"),
                base.with_extension("cmd"),
                base.with_extension("bat"),
            ]
        };
        for candidate in variants {
            // Never touch UNC/mapped/removable/relative PATH entries to probe them.
            if local_path(&candidate).is_err() {
                continue;
            }
            control.check()?;
            // No ambient metadata probe: a fixed drive can contain a junction
            // into a remote namespace. The component walk rejects that entry
            // before accessing anything beneath it and retains selected locks.
            if let Ok((locks, _)) = lock_path(&candidate, false, control) {
                return Ok((candidate, locks));
            }
        }
    }
    bail!("执行器程序不可用；仅支持本地固定盘符 EXE 或受信任 CMD/BAT 包装")
}
fn launch_line(selected: &Path, args: &[OsString]) -> Result<(PathBuf, Vec<u16>)> {
    if selected
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
    {
        return Ok((selected.to_path_buf(), command_line(selected, args)?));
    }
    if !selected
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat"))
    {
        bail!("执行器扩展名不受支持；未绕过 Job 启动");
    }
    let script = batch_path(selected)?;
    if script
        .chars()
        .any(|ch| ch.is_control() || matches!(ch, '%' | '!' | '"'))
        || args.len() > 16
        || args.iter().any(|arg| {
            arg.to_str().is_none_or(|arg| {
                arg.is_empty()
                    || !arg
                        .bytes()
                        .all(|ch| ch.is_ascii_alphanumeric() || b"_-./:=+".contains(&ch))
            })
        })
    {
        bail!("执行器 CMD/BAT 包装参数无法安全引用；未启动");
    }
    let mut system = vec![0u16; 32768];
    let count = unsafe { GetSystemDirectoryW(system.as_mut_ptr(), system.len() as u32) } as usize;
    if count == 0 || count >= system.len() {
        bail!("无法确认系统命令解释器路径");
    }
    system.truncate(count);
    let interpreter = PathBuf::from(OsString::from_wide(&system)).join("cmd.exe");
    let mut line = Vec::new();
    quote_arg(&mut line, interpreter.as_os_str());
    line.extend(OsStr::new(" /d /v:off /s /c ").encode_wide());
    line.push(b'"' as u16);
    quote_arg(&mut line, OsStr::new(&script));
    for arg in args {
        line.push(b' ' as u16);
        quote_arg(&mut line, arg);
    }
    line.push(b'"' as u16);
    if line.len() >= 32767 || line.contains(&0) {
        bail!("执行器命令行超过安全限制");
    }
    line.push(0);
    Ok((interpreter, line))
}

/// The Windows shell and extended-length filesystem namespace have different
/// normalization rules. Never strip a prefix from a trailing-dot/space or DOS
/// device name and accidentally execute a different path.
/// https://learn.microsoft.com/windows/win32/fileio/naming-a-file
fn batch_path(path: &Path) -> Result<String> {
    let text = path.to_str().context("执行器包装路径无法安全编码")?;
    let text = text.strip_prefix(r"\\?\").unwrap_or(text);
    if !matches!(Path::new(text).components().next(), Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_)))
        || !Path::new(text).is_absolute()
        || text.encode_utf16().count() >= 260
    {
        bail!("CMD/BAT 包装仅支持有界的普通本地盘符路径");
    }
    for component in Path::new(text).components() {
        if let Component::Normal(name) = component {
            let name = name.to_str().context("执行器包装路径无法安全编码")?;
            let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
            let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$")
                || stem
                    .strip_prefix("COM")
                    .or_else(|| stem.strip_prefix("LPT"))
                    .is_some_and(|tail| {
                        matches!(
                            tail,
                            "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                        )
                    });
            if name.ends_with(['.', ' '])
                || name
                    .chars()
                    .any(|ch| ch.is_control() || matches!(ch, ':' | '<' | '>' | '|' | '?' | '*'))
                || reserved
            {
                bail!("CMD/BAT 包装路径存在不同命名空间解释；未启动");
            }
        } else if matches!(component, Component::ParentDir | Component::CurDir) {
            bail!("CMD/BAT 包装路径不能包含相对组件");
        }
    }
    Ok(text.to_owned())
}
async fn stdio_end(
    reads: bool,
    piped: bool,
    control: Control<'_, impl Fn() -> bool>,
) -> Result<(Option<NamedPipeServer>, OwnedHandle)> {
    if piped {
        let (parent, child) = pipe(reads, control).await?;
        return Ok((Some(parent), child));
    }
    let attrs = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 1,
    };
    let name: Vec<u16> = OsStr::new("NUL").encode_wide().chain(Some(0)).collect();
    Ok((
        None,
        owned(unsafe {
            CreateFileW(
                name.as_ptr(),
                if reads { GENERIC_WRITE } else { GENERIC_READ },
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                &attrs,
                OPEN_EXISTING,
                0,
                null_mut(),
            )
        })?,
    ))
}

fn owned(handle: HANDLE) -> Result<OwnedHandle, SpawnError> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        Err(SpawnError::new("执行器 Windows 进程资源不可用"))
    } else {
        Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
    }
}
fn job_empty(job: &OwnedHandle) -> Option<bool> {
    let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
    let ok = unsafe {
        QueryInformationJobObject(
            job.as_raw_handle(),
            JobObjectBasicAccountingInformation,
            (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
            size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            null_mut(),
        )
    };
    (ok != 0).then_some(info.ActiveProcesses == 0)
}

/// Local drive paths only: UNC/device namespaces could perform unbounded remote
/// filesystem access during synchronous OS spawn. Reject rather than fallback.
fn local_path(path: &Path) -> Result<Vec<u16>, SpawnError> {
    let drive = match path.components().next() {
        Some(Component::Prefix(prefix)) if path.is_absolute() => match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive,
            _ => return Err(SpawnError::new("Windows 执行器 仅支持本地固定盘符绝对路径")),
        },
        _ => return Err(SpawnError::new("Windows 执行器 仅支持本地固定盘符绝对路径")),
    };
    let drive_root = [drive as u16, b':' as u16, b'\\' as u16, 0];
    // DRIVE_FIXED=3 (WinBase.h). A drive-letter prefix does NOT prove locality:
    // mapped SMB, removable, unknown and other drive types fail closed before
    // opening any path. Never probe a remote path to see whether it exists.
    if !fixed_drive(unsafe { GetDriveTypeW(drive_root.as_ptr()) }) {
        return Err(SpawnError::new("Windows 执行器 拒绝网络、可移动或未知盘符"));
    }
    if path
        .components()
        .any(|p| matches!(p, Component::ParentDir | Component::CurDir))
    {
        return Err(SpawnError::new("Windows 执行器 路径不能包含相对组件"));
    }
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) || wide.len() > 32760 {
        return Err(SpawnError::new("Windows 执行器 路径无效"));
    }
    wide.push(0);
    Ok(wide)
}
fn fixed_drive(kind: u32) -> bool {
    kind == 3
}
fn lock_path(
    root: &Path,
    directory: bool,
    control: Control<'_, impl Fn() -> bool>,
) -> Result<(Vec<OwnedHandle>, Vec<u16>), SpawnError> {
    lock_path_observed(root, directory, control, &mut |_| {})
}
fn lock_path_observed(
    root: &Path,
    directory: bool,
    control: Control<'_, impl Fn() -> bool>,
    before_open: &mut impl FnMut(&Path),
) -> Result<(Vec<OwnedHandle>, Vec<u16>), SpawnError> {
    local_path(root)?;
    let mut path = PathBuf::new();
    let mut handles = Vec::new();
    let components: Vec<_> = root.components().collect();
    for (index, component) in components.iter().enumerate() {
        control.check()?;
        path.push(component.as_os_str());
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        let wide = local_path(&path)?;
        before_open(&path);
        let handle = owned(unsafe {
            CreateFileW(
                wide.as_ptr(),
                // Metadata-only FILE_READ_ATTRIBUTES handles do not participate
                // in Windows share-access checks. Real read access is required
                // for this handle's deny-write/delete flags to block renames.
                GENERIC_READ,
                FILE_SHARE_READ,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                null_mut(),
            )
        })?;
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { zeroed() };
        if unsafe { GetFileInformationByHandle(handle.as_raw_handle(), &mut info) } == 0
            || ((info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0)
                != (directory || index + 1 != components.len()))
            || info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        {
            return Err(SpawnError::new("执行器 项目路径不可用或包含重解析点"));
        }
        handles.push(handle);
    }
    if handles.is_empty() {
        return Err(SpawnError::new("执行器 项目根目录不可用"));
    }
    // Confirm the handle's resolved path remains a fixed local DOS volume.
    // Ancestor locks prevent junction/rename replacement during this lookup and
    // process creation. Device namespace changes by privileged OS actors are
    // outside the trust boundary, as are filesystem/network accesses by server.
    let mut final_path = vec![0u16; 32768];
    let count = unsafe {
        GetFinalPathNameByHandleW(
            handles.last().unwrap().as_raw_handle(),
            final_path.as_mut_ptr(),
            final_path.len() as u32,
            0,
        )
    } as usize;
    if count == 0 || count >= final_path.len() {
        return Err(SpawnError::new("执行器 Windows 路径身份无法确认"));
    }
    final_path.truncate(count);
    let canonical = PathBuf::from(OsString::from_wide(&final_path));
    let canonical = local_path(&canonical)?;
    Ok((handles, canonical))
}

async fn pipe(
    parent_reads: bool,
    control: Control<'_, impl Fn() -> bool>,
) -> Result<(NamedPipeServer, OwnedHandle), SpawnError> {
    control.check()?;
    let mut random = [0u8; 16];
    if unsafe {
        BCryptGenRandom(
            null_mut(),
            random.as_mut_ptr(),
            random.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    } < 0
    {
        return Err(SpawnError::new("执行器 安全管道随机源不可用"));
    }
    let name = format!(
        r"\\.\pipe\self-agent-executor-{:032x}",
        u128::from_le_bytes(random)
    );
    let server = ServerOptions::new()
        .first_pipe_instance(true)
        .max_instances(1)
        .access_inbound(parent_reads)
        .access_outbound(!parent_reads)
        .reject_remote_clients(true)
        .in_buffer_size(4096)
        .out_buffer_size(4096)
        .create(&name)
        .map_err(|_| SpawnError::new("执行器 私有异步管道创建失败"))?;
    let wide: Vec<u16> = OsStr::new(&name).encode_wide().chain(Some(0)).collect();
    let child = {
        let attrs = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: null_mut(),
            bInheritHandle: 1,
        };
        owned(unsafe {
            CreateFileW(
                wide.as_ptr(),
                if parent_reads {
                    GENERIC_WRITE
                } else {
                    GENERIC_READ
                },
                0,
                &attrs,
                OPEN_EXISTING,
                0,
                null_mut(),
            )
        })?
    };
    tokio::select! {
        result = server.connect() => { result.map_err(|_| SpawnError::new("执行器 私有异步管道连接失败"))?; },
        _ = tokio::time::sleep_until(control.deadline) => return Err(SpawnError::new("执行器 启动已达到任务截止时间")),
        _ = async { loop { if (control.cancelled)() { break; } sleep(Duration::from_millis(10)).await; } } => return Err(SpawnError::new("执行器 启动已取消")),
    }
    let mut peer = 0;
    if unsafe { GetNamedPipeClientProcessId(server.as_raw_handle(), &mut peer) } == 0
        || peer != unsafe { GetCurrentProcessId() }
    {
        return Err(SpawnError::new("执行器 私有管道连接身份不匹配"));
    }
    control.check()?;
    Ok((server, child))
}

struct Attributes {
    memory: Vec<usize>,
}
impl Attributes {
    fn new(handles: &[HANDLE; 3]) -> Result<Self, SpawnError> {
        let mut bytes = 0;
        unsafe {
            InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut bytes);
        }
        if bytes == 0 || bytes > 65536 {
            return Err(SpawnError::new("执行器 句柄白名单不可用"));
        }
        let mut memory = vec![0usize; bytes.div_ceil(size_of::<usize>())];
        let ptr = memory.as_mut_ptr().cast();
        if unsafe { InitializeProcThreadAttributeList(ptr, 1, 0, &mut bytes) } == 0 {
            return Err(SpawnError::new("执行器 句柄白名单初始化失败"));
        }
        let attributes = Self { memory };
        if unsafe {
            UpdateProcThreadAttribute(
                attributes.ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_ptr().cast::<c_void>(),
                size_of::<[HANDLE; 3]>(),
                null_mut(),
                null(),
            )
        } == 0
        {
            return Err(SpawnError::new("执行器 句柄白名单设置失败"));
        }
        Ok(attributes)
    }
    fn ptr(&self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.memory.as_ptr().cast_mut().cast()
    }
}
impl Drop for Attributes {
    fn drop(&mut self) {
        unsafe {
            DeleteProcThreadAttributeList(self.ptr());
        }
    }
}

fn command_line(program: &Path, args: &[OsString]) -> Result<Vec<u16>, SpawnError> {
    let mut out = Vec::new();
    quote_arg(&mut out, program.as_os_str());
    for arg in args {
        out.push(b' ' as u16);
        quote_arg(&mut out, OsStr::new(arg));
    }
    if out.contains(&0) || out.len() >= 32767 {
        return Err(SpawnError::new("Windows 执行器 参数超过命令行限制"));
    }
    out.push(0);
    Ok(out)
}
/// Documented CRT argv quoting; never invokes a shell or expands metacharacters.
fn quote_arg(out: &mut Vec<u16>, arg: &OsStr) {
    out.push(b'"' as u16);
    let mut slashes = 0;
    for ch in arg.encode_wide() {
        if ch == b'\\' as u16 {
            slashes += 1;
            continue;
        }
        out.extend(std::iter::repeat_n(
            b'\\' as u16,
            slashes * if ch == b'"' as u16 { 2 } else { 1 },
        ));
        if ch == b'"' as u16 {
            out.push(b'\\' as u16);
        }
        out.push(ch);
        slashes = 0;
    }
    out.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
    out.push(b'"' as u16);
}
fn environment(env: &BTreeMap<String, OsString>) -> Result<Vec<u16>, SpawnError> {
    let mut entries: Vec<_> = env.iter().collect();
    entries.sort_by_key(|(name, _)| name.to_ascii_uppercase());
    if entries
        .windows(2)
        .any(|pair| pair[0].0.eq_ignore_ascii_case(pair[1].0))
    {
        return Err(SpawnError::new("Windows 执行器 环境变量名称重复"));
    }
    let mut block = Vec::new();
    for (name, value) in entries {
        if name.contains(['=', '\0']) || value.encode_wide().any(|ch| ch == 0) {
            return Err(SpawnError::new("Windows 执行器 环境变量无效"));
        }
        block.extend(OsStr::new(name).encode_wide());
        block.push(b'=' as u16);
        block.extend(OsStr::new(value).encode_wide());
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    if block.len() > 32767 {
        return Err(SpawnError::new("Windows 执行器 环境块超过限制"));
    }
    Ok(block)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    fn system_cmd() -> PathBuf {
        let mut value = vec![0u16; 32768];
        let n = unsafe { GetSystemDirectoryW(value.as_mut_ptr(), value.len() as u32) } as usize;
        assert!(n > 0 && n < value.len());
        value.truncate(n);
        PathBuf::from(OsString::from_wide(&value)).join("cmd.exe")
    }
    fn fixture_command(script: &str) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(system_cmd());
        // A fixed local system directory avoids temporary-root symlinks. This
        // fixture does not modify it; all observations travel through pipes.
        command.current_dir(system_cmd().parent().unwrap());
        command.args(["/d", "/v:off", "/s", "/c", script]);
        command
    }
    #[test]
    fn owned_launch_future_can_be_registered_on_the_multithread_runtime() {
        fn requires_send(_: impl Send) {}
        let command = fixture_command("exit /b 0");
        requires_send(spawn(&command, [false, true, true], &|| false, &|| Ok(())));
    }
    fn active_members(child: &Child) -> u32 {
        let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
        assert_ne!(
            unsafe {
                QueryInformationJobObject(
                    child.job.as_raw_handle(),
                    JobObjectBasicAccountingInformation,
                    (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                    size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    null_mut(),
                )
            },
            0
        );
        info.ActiveProcesses
    }
    #[tokio::test]
    async fn normal_exited_root_and_job_are_confirmed_without_taskkill() {
        let mut child = spawn(
            &fixture_command("echo fixture-output & exit /b 7"),
            [false, true, true],
            &|| false,
            &|| panic!("cleanup must be confirmed"),
        )
        .await
        .unwrap();
        let mut output = child.stdout.take().unwrap();
        let mut bytes = Vec::new();
        output.read_to_end(&mut bytes).await.unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("fixture-output"));
        assert_eq!(child.wait().await.unwrap().code(), Some(7));
        child
            .close(&|| panic!("completed Job cannot be unknown"))
            .await
            .unwrap();
        assert_eq!(job_empty(&child.job), Some(true));
    }
    #[tokio::test]
    async fn cancelling_an_ordinary_child_tree_confirms_the_job_is_empty() {
        let mut child = spawn(
            &fixture_command("ping -n 30 127.0.0.1 >NUL"),
            [false, true, true],
            &|| false,
            &|| panic!("cleanup must be confirmed"),
        )
        .await
        .unwrap();
        // Local ping is a child-process fixture, not an external network call.
        tokio::time::timeout(Duration::from_secs(5), async {
            while active_members(&child) < 2 {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fixture must actually create an ordinary descendant");
        child
            .close(&|| panic!("ordinary Job failed to stop"))
            .await
            .unwrap();
        assert!(child.try_wait().unwrap().is_some());
        assert_eq!(job_empty(&child.job), Some(true));
    }
    #[tokio::test]
    async fn an_exited_root_does_not_hide_a_remaining_job_descendant() {
        let mut child = spawn(
            &fixture_command("start /b ping -n 30 127.0.0.1 >NUL & exit /b 0"),
            [false, true, true],
            &|| false,
            &|| panic!("cleanup should be confirmed"),
        )
        .await
        .unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success());
        assert!(
            active_members(&child) >= 1,
            "root exited but ordinary descendant must remain"
        );
        child
            .close(&|| panic!("remaining Job member did not settle"))
            .await
            .unwrap();
        assert_eq!(job_empty(&child.job), Some(true));
    }
    #[tokio::test]
    async fn suspended_spawn_failures_are_reaped_before_returning() {
        for fault in [Fault::Assign, Fault::Resume] {
            let error = spawn_inner(
                &fixture_command("exit /b 0"),
                [false, true, true],
                &|| false,
                &|| panic!("suspended root cleanup should be confirmed"),
                fault,
            )
            .await;
            assert!(error.is_err());
            assert!(!error.err().unwrap().is::<CleanupUnconfirmed>());
        }
    }
    #[tokio::test]
    async fn trusted_batch_shim_preserves_literal_protocol_arguments_inside_the_job() {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "executor-job-{}-{} & fixture",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let shim = root.join("codex.cmd");
        std::fs::write(&shim, "@echo off\r\necho %~1:%~2:%~3\r\n").unwrap();
        let mut command = tokio::process::Command::new(shim);
        command
            .current_dir(&root)
            .args(["app-server", "--listen", "stdio://"]);
        let mut child = spawn(&command, [true, true, false], &|| false, &|| {
            panic!("shim cleanup unknown")
        })
        .await
        .unwrap();
        child.stdin.take();
        let mut output = child.stdout.take().unwrap();
        let mut bytes = Vec::new();
        output.read_to_end(&mut bytes).await.unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("app-server:--listen:stdio://"));
        assert!(child.wait().await.unwrap().success());
        child
            .close(&|| panic!("shim Job cleanup unknown"))
            .await
            .unwrap();
        drop(child);
        drop(output);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn precancel_does_not_create_a_process() {
        assert!(spawn(
            &fixture_command("exit /b 0"),
            [false, true, true],
            &|| true,
            &|| panic!("no cleanup exists before spawn")
        )
        .await
        .is_err());
    }
    #[test]
    fn batch_wrapper_rejects_expansion_and_nonliteral_arguments() {
        for path in [
            r"C:\fixture\%SECRET%.cmd",
            "C:\\fixture\\bad!name.cmd",
            "C:\\fixture\\bad\"name.cmd",
        ] {
            assert!(launch_line(Path::new(path), &["--version".into()]).is_err());
        }
        for argument in ["&whoami", "%SECRET%", "value with spaces", "line\nnext"] {
            assert!(launch_line(Path::new(r"C:\fixture\codex.cmd"), &[argument.into()]).is_err());
        }
        assert!(launch_line(
            Path::new(r"C:\fixture\codex.cmd"),
            &["app-server".into(), "--listen".into(), "stdio://".into()]
        )
        .is_ok());
        assert_eq!(
            batch_path(Path::new(r"\\?\C:\fixture\codex.cmd")).unwrap(),
            r"C:\fixture\codex.cmd"
        );
        for path in [
            r"\\?\C:\fixture.\codex.cmd",
            r"\\?\C:\NUL\codex.cmd",
            r"C:\COM¹\codex.cmd",
        ] {
            assert!(batch_path(Path::new(path)).is_err());
        }
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

    #[test]
    fn program_resolution_rejects_junction_before_opening_descendant_metadata() {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "executor-junction-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("executor.exe"), b"not executed").unwrap();
        let link = root.join("junction");
        junction(&link, &outside);
        let candidate = link.join("executor.exe");
        let control = Control {
            deadline: Instant::now() + Duration::from_secs(5),
            cancelled: &|| false,
        };
        let mut opened = Vec::new();
        assert!(
            lock_path_observed(&candidate, false, control, &mut |path| opened
                .push(path.to_owned()))
            .is_err()
        );
        assert_eq!(opened.last(), Some(&link));
        assert!(
            !opened.contains(&candidate),
            "must not probe metadata below the reparse ancestor"
        );
        assert!(resolve_program(candidate.as_os_str(), &root, &BTreeMap::new(), control).is_err());
        std::fs::remove_dir(&link).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn remote_paths_are_rejected_before_open_and_private_environment_keeps_removals() {
        assert!(local_path(Path::new(r"\\server.invalid\share\executor.exe")).is_err());
        assert!(local_path(Path::new(r"\\.\PhysicalDrive0")).is_err());
        let mut command = std::process::Command::new("fixture.exe");
        command
            .env("fixture_private", "canary-private-value")
            .env_remove("OPENAI_API_KEY");
        let env = effective_environment(&command).unwrap();
        assert_eq!(
            env.get("FIXTURE_PRIVATE"),
            Some(&OsString::from("canary-private-value"))
        );
        assert!(!env.contains_key("OPENAI_API_KEY"));
    }
}
