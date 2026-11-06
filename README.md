`cli-deps` checks, installs, and updates external command dependencies for Rust applications that need managed binaries and explicit installation consent.

# What It Does

- **Version checks**: Verify SemVer, calendar versions, tool banners, and development builds concurrently.
- **Managed binaries**: Install tools in an application data directory, prefer a satisfying global installation, and give child commands a `PATH` that falls back to the managed copy.
- **Release downloads**: Discover GitHub releases through redirects and install tar, ZIP, or raw binaries without a GitHub API token.
- **Updates**: Reinstall yt-dlp from its latest unpacked release, invoke native package managers, and replace the running executable.
- **Failure reporting**: Preserve typed process and cancellation errors, summarize dependency reports, and retain partial batch outcomes.
- **Subprocess cleanup**: Stop cancelled commands with SIGTERM to their process group and SIGKILL only after a grace period.

# How It Works

Declare `Dependency` values, select their installers, and construct a `Manager`. `verify` returns reports; `ensure` optionally installs missing or outdated tools and verifies the result. Interactive input and output belong to the caller. `AGENT=1`, `true`, or `yes` disables prompts; `auto_install` explicitly permits installation in either mode. Enter and EOF reject an interactive prompt.

The default directory is `$XDG_DATA_HOME/<app>/bin`, then `%LOCALAPPDATA%/<app>/bin` on Windows, then `~/.local/share/<app>/bin`. A `Config::bin_dir` override supports isolated installations.

A global executable takes precedence when it satisfies the minimum version. `verify` checks the first match on the search `PATH` (`Config::search_path`, or the process `PATH`) and falls back to the managed copy when the global one is missing or too old; `DependencyReport::path` names the executable that reported the version, and `Manager::executable` returns the one to run. Installers always write to the managed directory. `Manager::child_path` gives child processes a `PATH` that resolves dependencies by name the same way, including lookups a child makes itself, such as yt-dlp finding ffmpeg: global entries come first unless some dependency resolved to its managed copy, in which case the managed directories come first. When that order would let an outdated managed copy shadow a satisfying global one, `child_path` returns `Error::Unsatisfied` naming it; updating that dependency resolves it. `managed_path` builds the global-first order without verification.

# How it Really Works

Calls run asynchronously with cancellation, 15-second discovery and five-minute download timeouts. GitHub discovery uses a HEAD redirect with a GET fallback. Release assets follow `<repo>_<version>_<go-os>_<go-arch>.tar.gz` (ZIP on Windows), then try the binary name as the prefix. Extraction selects a regular file by basename and excludes ZIP traversal paths and symlinks. Random temporary files remain beside the destination; a completed download atomically replaces the executable. Failed downloads preserve the prior binary.

`YtDlpInstaller` installs the unpacked (PyInstaller one-folder) build: `yt-dlp_macos.zip` on macOS, `yt-dlp_linux.zip` or `yt-dlp_linux_aarch64.zip` on Linux x86-64 or ARM64, and `yt-dlp_win.zip`, `yt-dlp_win_x86.zip`, or `yt-dlp_win_arm64.zip` on Windows. It runs from its install folder and writes nothing to the system temporary directory, unlike the one-file builds, which unpack about 70 MB there on every run and leave it behind when killed. Other platforms receive the Python zipapp `yt-dlp`. The whole archive is extracted with traversal-safe paths, Unix modes, and symbolic links into `.yt-dlp/<install>/` inside the managed directory, with the executable renamed to `yt-dlp`. On Unix, `<bin>/yt-dlp` is a relative symbolic link to that executable; a new install replaces the link, or an obsolete one-file `yt-dlp`, with one atomic rename. It keeps the install it replaced, so a yt-dlp still running from it can finish, and removes older ones. On Windows the build lives in `.yt-dlp/current`, which managed lookups and `managed_path` search before the managed directory; a new install renames the previous folder aside, restores it if the swap fails, and removes an obsolete one-file `yt-dlp.exe`. Windows refuses that rename while yt-dlp runs from the folder, so such an install fails and keeps the running copy. Concurrent installs into one managed directory serialize on a lock file, `.yt-dlp/.lock`. Updating downloads the latest build again because the unpacked builds do not support `yt-dlp -U`. A failed download or extraction keeps the previous installation.

`ProcessRunner` starts each command in a new session on Unix, which makes it the leader of its own process group, with standard input closed. Cancelling the token, or dropping the `run` future, sends SIGTERM to the group and SIGKILL only to a group still alive after `termination_grace` (`DEFAULT_TERMINATION_GRACE`, five seconds). The output pipes stay open and are drained until the group is gone, so a child that writes while handling SIGTERM is not killed by SIGPIPE before its cleanup. A cancelled `run` returns `Error::Cancelled` only after the leader is reaped and the group is confirmed gone; an awaited cleanup-task failure returns `Error::Io`. The grace period bounds the time before SIGKILL, not the final exit wait: an uninterruptible or unreaped member, or an error inspecting its state, can keep cancellation pending indefinitely.

A dropped `run` finishes termination in a Tokio blocking task, which ordinary runtime shutdown waits for. If the runtime discards that task, its destructor sends SIGKILL at once and waits synchronously for confirmed exit. Both paths retain cleanup ownership until completion. `std::process::exit`, `Runtime::shutdown_background`, an expired `shutdown_timeout`, or process exit after a drop outside a Tokio runtime (which uses a background thread) can interrupt cleanup, so await cancellation when the program is about to exit. The session has no controlling terminal, so Ctrl-C does not reach the child and terminal prompts such as `sudo` fail instead of waiting: applications cancel the token on SIGINT and SIGTERM, for example with `tokio::signal::ctrl_c`. A just-written executable that is still briefly busy (ETXTBSY on Linux) is retried for up to one second. On Windows the child is terminated with `TerminateProcess`.

