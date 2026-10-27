---
created_on: 2026-10-14 05:46
last_modified: 2026-10-14 09:40
status: current
---

# cli-deps

This library manages external executables. It has no CLI entrypoint.

## Commands

- `just build`: build every target.
- `just test`: run behavioral tests and Rust documentation examples.
- `just lint`: check formatting and Clippy warnings.
- `just coverage`: require at least 90% line coverage with cargo-llvm-cov and llvm-tools.
- `just check`: run lint, tests, and coverage.

## Contracts

- Always test downloads against real local HTTP servers and command behavior with real subprocesses. Package-manager tests use isolated executable scripts; never install host packages to test this library.
- Always configure environment variables on child processes. Rust 2024 process-global environment mutation is unsafe with concurrent tests and Tokio runtimes.
- Preserve token-free HEAD/GET release discovery, standard Go release asset naming, camelCase report serialization, explicit cancellation, and caller-owned prompt streams.
- Preserve prior executables on failed downloads. Extract only the requested regular file, keep temporary files beside the destination, and use native atomic replacement. Keep self-update tied to the release tag used for the version check.
- Keep release configuration policy in the caller. `Updater` takes an explicit `ReleaseChannel`; absent channels return typed `Error::Unpublished` without network access. Test running-executable replacement only in disposable copied child binaries.
- Propagate cache failures and use typed missing-command errors. Never grant installation consent on EOF or prompt in agent mode.
- Any code change affecting runtime results requires a corresponding test-file change and at least 90% line coverage; `scripts/` is excluded. Develop red/green and verify a disabled implementation fails its behavioral tests before restoring it.
- Keep evidence in `.tmp/`. Persist standing rules here, updating existing guidance in place; keep assignments, progress, and test-run history outside this file.
- Never publish releases, tags, packages, or production deployments without explicit user authorization.
