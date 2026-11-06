// A harness-free test executable also serves as the native Windows subprocess
// fixture and archive payload. No scripts, shell tools, or environment mutation.
#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
fn main() {
    windows::main();
}

#[cfg(windows)]
mod windows {
    use axum::{Router, extract::Path as Route, http::StatusCode, routing::get};
    use cli_deps::{
        CancellationToken, CommandRunner, Config, Dependency, Error, Installer, Manager,
        ProcessRunner, ReleaseClient, YtDlpInstaller,
    };
    use std::{
        io::{Cursor, Write},
        os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
        path::{Path, PathBuf},
        process::Command,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    };
    use windows_sys::Win32::{
        Foundation::WAIT_OBJECT_0,
        System::{
            Console::{
                AllocConsole, CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent, SetConsoleCtrlHandler,
            },
            Threading::{
                INFINITE, OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess,
                WaitForSingleObject,
            },
        },
    };

    const WAIT: Duration = Duration::from_secs(15);
    const POLL: Duration = Duration::from_millis(10);
    static BREAK: AtomicBool = AtomicBool::new(false);

    pub fn main() {
        let args: Vec<_> = std::env::args().collect();
        match args.get(1).map(String::as_str) {
            Some("--output") => {
                let bytes = vec![b'x'; 256 * 1024];
                std::io::stdout().write_all(&bytes).unwrap();
                std::io::stderr().write_all(&bytes).unwrap();
            }
            Some("--failure") => {
                eprintln!("fixture command failed");
                std::process::exit(7);
            }
            Some("--version") => {
                let root = std::env::current_exe().unwrap();
                println!(
                    "{}",
                    std::fs::read_to_string(root.parent().unwrap().join("version")).unwrap()
                );
            }
            Some("--fixture-parent" | "--fixture-child") => fixture(&args),
            _ => {
                // This dedicated test process needs a console to exercise actual
                // CTRL_BREAK delivery on hosted runners. Failure can mean it
                // already has one; handler installation must still succeed.
                unsafe {
                    AllocConsole();
                }
                assert_ne!(unsafe { SetConsoleCtrlHandler(Some(control), 1) }, 0);
                let runtime = tokio::runtime::Runtime::new().unwrap();
                let installs = args.get(1).is_none_or(|arg| arg != "--processes");
                let processes = args.get(1).is_none_or(|arg| arg != "--installs");
                runtime.block_on(async {
                    if installs {
                        installs_remain_resolvable().await;
                    }
                    if processes {
                        command_results().await;
                        for abort in [false, true] {
                            for graceful in [false, true] {
                                cancellation_owns_tree(abort, graceful).await;
                            }
                        }
                    }
                });
                drop(runtime);
                if processes {
                    runtime_shutdown_owns_tree();
                }
                println!("Windows installation and process lifecycle tests passed");
            }
        }
    }

    unsafe extern "system" fn control(event: u32) -> i32 {
        if event == CTRL_BREAK_EVENT {
            BREAK.store(true, Ordering::SeqCst);
            1
        } else {
            0
        }
    }

    fn fixture(args: &[String]) {
        assert_ne!(unsafe { SetConsoleCtrlHandler(Some(control), 1) }, 0);
        let parent = args[1] == "--fixture-parent";
        let root = Path::new(&args[2]);
        let role = if parent { "parent" } else { "child" };
        std::fs::write(
            root.join(format!("{role}.pid")),
            std::process::id().to_string(),
        )
        .unwrap();
        let mut child = parent.then(|| {
            let child = Command::new(std::env::current_exe().unwrap())
                .args(["--fixture-child", &args[2], &args[3]])
                .spawn()
                .unwrap();
            std::fs::write(root.join("child.pid"), child.id().to_string()).unwrap();
            child
        });
        std::fs::write(root.join(format!("{role}.ready")), "ready").unwrap();
        loop {
            if BREAK.swap(false, Ordering::SeqCst) {
                std::fs::write(root.join(format!("{role}.break")), "received").unwrap();
                if args[3] == "graceful" {
                    // Cleanup output exceeds a pipe buffer; the owner must keep
                    // reading while it waits for graceful process-tree exit.
                    let bytes = vec![b'x'; 256 * 1024];
                    if parent {
                        std::io::stderr().write_all(&bytes).unwrap();
                    } else {
                        std::io::stdout().write_all(&bytes).unwrap();
                    }
                    if let Some(child) = &mut child {
                        assert!(child.wait().unwrap().success());
                    }
                    std::fs::write(root.join(format!("{role}.finished")), "clean").unwrap();
                    return;
                }
            }
            std::thread::sleep(POLL);
        }
    }

