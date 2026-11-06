---
created_on: 2026-10-14 05:46
last_modified: 2026-11-05 21:11
status: current
---

# cli-deps

This library manages external executables. It has no CLI entrypoint.

## Commands

- `just build`: build every target.
- `just test`: run behavioral tests and Rust documentation examples.
- `just lint`: check formatting and Clippy warnings.
- `just coverage`: require at least 90% line and function coverage of `src/` with cargo-llvm-cov and llvm-tools.
- `just check`: run lint, tests, and coverage.
- CI (`.github/workflows/ci.yml`) runs `just check` on Linux and macOS, and all-target lint plus native installation and process-ownership regressions on Windows, for pull requests and pushes to `main`.

## Contracts

- Always test downloads against real local HTTP servers and command behavior with real subprocesses. Package-manager tests use isolated executable scripts; never install host packages to test this library.
- Always configure environment variables on child processes. Rust 2024 process-global environment mutation is unsafe with concurrent tests and Tokio runtimes.
- Preserve token-free HEAD/GET release discovery, standard Go release asset naming, camelCase report serialization, explicit cancellation, and caller-owned prompt streams.
- Preserve prior executables on failed downloads. From release archives extract only the requested regular file, keep temporary files beside the destination, and use native atomic replacement. Keep self-update tied to the release tag used for the version check.
- Install yt-dlp from the unpacked (PyInstaller one-folder) release archives, never the one-file builds, which unpack into the system temporary directory on every run and leak it when killed. Extract the whole archive into `.yt-dlp/<install>/` under the managed directory with the zip crate's traversal-safe extraction, preserving Unix modes and symbolic links. On Unix, switch installs by atomically renaming the relative `yt-dlp` symbolic link and keep the replaced install for runs still using it. On Windows, atomically replace the `.yt-dlp/active` selector file with a completed immutable generation's basename; retain published generations while returned executable paths and child PATHs may still reference them. Resolve the selector afresh for each Manager operation, and use one generation for verification and child PATH construction. Hold the `.yt-dlp/.lock` file lock from extraction through publication. Update by downloading the build again; unpacked builds do not support `yt-dlp -U`.
- Prefer a global executable that satisfies the minimum version; fall back to the managed copy only when the global one is missing or too old. Installers write only to the managed directory. Child processes use `Manager::child_path`, which keeps by-name lookups consistent with that resolution.
- Start `ProcessRunner` children in a new session on Unix, so each leads its own process group without a controlling terminal. On cancellation or a dropped `run` future, send SIGTERM to the group and SIGKILL only after the bounded `termination_grace`. Keep ownership and drain open output pipes until the leader is reaped and the group is confirmed gone; neither a timeout nor an inspection error proves exit. The final wait has no deadline. Finish a dropped run's termination in a Tokio blocking task so runtime shutdown waits for it. If that task is discarded, send SIGKILL at once and wait synchronously for confirmed exit. Propagate awaited cleanup-task failures. Test termination with real subprocesses that trap signals and delayed group reaping. `process::exit`, `shutdown_background`, expired `shutdown_timeout`, and process exit after a no-runtime background-thread fallback can interrupt cleanup.
- On Windows, spawn suspended and assign the child to an owned kill-on-close Job Object before resuming any child code. Use a new console process group, request CTRL_BREAK on cancellation/drop when a shared console permits delivery, drain pipes during the grace period, then terminate the job. Keep ownership until the child is reaped and the job reports zero active processes. Run this lifecycle in a Tokio blocking task with cooperative cancellation so ordinary runtime shutdown waits for cleanup; a discarded queued task must not have spawned a child. Test both graceful and uncooperative descendants with native executable fixtures and independent failure-path cleanup.
- Keep release configuration policy in the caller. `Updater` takes an explicit `ReleaseChannel`; absent channels return typed `Error::Unpublished` without network access. Test running-executable replacement only in disposable copied child binaries.
- Propagate cache failures and use typed missing-command errors. Never grant installation consent on EOF or prompt in agent mode.
- Any code change affecting runtime results requires a corresponding test-file change and at least 90% line and function coverage; `scripts/` is excluded. Develop red/green and verify a disabled implementation fails its behavioral tests before restoring it.
- Before running a check, compare its source, tests, fixtures, configuration, lockfile, toolchain, and compiled or validated documentation inputs with the last successful run. Skip it only when all inputs are unchanged; report that reason. Run it when inputs changed or are unknown, and never bypass hooks, CI, or required gates.
- Keep evidence in `.tmp/`. Persist standing rules here, updating existing guidance in place; keep assignments, progress, and test-run history outside this file.
- Never publish releases, tags, packages, or production deployments without explicit user authorization.
