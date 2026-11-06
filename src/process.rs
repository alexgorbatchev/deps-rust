use crate::{CancellationToken, Error, Result, message};
use async_trait::async_trait;
#[cfg(not(windows))]
use std::process::{Output, Stdio};
use std::{
    ffi::{OsStr, OsString},
    path::{Component, Path, PathBuf},
    time::Duration,
};
#[cfg(windows)]
mod windows;

pub fn is_agent_mode() -> bool {
    std::env::var("AGENT")
        .is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

pub fn managed_bin_dir(app_name: &str) -> Result<PathBuf> {
    if app_name.trim().is_empty()
        || Path::new(app_name)
            .components()
            .any(|p| !matches!(p, Component::Normal(_)))
        || Path::new(app_name).components().count() != 1
    {
        return Err(message("app_name must be one nonempty path segment"));
    }
    let data = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            if cfg!(windows) {
                std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
            } else {
                None
            }
        })
        .or_else(|| dirs::home_dir().map(|p| p.join(".local/share")))
        .ok_or_else(|| message("cannot resolve home directory"))?;
    Ok(data.join(app_name).join("bin"))
}

pub fn ensure_managed_bin_dir(app_name: &str) -> Result<PathBuf> {
    let path = managed_bin_dir(app_name)?;
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

/// Directories that hold managed executables, in lookup order.
///
/// Windows cannot link an unpacked yt-dlp build into `bin_dir` without symbolic
/// links, so its install folder is searched directly.
pub(crate) fn managed_dirs(bin_dir: &Path) -> Result<Vec<PathBuf>> {
    let dirs = vec![bin_dir.to_owned()];
    #[cfg(not(unix))]
    let dirs = crate::yt_dlp::windows_entry_dir(bin_dir)?
        .into_iter()
        .chain(dirs)
        .collect();
    Ok(dirs)
}

pub(crate) fn is_managed_dir(bin_dir: &Path, directory: &Path) -> bool {
    directory == bin_dir || cfg!(windows) && directory.starts_with(bin_dir.join(".yt-dlp"))
}

/// Returns a child-process PATH without mutating the process environment.
///
/// Global executables keep precedence: the managed directories are appended
/// after `current`, so the managed copy is found only when no global executable
/// of that name exists. `Manager::child_path` also accounts for global copies
/// that are older than the minimum version.
pub fn managed_path(bin_dir: &Path, current: Option<&OsStr>) -> Result<OsString> {
    let mut paths = current
        .map(std::env::split_paths)
        .map(Iterator::collect::<Vec<_>>)
        .unwrap_or_default();
    paths.retain(|path| !is_managed_dir(bin_dir, path));
    for dir in managed_dirs(bin_dir)? {
        if !paths.contains(&dir) {
            paths.push(dir);
        }
    }
    join_path(paths)
}

pub(crate) fn join_path(paths: impl IntoIterator<Item = PathBuf>) -> Result<OsString> {
    std::env::join_paths(paths).map_err(|e| message(format!("building managed PATH: {e}")))
}

/// Removes the managed directories from a search PATH, leaving global entries.
pub(crate) fn global_path(bin_dir: &Path, current: Option<&OsStr>) -> Option<OsString> {
    let paths = current
        .map(std::env::split_paths)
        .map(|paths| {
            paths
                .filter(|p| !p.as_os_str().is_empty() && !is_managed_dir(bin_dir, p))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if paths.is_empty() {
        return None;
    }
    std::env::join_paths(paths).ok()
}

pub fn sanitize_stderr(stderr: &str) -> String {
    let raw = stderr.trim();
    let lines = || raw.lines().map(str::trim).filter(|s| !s.is_empty());
    if raw.contains("Traceback (most recent call last):") {
        return lines()
            .rfind(|s| !s.starts_with("File ") && !s.starts_with("Traceback"))
            .unwrap_or(raw)
            .to_owned();
    }
    if let Some(index) = raw.find("ERROR:") {
        return raw[index..].lines().next().unwrap_or("").trim().to_owned();
    }
    if raw.contains("panic:")
        && let Some(line) = lines().find(|s| s.starts_with("panic:"))
    {
        return line.to_owned();
    }
    if raw.contains("\n    at ") || raw.contains("\n\tat ") {
        return lines()
            .filter(|s| !s.starts_with("at "))
            .collect::<Vec<_>>()
            .join(" ");
    }
    lines().next_back().unwrap_or("").to_owned()
}

#[async_trait]
pub trait CommandRunner: Send + Sync {
    /// Runs `program` (a command name or an executable path) and returns its standard output.
    async fn run(
        &self,
        program: &OsStr,
        args: &[String],
        token: &CancellationToken,
    ) -> Result<Vec<u8>>;
}

/// Time a cancelled process group gets to exit after SIGTERM before SIGKILL.
pub const DEFAULT_TERMINATION_GRACE: Duration = Duration::from_secs(5);

/// Runs real child processes.
///
/// On Unix each child starts in a new session, which makes it the leader of its
/// own process group. Cancelling the token, or dropping the `run` future, sends
/// SIGTERM to the whole group and SIGKILL only when a member is still alive after
/// `termination_grace`. On Windows a child starts suspended, enters an owned Job
/// Object, and resumes in its own console process group. Cancellation requests
/// CTRL_BREAK when the caller and child share a console, then terminates the job
/// after the grace period. Both platforms retain ownership until exit is
/// confirmed, draining output while they wait.
///
/// The Unix session has no controlling terminal, so terminal signals such as
/// Ctrl-C do not reach the child: callers translate SIGINT and SIGTERM into token
/// cancellation, for example with `tokio::signal::ctrl_c`. A child that tries to
/// prompt on the terminal, such as `sudo`, fails instead of stopping in the
/// background.
#[derive(Clone, Debug)]
pub struct ProcessRunner {
    /// PATH used to resolve command names and passed to the child; `None` inherits the process environment.
    pub path: Option<OsString>,
    pub termination_grace: Duration,
}

impl Default for ProcessRunner {
    fn default() -> Self {
        Self {
            path: None,
            termination_grace: DEFAULT_TERMINATION_GRACE,
        }
    }
}

#[async_trait]
impl CommandRunner for ProcessRunner {
    async fn run(
        &self,
        program: &OsStr,
        args: &[String],
        token: &CancellationToken,
    ) -> Result<Vec<u8>> {
        if token.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let executable = if let Some(path) = &self.path {
            which::which_in(program, Some(path), std::env::current_dir()?).map_err(|_| {
                Error::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("executable {program:?} was not found in PATH"),
                ))
            })?
        } else {
            PathBuf::from(program)
        };
        #[cfg(windows)]
        let output = windows::run(
            executable,
            args,
            self.path.as_deref(),
            self.termination_grace,
            token,
        )
        .await?;
        #[cfg(not(windows))]
        let output = {
            let mut command = tokio::process::Command::new(executable);
            command
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if let Some(path) = &self.path {
                command.env("PATH", path);
            }
            let mut child = ChildGroup::spawn(command, self.termination_grace).await?;
            let finished = {
                let output = child.output();
                tokio::pin!(output);
                tokio::select! {
                    biased;
                    _ = token.cancelled() => None,
                    output = &mut output => Some(output),
                }
            };
            let Some(output) = finished else {
                child.terminate().await?;
                return Err(Error::Cancelled);
            };
            output?
        };
        if !output.status.success() {
            return Err(Error::Command {
                name: program.to_string_lossy().into_owned(),
                status: output.status,
                stderr: sanitize_stderr(&String::from_utf8_lossy(&output.stderr)),
            });
        }
        Ok(output.stdout)
    }
}

/// Retries of a spawn that fails because the executable is busy, 10 ms apart.
#[cfg(not(windows))]
const SPAWN_BUSY_RETRIES: u32 = 100;

/// A spawned child that is terminated with its process group unless it finishes.
#[cfg(not(windows))]
struct ChildGroup {
    child: Option<tokio::process::Child>,
    /// The output pipes stay open until the group is gone: closing them early
    /// would kill a member that writes while handling SIGTERM with SIGPIPE.
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    /// The session leader's ID, which is also the group ID. Recorded at spawn
    /// because Tokio forgets the ID once it reaps the leader
    /// while other members may still hold the output pipes open.
    #[cfg(unix)]
    group: Option<rustix::process::Pid>,
    grace: Duration,
}

#[cfg(not(windows))]
impl ChildGroup {
    async fn spawn(mut command: tokio::process::Command, grace: Duration) -> std::io::Result<Self> {
        #[cfg(unix)]
        // SAFETY: the closure runs in the forked child before `exec` and only
        // calls setsid(2), which is async-signal-safe and touches no memory
        // shared with the parent.
        unsafe {
            command.pre_exec(|| {
                rustix::process::setsid()
                    .map(drop)
                    .map_err(std::io::Error::from)
            });
        }
        #[cfg(not(unix))]
        command.kill_on_drop(true);
        // On Linux, `exec` of a just-written executable fails with ETXTBSY while
        // a child forked concurrently elsewhere in the process still holds the
        // descriptor that wrote it; the condition clears once that child execs.
        let mut attempts = 0;
        let mut child = loop {
            match command.spawn() {
                Err(error)
                    if error.kind() == std::io::ErrorKind::ExecutableFileBusy
                        && attempts < SPAWN_BUSY_RETRIES =>
                {
                    attempts += 1;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                child => break child?,
            }
        };
        Ok(Self {
            #[cfg(unix)]
            group: child
                .id()
                .and_then(|id| i32::try_from(id).ok())
                .and_then(rustix::process::Pid::from_raw),
            stdout: child.stdout.take(),
            stderr: child.stderr.take(),
            child: Some(child),
            grace,
        })
    }

    async fn output(&mut self) -> std::io::Result<Output> {
        let child = self
            .child
            .as_mut()
            .expect("the child is present until output or termination completes");
        let (status, stdout, stderr) = tokio::try_join!(
            child.wait(),
            read_pipe(self.stdout.as_mut()),
            read_pipe(self.stderr.as_mut())
        )?;
        self.child = None;
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }

    /// Ends the group and waits until every member has exited or been killed.
    async fn terminate(&mut self) -> std::io::Result<()> {
        #[cfg(unix)]
        if let Some(reaper) = self.start_termination() {
            // Should this wait be dropped, the blocking task still finishes, and
            // the runtime waits for it when it shuts down.
            tokio::task::spawn_blocking(move || reaper.run())
                .await
                .map_err(std::io::Error::other)?;
        }
        #[cfg(not(unix))]
        if let Some(mut child) = self.child.take() {
            let _ = self.grace;
            child.kill().await?;
        }
        Ok(())
    }

    /// Sends SIGTERM to the group and returns the reaper that finishes ending it.
    #[cfg(unix)]
    fn start_termination(&mut self) -> Option<Reaper> {
        let child = self.child.take()?;
        signal_group(self.group, rustix::process::Signal::TERM);
        Some(Reaper {
            child,
            pipes: [
                self.stdout
                    .take()
                    .and_then(|pipe| pipe.into_owned_fd().ok()),
                self.stderr
                    .take()
                    .and_then(|pipe| pipe.into_owned_fd().ok()),
            ]
            .into_iter()
            .flatten()
            .filter(|pipe| rustix::io::ioctl_fionbio(pipe, true).is_ok())
            .collect(),
            group: self.group,
            grace: self.grace,
            finished: false,
        })
    }
}

#[cfg(not(windows))]
async fn read_pipe(
    pipe: Option<&mut (impl tokio::io::AsyncRead + Unpin)>,
) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    if let Some(pipe) = pipe {
        pipe.read_to_end(&mut bytes).await?;
    }
    Ok(bytes)
}

#[cfg(not(windows))]
impl Drop for ChildGroup {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(reaper) = self.start_termination() {
            // Runtime shutdown waits for blocking tasks, so the escalation still
            // happens when the program exits right after dropping `run`. A task
            // the shutting-down runtime discards drops its reaper, which then
            // sends SIGKILL at once and waits synchronously for group exit.
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn_blocking(move || reaper.run());
            } else {
                let _ = std::thread::Builder::new()
                    .name("cli-deps-reaper".into())
                    .spawn(move || reaper.run());
            }
        }
        // On other platforms `kill_on_drop` terminates the child when it drops.
    }
}

