use cli_deps::{
    CancellationToken, Config, Dependency, EnsureOptions, Installer, Manager, PackageInstaller,
    ProcessRunner, ensure_managed_bin_dir, is_agent_mode, managed_bin_dir,
};
use std::{io::Cursor, path::PathBuf, sync::Arc};

#[test]
fn isolated_environment_helper() {
    let Ok(case) = std::env::var("CLI_DEPS_ENV_CASE") else {
        return;
    };
    let root = PathBuf::from(std::env::var_os("CLI_DEPS_TEST_ROOT").unwrap());
    match case.as_str() {
        "xdg" => {
            let expected = root.join("data/app/bin");
            assert_eq!(managed_bin_dir("app").unwrap(), expected);
            assert_eq!(ensure_managed_bin_dir("app").unwrap(), expected);
            assert!(expected.is_dir());
            let manager = Manager::new(Config {
                app_name: "app".into(),
                ..Default::default()
            })
            .unwrap();
            assert_eq!(manager.bin_dir(), expected);
        }
        "home" => {
            assert_eq!(
                managed_bin_dir("app").unwrap(),
                root.join(".local/share/app/bin")
            );
        }
        "agent" | "human" => {
            assert_eq!(is_agent_mode(), case == "agent");
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime.block_on(async {
                let manager = Manager::new(Config {
                    bin_dir: Some(root.join("bin")),
                    dependencies: vec![Dependency::new("absent-cli-deps-tool", "1")],
                    ..Default::default()
                })
                .unwrap();
                let token = CancellationToken::new();
                let report = manager.verify(&token).await.unwrap().remove(0);
                if case == "agent" {
                    assert!(report.error.contains("install version 1"));
                }
                let mut output = Vec::new();
                assert!(
                    manager
                        .ensure(
                            EnsureOptions::default(),
                            &mut Cursor::new(b"yes\n"),
                            &mut output,
                            &token
                        )
                        .await
                        .is_err()
                );
                if case == "agent" {
                    assert!(output.is_empty(), "agent mode must not prompt");
                } else {
                    assert!(String::from_utf8(output).unwrap().contains("[y/N]"));
                }
            });
        }
        "package" | "no-package" => {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            let installer = PackageInstaller {
                package: "example-package".into(),
                runner: Arc::new(ProcessRunner::default()),
            };
            let result = runtime.block_on(installer.install(&root, &CancellationToken::new()));
            if case == "package" {
                result.unwrap();
            } else {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("no supported package manager")
                );
            }
        }
        _ => panic!("unknown test case"),
    }
}

fn invoke(case: &str, root: &std::path::Path, extras: &[(&str, &std::ffi::OsStr)]) {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "isolated_environment_helper", "--nocapture"])
        .env("CLI_DEPS_ENV_CASE", case)
        .env("CLI_DEPS_TEST_ROOT", root)
        .env_remove("XDG_DATA_HOME")
        .env("HOME", root)
        .env("AGENT", "0");
    for (name, value) in extras {
        command.env(name, value);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn managed_directories_follow_xdg_and_home_and_agent_mode_never_prompts() {
    std::fs::create_dir_all(".tmp").unwrap();
    let dir = tempfile::tempdir_in(".tmp").unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    invoke(
        "xdg",
        &root,
        &[("XDG_DATA_HOME", root.join("data").as_os_str())],
    );
    invoke("home", &root, &[]);
    invoke("human", &root, &[]);
    for value in ["1", "true", "YES"] {
        invoke("agent", &root, &[("AGENT", value.as_ref())]);
    }
    for invalid in ["", "..", "../outside", "/absolute", "a/b"] {
        assert!(managed_bin_dir(invalid).is_err());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn package_installers_select_available_native_manager_and_preserve_arguments() {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(".tmp").unwrap();
    let dir = tempfile::tempdir_in(".tmp").unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let record = root.join("arguments");
    for (program, expected) in [
        ("apt-get", "install\n-y\nexample-package\n"),
        ("pacman", "-S\n--noconfirm\nexample-package\n"),
        ("dnf", "install\n-y\nexample-package\n"),
    ] {
        let executable = root.join(program);
        std::fs::write(
            &executable,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$CLI_DEPS_RECORD\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        invoke(
            "package",
            &root,
            &[
                ("PATH", root.as_os_str()),
                ("CLI_DEPS_RECORD", record.as_os_str()),
            ],
        );
        assert_eq!(std::fs::read_to_string(&record).unwrap(), expected);
        std::fs::remove_file(executable).unwrap();
    }
    invoke("no-package", &root, &[("PATH", root.as_os_str())]);
}
