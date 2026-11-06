#![cfg(target_os = "linux")]

use cli_deps::{CancellationToken, CommandRunner, Error, ProcessRunner};
use rustix::process::{Pid, Signal, WaitOptions};
use std::{
    io,
    path::PathBuf,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const WAIT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(10);

// This integration-test binary owns all its children. Linux subreaping lets it
// retain an exited grandchild in the process group until the test releases it,
// exercising the actual post-SIGKILL wait without mocking process operations.
#[test]
fn cleanup_retains_ownership_until_the_killed_group_is_reaped()
-> Result<(), Box<dyn std::error::Error>> {
    rustix::process::set_child_subreaper(Pid::from_raw(1))?;
    for cancel in [true, false] {
        let mut fixture = Fixture::new()?;
        let path = fixture.dir.as_ref().unwrap().path().join("pids");
        let task = fixture
            .runtime
            .as_ref()
            .unwrap()
            .spawn(command(path, fixture.token.clone()));
        fixture.wait_ready()?;
        if cancel {
            fixture.token.cancel();
        } else {
            task.abort();
        }
        fixture.wait_for_zombie()?;

        let runtime = fixture.runtime.take().unwrap();
        let (finished, receiver) = mpsc::channel();
        fixture.shutdown = Some(thread::spawn(move || {
            let result = runtime.block_on(task);
            drop(runtime);
            let _ = finished.send(result); // The fixture may already be unwinding.
        }));
        let result = fixture.observe_completion(receiver)?;
        if cancel {
            assert!(matches!(result, Ok(Err(Error::Cancelled))));
        } else {
            assert!(result.unwrap_err().is_cancelled());
        }
    }

    // A handle can outlive its runtime. Entering it makes spawn_blocking reject
    // the cleanup task, exercising Reaper's destructor instead of its run body.
    let mut fixture = Fixture::new()?;
    let path = fixture.dir.as_ref().unwrap().path().join("pids");
    let mut run = Box::pin(command(path.clone(), fixture.token.clone()));
    fixture.runtime.as_ref().unwrap().block_on(async {
        tokio::select! {
            result = &mut run => panic!("command finished before cancellation: {result:?}"),
            ready = tokio::time::timeout(WAIT, async {
                while std::fs::read_to_string(&path).unwrap_or_default().lines().count() != 2 {
                    tokio::time::sleep(POLL).await;
                }
            }) => ready,
        }
    })?;
    fixture.wait_ready()?;
    let runtime = fixture.runtime.take().unwrap();
    let handle = runtime.handle().clone();
    let (finished, receiver) = mpsc::channel();
    fixture.shutdown = Some(thread::spawn(move || {
        drop(runtime);
        let _entered = handle.enter();
        drop(run);
        let _ = finished.send(());
    }));
    fixture.wait_for_zombie()?;
    fixture.observe_completion(receiver)?;
    rustix::process::set_child_subreaper(None)?;
    Ok(())
}

async fn command(path: PathBuf, token: CancellationToken) -> cli_deps::Result<Vec<u8>> {
    ProcessRunner {
        termination_grace: Duration::from_millis(50),
        ..Default::default()
    }
    .run(
        "sh".as_ref(),
        &[
            "-c".into(),
            "set -e; echo $$ > \"$1\"; trap '' TERM; sleep 30 & echo $! >> \"$1\"; wait".into(),
            "sh".into(),
            path.into_os_string().into_string().unwrap(),
        ],
        &token,
    )
    .await
}

struct Fixture {
    dir: Option<tempfile::TempDir>,
    runtime: Option<tokio::runtime::Runtime>,
    token: CancellationToken,
    group: Option<Pid>,
    shutdown: Option<thread::JoinHandle<()>>,
    finished: bool,
}

impl Fixture {
    fn new() -> io::Result<Self> {
        Ok(Self {
            dir: Some(tempfile::tempdir()?),
            runtime: Some(tokio::runtime::Runtime::new()?),
            token: CancellationToken::new(),
            group: None,
            shutdown: None,
            finished: false,
        })
    }

    fn observe_completion<T>(
        &mut self,
        receiver: mpsc::Receiver<T>,
    ) -> Result<T, Box<dyn std::error::Error>> {
        // The old implementation abandoned this still-populated group after a
        // one-second final wait. The channel observes completion, not scheduling.
        let premature = match receiver.recv_timeout(Duration::from_millis(1500)) {
            Ok(result) => Some(result),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(error) => return Err(error.into()),
        };
        self.reap_members()?;
        let completed_early = premature.is_some();
        let result = match premature {
            Some(result) => result,
            None => receiver.recv_timeout(WAIT)?,
        };
        self.finish()?;
        assert!(!completed_early, "cleanup completed with an unreaped group");
        Ok(result)
    }

    fn pids(&mut self) -> io::Result<Vec<Pid>> {
        let path = self.dir.as_ref().unwrap().path().join("pids");
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let pids = contents
            .split_whitespace()
            .map(|value| {
                value
                    .parse()
                    .ok()
                    .and_then(Pid::from_raw)
                    .ok_or_else(|| io::Error::other("invalid fixture PID"))
            })
            .collect::<io::Result<Vec<_>>>()?;
        self.group = pids.first().copied();
        Ok(pids)
    }

    fn wait_ready(&mut self) -> io::Result<()> {
        let deadline = Instant::now() + WAIT;
        while self.pids()?.len() != 2 {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "fixture did not start",
                ));
            }
            thread::sleep(POLL);
        }
        Ok(())
    }

    fn wait_for_zombie(&mut self) -> io::Result<()> {
        let member = self.pids()?[1];
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some((state, parent, _)) = process_state(member)?
                && state == "Z"
                && parent == std::process::id() as i32
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "member did not exit",
                ));
            }
            thread::sleep(POLL);
        }
    }

    fn reap_members(&self) -> io::Result<()> {
        let Some(group) = self.group else {
            return Ok(());
        };
        // On failure the shell could be killed between spawning and recording
        // its member. Discover only this fixture's adopted group members, and
        // leave the leader to Tokio's Child handle.
        for entry in std::fs::read_dir("/proc")? {
            let entry = entry?;
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse().ok())
                .and_then(Pid::from_raw)
            else {
                continue;
            };
            if pid == group
                && !(self.runtime.is_none()
                    && self
                        .shutdown
                        .as_ref()
                        .is_some_and(|task| task.is_finished()))
            {
                continue;
            }
            if let Some((state, parent, member_group)) = process_state(pid)?
                && state == "Z"
                && parent == std::process::id() as i32
                && member_group == group.as_raw_nonzero().get()
            {
                rustix::process::waitpid(Some(pid), WaitOptions::NOHANG)?;
            }
        }
        Ok(())
    }

    fn finish(&mut self) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.token.cancel();
        self.pids()?;
        if let Some(group) = self.group {
            let _ = rustix::process::kill_process_group(group, Signal::TERM);
            let kill_at = Instant::now() + Duration::from_millis(200);
            let deadline = Instant::now() + WAIT;
            loop {
                self.reap_members()?;
                match rustix::process::test_kill_process_group(group) {
                    Err(rustix::io::Errno::SRCH) => break,
                    Err(error) => return Err(error.into()),
                    Ok(()) => {}
                }
                if Instant::now() >= kill_at {
                    let _ = rustix::process::kill_process_group(group, Signal::KILL);
                }
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "fixture group survived",
                    ));
                }
                thread::sleep(POLL);
            }
        }
        drop(self.runtime.take());
        if let Some(shutdown) = self.shutdown.take() {
            shutdown
                .join()
                .map_err(|_| io::Error::other("runtime shutdown panicked"))?;
        }
        self.finished = true;
        Ok(())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            if let Some(dir) = self.dir.take() {
                eprintln!(
                    "fixture cleanup failed: {error}; retained {}",
                    dir.keep().display()
                );
            }
            if let Some(runtime) = self.runtime.take() {
                runtime.shutdown_timeout(WAIT);
            }
        }
    }
}

fn process_state(pid: Pid) -> io::Result<Option<(String, i32, i32)>> {
    let path = PathBuf::from(format!("/proc/{}/stat", pid.as_raw_nonzero()));
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let (_, fields) = contents
        .rsplit_once(") ")
        .ok_or_else(|| io::Error::other("invalid process stat"))?;
    let mut fields = fields.split_whitespace();
    let state = fields
        .next()
        .ok_or_else(|| io::Error::other("missing process state"))?;
    let parent = fields
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| io::Error::other("missing parent PID"))?;
    let group = fields
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| io::Error::other("missing group PID"))?;
    Ok(Some((state.into(), parent, group)))
}
