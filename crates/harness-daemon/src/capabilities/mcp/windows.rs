//! Windows stdio ownership: never run an uncontained server even briefly.
//! CreateProcessW(CREATE_SUSPENDED) -> assign a non-breakaway, kill-on-close Job
//! -> ResumeThread. Failures terminate the suspended child; there is no fallback.
//! This is process-tree ownership, not an OS filesystem/network sandbox.
use super::super::{Control, McpError, ServerConfig};
use super::CleanupStatus;
use std::{
    collections::BTreeMap,
    ffi::{c_void, OsStr, OsString},
    mem::{size_of, zeroed},
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Component, Path, PathBuf, Prefix},
    ptr::{null, null_mut},
    time::Duration,
};
use tokio::{
    net::windows::named_pipe::{NamedPipeServer, ServerOptions},
    time::{sleep, Instant},
};
use windows_sys::Win32::{
    Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0},
    Security::{
        Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG},
        SECURITY_ATTRIBUTES,
    },
    Storage::FileSystem::{
        CreateFileW, GetDriveTypeW, GetFileInformationByHandle, GetFinalPathNameByHandleW,
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ, OPEN_EXISTING,
    },
    System::{
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
            JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
            TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        },
        Pipes::GetNamedPipeClientProcessId,
        Threading::{
            CreateProcessW, DeleteProcThreadAttributeList, GetCurrentProcessId,
            InitializeProcThreadAttributeList, ResumeThread, TerminateProcess,
            UpdateProcThreadAttribute, WaitForSingleObject, CREATE_NO_WINDOW, CREATE_SUSPENDED,
            CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST,
            PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, STARTF_USESTDHANDLES,
            STARTUPINFOEXW,
        },
    },
};

pub(in super::super) struct Child {
    pub stdin: Option<NamedPipeServer>,
    pub stdout: Option<NamedPipeServer>,
    pub stderr: Option<NamedPipeServer>,
    handle: OwnedHandle,
    #[cfg(test)]
    pid: u32,
}
impl Child {
    #[cfg(test)]
    pub fn id(&self) -> Option<u32> {
        Some(self.pid)
    }
    fn exited(&self) -> bool {
        // Zero-time polling never blocks the runtime or cancellation.
        unsafe { WaitForSingleObject(self.handle.as_raw_handle(), 0) == WAIT_OBJECT_0 }
    }
}

pub(in super::super) struct Process {
    pub child: Child,
    job: OwnedHandle,
    cleanup: Option<CleanupStatus>,
    // Deny-write/delete handles pin every pathname ancestor until all Job members exit.
    // No ambient reopen is trusted after releasing these locks.
    _roots: Vec<OwnedHandle>,
}
impl Process {
    pub async fn spawn(
        config: &ServerConfig,
        root: &Path,
        control: Control<'_>,
    ) -> Result<Self, McpError> {
        Self::spawn_inner(config, root, control, Fault::None, || {}).await
    }

    async fn spawn_inner(
        config: &ServerConfig,
        root: &Path,
        control: Control<'_>,
        fault: Fault,
        before_create: impl FnOnce(),
    ) -> Result<Self, McpError> {
        let control = Control {
            deadline: control
                .deadline
                .min(Instant::now() + config.limits.setup_timeout),
            cancelled: control.cancelled,
        };
        control.check()?;
        let (mut roots, root_wide) = lock_path(root, true, control)?;
        let (program_handles, program) = lock_path(&config.program, false, control)?;
        roots.extend(program_handles);
        if !config
            .program
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
        {
            return Err(McpError::new("Windows MCP 仅允许直接启动受信任的本地 EXE"));
        }
        let mut command_line = command_line(&config.program, &config.args)?;
        let mut environment = environment(&config.env)?;
        control.check()?;
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
            return Err(McpError::new("MCP Windows Job 安全策略不可用，未启动服务"));
        }
        // Each pipe is a single local instance with a random 128-bit name.
        // The parent opens the only client endpoint and verifies its own PID
        // before passing the handle to the child. No peer can receive bytes.
        let (stdin, input_child) = pipe(false, control).await?;
        let (stdout, output_child) = pipe(true, control).await?;
        let (stderr, error_child) = pipe(true, control).await?;
        let (process_handle, thread_handle, _pid) = {
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
            before_create();
            let mut info: PROCESS_INFORMATION = unsafe { zeroed() };
            if unsafe {
                CreateProcessW(
                    program.as_ptr(),
                    command_line.as_mut_ptr(),
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
                return Err(McpError::new("MCP Windows 服务创建失败，未执行服务代码"));
            }
            // CreateProcess succeeded: immediately own every returned handle. The
            // primary thread remains suspended until Job assignment and all checks.
            let process_handle = unsafe { OwnedHandle::from_raw_handle(info.hProcess) };
            let thread_handle = unsafe { OwnedHandle::from_raw_handle(info.hThread) };
            (process_handle, thread_handle, info.dwProcessId)
        };
        let mut process = Self {
            child: Child {
                stdin: Some(stdin),
                stdout: Some(stdout),
                stderr: Some(stderr),
                handle: process_handle,
                #[cfg(test)]
                pid: _pid,
            },
            job,
            cleanup: None,
            _roots: roots,
        };
        let assigned = fault != Fault::Assign
            && unsafe {
                AssignProcessToJobObject(
                    process.job.as_raw_handle(),
                    process.child.handle.as_raw_handle(),
                )
            } != 0;
        if !assigned {
            // Unassigned suspended roots are not killed by closing the Job.
            // TerminateProcess is mandatory here; close() verifies root exit.
            unsafe {
                TerminateProcess(process.child.handle.as_raw_handle(), 1);
            }
            let cleanup = process.close(control).await;
            return Err(McpError {
                reason: "MCP Windows Job 绑定失败，服务未恢复执行",
                cleanup: Some(cleanup),
            });
        }
        if let Err(mut error) = control.check() {
            error.cleanup = Some(process.close(control).await);
            return Err(error);
        }
        if fault == Fault::Resume || unsafe { ResumeThread(thread_handle.as_raw_handle()) } != 1 {
            let cleanup = process.close(control).await;
            return Err(McpError {
                reason: "MCP Windows 服务恢复失败，已停止启动",
                cleanup: Some(cleanup),
            });
        }
        // Close parent copies of the child pipe ends before returning. HANDLE_LIST
        // excludes every other inheritable process, credential or Job handle.
        drop((input_child, output_child, error_child, thread_handle));
        if let Err(mut error) = control.check() {
            error.cleanup = Some(process.close(control).await);
            return Err(error);
        }
        Ok(process)
    }