Cache and directory failures propagate to callers; missing commands return typed I/O errors. Batch failures retain dependency names. Managed tools receive a child-command environment without mutating the parent process. Self-update downloads exactly the tag it checked. An installed version equal to or newer than that tag returns `Update::Current` with the actual installed version. Status text contains no emoji.

`Updater` requires an explicit `ReleaseChannel`; an absent channel returns `Error::Unpublished` without network access or executable replacement. Channel construction reads no environment and accepts HTTPS or loopback HTTP, including IPv6. Credentials, query strings, fragments, and invalid repository path segments are rejected. `update_running` stages beside the current executable and replaces it only after a successful download; callers choose their own release source and configuration policy.

Version comparison uses `versions`; abbreviated numeric versions are padded to three segments. As in godeps, empty versions and `dev`, `main`, `master`, `git-*`, and `N-*` snapshots satisfy any minimum. `Manager` rejects an empty parsed banner before comparison. Cache entries contain the successful raw version banner, keyed by dependency and executable path (`godeps_<name>.json@<path>`, or `godeps_<name>.json` when the runner resolves the name itself), and are invalidated before installation or update. Executables at non-UTF-8 paths are not cached. A cache implementation can persist them through the `Cache` trait.

# Prerequisites

- [Rust](https://www.rust-lang.org/) 1.89 or later and a [Tokio](https://tokio.rs/) runtime for asynchronous operations.
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

Install yt-dlp into the managed directory when no satisfying global copy exists, then run whichever copy satisfies the minimum:

```rust,no_run
use cli_deps::{
    CancellationToken, CommandRunner, Config, Dependency, EnsureOptions, Manager, ProcessRunner,
    ReleaseClient, YtDlpInstaller,
};
use std::sync::Arc;

# async fn example() -> cli_deps::Result<()> {
let mut yt_dlp = Dependency::new("yt-dlp", "2024.08.01");
yt_dlp.installer = Some(Arc::new(YtDlpInstaller {
    client: ReleaseClient::github()?,
}));
let manager = Manager::new(Config {
    app_name: "video-tool".into(),
    dependencies: vec![yt_dlp],
    ..Default::default()
})?;
let token = CancellationToken::new();
let options = EnsureOptions { auto_install: true };
manager
    .ensure(options, &mut std::io::empty(), &mut std::io::sink(), &token)
    .await?;
let executable = manager.executable("yt-dlp", &token).await?;
let runner = ProcessRunner {
    path: Some(manager.child_path(&token).await?),
    ..Default::default()
};
let version = runner
    .run(executable.as_os_str(), &["--version".into()], &token)
    .await?;
println!("{}", String::from_utf8_lossy(&version).trim());
# Ok(())
# }
```

# API

| API | Purpose |
| --- | --- |
| `Manager::{verify, ensure}` | Check dependencies and optionally install them; `ensure` receives `EnsureOptions`, `BufRead`, `Write`, and a cancellation token. |
| `Manager::executable` | Return the satisfying global or managed executable for one dependency, or `Error::Unsatisfied`. |
| `Manager::child_path` | Return a child-process `PATH` that resolves every dependency to the copy `verify` selects. |
| `Manager::{install, update}` | Operate on one dependency, matching its name without case sensitivity. |
| `Manager::{install_unsatisfied, update_all}` | Return `BatchOutcome` with completed names and typed failures. |
| `ReleaseClient::{github, new}` | Select GitHub or an explicit release base URL. |
| `ReleaseClient::{latest_tag, install_release, install_asset}` | Discover releases and download standard archives or direct assets. |
| `ReleaseChannel::new(base, owner, repository)` | Validate an explicit release source without reading environment variables. |
| `Updater::{update_to, update_running}` | Replace a selected or running executable; return `Update::Installed(version)` or `Update::Current(version)`. |
| `ReleaseInstaller` | Install a repository's standard release assets. |
| `YtDlpInstaller { client }` | Install and update the unpacked yt-dlp build. |
| `PackageInstaller` | Use brew, apt-get, pacman, dnf, winget, or choco; inherit the caller's permissions. |
| `InstallerFn` | Supply an asynchronous custom installer callback. |
| `ProcessRunner { path, termination_grace }`, `CommandRunner` | Run real child processes or supply an application-specific runner; `run` receives the program name or path as `&OsStr`. |
| `MemoryCache`, `Cache` | Cache version banners in memory or application-owned storage. |
| `default_version_parser`, `compare_versions`, `sanitize_stderr` | Parse banners, check minimum versions, and shorten stack traces. |

`DependencyReport` serializes using godeps' camelCase field names; `path` is not serialized. Cancellation returns `Error::Cancelled`; failed commands include their native exit status and sanitized stderr. Construct a `PackageInstaller` with an `Arc<dyn CommandRunner>`; run a dependency at its required version with the path from `Manager::executable`, and give its child processes `ProcessRunner { path: Some(manager.child_path(&token).await?), ..Default::default() }`.

# License

MIT. Original and Rust code copyright Alex Gorbatchev. See [LICENSE](LICENSE).