    fn archive(executable: &str, version: &str) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file(executable, options).unwrap();
        zip.write_all(&std::fs::read(std::env::current_exe().unwrap()).unwrap())
            .unwrap();
        zip.start_file("version", options).unwrap();
        zip.write_all(version.as_bytes()).unwrap();
        zip.start_file("_internal/runtime.dat", options).unwrap();
        zip.write_all(b"unpacked support file").unwrap();
        zip.finish().unwrap().into_inner()
    }

    fn manager(directory: &Path, search: std::ffi::OsString, minimum: &str) -> Manager {
        Manager::new(Config {
            bin_dir: Some(directory.to_owned()),
            search_path: Some(search),
            dependencies: vec![Dependency::new("yt-dlp", minimum)],
            ..Default::default()
        })
        .unwrap()
    }

    async fn version(entry: &Path) -> String {
        let output = ProcessRunner::default()
            .run(
                entry.as_os_str(),
                &["--version".into()],
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        String::from_utf8(output).unwrap().trim().to_owned()
    }

    // The test coordinator runs on Runtime::block_on's calling thread. Runtime
    // workers remain available while unwinding waits for these owned tasks.
    struct Background<T> {
        stop: CancellationToken,
        task: tokio::task::JoinHandle<T>,
    }

    impl<T: Send + 'static> Background<T> {
        fn spawn<F: Future<Output = T> + Send + 'static>(
            build: impl FnOnce(CancellationToken) -> F,
        ) -> Self {
            let stop = CancellationToken::new();
            let task = tokio::spawn(build(stop.clone()));
            Self { stop, task }
        }

        async fn finish(mut self) -> T {
            self.stop.cancel();
            (&mut self.task).await.unwrap()
        }
    }

    impl<T> Drop for Background<T> {
        fn drop(&mut self) {
            self.stop.cancel();
            while !self.task.is_finished() {
                std::thread::sleep(POLL);
            }
        }
    }

    async fn installs_remain_resolvable() {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        let assets = Arc::new(Mutex::new(archive("yt-dlp.exe", "2026.01.01")));
        let server_assets = assets.clone();
        let app = Router::new().route(
            "/yt-dlp/yt-dlp/releases/latest/download/{asset}",
            get(move |Route(name): Route<String>| {
                let assets = server_assets.clone();
                async move {
                    if name == "yt-dlp_win.zip" {
                        Ok(assets.lock().unwrap().clone())
                    } else {
                        Err(StatusCode::NOT_FOUND)
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client =
            ReleaseClient::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let server = Background::spawn(move |shutdown| async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
                .unwrap();
        });
        let token = CancellationToken::new();
        let managed = manager(&bin, "".into(), "2026.01.01");
        std::fs::write(bin.join("yt-dlp.exe"), b"obsolete one-file executable").unwrap();
        let first = client
            .install_yt_dlp_for_platform("windows", "amd64", &bin, &token)
            .await
            .unwrap();
        assert!(!bin.join("yt-dlp.exe").exists());
        assert_eq!(version(&first).await, "2026.01.01");
        assert_eq!(managed.executable("yt-dlp", &token).await.unwrap(), first);
        let first_path = managed.child_path(&token).await.unwrap();

        for next in ["2026.02.01", "2026.03.01"] {
            *assets.lock().unwrap() = archive("yt-dlp.exe", next);
            let installer = YtDlpInstaller {
                client: client.clone(),
            };
            installer.update(&bin, &token).await.unwrap();
            let selected = managed.executable("yt-dlp", &token).await.unwrap();
            assert_eq!(
                version(&selected).await,
                next,
                "an existing Manager must see updates"
            );
            assert_eq!(
                version(&first).await,
                "2026.01.01",
                "an earlier returned path stays usable"
            );
            let pinned = which::which_in("yt-dlp", Some(&first_path), root.path()).unwrap();
            assert_eq!(
                version(&pinned).await,
                "2026.01.01",
                "an earlier child PATH stays usable"
            );
        }

        // Continuously resolve while writers publish. No lookup may observe an
        // absent or partially extracted install, and retained results remain valid.
        let reader_bin = bin.clone();
        let reader = Background::spawn(move |reader_done| async move {
            let manager = manager(&reader_bin, "".into(), "2026.01.01");
            let mut count = 0;
            while !reader_done.is_cancelled() {
                let entry = manager
                    .executable("yt-dlp", &CancellationToken::new())
                    .await
                    .unwrap();
                assert_eq!(version(&entry).await, "2026.03.01");
                count += 1;
            }
            count
        });
        for _ in 0..3 {
            let installs = (0..3)
                .map(|_| client.install_yt_dlp_for_platform("windows", "amd64", &bin, &token));
            for result in futures::future::join_all(installs).await {
                result.unwrap();
            }
        }
        assert!(reader.finish().await > 0);

        *assets.lock().unwrap() = b"corrupt archive".to_vec();
        assert!(
            client
                .install_yt_dlp_for_platform("windows", "amd64", &bin, &token)
                .await
                .is_err()
        );
        assert_eq!(
            version(&managed.executable("yt-dlp", &token).await.unwrap()).await,
            "2026.03.01"
        );
        let global = root.path().join("global");
        std::fs::create_dir(&global).unwrap();
        std::fs::copy(std::env::current_exe().unwrap(), global.join("yt-dlp.exe")).unwrap();
        std::fs::write(global.join("version"), "2026.04.01").unwrap();
        let search = std::env::join_paths([first.parent().unwrap(), global.as_path()]).unwrap();
        let global_manager = manager(&bin, search, "2026.01.01");
        assert_eq!(
            global_manager.executable("yt-dlp", &token).await.unwrap(),
            global.join("yt-dlp.exe")
        );
        std::fs::write(global.join("version"), "2025.01.01").unwrap();
        let selected = global_manager.executable("yt-dlp", &token).await.unwrap();
        assert_eq!(version(&selected).await, "2026.03.01");
        assert_eq!(
            which::which_in(
                "yt-dlp",
                Some(global_manager.child_path(&token).await.unwrap()),
                root.path()
            )
            .unwrap(),
            selected
        );
        let selector = bin.join(".yt-dlp/active");
        let previous = std::fs::read(&selector).unwrap();
        for invalid in ["../outside", "install-missing"] {
            std::fs::write(&selector, invalid).unwrap();
            assert!(managed.executable("yt-dlp", &token).await.is_err());
            assert!(managed.child_path(&token).await.is_err());
        }
        std::fs::write(&selector, previous).unwrap();
        assert_eq!(version(&first).await, "2026.01.01");
        server.finish().await;
    }

    async fn command_results() {
        let runner = ProcessRunner::default();
        let executable = std::env::current_exe().unwrap();
        let token = CancellationToken::new();
        let output = runner
            .run(executable.as_os_str(), &["--output".into()], &token)
            .await
            .unwrap();
        assert_eq!(output, vec![b'x'; 256 * 1024]);
        let error = runner
            .run(executable.as_os_str(), &["--failure".into()], &token)
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Command { status, stderr, .. }
            if status.code() == Some(7) && stderr.contains("fixture command failed")));
        let root = tempfile::tempdir().unwrap();
        assert!(matches!(
            runner
                .run(root.path().join("missing.exe").as_os_str(), &[], &token)
                .await,
            Err(Error::Io(_))
        ));
        token.cancel();
        assert!(matches!(
            runner
                .run(executable.as_os_str(), &["--failure".into()], &token)
                .await,
            Err(Error::Cancelled)
        ));
    }

    // Independent safety net: assertions observe the runner's cleanup before
    // this guard acts. A broken runner must fail without leaking the fixtures.
    struct FixtureTree {
        root: tempfile::TempDir,
        token: CancellationToken,
        task: tokio::task::JoinHandle<cli_deps::Result<Vec<u8>>>,
        handles: [Option<(u32, OwnedHandle)>; 2],
    }

    impl FixtureTree {
        fn start(runtime: &tokio::runtime::Handle, graceful: bool) -> Self {
            let root = tempfile::tempdir().unwrap();
            let token = CancellationToken::new();
            let task = runtime.spawn(run_tree(root.path().to_owned(), token.clone(), graceful));
            Self {
                root,
                token,
                task,
                handles: [None, None],
            }
        }

        fn capture_handles(&mut self) {
            for (role, slot) in ["parent", "child"].into_iter().zip(&mut self.handles) {
                if slot.is_some() {
                    continue;
                }
                let Some(pid) =
                    std::fs::read_to_string(self.root.path().join(format!("{role}.pid")))
                        .ok()
                        .and_then(|pid| pid.parse::<u32>().ok())
                else {
                    continue;
                };
                // SAFETY: request only wait/termination rights for this fixture.
                let handle =
                    unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE, 0, pid) };
                if !handle.is_null() {
                    // SAFETY: OpenProcess returned a new owned handle. Retain it
                    // through teardown so PID reuse cannot redirect termination.
                    *slot = Some((pid, unsafe { OwnedHandle::from_raw_handle(handle) }));
                }
            }
        }

        async fn wait_ready(&mut self) {
            tokio::time::timeout(WAIT, async {
                loop {
                    self.capture_handles();
                    if self.handles.iter().all(Option::is_some)
                        && ["parent", "child"]
                            .iter()
                            .all(|role| self.root.path().join(format!("{role}.ready")).exists())
                    {
                        return;
                    }
                    tokio::time::sleep(POLL).await;
                }
            })
            .await
            .expect("fixture tree did not become ready");
        }

        fn all_exited(&self) -> bool {
            self.handles
                .iter()
                .flatten()
                .all(|(_, handle)| exited(handle))
        }
    }

    fn exited(handle: &OwnedHandle) -> bool {
        // SAFETY: the handle remains owned throughout the wait.
        unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) == WAIT_OBJECT_0 }
    }

    impl Drop for FixtureTree {
        fn drop(&mut self) {
            self.token.cancel();
            self.task.abort();
            // The async runner must finish dropping before scratch can go away.
            while !self.task.is_finished() {
                std::thread::sleep(POLL);
            }
            self.capture_handles();
            if let Some((pid, handle)) = &self.handles[0]
                && !exited(handle)
            {
                // SAFETY: this held process handle identifies our fixture group.
                unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, *pid) };
            }
            let deadline = Instant::now() + Duration::from_secs(1);
            while Instant::now() < deadline {
                self.capture_handles();
                if self.handles.iter().all(Option::is_some) && self.all_exited() {
                    return;
                }
                std::thread::sleep(POLL);
            }
            for (_, handle) in self.handles.iter().flatten() {
                if !exited(handle) {
                    // SAFETY: this exact fixture process is still held by its
                    // handle. Cleanup waits for exit before deleting its files.
                    unsafe { TerminateProcess(handle.as_raw_handle(), 1) };
                    assert_eq!(
                        unsafe { WaitForSingleObject(handle.as_raw_handle(), INFINITE) },
                        WAIT_OBJECT_0,
                        "fixture cleanup could not confirm process exit"
                    );
                }
            }
        }
    }

    async fn run_tree(
        root: PathBuf,
        token: CancellationToken,
        graceful: bool,
    ) -> cli_deps::Result<Vec<u8>> {
        ProcessRunner {
            termination_grace: Duration::from_secs(1),
            ..Default::default()
        }
        .run(
            std::env::current_exe().unwrap().as_os_str(),
            &[
                "--fixture-parent".into(),
                root.into_os_string().into_string().unwrap(),
                if graceful { "graceful" } else { "ignore" }.into(),
            ],
            &token,
        )
        .await
    }

    fn assert_finished(tree: &FixtureTree, graceful: bool) {
        let root = tree.root.path();
        for (role, slot) in ["parent", "child"].iter().zip(&tree.handles) {
            let (_, handle) = slot.as_ref().unwrap();
            assert!(exited(handle), "{role} still alive");
            assert!(
                root.join(format!("{role}.break")).exists(),
                "{role} did not receive CTRL_BREAK"
            );
            assert_eq!(root.join(format!("{role}.finished")).exists(), graceful);
        }
    }

    async fn cancellation_owns_tree(abort: bool, graceful: bool) {
        let mut tree = FixtureTree::start(&tokio::runtime::Handle::current(), graceful);
        tree.wait_ready().await;
        let started = Instant::now();
        if abort {
            tree.task.abort();
        } else {
            tree.token.cancel();
        }
        let result = tokio::time::timeout(WAIT, &mut tree.task).await.unwrap();
        if abort {
            assert!(result.unwrap_err().is_cancelled());
            // An aborted async future cannot be awaited for cleanup. Observe
            // owned native handles; runtime shutdown is tested separately below.
            tokio::time::timeout(WAIT, async {
                while !tree.all_exited() {
                    tokio::time::sleep(POLL).await;
                }
            })
            .await
            .unwrap();
        } else {
            assert!(matches!(result.unwrap(), Err(Error::Cancelled)));
        }
        if !graceful {
            assert!(started.elapsed() >= Duration::from_secs(1));
        }
        assert_finished(&tree, graceful);
    }

    fn runtime_shutdown_owns_tree() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut tree = FixtureTree::start(runtime.handle(), false);
        runtime.block_on(tree.wait_ready());
        tree.task.abort();
        drop(runtime);
        assert_finished(&tree, false);
    }
}
