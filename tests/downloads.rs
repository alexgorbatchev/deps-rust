use axum::{
    Router,
    http::{StatusCode, header::LOCATION},
    routing::get,
};
use cli_deps::{CancellationToken, Error, Installer, ProcessRunner, ReleaseClient, YtDlpInstaller};
use std::{
    io::{Cursor, Write},
    sync::Arc,
};

fn temp() -> tempfile::TempDir {
    std::fs::create_dir_all(".tmp").unwrap();
    tempfile::tempdir_in(".tmp").unwrap()
}
async fn server(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, server)
}

#[tokio::test]
async fn zip_raw_bad_archive_and_http_failure_preserve_existing_binary() {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    zip.start_file("dir/tool", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(b"zipped").unwrap();
    let zip = zip.finish().unwrap().into_inner();
    let (url, task) = server(
        Router::new()
            .route(
                "/tool.zip",
                get(move || {
                    let zip = zip.clone();
                    async move { zip }
                }),
            )
            .route("/raw", get(|| async { b"raw".to_vec() }))
            .route("/bad.tar.gz", get(|| async { b"garbage".to_vec() }))
            .route("/missing", get(|| async { StatusCode::NOT_FOUND })),
    )
    .await;
    let client = ReleaseClient::new(&url).unwrap();
    let dir = temp();
    let token = CancellationToken::new();
    client
        .install_asset(&format!("{url}/tool.zip"), "tool", dir.path(), &token)
        .await
        .unwrap();
    assert_eq!(std::fs::read(dir.path().join("tool")).unwrap(), b"zipped");
    client
        .install_asset(&format!("{url}/raw"), "tool", dir.path(), &token)
        .await
        .unwrap();
    for endpoint in ["/bad.tar.gz", "/missing"] {
        assert!(
            client
                .install_asset(&format!("{url}{endpoint}"), "tool", dir.path(), &token)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(dir.path().join("tool")).unwrap(), b"raw");
    }
    assert!(
        client
            .install_asset(&format!("{url}/raw"), "../escape", dir.path(), &token)
            .await
            .is_err()
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    token.cancel();
    assert!(matches!(
        client
            .install_asset(&format!("{url}/raw"), "tool", dir.path(), &token)
            .await,
        Err(Error::Cancelled)
    ));
    task.abort();
}

#[tokio::test]
async fn release_get_fallback_invalid_discovery_and_slow_request_cancellation() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = hits.clone();
    let route = get(move || {
        seen.fetch_add(1, Ordering::SeqCst);
        async { (StatusCode::FOUND, [(LOCATION, "/tags/v2")]) }
    })
    .head(|| async { StatusCode::METHOD_NOT_ALLOWED });
    let (url, task) = server(
        Router::new()
            .route("/owner/repo/releases/latest", route)
            .route(
                "/owner/slow/releases/latest",
                get(|| async {
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    StatusCode::NOT_FOUND
                }),
            ),
    )
    .await;
    let client = ReleaseClient::new(&url).unwrap();
    let token = CancellationToken::new();
    assert_eq!(
        client.latest_tag("owner", "repo", &token).await.unwrap(),
        "v2"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert!(client.latest_tag("owner", "missing", &token).await.is_err());
    let cancelled = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cancelled.cancel();
    });
    assert!(matches!(
        client.latest_tag("owner", "slow", &token).await,
        Err(Error::Cancelled)
    ));
    task.abort();
}

#[tokio::test]
async fn yt_dlp_platform_selection_and_native_update_fallback() {
    let (url, task) = server(
        Router::new()
            .route(
                "/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_linux",
                get(|| async { "linux" }),
            )
            .route(
                "/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_linux_aarch64",
                get(|| async { "arm" }),
            )
            .route(
                "/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_macos",
                get(|| async { "mac" }),
            )
            .route(
                "/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe",
                get(|| async { "windows" }),
            ),
    )
    .await;
    let client = ReleaseClient::new(&url).unwrap();
    let dir = temp();
    let token = CancellationToken::new();
    for (os, arch, binary, bytes) in [
        ("linux", "amd64", "yt-dlp", "linux"),
        ("linux", "arm64", "yt-dlp", "arm"),
        ("darwin", "arm64", "yt-dlp", "mac"),
        ("windows", "amd64", "yt-dlp.exe", "windows"),
    ] {
        client
            .install_yt_dlp_for_platform(os, arch, dir.path(), &token)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join(binary)).unwrap(),
            bytes
        );
    }
    let runner = ProcessRunner {
        path: Some(std::ffi::OsString::new()),
    };
    let installer = YtDlpInstaller {
        client,
        runner: Arc::new(runner),
    };
    installer.update(dir.path(), &token).await.unwrap();
    token.cancel();
    assert!(matches!(
        installer.update(dir.path(), &token).await,
        Err(Error::Cancelled)
    ));
    task.abort();
}
