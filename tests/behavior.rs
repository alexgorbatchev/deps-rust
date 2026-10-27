use cli_deps::{
    ReleaseClient, compare_versions, default_version_parser, managed_path, sanitize_stderr,
};
use std::{ffi::OsStr, io::Write};

#[test]
fn versions_parse_banners_and_compare_semver_calver_and_development() {
    for (name, output, expected) in [
        ("yt-dlp", "yt-dlp 2026.08.23\nmore", "2026.08.23"),
        ("ffmpeg", "ffmpeg version n6.1.1 Copyright", "6.1.1"),
        ("remarkable", "remarkable version v1.2.3", "1.2.3"),
        ("tool", "tool v2.0\nextra", "2.0"),
        ("tool", "version v2.0", "2.0"),
        ("ffmpeg", "development", "development"),
        ("yt-dlp", "unknown", "unknown"),
    ] {
        assert_eq!(default_version_parser(name, output), expected);
    }
    for (actual, min, ok) in [
        ("1.2", "1.2.0", true),
        ("1.2.0-rc1", "1.2.0", false),
        ("2026.08.23", "2026.08.22", true),
        ("dev", "999", true),
        ("git-master", "999", true),
        ("6.0", "6.1", false),
        ("", "9", true),
        ("main", "9", true),
        ("N-12345", "9", true),
        ("v2.1", "v2.0", true),
        ("@1.2", "@1.3", false),
        ("@1.2.1", "@1.2", true),
        ("@unknown", "@9", true),
    ] {
        assert_eq!(
            compare_versions(actual, min).is_ok(),
            ok,
            "{actual} >= {min}"
        );
    }
}

#[test]
fn stderr_and_path_cleaning_preserve_meaning() {
    assert_eq!(
        sanitize_stderr("Traceback (most recent call last):\n  File a\nValueError: bad"),
        "ValueError: bad"
    );
    assert_eq!(sanitize_stderr("panic: broken\nstack"), "panic: broken");
    assert_eq!(
        sanitize_stderr("notice\nERROR: denied\nstack"),
        "ERROR: denied"
    );
    assert_eq!(sanitize_stderr("first\n\n last "), "last");
    assert_eq!(sanitize_stderr("  "), "");
    assert_eq!(
        sanitize_stderr("Traceback (most recent call last):"),
        "Traceback (most recent call last):"
    );
    assert_eq!(
        sanitize_stderr("Error: bad\n    at function (a:1)"),
        "Error: bad"
    );
    let path = managed_path(
        std::path::Path::new("/managed/bin"),
        Some(OsStr::new("/usr/bin")),
    )
    .unwrap();
    assert_eq!(
        std::env::split_paths(&path).next().unwrap(),
        std::path::Path::new("/managed/bin")
    );
    assert_eq!(
        managed_path(std::path::Path::new("/managed/bin"), Some(&path)).unwrap(),
        path
    );
    #[cfg(unix)]
    assert!(managed_path(std::path::Path::new("/invalid:path"), None).is_err());
}

#[tokio::test]
async fn release_discovery_uses_head_and_install_replaces_only_requested_file() {
    use axum::{
        Router,
        http::{StatusCode, header::LOCATION},
        routing::get,
    };
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(5);
    header.set_mode(0o755);
    header.set_cksum();
    tar.append_data(&mut header, "nested/tool", &b"hello"[..])
        .unwrap();
    let bytes = tar.into_inner().unwrap();
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(&bytes).unwrap();
    let archive = gzip.finish().unwrap();
    let app = Router::new()
        .route(
            "/owner/repo/releases/latest",
            get(|| async {
                (
                    StatusCode::FOUND,
                    [(LOCATION, "/owner/repo/releases/tag/v1.2.3")],
                )
            }),
        )
        .route(
            "/asset.tar.gz",
            get(move || {
                let bytes = archive.clone();
                async move { bytes }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = ReleaseClient::new(&url).unwrap();
    let token = tokio_util::sync::CancellationToken::new();
    assert_eq!(
        client.latest_tag("owner", "repo", &token).await.unwrap(),
        "v1.2.3"
    );
    std::fs::create_dir_all(".tmp").unwrap();
    let dir = tempfile::tempdir_in(".tmp").unwrap();
    std::fs::write(dir.path().join("tool"), "old").unwrap();
    client
        .install_asset(&format!("{url}/asset.tar.gz"), "tool", dir.path(), &token)
        .await
        .unwrap();
    assert_eq!(std::fs::read(dir.path().join("tool")).unwrap(), b"hello");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    server.abort();
}
