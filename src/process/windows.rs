//! Windows process ownership independent of the async runtime's IO driver.
use crate::{CancellationToken, Error, Result};
use std::{
    ffi::OsStr,
    io::{self, Read},
    mem::{size_of, zeroed},
    os::windows::{
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::CommandExt,
    },
    path::PathBuf,
    process::{Child, Command, ExitStatus, Output, Stdio},
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{ERROR_BROKEN_PIPE, ERROR_NO_MORE_FILES, HANDLE, INVALID_HANDLE_VALUE},
    System::{
        Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent},
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
        },
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
            QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
        },
        Pipes::PeekNamedPipe,
        Threading::{
            CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED, OpenThread, ResumeThread,
            THREAD_SUSPEND_RESUME,
        },
    },
};

const POLL: Duration = Duration::from_millis(10);

pub(super) async fn run(
    executable: PathBuf,
    args: &[String],
    path: Option<&OsStr>,
    grace: Duration,
    token: &CancellationToken,
) -> Result<Output> {
    let mut command = Command::new(executable);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(path) = path {
        command.env("PATH", path);
    }
    let cancellation = token.child_token();
    // Dropping this future requests graceful cleanup even if its blocking task
    // has not started. A discarded queued task has not spawned a process yet.
    let guard = cancellation.clone().drop_guard();
    let output = tokio::task::spawn_blocking(move || execute(command, grace, &cancellation))
        .await
        .map_err(io::Error::other)?;
    guard.disarm();
    output
}

fn execute(mut command: Command, grace: Duration, token: &CancellationToken) -> Result<Output> {
    if token.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let job = Job::new()?;
    // No child code runs before assignment, so even an immediate grandchild
    // inherits the job. Do not permit breakaway from this ownership boundary.
    command.creation_flags(CREATE_SUSPENDED | CREATE_NEW_PROCESS_GROUP);
    let child = command.spawn()?;
    let mut tree = Tree {
        child,
        job,
        grace,
        finished: false,
    };
    tree.job.assign(&tree.child)?;
    resume(tree.child.id())?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    loop {
        drain(tree.child.stdout.as_mut(), Some(&mut stdout))?;
        drain(tree.child.stderr.as_mut(), Some(&mut stderr))?;
        if token.is_cancelled() {
            tree.stop();
            return Err(Error::Cancelled);
        }
        if let Some(status) = tree.completed()? {
            // No writer remains. Drain to EOF, including the final buffer.
            if let Some(pipe) = &mut tree.child.stdout {
                pipe.read_to_end(&mut stdout)?;
            }
            if let Some(pipe) = &mut tree.child.stderr {
                pipe.read_to_end(&mut stderr)?;
            }
            tree.finished = true;
            return Ok(Output {
                status,
                stdout,
                stderr,
            });
        }
        thread::sleep(POLL);
    }
}

struct Tree {
    child: Child,
    job: Job,
    grace: Duration,
    finished: bool,
}

impl Tree {
    fn completed(&mut self) -> io::Result<Option<ExitStatus>> {
        let status = self.child.try_wait()?;
        if self.job.active()? {
            Ok(None)
        } else {
            Ok(status)
        }
    }

    fn stop(&mut self) {
        // CTRL_BREAK is the Windows console's group-scoped graceful request.
        // A detached/headless caller may have no shared console; still allow the
        // grace period before terminating the owned job in that case.
        // SAFETY: the nonzero ID names our still-owned process group. No global
        // console attachment or handler state is changed in the calling process.
        unsafe {
            GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, self.child.id());
        }
        let started = Instant::now();
        loop {
            let _ = drain(self.child.stdout.as_mut(), None);
            let _ = drain(self.child.stderr.as_mut(), None);
            if matches!(self.completed(), Ok(Some(_))) {
                self.finished = true;
                return;
            }
            if started.elapsed() >= self.grace {
                self.job.kill();
                // Also covers a suspended child whose job assignment failed.
                let _ = self.child.kill();
            }
            // Neither an inspection error nor elapsed time establishes exit.
            thread::sleep(POLL);
        }
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        if !self.finished {
            self.stop();
        }
    }
}

