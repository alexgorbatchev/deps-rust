use crate::{CancellationToken, CommandRunner, Error, Result, cancellable, message};
use async_trait::async_trait;
use futures::TryStreamExt;
use reqwest::{Client, Method, Url, header::LOCATION};
use std::{
    future::Future,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};

#[derive(Clone, Debug)]
pub struct ReleaseClient {
    base_url: Url,
    discovery: Client,
    download: Client,
}

impl ReleaseClient {
    pub fn new(base_url: &str) -> Result<Self> {
        let base_url = Url::parse(&format!("{}/", base_url.trim_end_matches('/')))
            .map_err(|e| message(e.to_string()))?;
        Ok(Self {
            base_url,
            discovery: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(15))
                .build()?,
            download: Client::builder()
                .timeout(Duration::from_secs(300))
                .build()?,
        })
    }
    pub fn github() -> Result<Self> {
        Self::new("https://github.com")
    }

    fn url(&self, segments: &[&str]) -> Result<Url> {
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .map_err(|_| message("base URL cannot contain path segments"))?
            .pop_if_empty()
            .extend(segments);
        Ok(url)
    }

    /// Discovers a release using HEAD redirects and a GET fallback, without the GitHub API.
    pub async fn latest_tag(
        &self,
        owner: &str,
        repo: &str,
        token: &CancellationToken,
    ) -> Result<String> {
        let url = self.url(&[owner, repo, "releases", "latest"])?;
        cancellable(token, async {
            for method in [Method::HEAD, Method::GET] {
                let response = self.discovery.request(method, url.clone()).send().await?;
                if matches!(response.status().as_u16(), 301..=303)
                    && let Some(location) = response
                        .headers()
                        .get(LOCATION)
                        .and_then(|v| v.to_str().ok())
                {
                    let target = url.join(location).map_err(|e| message(e.to_string()))?;
                    if let Some(tag) = target
                        .path_segments()
                        .and_then(|mut s| s.next_back())
                        .filter(|t| !t.is_empty())
                    {
                        return Ok(tag.to_owned());
                    }
                }
            }
            Err(message(format!(
                "could not resolve latest release for {owner}/{repo}"
            )))
        })
        .await
    }

    /// Downloads only the named regular file from an archive, or a raw executable.
    pub async fn install_asset(
        &self,
        url: &str,
        binary: &str,
        directory: &Path,
        token: &CancellationToken,
    ) -> Result<PathBuf> {
        validate_binary(binary)?;
        std::fs::create_dir_all(directory)?;
        let download = tempfile::NamedTempFile::new_in(directory)?;
        let mut writer = tokio::fs::File::from_std(download.reopen()?);
        cancellable(token, async {
            use tokio::io::AsyncWriteExt;
            let mut stream = self
                .download
                .get(url)
                .send()
                .await?
                .error_for_status()?
                .bytes_stream();
            while let Some(bytes) = stream.try_next().await? {
                writer.write_all(&bytes).await?;
            }
            writer.flush().await?;
            Ok(())
        })
        .await?;
        drop(writer);
        let binary = binary.to_owned();
        let directory = directory.to_owned();
        let token = token.clone();
        let url = url.to_owned();
        tokio::task::spawn_blocking(move || {
            extract_asset(download, &url, &binary, &directory, &token)
        })
        .await
        .map_err(|e| message(format!("extracting release asset: {e}")))?
    }

    pub async fn install_release(
        &self,
        owner: &str,
        repo: &str,
        binary: &str,
        directory: &Path,
        token: &CancellationToken,
    ) -> Result<String> {
        let tag = self.latest_tag(owner, repo, token).await?;
        self.install_tag(owner, repo, binary, directory, &tag, token)
            .await?;
        Ok(tag)
    }
    pub(crate) async fn install_tag(
        &self,
        owner: &str,
        repo: &str,
        binary: &str,
        directory: &Path,
        tag: &str,
        token: &CancellationToken,
    ) -> Result<()> {
        let version = tag.trim_start_matches('v');
        let (os, arch) = release_platform();
        let ext = if os == "windows" { "zip" } else { "tar.gz" };
        let bare = binary.trim_end_matches(".exe");
        let final_binary = if os == "windows" {
            format!("{bare}.exe")
        } else {
            binary.to_owned()
        };
        let mut last_error = None;
        for prefix in [repo, bare] {
            let asset = format!("{prefix}_{version}_{os}_{arch}.{ext}");
            let url = self.url(&[owner, repo, "releases", "download", tag, &asset])?;
            match self
                .install_asset(url.as_str(), &final_binary, directory, token)
                .await
            {
                Ok(_) => return Ok(()),
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(e) => last_error = Some(e),
            }
        }
        Err(last_error.expect("two download attempts"))
    }

    pub async fn install_yt_dlp_for_platform(
        &self,
        os: &str,
        arch: &str,
        directory: &Path,
        token: &CancellationToken,
    ) -> Result<PathBuf> {
        let (asset, binary) = match (os, arch) {
            ("darwin" | "macos", _) => ("yt-dlp_macos", "yt-dlp"),
            ("windows", _) => ("yt-dlp.exe", "yt-dlp.exe"),
            ("linux", "arm64" | "aarch64") => ("yt-dlp_linux_aarch64", "yt-dlp"),
            ("linux", _) => ("yt-dlp_linux", "yt-dlp"),
            _ => ("yt-dlp", "yt-dlp"),
        };
        let url = self.url(&["yt-dlp", "yt-dlp", "releases", "latest", "download", asset])?;
        self.install_asset(url.as_str(), binary, directory, token)
            .await
    }
}