#[cfg(unix)]
fn signal_group(pid: Option<rustix::process::Pid>, signal: rustix::process::Signal) {
    if let Some(pid) = pid {
        // ESRCH means every member already exited.
        let _ = rustix::process::kill_process_group(pid, signal);
    }
}

/// Finishes terminating a process group that has received SIGTERM.
///
/// Dropping a reaper that has not finished sends SIGKILL to the group at once,
/// and waits for group exit, so discarding it, for example with a shutting-down
/// runtime's queued blocking tasks, retains ownership until cleanup completes.
#[cfg(unix)]
struct Reaper {
    child: tokio::process::Child,
    /// Nonblocking read ends of the output pipes, drained while waiting.
    pipes: Vec<std::os::fd::OwnedFd>,
    group: Option<rustix::process::Pid>,
    grace: Duration,
    finished: bool,
}

#[cfg(unix)]
impl Reaper {
    /// Waits up to `grace` for the leader and the rest of its group to exit,
    /// then sends SIGKILL to the group and waits for it to empty.
    ///
    /// A process group ID is not reused while the group has a member, and polling
    /// stops as soon as the group is empty, so the signals reach only this group.
    fn run(mut self) {
        if !self.wait(self.grace) {
            self.kill_and_wait();
        }
        self.finished = true;
    }

