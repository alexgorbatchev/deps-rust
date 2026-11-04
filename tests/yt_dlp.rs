#![cfg(unix)]
use axum::{Router, extract::Path as Route, http::StatusCode, routing::get};
use cli_deps::{CancellationToken, Error, Installer, ReleaseClient, YtDlpInstaller};
use std::{
    io::{Cursor, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use zip::write::SimpleFileOptions;

fn temp() -> tempfile::TempDir {
    std::fs::create_dir_all(".tmp").unwrap();
    tempfile::tempdir_in(".tmp").unwrap()
}

/// An unpacked build: the executable at the root plus a support folder.
fn unpacked(executable: &str, script: &str) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let data = SimpleFileOptions::default().unix_permissions(0o644);
    zip.add_directory("_internal/", data.unix_permissions(0o755))
        .unwrap();
    zip.start_file("_internal/library.dylib", data.unix_permissions(0o755))
        .unwrap();
    zip.write_all(b"library").unwrap();
    zip.start_file("_internal/data.txt", data).unwrap();
    zip.write_all(b"data").unwrap();
    zip.add_symlink("_internal/Current", "data.txt", data)
        .unwrap();
    zip.start_file(executable, data.unix_permissions(0o755))
        .unwrap();
    zip.write_all(script.as_bytes()).unwrap();
    zip.finish().unwrap().into_inner()
}

/// Release assets by name, replaceable while the server runs.
type Assets = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

/// Serves `/yt-dlp/yt-dlp/releases/latest/download/{asset}` from `assets`.
async fn server(
    assets: Assets,
    requests: Arc<AtomicUsize>,
) -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new().route(
        "/yt-dlp/yt-dlp/releases/latest/download/{asset}",
        get(move |Route(asset): Route<String>| {
            let assets = assets.clone();
            let requests = requests.clone();
            async move {
                requests.fetch_add(1, Ordering::SeqCst);
                let found = assets
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|(name, _)| *name == asset)
                    .map(|(_, bytes)| bytes.clone());
                found.ok_or(StatusCode::NOT_FOUND)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, task)
}

/// Runs a just-installed entry point. On Linux, a child forked concurrently by
/// another test can briefly hold the descriptor that wrote it, so `exec` fails
/// with ETXTBSY until that child execs.
fn run(entry: &Path) -> String {
    for _ in 0..100 {
        match std::process::Command::new(entry).arg("--version").output() {
            Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            output => {
                let output = output.unwrap();
                assert!(output.status.success());
                return String::from_utf8(output.stdout).unwrap();
            }
        }
    }
    panic!("{} stayed busy", entry.display());
}

/// Install folders under `.yt-dlp`, sorted.
fn install_dirs(directory: &Path) -> Vec<PathBuf> {
    let mut dirs = std::fs::read_dir(directory.join(".yt-dlp"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    dirs.sort();
    dirs
}

/// The install folder the managed `yt-dlp` link points to.
fn current_install(directory: &Path) -> PathBuf {
    let target = std::fs::read_link(directory.join("yt-dlp")).unwrap();
    directory.join(target.parent().unwrap())
}

#[tokio::test]
async fn yt_dlp_installs_the_unpacked_build_selected_for_each_platform() {
    let cases = [
        ("darwin", "arm64", "yt-dlp_macos.zip", "yt-dlp_macos"),
        ("macos", "x86_64", "yt-dlp_macos.zip", "yt-dlp_macos"),
        ("linux", "amd64", "yt-dlp_linux.zip", "yt-dlp_linux"),
        ("linux", "x86_64", "yt-dlp_linux.zip", "yt-dlp_linux"),
        (
            "linux",
            "arm64",
            "yt-dlp_linux_aarch64.zip",
            "yt-dlp_linux_aarch64",
        ),
        (
            "linux",
            "aarch64",
            "yt-dlp_linux_aarch64.zip",
            "yt-dlp_linux_aarch64",
        ),
        ("windows", "amd64", "yt-dlp_win.zip", "yt-dlp.exe"),
        ("windows", "386", "yt-dlp_win_x86.zip", "yt-dlp_x86.exe"),
        (
            "windows",
            "arm64",
            "yt-dlp_win_arm64.zip",
            "yt-dlp_arm64.exe",
        ),
    ];
    let assets = cases
        .iter()
        .map(|(_, _, archive, executable)| {
            let script = format!("#!/bin/sh\nprintf '%s\\n' '{archive}'\n");
            (archive.to_string(), unpacked(executable, &script))
        })
        .chain([(
            "yt-dlp".to_string(),
            b"#!/bin/sh\nprintf 'zipapp\\n'\n".to_vec(),
        )])
        .collect();
    let (url, task) = server(Arc::new(Mutex::new(assets)), Arc::default()).await;
    let client = ReleaseClient::new(&url).unwrap();
    let token = CancellationToken::new();
    for (os, arch, archive, _) in cases {
        let dir = temp();
        let entry = client
            .install_yt_dlp_for_platform(os, arch, dir.path(), &token)
            .await
            .unwrap();
        assert_eq!(entry, dir.path().join("yt-dlp"));
        assert_eq!(run(&entry).trim(), archive, "{os}/{arch}");
        let target = std::fs::read_link(&entry).unwrap();
        assert!(target.is_relative());
        assert!(target.starts_with(".yt-dlp"));
        assert_eq!(target.file_name().unwrap(), "yt-dlp");
        let install = dir.path().join(target.parent().unwrap());
        assert_eq!(install_dirs(dir.path()), vec![install.clone()]);
        let support = install.join("_internal");
        assert_eq!(std::fs::read(support.join("Current")).unwrap(), b"data");
        assert_eq!(
            std::fs::read_link(support.join("Current")).unwrap(),
            Path::new("data.txt")
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = |name: &str| {
            std::fs::metadata(support.join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode("library.dylib"), 0o755);
        assert_eq!(mode("data.txt"), 0o644);
    }
    let dir = temp();
    std::fs::create_dir_all(dir.path().join(".yt-dlp/install-old")).unwrap();
    let entry = client
        .install_yt_dlp_for_platform("freebsd", "amd64", dir.path(), &token)
        .await
        .unwrap();
    assert!(
        std::fs::symlink_metadata(&entry)
            .unwrap()
            .file_type()
            .is_file()
    );
    assert_eq!(run(&entry), "zipapp\n");
    assert!(!dir.path().join(".yt-dlp").exists());
    task.abort();
}

#[tokio::test]
async fn yt_dlp_replaces_one_file_install_and_updates_by_downloading_again() {
    let dir = temp();
    let calls = dir.path().join("calls");
    let script = |version: &str| {
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nprintf '%s\\n' '{version}'\n",
            calls.display()
        )
    };
    let (os, arch) = match std::env::consts::OS {
        "macos" => ("darwin", "arm64"),
        _ => ("linux", std::env::consts::ARCH),
    };
    let (archive, executable) = if os == "darwin" {
        ("yt-dlp_macos.zip", "yt-dlp_macos")
    } else if arch == "aarch64" {
        ("yt-dlp_linux_aarch64.zip", "yt-dlp_linux_aarch64")
    } else {
        ("yt-dlp_linux.zip", "yt-dlp_linux")
    };
    let assets = Arc::new(Mutex::new(vec![(
        archive.to_string(),
        unpacked(executable, &script("2026.01.01")),
    )]));
    let requests = Arc::new(AtomicUsize::new(0));
    let (url, task) = server(assets.clone(), requests.clone()).await;
    let installer = YtDlpInstaller {
        client: ReleaseClient::new(&url).unwrap(),
    };
    let entry = dir.path().join("yt-dlp");
    std::fs::write(&entry, "obsolete one-file build").unwrap();
    let token = CancellationToken::new();
    installer.install(dir.path(), &token).await.unwrap();
    assert!(std::fs::symlink_metadata(&entry).unwrap().is_symlink());
    assert_eq!(run(&entry), "2026.01.01\n");
    let first = current_install(dir.path());
    assert_eq!(install_dirs(dir.path()), std::slice::from_ref(&first));

    assets.lock().unwrap()[0].1 = unpacked(executable, &script("2026.02.02"));
    installer.update(dir.path(), &token).await.unwrap();
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    assert_eq!(run(&entry), "2026.02.02\n");
    let second = current_install(dir.path());
    let mut kept = vec![first.clone(), second.clone()];
    kept.sort();
    assert_eq!(
        install_dirs(dir.path()),
        kept,
        "the replaced install stays for runs still using it"
    );

    assets.lock().unwrap()[0].1 = unpacked(executable, &script("2026.02.03"));
    installer.update(dir.path(), &token).await.unwrap();
    assert_eq!(run(&entry), "2026.02.03\n");
    let third = current_install(dir.path());
    let mut kept = vec![second.clone(), third.clone()];
    kept.sort();
    assert_eq!(install_dirs(dir.path()), kept, "older installs are removed");
    assert_eq!(
        std::fs::read_to_string(&calls).unwrap(),
        "--version\n--version\n--version\n",
        "updating never runs yt-dlp -U"
    );

    let mut escaping = zip::ZipWriter::new(Cursor::new(Vec::new()));
    escaping
        .start_file("../escape", SimpleFileOptions::default())
        .unwrap();
    let escaping = escaping.finish().unwrap().into_inner();
    let broken = [
        b"not a zip".to_vec(),
        escaping,
        unpacked("yt-dlp_other", &script("2026.03.03")),
    ];
    for bytes in broken {
        assets.lock().unwrap()[0].1 = bytes;
        assert!(installer.update(dir.path(), &token).await.is_err());
        assert_eq!(run(&entry), "2026.02.03\n");
        assert_eq!(install_dirs(dir.path()), kept);
        assert!(!dir.path().join("escape").exists());
    }
    assets.lock().unwrap().clear();
    assert!(installer.update(dir.path(), &token).await.is_err());
    assert_eq!(run(&entry), "2026.02.03\n");
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(matches!(
        installer.update(dir.path(), &cancelled).await,
        Err(Error::Cancelled)
    ));
    assert_eq!(install_dirs(dir.path()), kept);
    let leftovers = std::fs::read_dir(dir.path().join(".yt-dlp"))
        .unwrap()
        .map(|e| e.unwrap())
        .filter(|e| e.file_type().unwrap().is_file() && e.file_name() != ".lock")
        .count();
    assert_eq!(leftovers, 0, "temporary downloads are removed");
    task.abort();
}

#[tokio::test]
async fn concurrent_yt_dlp_installs_leave_a_working_install() {
    let (os, arch) = match std::env::consts::OS {
        "macos" => ("darwin", "arm64"),
        _ => ("linux", "amd64"),
    };
    let archive = if os == "darwin" {
        ("yt-dlp_macos.zip", "yt-dlp_macos")
    } else {
        ("yt-dlp_linux.zip", "yt-dlp_linux")
    };
    let bytes = unpacked(archive.1, "#!/bin/sh\nprintf 'ok\\n'\n");
    let assets = Arc::new(Mutex::new(vec![(archive.0.to_string(), bytes)]));
    let (url, task) = server(assets, Arc::default()).await;
    let client = ReleaseClient::new(&url).unwrap();
    let dir = temp();
    let token = CancellationToken::new();
    for _ in 0..3 {
        let installs =
            (0..4).map(|_| client.install_yt_dlp_for_platform(os, arch, dir.path(), &token));
        for result in futures::future::join_all(installs).await {
            result.unwrap();
        }
        assert_eq!(run(&dir.path().join("yt-dlp")), "ok\n");
        let dirs = install_dirs(dir.path());
        assert!(dirs.len() <= 2, "{dirs:?}");
        assert!(dirs.contains(&current_install(dir.path())));
    }
    task.abort();
}
