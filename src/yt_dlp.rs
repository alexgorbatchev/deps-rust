//! Managed yt-dlp installation from the unpacked (PyInstaller one-folder) builds.
//!
//! The one-file builds extract their runtime into the system temporary directory on
//! every run and leave it behind when killed. The unpacked builds run in place from
//! their install folder.
//!
//! Layout inside the managed directory:
//!
//! - `.yt-dlp/<install>/`: one unpacked build, its executable renamed to `yt-dlp`
//!   (`yt-dlp.exe` on Windows).
//! - Unix: `yt-dlp` is a relative symbolic link to `.yt-dlp/<install>/yt-dlp`. A new
//!   install replaces the link with one atomic rename, which also replaces an
//!   obsolete one-file `yt-dlp`. It keeps the install it replaced, so a yt-dlp
//!   still running from it can finish, and removes older ones.
//! - Windows: `.yt-dlp/active` selects a complete, immutable `install-*` folder.
//!   Publication atomically replaces this selector file; readers resolve either
//!   complete generation. Managed lookups search the selected folder before the
//!   managed directory. Published generations are retained because returned paths
//!   and child PATHs can outlive an update. The obsolete one-file `yt-dlp.exe` is
//!   removed after publication.
//!
//! Installs into one managed directory hold an exclusive lock on `.yt-dlp/.lock`
//! from extraction through cleanup, so concurrent installs never remove each
//! other's folders.
use crate::{CancellationToken, Error, Installer, ReleaseClient, Result, message};
use async_trait::async_trait;
use std::path::{Path, PathBuf};

/// Folder under the managed directory that holds unpacked yt-dlp builds.
const PACKAGE_DIR: &str = ".yt-dlp";

/// Resolve the atomically published Windows install generation.
#[cfg(not(unix))]
pub(crate) fn windows_entry_dir(bin_dir: &Path) -> Result<Option<PathBuf>> {
    let root = bin_dir.join(PACKAGE_DIR);
    let name = match std::fs::read_to_string(root.join("active")) {
        Ok(name) => name,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    // Only our generated basename is a selector, never an arbitrary path.
    if !name.starts_with("install-")
        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(message("invalid yt-dlp install selector"));
    }
    let install = root.join(name);
    if !install.is_dir() {
        return Err(message("selected yt-dlp install directory is missing"));
    }
    Ok(Some(install))
}

/// An unpacked release archive and the executable at its root.
struct UnpackedAsset {
    archive: &'static str,
    executable: &'static str,
}

/// Selects the unpacked build for an OS and architecture, accepting Go and Rust names.
fn unpacked_asset(os: &str, arch: &str) -> Option<UnpackedAsset> {
    let (archive, executable) = match (os, arch) {
        ("darwin" | "macos", _) => ("yt-dlp_macos.zip", "yt-dlp_macos"),
        ("linux", "amd64" | "x86_64") => ("yt-dlp_linux.zip", "yt-dlp_linux"),
        ("linux", "arm64" | "aarch64") => ("yt-dlp_linux_aarch64.zip", "yt-dlp_linux_aarch64"),
        ("windows", "386" | "x86") => ("yt-dlp_win_x86.zip", "yt-dlp_x86.exe"),
        ("windows", "arm64" | "aarch64") => ("yt-dlp_win_arm64.zip", "yt-dlp_arm64.exe"),
        ("windows", _) => ("yt-dlp_win.zip", "yt-dlp.exe"),
        _ => return None,
    };
    Some(UnpackedAsset {
        archive,
        executable,
    })
}

impl ReleaseClient {
    /// Installs yt-dlp for `os`/`arch` into `directory` and returns its entry point.
    ///
    /// macOS, Linux x86-64 and ARM64, and Windows receive the unpacked build. Other
    /// platforms receive the Python zipapp `yt-dlp`, which needs Python and does not
    /// extract into the temporary directory. A failed download or extraction keeps
    /// the previous installation.
    pub async fn install_yt_dlp_for_platform(
        &self,
        os: &str,
        arch: &str,
        directory: &Path,
        token: &CancellationToken,
    ) -> Result<PathBuf> {
        let root = directory.join(PACKAGE_DIR);
        let Some(asset) = unpacked_asset(os, arch) else {
            let url = self.url(&[
                "yt-dlp", "yt-dlp", "releases", "latest", "download", "yt-dlp",
            ])?;
            let entry = self
                .install_asset(url.as_str(), "yt-dlp", directory, token)
                .await?;
            remove_dir_if_present(&root)?;
            return Ok(entry);
        };
        let url = self.url(&[
            "yt-dlp",
            "yt-dlp",
            "releases",
            "latest",
            "download",
            asset.archive,
        ])?;
        let download = self.download(url.as_str(), &root, token).await?;
        let directory = directory.to_owned();
        let token = token.clone();
        tokio::task::spawn_blocking(move || {
            install_unpacked(&download, asset.executable, &directory, &token)
        })
        .await
        .map_err(|e| message(format!("installing yt-dlp: {e}")))?
    }
}

