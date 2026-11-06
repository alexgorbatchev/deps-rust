use cli_deps::{
    CancellationToken, CommandRunner, Config, Dependency, Installer, Manager, ProcessRunner,
    ReleaseClient, YtDlpInstaller,
};
use std::{collections::BTreeSet, ffi::OsString};

fn unpacked_runtimes() -> BTreeSet<OsString> {
    std::fs::read_dir(std::env::temp_dir())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().starts_with("_MEI"))
        .collect()
}

#[tokio::test]
#[ignore = "downloads the current upstream yt-dlp release; run explicitly in native CI"]
async fn real_managed_release_leaves_no_unpacked_runtime() {
    let root = tempfile::tempdir().unwrap();
    let before = unpacked_runtimes();
    let token = CancellationToken::new();
    let installer = YtDlpInstaller {
        client: ReleaseClient::github().unwrap(),
    };
    installer.install(root.path(), &token).await.unwrap();
    let manager = Manager::new(Config {
        bin_dir: Some(root.path().to_owned()),
        search_path: Some(OsString::new()),
        dependencies: vec![Dependency::new("yt-dlp", "2020.01.01")],
        ..Default::default()
    })
    .unwrap();
    let executable = manager.executable("yt-dlp", &token).await.unwrap();
    assert!(executable.starts_with(root.path()));
    let output = ProcessRunner {
        path: Some(manager.child_path(&token).await.unwrap()),
        ..Default::default()
    }
    .run(executable.as_os_str(), &["--version".into()], &token)
    .await
    .unwrap();
    let version = String::from_utf8(output).unwrap();
    assert!(!version.trim().is_empty());
    let after = unpacked_runtimes();
    let added: Vec<_> = after.difference(&before).collect();
    assert!(
        added.is_empty(),
        "new unpacked runtime directories: {added:?}"
    );
    println!("Managed yt-dlp {} left no new _MEI entries", version.trim());
}
