#![cfg(unix)]
use cli_deps::{CancellationToken, CommandRunner, Error, ProcessRunner};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const WAIT: Duration = Duration::from_secs(10);

fn temp() -> tempfile::TempDir {
    std::fs::create_dir_all(".tmp").unwrap();
    tempfile::tempdir_in(".tmp").unwrap()
}

/// Writes a script that starts a grandchild in its process group, records the
/// SIGTERM it traps, and reports both PIDs once ready. A stubborn script keeps
/// running after SIGTERM, and its grandchild ignores SIGTERM.
fn script(dir: &Path, stubborn: bool) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::fs::canonicalize(dir).unwrap();
    let path = dir.join("child.sh");
    let record = dir.join("record");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\n\
             {ignore}\
             sleep 30 &\n\
             trap 'echo term >> \"{record}\"; {on_term}' TERM\n\
             echo \"$$ $!\" > \"{dir}/pids.tmp\"\n\
             mv \"{dir}/pids.tmp\" \"{dir}/pids\"\n\
             while :; do wait; done\n",
            record = record.display(),
            dir = dir.display(),
            ignore = if stubborn { "trap '' TERM\n" } else { "" },
            on_term = if stubborn { ":" } else { "exit 0" },
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

async fn pids(dir: &Path) -> Vec<i32> {
    let path = dir.join("pids");
    let deadline = Instant::now() + WAIT;
    while !path.exists() {
        assert!(Instant::now() < deadline, "child never became ready");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    std::fs::read_to_string(path)
        .unwrap()
        .split_whitespace()
        .map(|pid| pid.parse().unwrap())
        .collect()
}

fn alive(pid: i32) -> bool {
    rustix::process::test_kill_process(rustix::process::Pid::from_raw(pid).unwrap()).is_ok()
}

async fn wait_until_exited(pids: &[i32]) {
    let deadline = Instant::now() + WAIT;
    while pids.iter().any(|pid| alive(*pid)) {
        assert!(Instant::now() < deadline, "processes {pids:?} survived");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn records(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("record")).unwrap_or_default()
}

/// Cancels `token` once the child reports ready and returns the child's PIDs.
async fn cancel_when_ready(dir: &Path, token: CancellationToken) -> Vec<i32> {
    let pids = pids(dir).await;
    token.cancel();
    pids
}

#[tokio::test]
async fn cancellation_sends_sigterm_to_the_process_group_first() {
    let dir = temp();
    let child = script(dir.path(), false);
    let runner = ProcessRunner {
        termination_grace: WAIT,
        ..Default::default()
    };
    let token = CancellationToken::new();
    let started = Instant::now();
    let (result, pids) = tokio::join!(
        runner.run(child.as_os_str(), &[], &token),
        cancel_when_ready(dir.path(), token.clone()),
    );
    assert!(matches!(result, Err(Error::Cancelled)));
    assert!(
        started.elapsed() < WAIT,
        "the child exited on SIGTERM without waiting for SIGKILL"
    );
    assert_eq!(records(dir.path()), "term\n");
    wait_until_exited(&pids).await;
}

#[tokio::test]
async fn cancellation_sends_sigkill_after_the_grace_period() {
    let dir = temp();
    let child = script(dir.path(), true);
    let grace = Duration::from_millis(300);
    let runner = ProcessRunner {
        termination_grace: grace,
        ..Default::default()
    };
    let token = CancellationToken::new();
    let (result, pids) = tokio::join!(
        async {
            let result = runner.run(child.as_os_str(), &[], &token).await;
            (result, Instant::now())
        },
        async {
            let pids = cancel_when_ready(dir.path(), token.clone()).await;
            (pids, Instant::now())
        },
    );
    let ((result, finished), (pids, cancelled)) = (result, pids);
    assert!(matches!(result, Err(Error::Cancelled)));
    assert!(
        finished - cancelled >= grace,
        "SIGKILL waits for the grace period"
    );
    assert_eq!(records(dir.path()), "term\n");
    assert!(!alive(pids[0]), "the leader is killed and reaped");
    wait_until_exited(&pids[1..]).await;
}

#[tokio::test]
async fn dropping_a_running_command_terminates_its_process_group() {
    let dir = temp();
    let child = script(dir.path(), false);
    let runner = ProcessRunner {
        termination_grace: WAIT,
        ..Default::default()
    };
    let token = CancellationToken::new();
    let pids = tokio::select! {
        result = runner.run(child.as_os_str(), &[], &token) => panic!("child finished: {result:?}"),
        pids = pids(dir.path()) => pids,
    };
    wait_until_exited(&pids).await;
    assert_eq!(records(dir.path()), "term\n");
}

#[tokio::test]
async fn completed_commands_return_output_written_by_background_members() {
    let runner = ProcessRunner::default();
    let output = runner
        .run(
            "sh".as_ref(),
            &["-c".into(), "(sleep 0.1; echo late) & echo early".into()],
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(output, b"early\nlate\n");
}

#[tokio::test]
async fn cancellation_reaches_members_left_after_the_leader_exits() {
    let dir = temp();
    let pids_file = std::fs::canonicalize(dir.path()).unwrap().join("pids");
    let runner = ProcessRunner {
        termination_grace: WAIT,
        ..Default::default()
    };
    let token = CancellationToken::new();
    let args = [
        "-c".into(),
        "sleep 30 & echo $! > \"$1.tmp\"; mv \"$1.tmp\" \"$1\"".into(),
        "sh".into(),
        pids_file.display().to_string(),
    ];
    let started = Instant::now();
    let (result, pids) = tokio::join!(
        runner.run("sh".as_ref(), &args, &token),
        cancel_when_ready(dir.path(), token.clone()),
    );
    assert!(matches!(result, Err(Error::Cancelled)));
    wait_until_exited(&pids).await;
    assert!(started.elapsed() < WAIT, "the member exited on SIGTERM");
}

#[tokio::test]
async fn children_lead_their_own_session() {
    let dir = temp();
    let pids_file = std::fs::canonicalize(dir.path()).unwrap().join("pids");
    let args = [
        "-c".into(),
        "echo $$ > \"$1.tmp\"; mv \"$1.tmp\" \"$1\"; exec sleep 30".into(),
        "sh".into(),
        pids_file.display().to_string(),
    ];
    let token = CancellationToken::new();
    let runner = ProcessRunner::default();
    let check = async {
        let pid = pids(dir.path()).await[0];
        let session = rustix::process::getsid(rustix::process::Pid::from_raw(pid)).unwrap();
        token.cancel();
        session.as_raw_nonzero().get() == pid
    };
    let (result, leads_session) = tokio::join!(runner.run("sh".as_ref(), &args, &token), check,);
    assert!(matches!(result, Err(Error::Cancelled)));
    assert!(leads_session, "the child leads a new session");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn executables_still_open_for_writing_are_retried_until_closed() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp();
    let path = std::fs::canonicalize(dir.path()).unwrap().join("tool");
    let mut writer = std::fs::File::create(&path).unwrap();
    std::io::Write::write_all(&mut writer, b"#!/bin/sh\necho ran\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let close = async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(writer);
    };
    let runner = ProcessRunner::default();
    let token = CancellationToken::new();
    let (output, ()) = tokio::join!(runner.run(path.as_os_str(), &[], &token), close);
    assert_eq!(output.unwrap(), b"ran\n");
}