    fn kill_and_wait(&mut self) {
        const REAP_INTERVAL: Duration = Duration::from_secs(1);
        signal_group(self.group, rustix::process::Signal::KILL);
        // Kill the leader directly too, in case it left its group or
        // signalling the group was refused. SIGKILL does not establish exit:
        // a member can remain in uninterruptible sleep or await reaping.
        // Keep ownership and drain output until the group is actually gone.
        let _ = self.child.start_kill();
        while !self.wait(REAP_INTERVAL) {}
        self.finished = true;
    }

    /// Drains the pipes until the leader is reaped and the group is empty, or
    /// `limit` passes; reports whether the group is gone.
    fn wait(&mut self, limit: Duration) -> bool {
        const POLL: Duration = Duration::from_millis(20);
        let deadline = std::time::Instant::now() + limit;
        loop {
            self.drain();
            if self.exited() {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(POLL);
        }
    }

    fn exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
            && self.group.is_none_or(|pid| {
                matches!(
                    rustix::process::test_kill_process_group(pid),
                    Err(rustix::io::Errno::SRCH)
                )
            })
    }

    /// Discards up to `DRAIN_LIMIT` bytes of available output from each pipe,
    /// closing pipes that reach end of file or fail. The limit keeps a member
    /// that writes without pause from holding off the deadline checks.
    fn drain(&mut self) {
        const DRAIN_LIMIT: usize = 1 << 20;
        let mut buffer = [0; 8192];
        self.pipes.retain(|pipe| {
            let mut drained = 0;
            while drained < DRAIN_LIMIT {
                match rustix::io::read(pipe, &mut buffer) {
                    Ok(0) => return false,
                    Ok(read) => drained += read,
                    Err(rustix::io::Errno::INTR) => {}
                    Err(rustix::io::Errno::AGAIN) => return true,
                    Err(_) => return false,
                }
            }
            true
        });
    }
}

#[cfg(unix)]
impl Drop for Reaper {
    fn drop(&mut self) {
        if !self.finished {
            self.kill_and_wait();
        }
    }
}
