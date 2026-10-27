use axum::{
    Router,
    http::{StatusCode, header},
    response::Redirect,
    routing::get,
};
use cli_deps::{CancellationToken, Error, ReleaseChannel, Update, Updater};
use std::io::Write;

fn archive() -> Vec<u8> {
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut tar = tar::Builder::new(encoder);
    let bytes = b"#!/bin/sh\nprintf 'new-version\\n'\n";
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    tar.append_data(&mut header, "fetch-track", &bytes[..])
        .unwrap();
    tar.into_inner().unwrap().finish().unwrap()
}

#[tokio::test]
async fn explicit_release_channel_updates_only_fixture_and_current_is_success() {
    let bytes = archive();
    let app = Router::new()
        .route(
            "/owner/rust-track/releases/latest",
            get(|| async { Redirect::to("/owner/rust-track/releases/tag/v9.0.0") }),
        )
        .route(
            "/owner/rust-track/releases/download/v9.0.0/{asset}",
            get(move || {
                let bytes = bytes.clone();
                async move { ([(header::CONTENT_TYPE, "application/gzip")], bytes) }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let updater = Updater {
        channel: Some(ReleaseChannel {
            client: cli_deps::ReleaseClient::new(&base).unwrap(),
            owner: "owner".into(),
            repository: "rust-track".into(),
        }),
    };
    let temp = tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/.tmp")).unwrap();
    let destination = temp.path().join("fetch-track");
    std::fs::write(&destination, b"original").unwrap();
    let token = CancellationToken::new();
    assert_eq!(
        updater
            .update_to("0.1.0", &destination, &token)
            .await
            .unwrap(),
        Update::Installed("9.0.0".into())
    );
    let output = std::process::Command::new(&destination).output().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"new-version\n");
    let before = std::fs::read(&destination).unwrap();
    assert_eq!(
        updater
            .update_to("9.0.0", &destination, &token)
            .await
            .unwrap(),
        Update::Current("9.0.0".into())
    );
    assert_eq!(std::fs::read(&destination).unwrap(), before);
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(matches!(
        updater.update_to("0.1.0", &destination, &cancelled).await,
        Err(Error::Cancelled)
    ));
    assert!(matches!(
        Updater::default()
            .update_to("0.1.0", &destination, &token)
            .await,
        Err(Error::Unpublished)
    ));
    assert!(matches!(
        Updater::default().update_running("0.1.0", &token).await,
        Err(Error::Unpublished)
    ));
    server.abort();
}
#[tokio::test]
async fn broken_release_keeps_previous_executable() {
    let app = Router::new()
        .route(
            "/owner/rust-track/releases/latest",
            get(|| async { Redirect::to("/owner/rust-track/releases/tag/v9.0.0") }),
        )
        .fallback(|| async { (StatusCode::OK, b"not an archive") });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let updater = Updater {
        channel: Some(ReleaseChannel {
            client: cli_deps::ReleaseClient::new(&base).unwrap(),
            owner: "owner".into(),
            repository: "rust-track".into(),
        }),
    };
    let temp = tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/.tmp")).unwrap();
    let destination = temp.path().join("fetch-track");
    let mut file = std::fs::File::create(&destination).unwrap();
    file.write_all(b"keep-original").unwrap();
    assert!(
        updater
            .update_to("0.1.0", &destination, &CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(destination).unwrap(), b"keep-original");
    server.abort();
}

#[test]
fn release_channel_validates_native_transport_and_repository_segments() {
    for (base, owner, repository) in [
        ("::::", "owner", "repo"),
        ("http://example.org", "owner", "repo"),
        ("https://user@example.org", "owner", "repo"),
        ("https://example.org?token=x", "owner", "repo"),
        ("https://example.org#fragment", "owner", "repo"),
        ("https://example.org", "..", "repo"),
        ("https://example.org", "owner", "a/b"),
        ("https://example.org", "", "repo"),
    ] {
        assert!(ReleaseChannel::new(base, owner, repository).is_err());
    }
    for base in [
        "https://example.org",
        "http://localhost:8080",
        "http://127.0.0.1:8080",
        "http://[::1]:8080",
    ] {
        assert!(ReleaseChannel::new(base, "owner", "rust-repo").is_ok());
    }
}

#[tokio::test]
async fn updater_pins_checked_tag_when_latest_changes_during_download() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let requests = Arc::new(AtomicUsize::new(0));
    let seen = requests.clone();
    let bytes = archive();
    let app = Router::new()
        .route(
            "/owner/rust-track/releases/latest",
            get(move || {
                let call = seen.fetch_add(1, Ordering::SeqCst);
                async move {
                    Redirect::to(if call == 0 {
                        "/owner/rust-track/releases/tag/v9.0.0"
                    } else {
                        "/owner/rust-track/releases/tag/v10.0.0"
                    })
                }
            }),
        )
        .route(
            "/owner/rust-track/releases/download/v9.0.0/{asset}",
            get(move || {
                let bytes = bytes.clone();
                async { bytes }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let updater = Updater {
        channel: Some(ReleaseChannel::new(&base, "owner", "rust-track").unwrap()),
    };
    let temp = tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/.tmp")).unwrap();
    let target = temp.path().join("fetch-track");
    assert_eq!(
        updater
            .update_to("dev", &target, &CancellationToken::new())
            .await
            .unwrap(),
        Update::Installed("9.0.0".into())
    );
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert_eq!(
        std::process::Command::new(target).output().unwrap().stdout,
        b"new-version\n"
    );
    server.abort();
}
