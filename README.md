`cli-deps` checks, installs, and updates external command dependencies for Rust applications that need managed binaries and explicit installation consent.

# What It Does

- **Version checks**: Verify SemVer, calendar versions, tool banners, and development builds concurrently.
- **Managed binaries**: Install tools in an application data directory and give child commands a managed `PATH`.
- **Release downloads**: Discover GitHub releases through redirects and install tar, ZIP, or raw binaries without a GitHub API token.
- **Updates**: Update yt-dlp, invoke native package managers, and replace the running executable.
- **Failure reporting**: Preserve typed process and cancellation errors, summarize dependency reports, and retain partial batch outcomes.

# How It Works

Declare `Dependency` values, select their installers, and construct a `Manager`. `verify` returns reports; `ensure` optionally installs missing or outdated tools and verifies the result. Interactive input and output belong to the caller. `AGENT=1`, `true`, or `yes` disables prompts; `auto_install` explicitly permits installation in either mode. Enter and EOF reject an interactive prompt.

The default directory is `$XDG_DATA_HOME/<app>/bin`, then `%LOCALAPPDATA%/<app>/bin` on Windows, then `~/.local/share/<app>/bin`. A `Config::bin_dir` override supports isolated installations. The manager supplies its directory to child-process `PATH`; applications can obtain the same value through `managed_path`.

# How it Really Works

Calls run asynchronously with cancellation, 15-second discovery and five-minute download timeouts. GitHub discovery uses a HEAD redirect with a GET fallback. Release assets follow `<repo>_<version>_<go-os>_<go-arch>.tar.gz` (ZIP on Windows), then try the binary name as the prefix. Extraction selects a regular file by basename and excludes ZIP traversal paths and symlinks. Random temporary files remain beside the destination; a completed download atomically replaces the executable. Failed downloads preserve the prior binary.

Cache and directory failures propagate to callers; missing commands return typed I/O errors. Batch failures retain dependency names. Managed tools receive a child-command environment without mutating the parent process. Self-update downloads exactly the tag it checked. An installed version equal to or newer than that tag returns `Update::Current` with the actual installed version. Status text contains no emoji.

`Updater` requires an explicit `ReleaseChannel`; an absent channel returns `Error::Unpublished` without network access or executable replacement. Channel construction reads no environment and accepts HTTPS or loopback HTTP, including IPv6. Credentials, query strings, fragments, and invalid repository path segments are rejected. `update_running` stages beside the current executable and replaces it only after a successful download; callers choose their own release source and configuration policy.

Version comparison uses `versions`; abbreviated numeric versions are padded to three segments. As in godeps, empty versions and `dev`, `main`, `master`, `git-*`, and `N-*` snapshots satisfy any minimum. `Manager` rejects an empty parsed banner before comparison. Cache entries contain the successful raw version banner and are invalidated before installation or update. A cache implementation can persist them through the `Cache` trait.

# Prerequisites

- [Rust](https://www.rust-lang.org/) 1.88 or later and a [Tokio](https://tokio.rs/) runtime for asynchronous operations.
- Network access for release downloads; native package installation needs the selected package manager and its usual permissions.

# Installation

This project is a local crate. From a sibling Rust project's directory:

```sh
cargo add cli-deps --path ../deps
```

# Quick Start

```rust,no_run
use cli_deps::{CancellationToken, Config, Dependency, Manager};

# async fn example() -> cli_deps::Result<()> {
let mut ffmpeg = Dependency::new("ffmpeg", "4.4");
ffmpeg.version_args = vec!["-version".into()];
let manager = Manager::new(Config {
    app_name: "audio-tool".into(),
    dependencies: vec![ffmpeg],
    ..Default::default()
})?;
for report in manager.verify(&CancellationToken::new()).await? {
    println!("{}: {}", report.satisfied, report.summary());
}
# Ok(())
# }
```

# API

| API | Purpose |
| --- | --- |
| `Manager::{verify, ensure}` | Check dependencies and optionally install them; `ensure` receives `EnsureOptions`, `BufRead`, `Write`, and a cancellation token. |
| `Manager::{install, update}` | Operate on one dependency, matching its name without case sensitivity. |
| `Manager::{install_unsatisfied, update_all}` | Return `BatchOutcome` with completed names and typed failures. |
| `ReleaseClient::{github, new}` | Select GitHub or an explicit release base URL. |
| `ReleaseClient::{latest_tag, install_release, install_asset}` | Discover releases and download standard archives or direct assets. |
| `ReleaseChannel::new(base, owner, repository)` | Validate an explicit release source without reading environment variables. |
| `Updater::{update_to, update_running}` | Replace a selected or running executable; return `Update::Installed(version)` or `Update::Current(version)`. |
| `ReleaseInstaller` | Install a repository's standard release assets. |
| `YtDlpInstaller` | Install standalone yt-dlp or try `yt-dlp -U` before downloading a replacement. |
| `PackageInstaller` | Use brew, apt-get, pacman, dnf, winget, or choco; inherit the caller's permissions. |
| `InstallerFn` | Supply an asynchronous custom installer callback. |
| `ProcessRunner`, `CommandRunner` | Run real child processes or supply an application-specific runner. |
| `MemoryCache`, `Cache` | Cache version banners in memory or application-owned storage. |
| `default_version_parser`, `compare_versions`, `sanitize_stderr` | Parse banners, check minimum versions, and shorten stack traces. |

`DependencyReport` serializes using godeps' camelCase field names. Cancellation returns `Error::Cancelled`; failed commands include their native exit status and sanitized stderr. Construct a `YtDlpInstaller` or `PackageInstaller` with an `Arc<dyn CommandRunner>`; use `ProcessRunner { path: Some(managed_path(manager.bin_dir(), std::env::var_os("PATH").as_deref())?) }` when those commands must see managed tools.

# License

MIT. Original and Rust code copyright Alex Gorbatchev. See [LICENSE](LICENSE).
