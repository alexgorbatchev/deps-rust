use crate::{CancellationToken, Error, Result, cancellable, message};
use async_trait::async_trait;
use std::{
    ffi::{OsStr, OsString},
    path::{Component, Path, PathBuf},
};

pub fn is_agent_mode() -> bool {
    std::env::var("AGENT")
        .is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

pub fn managed_bin_dir(app_name: &str) -> Result<PathBuf> {
    if app_name.trim().is_empty()
        || Path::new(app_name)
            .components()
            .any(|p| !matches!(p, Component::Normal(_)))
        || Path::new(app_name).components().count() != 1
    {
        return Err(message("app_name must be one nonempty path segment"));
    }
    let data = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            if cfg!(windows) {
                std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
            } else {
                None
            }
        })
        .or_else(|| dirs::home_dir().map(|p| p.join(".local/share")))
        .ok_or_else(|| message("cannot resolve home directory"))?;
    Ok(data.join(app_name).join("bin"))
}

pub fn ensure_managed_bin_dir(app_name: &str) -> Result<PathBuf> {
    let path = managed_bin_dir(app_name)?;
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

/// Returns a child-process PATH without mutating the process environment.
pub fn managed_path(bin_dir: &Path, current: Option<&OsStr>) -> Result<OsString> {
    let mut paths = current
        .map(std::env::split_paths)
        .map(Iterator::collect::<Vec<_>>)
        .unwrap_or_default();
    if !paths.iter().any(|p| p == bin_dir) {
        paths.insert(0, bin_dir.to_owned());
    }
    std::env::join_paths(paths).map_err(|e| message(format!("building managed PATH: {e}")))
}

pub fn sanitize_stderr(stderr: &str) -> String {
    let raw = stderr.trim();
    let lines = || raw.lines().map(str::trim).filter(|s| !s.is_empty());
    if raw.contains("Traceback (most recent call last):") {
        return lines()
            .rfind(|s| !s.starts_with("File ") && !s.starts_with("Traceback"))
            .unwrap_or(raw)
            .to_owned();
    }
    if let Some(index) = raw.find("ERROR:") {
        return raw[index..].lines().next().unwrap_or("").trim().to_owned();
    }
    if raw.contains("panic:")
        && let Some(line) = lines().find(|s| s.starts_with("panic:"))
    {
        return line.to_owned();
    }
    if raw.contains("\n    at ") || raw.contains("\n\tat ") {
        return lines()
            .filter(|s| !s.starts_with("at "))
            .collect::<Vec<_>>()
            .join(" ");
    }
    lines().next_back().unwrap_or("").to_owned()
}

#[async_trait]
pub trait CommandRunner: Send + Sync {
    async fn run(&self, name: &str, args: &[String], token: &CancellationToken) -> Result<Vec<u8>>;
}

#[derive(Clone, Debug, Default)]
pub struct ProcessRunner {
    pub path: Option<OsString>,
}

#[async_trait]
impl CommandRunner for ProcessRunner {
    async fn run(&self, name: &str, args: &[String], token: &CancellationToken) -> Result<Vec<u8>> {
        let program = if let Some(path) = &self.path {
            which::which_in(name, Some(path), std::env::current_dir()?).map_err(|_| {
                Error::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("executable {name:?} was not found in PATH"),
                ))
            })?
        } else {
            PathBuf::from(name)
        };
        let mut command = tokio::process::Command::new(program);
        command.args(args).kill_on_drop(true);
        if let Some(path) = &self.path {
            command.env("PATH", path);
        }
        let output = cancellable(token, async { Ok(command.output().await?) }).await?;
        if !output.status.success() {
            return Err(Error::Command {
                name: name.to_owned(),
                status: output.status,
                stderr: sanitize_stderr(&String::from_utf8_lossy(&output.stderr)),
            });
        }
        Ok(output.stdout)
    }
}
