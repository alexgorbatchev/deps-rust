use crate::{CancellationToken, Error, Result, message};
use async_trait::async_trait;
use std::{
    ffi::{OsStr, OsString},
    path::{Component, Path, PathBuf},
    process::{Output, Stdio},
    time::Duration,
};

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
pub(crate) fn managed_dirs(bin_dir: &Path) -> Vec<PathBuf> {
    vec![
        #[cfg(not(unix))]
        crate::yt_dlp::windows_entry_dir(bin_dir),
        bin_dir.to_owned(),
    ]
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
    for dir in managed_dirs(bin_dir) {
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
    let managed = managed_dirs(bin_dir);
    let paths = current
        .map(std::env::split_paths)
        .map(|paths| {
            paths
                .filter(|p| !p.as_os_str().is_empty() && !managed.contains(p))
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
/// `termination_grace`. On Windows the child is terminated with `TerminateProcess`.
///
/// The new session has no controlling terminal, so terminal signals such as
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
            child.terminate().await;
            return Err(Error::Cancelled);
        };
        let output = output?;
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
const SPAWN_BUSY_RETRIES: u32 = 100;

/// A spawned child that is terminated with its process group unless it finishes.
struct ChildGroup {
    child: Option<tokio::process::Child>,
    /// The session leader's ID, which is also the group ID. Recorded at spawn
    /// because Tokio forgets the ID once it reaps the leader
    /// while other members may still hold the output pipes open.
    #[cfg(unix)]
    group: Option<rustix::process::Pid>,
    grace: Duration,
}

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
        let child = loop {
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
            child: Some(child),
            grace,
        })
    }

    async fn output(&mut self) -> std::io::Result<Output> {
        let child = self
            .child
            .as_mut()
            .expect("the child is present until output or termination completes");
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (status, stdout, stderr) =
            tokio::try_join!(child.wait(), read_pipe(stdout), read_pipe(stderr))?;
        self.child = None;
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }

    async fn terminate(&mut self) {
        let Some(child) = self.child.take() else {
            return;
        };
        let grace = self.grace;
        #[cfg(unix)]
        {
            let pid = self.group;
            signal_group(pid, rustix::process::Signal::TERM);
            let _ = tokio::task::spawn_blocking(move || reap_group(child, pid, grace)).await;
        }
        #[cfg(not(unix))]
        {
            let mut child = child;
            let _ = grace;
            let _ = child.kill().await;
        }
    }
}

async fn read_pipe(pipe: Option<impl tokio::io::AsyncRead + Unpin>) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    if let Some(mut pipe) = pipe {
        pipe.read_to_end(&mut bytes).await?;
    }
    Ok(bytes)
}

impl Drop for ChildGroup {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(child) = self.child.take() {
            let pid = self.group;
            let grace = self.grace;
            signal_group(pid, rustix::process::Signal::TERM);
            let reaper = std::thread::Builder::new()
                .name("cli-deps-reaper".into())
                .spawn(move || reap_group(child, pid, grace));
            if reaper.is_err() {
                signal_group(pid, rustix::process::Signal::KILL);
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

/// Waits up to `grace` for the leader and the rest of its group to exit, then
/// sends SIGKILL to the group and reaps the leader.
///
/// A process group ID is not reused while the group has a member, and polling
/// stops as soon as the group is empty, so the signals reach only this group.
#[cfg(unix)]
fn reap_group(
    mut child: tokio::process::Child,
    pid: Option<rustix::process::Pid>,
    grace: Duration,
) {
    const POLL: Duration = Duration::from_millis(20);
    const KILL_WAIT: Duration = Duration::from_secs(1);
    let deadline = std::time::Instant::now() + grace;
    let group_alive =
        || pid.is_some_and(|pid| rustix::process::test_kill_process_group(pid).is_ok());
    loop {
        let leader_exited = !matches!(child.try_wait(), Ok(None));
        if leader_exited && !group_alive() {
            return;
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(POLL);
    }
    signal_group(pid, rustix::process::Signal::KILL);
    // Kill the leader directly too, in case it left its group or signalling the
    // group was refused, then reap it. A child that still has not exited (for
    // example, in uninterruptible sleep) is left to Tokio's orphan reaper.
    let _ = child.start_kill();
    let deadline = std::time::Instant::now() + KILL_WAIT;
    while matches!(child.try_wait(), Ok(None)) && std::time::Instant::now() < deadline {
        std::thread::sleep(POLL);
    }
}