fn install_unpacked(
    download: &tempfile::NamedTempFile,
    executable: &str,
    directory: &Path,
    token: &CancellationToken,
) -> Result<PathBuf> {
    let root = directory.join(PACKAGE_DIR);
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join(".lock"))?;
    lock.lock()?;
    let staging = tempfile::Builder::new()
        .prefix("install-")
        .tempdir_in(&root)?;
    let mut archive = zip::ZipArchive::new(download.reopen()?)
        .map_err(|e| message(format!("reading yt-dlp archive: {e}")))?;
    if token.is_cancelled() {
        return Err(Error::Cancelled);
    }
    archive
        .extract(staging.path())
        .map_err(|e| message(format!("extracting yt-dlp archive: {e}")))?;
    let source = staging.path().join(executable);
    if !std::fs::symlink_metadata(&source).is_ok_and(|m| m.is_file()) {
        return Err(message(format!(
            "yt-dlp archive has no {executable:?} executable at its root"
        )));
    }
    let entry_name = format!("yt-dlp{}", std::env::consts::EXE_SUFFIX);
    std::fs::rename(&source, staging.path().join(&entry_name))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            staging.path().join(&entry_name),
            std::fs::Permissions::from_mode(0o755),
        )?;
    }
    if token.is_cancelled() {
        return Err(Error::Cancelled);
    }
    activate(staging, &entry_name, directory)
}

/// Points the managed `yt-dlp` at the staged install, keeping the install it
/// replaces and removing older ones. The caller holds the install lock.
#[cfg(unix)]
fn activate(staging: tempfile::TempDir, entry_name: &str, directory: &Path) -> Result<PathBuf> {
    let root = directory.join(PACKAGE_DIR);
    let install = staging.keep();
    let name = install
        .file_name()
        .ok_or_else(|| message("staged yt-dlp install has no name"))?
        .to_owned();
    let target = Path::new(PACKAGE_DIR).join(&name).join(entry_name);
    let entry = directory.join(entry_name);
    let previous = std::fs::read_link(&entry)
        .ok()
        .filter(|link| link.starts_with(PACKAGE_DIR))
        .and_then(|link| link.parent()?.file_name().map(ToOwned::to_owned));
    let link = tempfile::Builder::new()
        .prefix(".yt-dlp-")
        .make_in(directory, |path| std::os::unix::fs::symlink(&target, path));
    let persisted =
        link.and_then(|link| link.into_temp_path().persist(&entry).map_err(|e| e.error));
    if let Err(error) = persisted {
        std::fs::remove_dir_all(&install)?;
        return Err(error.into());
    }
    remove_other_installs(&root, &[Some(name), previous])?;
    Ok(entry)
}

/// Publish a complete immutable generation with one file replacement. The
/// caller holds the install lock. Returned paths and child PATHs can outlive an
/// update, so published generations must not be reclaimed by the installer.
#[cfg(not(unix))]
fn activate(staging: tempfile::TempDir, entry_name: &str, directory: &Path) -> Result<PathBuf> {
    use std::io::Write;
    let root = directory.join(PACKAGE_DIR);
    let mut selector = tempfile::Builder::new()
        .prefix(".active-")
        .tempfile_in(&root)?;
    let name = staging
        .path()
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| message("staged yt-dlp install has no UTF-8 basename"))?;
    selector.write_all(name.as_bytes())?;
    selector.flush()?;
    let install = staging.keep();
    if let Err(error) = selector.persist(root.join("active")) {
        std::fs::remove_dir_all(&install)?;
        return Err(error.error.into());
    }
    let obsolete = directory.join(entry_name);
    if obsolete.is_file() {
        std::fs::remove_file(obsolete)?;
    }
    Ok(install.join(entry_name))
}

/// Removes install folders other than `keep`; files such as in-flight downloads stay.
#[cfg(unix)]
fn remove_other_installs(root: &Path, keep: &[Option<std::ffi::OsString>]) -> Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        if entry.file_type()?.is_dir() && !keep.iter().flatten().any(|k| *k == name) {
            std::fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}

fn remove_dir_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_dir_all(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

/// Installs and updates yt-dlp from its latest GitHub release without a token.
///
/// Updating downloads the unpacked build again; it never runs `yt-dlp -U`, which the
/// unpacked builds do not support.
pub struct YtDlpInstaller {
    pub client: ReleaseClient,
}

#[async_trait]
impl Installer for YtDlpInstaller {
    async fn install(&self, directory: &Path, token: &CancellationToken) -> Result<()> {
        let (os, arch) = crate::installer::release_platform();
        self.client
            .install_yt_dlp_for_platform(os, arch, directory, token)
            .await
            .map(|_| ())
    }
}
