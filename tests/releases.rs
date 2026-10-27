use axum::{
    Router,
    http::{StatusCode, header::LOCATION},
    routing::get,
};
use cli_deps::{
    CancellationToken, Installer, ReleaseChannel, ReleaseClient, ReleaseInstaller, Update, Updater,
};
use std::io::Write;

fn archive(binary: &str, contents: &[u8]) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(contents.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    tar.append_data(&mut header, binary, contents).unwrap();
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(&tar.into_inner().unwrap()).unwrap();
    gzip.finish().unwrap()
}
fn temp() -> tempfile::TempDir {
    std::fs::create_dir_all(".tmp").unwrap();
    tempfile::tempdir_in(".tmp").unwrap()
}
async fn server(binary: &str, contents: &[u8]) -> (String, tokio::task::JoinHandle<()>) {
    let bytes = archive(binary, contents);
    let os = if std::env::consts::OS == "macos" {
        "darwin"
    } else {
        std::env::consts::OS
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        arch => arch,
    };
    let endpoint =
        format!("/owner/repo/releases/download/v2.0.0/{binary}_2.0.0_{os}_{arch}.tar.gz");
    let app = Router::new()
        .route(
            "/owner/repo/releases/latest",
            get(|| async {
                (
                    StatusCode::FOUND,
                    [(LOCATION, "/owner/repo/releases/tag/v2.0.0")],
                )
            }),
        )
        .route(
            &endpoint,
            get(move || {
                let bytes = bytes.clone();
                async move { bytes }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, task)
}

#[cfg(unix)]
#[tokio::test]
async fn release_fallback_installer_self_upgrade_and_latest_guard() {
    let (url, task) = server("tool", b"replacement").await;
    let client = ReleaseClient::new(&url).unwrap();
    let dir = temp();
    let token = CancellationToken::new();
    let installer = ReleaseInstaller {
        client: client.clone(),
        owner: "owner".into(),
        repo: "repo".into(),
        binary: "tool".into(),
    };
    installer.install(dir.path(), &token).await.unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("tool")).unwrap(),
        b"replacement"
    );
    std::fs::write(dir.path().join("tool"), "old").unwrap();
    let updater = Updater {
        channel: Some(ReleaseChannel::new(&url, "owner", "repo").unwrap()),
    };
    assert_eq!(
        updater
            .update_to("1.0", &dir.path().join("tool"), &token)
            .await
            .unwrap(),
        Update::Installed("2.0.0".into())
    );
    assert_eq!(
        std::fs::read(dir.path().join("tool")).unwrap(),
        b"replacement"
    );
    for current in ["2.0.0", "3.0.0"] {
        assert_eq!(
            updater
                .update_to(current, &dir.path().join("tool"), &token)
                .await
                .unwrap(),
            Update::Current(current.into())
        );
    }
    assert!(
        updater
            .update_to("dev", std::path::Path::new("/"), &token)
            .await
            .is_err()
    );
    assert!(
        client
            .install_release("owner", "missing", "tool", dir.path(), &token)
            .await
            .is_err()
    );
    token.cancel();
    assert!(installer.install(dir.path(), &token).await.is_err());
    task.abort();
}

#[cfg(unix)]
#[test]
fn self_update_subprocess_helper() {
    let Ok(url) = std::env::var("CLI_DEPS_SELF_UPDATE_URL") else {
        return;
    };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        assert_eq!(
            Updater {
                channel: Some(ReleaseChannel::new(&url, "owner", "repo").unwrap())
            }
            .update_running("dev", &CancellationToken::new())
            .await
            .unwrap(),
            Update::Installed("2.0.0".into())
        );
    });
}

#[cfg(unix)]
#[tokio::test]
async fn self_update_replaces_an_actual_running_executable() {
    let dir = temp();
    let executable = dir.path().join("child-test");
    std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
    let (url, task) = server("child-test", b"#!/bin/sh\nprintf 'updated\\n'\n").await;
    let output = tokio::process::Command::new(&executable)
        .args(["--exact", "self_update_subprocess_helper", "--nocapture"])
        .env("CLI_DEPS_SELF_UPDATE_URL", url)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = tokio::process::Command::new(&executable)
        .output()
        .await
        .unwrap();
    assert_eq!(output.stdout, b"updated\n");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    task.abort();
}