    pub async fn close(&mut self, control: Control<'_>) -> CleanupStatus {
        if let Some(status) = self.cleanup {
            return status;
        }
        self.child.stdin.take();
        if !(control.cancelled)() && Instant::now() < control.deadline {
            let grace = control
                .deadline
                .min(Instant::now() + Duration::from_millis(100));
            while Instant::now() < grace && !self.child.exited() && !(control.cancelled)() {
                sleep(Duration::from_millis(10)).await;
            }
        }
        // Cancellation never skips cleanup. Query until both root and all Job
        // members are stopped, with a small independent cleanup deadline.
        unsafe {
            TerminateJobObject(self.job.as_raw_handle(), 1);
            TerminateProcess(self.child.handle.as_raw_handle(), 1);
        }
        let until = Instant::now() + Duration::from_millis(800);
        loop {
            if self.child.exited() && job_empty(&self.job) == Some(true) {
                self.cleanup = Some(CleanupStatus::JobStopped);
                return CleanupStatus::JobStopped;
            }
            if Instant::now() >= until {
                self.cleanup = Some(CleanupStatus::Unknown);
                return CleanupStatus::Unknown;
            }
            sleep(Duration::from_millis(10)).await;
        }
    }

    #[cfg(test)]
    pub async fn spawn_with_fault(
        config: &ServerConfig,
        root: &Path,
        control: Control<'_>,
        assign_failure: bool,
    ) -> Result<Self, McpError> {
        Self::spawn_inner(
            config,
            root,
            control,
            if assign_failure {
                Fault::Assign
            } else {
                Fault::Resume
            },
            || {},
        )
        .await
    }
    #[cfg(test)]
    pub async fn spawn_with_root_hook(
        config: &ServerConfig,
        root: &Path,
        control: Control<'_>,
        hook: impl FnOnce(),
    ) -> Result<Self, McpError> {
        Self::spawn_inner(config, root, control, Fault::None, hook).await
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        // Including Unknown: retry termination best-effort and close the last
        // non-inherited kill-on-close Job handle. Do not claim confirmed cleanup.
        unsafe {
            TerminateJobObject(self.job.as_raw_handle(), 1);
            TerminateProcess(self.child.handle.as_raw_handle(), 1);
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    None,
    Assign,
    Resume,
}

fn owned(handle: HANDLE) -> Result<OwnedHandle, McpError> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        Err(McpError::new("MCP Windows 进程资源不可用"))
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
fn local_path(path: &Path) -> Result<Vec<u16>, McpError> {
    let drive = match path.components().next() {
        Some(Component::Prefix(prefix)) if path.is_absolute() => match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive,
            _ => return Err(McpError::new("Windows MCP 仅支持本地固定盘符绝对路径")),
        },
        _ => return Err(McpError::new("Windows MCP 仅支持本地固定盘符绝对路径")),
    };
    let drive_root = [drive as u16, b':' as u16, b'\\' as u16, 0];
    // DRIVE_FIXED=3 (WinBase.h). A drive-letter prefix does NOT prove locality:
    // mapped SMB, removable, unknown and other drive types fail closed before
    // opening any path. Never probe a remote path to see whether it exists.
    if !fixed_drive(unsafe { GetDriveTypeW(drive_root.as_ptr()) }) {
        return Err(McpError::new("Windows MCP 拒绝网络、可移动或未知盘符"));
    }
    if path
        .components()
        .any(|p| matches!(p, Component::ParentDir | Component::CurDir))
    {
        return Err(McpError::new("Windows MCP 路径不能包含相对组件"));
    }
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) || wide.len() > 32760 {
        return Err(McpError::new("Windows MCP 路径无效"));
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
    control: Control<'_>,
) -> Result<(Vec<OwnedHandle>, Vec<u16>), McpError> {
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
        let handle = owned(unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_READ_ATTRIBUTES,
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
            return Err(McpError::new("MCP 项目路径不可用或包含重解析点"));
        }
        handles.push(handle);
    }
    if handles.is_empty() {
        return Err(McpError::new("MCP 项目根目录不可用"));
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
        return Err(McpError::new("MCP Windows 路径身份无法确认"));
    }
    final_path.truncate(count);
    let canonical = PathBuf::from(OsString::from_wide(&final_path));
    let canonical = local_path(&canonical)?;
    Ok((handles, canonical))
}