fn extract_asset(
    download: tempfile::NamedTempFile,
    url: &str,
    binary: &str,
    directory: &Path,
    token: &CancellationToken,
) -> Result<PathBuf> {
    let mut extracted = tempfile::NamedTempFile::new_in(directory)?;
    let path = Url::parse(url)
        .map_err(|e| message(e.to_string()))?
        .path()
        .to_owned();
    if path.ends_with(".tar.gz") || path.ends_with(".tgz") {
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(download.reopen()?));
        let mut found = false;
        for entry in archive.entries()? {
            if token.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let mut entry = entry?;
            if entry.header().entry_type().is_file() && file_matches(&entry.path()?, binary) {
                copy_checked(&mut entry, &mut extracted, token)?;
                found = true;
                break;
            }
        }
        if !found {
            return Err(message(format!(
                "binary {binary:?} not found in tar archive"
            )));
        }
    } else if path.ends_with(".zip") {
        let mut archive = zip::ZipArchive::new(download.reopen()?)
            .map_err(|e| message(format!("reading zip archive: {e}")))?;
        let mut found = false;
        for i in 0..archive.len() {
            let mut entry = archive
                .by_index(i)
                .map_err(|e| message(format!("reading zip entry: {e}")))?;
            if entry.is_file()
                && !entry.is_symlink()
                && entry
                    .enclosed_name()
                    .is_some_and(|p| file_matches(&p, binary))
            {
                copy_checked(&mut entry, &mut extracted, token)?;
                found = true;
                break;
            }
        }
        if !found {
            return Err(message(format!(
                "binary {binary:?} not found in zip archive"
            )));
        }
    } else {
        copy_checked(&mut download.reopen()?, &mut extracted, token)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        extracted
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o755))?;
    }
    extracted.as_file().sync_all()?;
    if token.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let destination = directory.join(binary);
    extracted
        .persist(&destination)
        .map_err(|e| Error::Io(e.error))?;
    Ok(destination)
}