struct Job(OwnedHandle);

impl Job {
    fn new() -> io::Result<Self> {
        // SAFETY: null security/name pointers create an unnamed, noninherited job.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateJobObjectW returned a new owned kernel handle.
        let job = Self(unsafe { OwnedHandle::from_raw_handle(handle) });
        // SAFETY: this Win32 information structure has no invalid zero values.
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: pointer and size match the specified information class.
        if unsafe {
            SetInformationJobObject(
                job.handle(),
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    fn handle(&self) -> HANDLE {
        self.0.as_raw_handle()
    }

    fn assign(&self, child: &Child) -> io::Result<()> {
        // SAFETY: both handles remain owned for this call; the child is suspended.
        if unsafe { AssignProcessToJobObject(self.handle(), child.as_raw_handle()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn active(&self) -> io::Result<bool> {
        // SAFETY: this Win32 output structure has no invalid zero values.
        let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
        // SAFETY: output buffer and size match the information class, and our job
        // handle stays open until every process has exited.
        if unsafe {
            QueryInformationJobObject(
                self.handle(),
                JobObjectBasicAccountingInformation,
                (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(info.ActiveProcesses != 0)
    }

    fn kill(&self) {
        // SAFETY: this is our owned job; descendants cannot break away from it.
        unsafe {
            TerminateJobObject(self.handle(), 1);
        }
    }
}

/// Resume only threads in the suspended, job-assigned child. std::process keeps
/// the process handle but closes CreateProcessW's primary-thread handle.
fn resume(pid: u32) -> io::Result<()> {
    // SAFETY: documented snapshot flag; no pointers are passed.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful snapshot is an owned handle requiring CloseHandle.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    // SAFETY: this output structure consists entirely of integers.
    let mut entry: THREADENTRY32 = unsafe { zeroed() };
    entry.dwSize = size_of::<THREADENTRY32>() as u32;
    // SAFETY: entry is initialized with the required structure size.
    let mut found = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) };
    let mut resumed = false;
    while found != 0 {
        if entry.th32OwnerProcessID == pid {
            // SAFETY: pid is held alive by our process handle and has not run.
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if thread.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: OpenThread returned a new owned handle.
            let thread = unsafe { OwnedHandle::from_raw_handle(thread) };
            // SAFETY: this thread belongs to the suspended child in our job.
            if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
                return Err(io::Error::last_os_error());
            }
            resumed = true;
        }
        // SAFETY: the snapshot and correctly sized output structure remain valid.
        found = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) };
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() != Some(ERROR_NO_MORE_FILES as i32) {
        return Err(error);
    }
    if !resumed {
        return Err(io::Error::other("suspended child has no resumable thread"));
    }
    Ok(())
}

/// std::process uses synchronous anonymous pipes. Peek before reading so a
/// silent child cannot prevent cancellation; cap each pass to avoid starvation.
fn drain(
    pipe: Option<&mut (impl Read + AsRawHandle)>,
    mut output: Option<&mut Vec<u8>>,
) -> io::Result<()> {
    let Some(pipe) = pipe else {
        return Ok(());
    };
    let mut buffer = [0; 8192];
    for _ in 0..8 {
        let mut available = 0;
        // SAFETY: only the byte-count output is requested for our live pipe.
        let success = unsafe {
            PeekNamedPipe(
                pipe.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        };
        if success == 0 {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
                Ok(())
            } else {
                Err(error)
            };
        }
        if available == 0 {
            break;
        }
        let count = pipe.read(&mut buffer[..(available as usize).min(8192)])?;
        if count == 0 {
            break;
        }
        if let Some(output) = &mut output {
            output.extend_from_slice(&buffer[..count]);
        }
    }
    Ok(())
}