async fn pipe(
    parent_reads: bool,
    control: Control<'_>,
) -> Result<(NamedPipeServer, OwnedHandle), McpError> {
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
        return Err(McpError::new("MCP 安全管道随机源不可用"));
    }
    let name = format!(
        r"\\.\pipe\self-agent-mcp-{:032x}",
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
        .map_err(|_| McpError::new("MCP 私有异步管道创建失败"))?;
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
        result = server.connect() => { result.map_err(|_| McpError::new("MCP 私有异步管道连接失败"))?; },
        _ = tokio::time::sleep_until(control.deadline) => return Err(McpError::new("MCP 启动已达到任务截止时间")),
        _ = async { loop { if (control.cancelled)() { break; } sleep(Duration::from_millis(10)).await; } } => return Err(McpError::new("MCP 启动已取消")),
    }
    let mut peer = 0;
    if unsafe { GetNamedPipeClientProcessId(server.as_raw_handle(), &mut peer) } == 0
        || peer != unsafe { GetCurrentProcessId() }
    {
        return Err(McpError::new("MCP 私有管道连接身份不匹配"));
    }
    control.check()?;
    Ok((server, child))
}

struct Attributes {
    memory: Vec<usize>,
}
impl Attributes {
    fn new(handles: &[HANDLE; 3]) -> Result<Self, McpError> {
        let mut bytes = 0;
        unsafe {
            InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut bytes);
        }
        if bytes == 0 || bytes > 65536 {
            return Err(McpError::new("MCP 句柄白名单不可用"));
        }
        let mut memory = vec![0usize; bytes.div_ceil(size_of::<usize>())];
        let ptr = memory.as_mut_ptr().cast();
        if unsafe { InitializeProcThreadAttributeList(ptr, 1, 0, &mut bytes) } == 0 {
            return Err(McpError::new("MCP 句柄白名单初始化失败"));
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
            return Err(McpError::new("MCP 句柄白名单设置失败"));
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

fn command_line(program: &Path, args: &[String]) -> Result<Vec<u16>, McpError> {
    let mut out = Vec::new();
    quote_arg(&mut out, program.as_os_str());
    for arg in args {
        out.push(b' ' as u16);
        quote_arg(&mut out, OsStr::new(arg));
    }
    if out.contains(&0) || out.len() >= 32767 {
        return Err(McpError::new("Windows MCP 参数超过命令行限制"));
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
fn environment(env: &BTreeMap<String, String>) -> Result<Vec<u16>, McpError> {
    let mut entries: Vec<_> = env.iter().collect();
    entries.sort_by_key(|(name, _)| name.to_ascii_uppercase());
    if entries
        .windows(2)
        .any(|pair| pair[0].0.eq_ignore_ascii_case(pair[1].0))
    {
        return Err(McpError::new("Windows MCP 环境变量名称重复"));
    }
    let mut block = Vec::new();
    for (name, value) in entries {
        if name.contains(['=', '\0']) || value.contains('\0') {
            return Err(McpError::new("Windows MCP 环境变量无效"));
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
        return Err(McpError::new("Windows MCP 环境块超过限制"));
    }
    Ok(block)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn drive_kind_filter_rejects_mapped_remote_and_unknown_before_path_access() {
        for kind in [0, 1, 2, 4, 5, 6, u32::MAX] {
            assert!(!fixed_drive(kind));
        }
        assert!(fixed_drive(3));
        assert!(local_path(Path::new(r"\\server.invalid\share\must-not-access")).is_err());
        assert!(local_path(Path::new(r"\\?\UNC\server.invalid\share")).is_err());
        assert!(local_path(Path::new(r"\\.\PhysicalDrive0")).is_err());
    }
}
