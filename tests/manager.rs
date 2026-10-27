use cli_deps::{
    Cache, CancellationToken, CommandRunner, Config, Dependency, EnsureOptions, Error, InstallerFn,
    Manager, MemoryCache, ProcessRunner,
};
use std::{
    io::Cursor,
    path::{Path, PathBuf},
    sync::Arc,
};

fn temp() -> tempfile::TempDir {
    std::fs::create_dir_all(".tmp").unwrap();
    tempfile::tempdir_in(".tmp").unwrap()
}

#[cfg(unix)]
fn executable(path: &Path, version: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, format!("#!/bin/sh\nprintf '%s\\n' '{version}'\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn manager_installs_checks_cache_invalidates_and_updates_real_executables() {
    let dir = temp();
    let mut dependency = Dependency::new("example-tool", "2.0");
    dependency.installer = Some(Arc::new(InstallerFn(
        |dir: PathBuf, _: CancellationToken| async move {
            executable(&dir.join("example-tool"), "2.1.0");
            Ok(())
        },
    )));
    let manager = Manager::new(Config {
        dependencies: vec![dependency],
        bin_dir: Some(dir.path().to_owned()),
        cache: Some(Arc::new(MemoryCache::default())),
        ..Default::default()
    })
    .unwrap();
    let token = CancellationToken::new();
    let report = manager.verify(&token).await.unwrap().remove(0);
    assert!(!report.installed);
    assert!(!report.satisfied);
    assert!(report.summary().contains("not installed"));
    manager
        .ensure(
            EnsureOptions { auto_install: true },
            &mut Cursor::new(Vec::<u8>::new()),
            &mut Vec::<u8>::new(),
            &token,
        )
        .await
        .unwrap();
    let report = manager.verify(&token).await.unwrap().remove(0);
    assert!(report.satisfied);
    assert!(report.summary().contains("version 2.1.0"));
    executable(&dir.path().join("example-tool"), "0.5.0");
    assert!(
        manager.verify(&token).await.unwrap()[0].satisfied,
        "cache retains last verified banner"
    );
    manager.update("EXAMPLE-TOOL", &token).await.unwrap();
    assert!(manager.verify(&token).await.unwrap()[0].satisfied);
    assert_eq!(
        manager.update_all(&token).await.unwrap().completed,
        vec!["example-tool"]
    );
    assert!(manager.install("unknown", &token).await.is_err());
    token.cancel();
    assert!(matches!(
        manager.verify(&token).await,
        Err(Error::Cancelled)
    ));
}

#[tokio::test]
async fn ensure_eof_rejects_install_and_batches_keep_partial_failures() {
    let dir = temp();
    let mut fail = Dependency::new("missing-failure-12345", "1");
    fail.installer = Some(Arc::new(InstallerFn(
        |_: PathBuf, _: CancellationToken| async move { Err(Error::Message("installer failed".into())) },
    )));
    let mut successful = Dependency::new("missing-success-12345", "1");
    successful.installer = Some(Arc::new(InstallerFn(
        |_: PathBuf, _: CancellationToken| async move { Ok(()) },
    )));
    let manager = Manager::new(Config {
        dependencies: vec![fail, successful],
        bin_dir: Some(dir.path().into()),
        ..Default::default()
    })
    .unwrap();
    let token = CancellationToken::new();
    let mut output = Vec::new();
    assert!(matches!(
        manager
            .ensure(
                EnsureOptions::default(),
                &mut Cursor::new(Vec::<u8>::new()),
                &mut output,
                &token
            )
            .await,
        Err(Error::Unsatisfied(_))
    ));
    let outcome = manager.install_unsatisfied(&token).await.unwrap();
    assert_eq!(outcome.completed, vec!["missing-success-12345"]);
    assert_eq!(outcome.failures[0].name, "missing-failure-12345");
    assert!(
        outcome.failures[0]
            .error
            .to_string()
            .contains("installer failed")
    );
    assert!(!outcome.is_success());
    assert!(matches!(outcome.into_result(), Err(Error::Batch(1))));
    assert!(matches!(
        manager
            .ensure(
                EnsureOptions { auto_install: true },
                &mut Cursor::new(b""),
                &mut Vec::new(),
                &token
            )
            .await,
        Err(Error::Batch(1))
    ));
}

struct BrokenCache;
impl Cache for BrokenCache {
    fn get(&self, _: &str) -> cli_deps::Result<Option<String>> {
        Err(Error::Message("cache read failed".into()))
    }
    fn put(&self, _: &str, _: &str) -> cli_deps::Result<()> {
        Err(Error::Message("cache write failed".into()))
    }
    fn delete(&self, _: &str) -> cli_deps::Result<()> {
        Err(Error::Message("cache delete failed".into()))
    }
}

#[cfg(unix)]
#[tokio::test]
async fn validation_cache_failures_prompt_consent_and_post_install_verification() {
    let dir = temp();
    for names in [vec![""], vec!["tool", "TOOL"]] {
        assert!(
            Manager::new(Config {
                bin_dir: Some(dir.path().into()),
                dependencies: names
                    .into_iter()
                    .map(|name| Dependency::new(name, "1"))
                    .collect(),
                ..Default::default()
            })
            .is_err()
        );
    }
    executable(&dir.path().join("tool"), "2.0");
    let mut dependency = Dependency::new("tool", "1");
    dependency.installer = Some(Arc::new(InstallerFn(
        |_: PathBuf, _: CancellationToken| async { Ok(()) },
    )));
    let manager = Manager::new(Config {
        bin_dir: Some(dir.path().into()),
        dependencies: vec![dependency],
        cache: Some(Arc::new(BrokenCache)),
        ..Default::default()
    })
    .unwrap();
    let token = CancellationToken::new();
    assert!(
        manager
            .verify(&token)
            .await
            .unwrap_err()
            .to_string()
            .contains("cache read failed")
    );
    assert!(
        manager
            .update("tool", &token)
            .await
            .unwrap_err()
            .to_string()
            .contains("cache delete failed")
    );
    let mut dependency = Dependency::new("missing-after-install", "1");
    dependency.version_args = Vec::new();
    dependency.installer = Some(Arc::new(InstallerFn(
        |_: PathBuf, _: CancellationToken| async { Ok(()) },
    )));
    let manager = Manager::new(Config {
        bin_dir: Some(dir.path().into()),
        dependencies: vec![dependency],
        ..Default::default()
    })
    .unwrap();
    let mut output = Vec::new();
    assert!(matches!(
        manager
            .ensure(
                EnsureOptions { auto_install: true },
                &mut Cursor::new(b"yes\n"),
                &mut output,
                &token
            )
            .await,
        Err(Error::Unsatisfied(_))
    ));
    assert!(
        String::from_utf8(output)
            .unwrap()
            .contains("Installed: missing-after-install")
    );
    assert_eq!(
        manager
            .update_all(&token)
            .await
            .unwrap()
            .into_result()
            .unwrap(),
        vec!["missing-after-install"]
    );
    let mut dependency = Dependency::new("cancel-install", "1");
    dependency.installer = Some(Arc::new(InstallerFn(
        |_: PathBuf, token: CancellationToken| async move {
            token.cancel();
            Err(Error::Cancelled)
        },
    )));
    let manager = Manager::new(Config {
        bin_dir: Some(dir.path().into()),
        dependencies: vec![dependency],
        ..Default::default()
    })
    .unwrap();
    assert!(matches!(
        manager.update_all(&token).await,
        Err(Error::Cancelled)
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn satisfied_ensure_does_not_prompt_and_failed_commands_are_reports() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp();
    executable(&dir.path().join("ok-tool"), "2.0");
    std::fs::write(
        dir.path().join("failed-tool"),
        "#!/bin/sh\nprintf 'ERROR: failed\\n' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(
        dir.path().join("failed-tool"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let token = CancellationToken::new();
    let manager = Manager::new(Config {
        bin_dir: Some(dir.path().into()),
        dependencies: vec![Dependency::new("failed-tool", "1")],
        ..Default::default()
    })
    .unwrap();
    assert!(
        manager.verify(&token).await.unwrap()[0]
            .error
            .contains("ERROR: failed")
    );
    let manager = Manager::new(Config {
        bin_dir: Some(dir.path().into()),
        dependencies: vec![Dependency::new("ok-tool", "1")],
        ..Default::default()
    })
    .unwrap();
    let mut output = Vec::new();
    manager
        .ensure(
            EnsureOptions::default(),
            &mut Cursor::new(b""),
            &mut output,
            &token,
        )
        .await
        .unwrap();
    assert!(output.is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn subprocess_errors_are_sanitized_and_cancellation_interrupts_waiting() {
    let runner = ProcessRunner::default();
    let token = CancellationToken::new();
    let error = runner
        .run(
            "sh",
            &[
                "-c".into(),
                "printf 'panic: broken\\nstack\\n' >&2; exit 3".into(),
            ],
            &token,
        )
        .await
        .unwrap_err();
    match error {
        Error::Command { stderr, status, .. } => {
            assert_eq!(stderr, "panic: broken");
            assert_eq!(status.code(), Some(3));
        }
        error => panic!("unexpected {error}"),
    }
    let cancelled = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cancelled.cancel();
    });
    assert!(matches!(
        runner.run("sleep", &["30".into()], &token).await,
        Err(Error::Cancelled)
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn version_check_distinguishes_outdated_invalid_and_process_failure() {
    let dir = temp();
    executable(&dir.path().join("old"), "1.0.0");
    executable(&dir.path().join("invalid"), "");
    executable(&dir.path().join("custom"), "release: 3.0.0");
    let mut custom = Dependency::new("custom", "2");
    custom.parse_version = Some(Arc::new(|_, output| {
        output.trim().trim_start_matches("release: ").to_owned()
    }));
    let manager = Manager::new(Config {
        bin_dir: Some(dir.path().into()),
        dependencies: vec![
            Dependency::new("old", "2"),
            Dependency::new("invalid", "1"),
            custom,
        ],
        ..Default::default()
    })
    .unwrap();
    let reports = manager.verify(&CancellationToken::new()).await.unwrap();
    assert!(reports[0].installed && !reports[0].satisfied);
    assert!(reports[0].summary().contains("installed 1.0.0"));
    assert!(reports[1].error.contains("could not parse"));
    assert!(reports[2].satisfied);
    assert!(
        manager
            .install("old", &CancellationToken::new())
            .await
            .unwrap_err()
            .to_string()
            .contains("no installer")
    );
}