fn validate_binary(binary: &str) -> Result<()> {
    let mut parts = Path::new(binary).components();
    if binary.is_empty()
        || !matches!(parts.next(), Some(Component::Normal(_)))
        || parts.next().is_some()
    {
        Err(message("binary must be a single filename"))
    } else {
        Ok(())
    }
}
fn file_matches(path: &Path, binary: &str) -> bool {
    path.file_name()
        .is_some_and(|name| name == binary || name == binary.trim_end_matches(".exe"))
}
fn copy_checked(
    reader: &mut impl Read,
    writer: &mut impl Write,
    token: &CancellationToken,
) -> Result<()> {
    let mut bytes = [0; 64 * 1024];
    loop {
        if token.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let n = reader.read(&mut bytes)?;
        if n == 0 {
            return Ok(());
        }
        writer.write_all(&bytes[..n])?;
    }
}
fn release_platform() -> (&'static str, &'static str) {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        os => os,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        arch => arch,
    };
    (os, arch)
}

#[async_trait]
pub trait Installer: Send + Sync {
    async fn install(&self, directory: &Path, token: &CancellationToken) -> Result<()>;
    async fn update(&self, directory: &Path, token: &CancellationToken) -> Result<()> {
        self.install(directory, token).await
    }
}

pub struct InstallerFn<F>(pub F);
#[async_trait]
impl<F, Fut> Installer for InstallerFn<F>
where
    F: Fn(PathBuf, CancellationToken) -> Fut + Send + Sync,
    Fut: Future<Output = Result<()>> + Send,
{
    async fn install(&self, directory: &Path, token: &CancellationToken) -> Result<()> {
        (self.0)(directory.to_owned(), token.clone()).await
    }
}

pub struct ReleaseInstaller {
    pub client: ReleaseClient,
    pub owner: String,
    pub repo: String,
    pub binary: String,
}
#[async_trait]
impl Installer for ReleaseInstaller {
    async fn install(&self, directory: &Path, token: &CancellationToken) -> Result<()> {
        self.client
            .install_release(&self.owner, &self.repo, &self.binary, directory, token)
            .await
            .map(|_| ())
    }
}

pub struct YtDlpInstaller {
    pub client: ReleaseClient,
    pub runner: Arc<dyn CommandRunner>,
}
#[async_trait]
impl Installer for YtDlpInstaller {
    async fn install(&self, directory: &Path, token: &CancellationToken) -> Result<()> {
        let (os, arch) = release_platform();
        self.client
            .install_yt_dlp_for_platform(os, arch, directory, token)
            .await
            .map(|_| ())
    }
    async fn update(&self, directory: &Path, token: &CancellationToken) -> Result<()> {
        match self.runner.run("yt-dlp", &["-U".into()], token).await {
            Ok(out)
                if !String::from_utf8_lossy(&out)
                    .to_ascii_lowercase()
                    .contains("error") =>
            {
                Ok(())
            }
            Err(Error::Cancelled) => Err(Error::Cancelled),
            _ => self.install(directory, token).await,
        }
    }
}

pub struct PackageInstaller {
    pub package: String,
    pub runner: Arc<dyn CommandRunner>,
}
#[async_trait]
impl Installer for PackageInstaller {
    async fn install(&self, _directory: &Path, token: &CancellationToken) -> Result<()> {
        let choices: &[(&str, &[&str])] = match std::env::consts::OS {
            "macos" => &[("brew", &["install"])],
            "linux" => &[
                ("apt-get", &["install", "-y"]),
                ("pacman", &["-S", "--noconfirm"]),
                ("dnf", &["install", "-y"]),
            ],
            "windows" => &[("winget", &["install"]), ("choco", &["install"])],
            _ => &[],
        };
        for (name, arguments) in choices {
            if which::which(name).is_ok() {
                let mut args = arguments
                    .iter()
                    .map(|s| (*s).to_owned())
                    .collect::<Vec<_>>();
                args.push(self.package.clone());
                if *name == "winget" {
                    args.extend(["-e".into(), "--silent".into()]);
                }
                if *name == "choco" {
                    args.push("-y".into());
                }
                return self.runner.run(name, &args, token).await.map(|_| ());
            }
        }
        Err(message(format!(
            "no supported package manager for {:?}",
            self.package
        )))
    }
}
