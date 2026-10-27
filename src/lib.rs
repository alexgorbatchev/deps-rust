//! Managed external binaries, version checks and GitHub release installation.
mod installer;
mod manager;
mod process;
mod updater;
mod version;
pub use updater::{ReleaseChannel, Update, Updater};

pub use installer::{
    Installer, InstallerFn, PackageInstaller, ReleaseClient, ReleaseInstaller, YtDlpInstaller,
};
pub use manager::{
    BatchOutcome, Cache, Config, Dependency, DependencyFailure, DependencyReport, EnsureOptions,
    Manager, MemoryCache,
};
pub use process::{
    CommandRunner, ProcessRunner, ensure_managed_bin_dir, is_agent_mode, managed_bin_dir,
    managed_path, sanitize_stderr,
};
pub use tokio_util::sync::CancellationToken;
pub use version::{compare_versions, default_version_parser};

use std::{future::Future, io};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Rust releases are unpublished; self update requires a published Rust release channel")]
    Unpublished,
    #[error("{0}")]
    Message(String),
    #[error("operation cancelled")]
    Cancelled,
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error("command {name} failed ({status}): {stderr}")]
    Command {
        name: String,
        status: std::process::ExitStatus,
        stderr: String,
    },
    #[error("version {actual} is below required minimum {minimum}")]
    VersionTooOld { actual: String, minimum: String },
    #[error("dependencies are unsatisfied: {0}")]
    Unsatisfied(String),
    #[error("{0} dependency operations failed")]
    Batch(usize),
}
pub type Result<T> = std::result::Result<T, Error>;

pub(crate) async fn cancellable<T>(
    token: &CancellationToken,
    task: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::select! { biased; _ = token.cancelled() => Err(Error::Cancelled), result = task => result }
}
pub(crate) fn message(text: impl Into<String>) -> Error {
    Error::Message(text.into())
}
