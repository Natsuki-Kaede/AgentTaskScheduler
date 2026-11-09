# Working in this repository

Agent Task Scheduler (`ats`) queues resource-heavy commands from AI agents and admits them by machine capacity. It is one Rust crate that builds one binary, `ats`; the daemon is its `daemon` subcommand.

## Layout

- `src/main.rs`: command-line entry point.
- `src/daemon/`: component assembly, startup, shutdown and background-task lifetimes.
- `src/app/`: application operations and private coordination state: submissions, lifecycle reports, queries, scheduling, process monitoring, recovery and pressure response.
- `src/domain/`: job transitions, process identities, resource values and measurement snapshots, independent of I/O.
- `src/runtime/`: pure process-family and membership reconciliation.
- `src/planner/`: submission planning, history statistics and filesystem-aware lock derivation.
- `src/protocol.rs`: shared request and response messages, with explicit conversion from domain records.
- `src/api/`: the HTTP/JSON API, served on a unix socket and through the request-file channel (`src/channel.rs`).
- `src/client.rs`: how commands reach the daemon.
- `src/executor/`: the per-job process that waits for its lease and runs the command.
- `src/hook.rs`: the agent hooks, which remind agents of their submissions.
- `plugins/dsh/`: the dependency-free DSH reminder bundle, embedded in the binary by `src/install/dsh.rs`, plus its Node tests.
- `src/install/`: `ats install` and `ats uninstall` for the agents' instructions, reminder hooks and the login service (`service.rs`: LaunchAgent or systemd user unit).
- `src/recognize/`, `src/profile.rs`, `profiles/builtin.toml`: command recognition and tool profiles.
- `src/sched/`: admission and pressure policy as pure functions.
- `src/sample/`: memory measurement.
- `src/store/`: the SQLite store and its schema.
- `api/openapi.yaml`: the API description shared by the CLI, executors and hooks.
- `scripts/release.sh`, `scripts/formula.sh`, `packaging/homebrew/ats.rb`: the release archive of one target, and the checksums and Homebrew formula filled in from all archives.
- `.github/workflows/`: checks on Linux and macOS, Arm and x86; release builds for the four targets.
- `INSTALL.md`: install steps written for an AI agent, which the README prompt points to.
- `docs/architecture.md`: module boundaries, state ownership and recovery guarantees.

ats runs on macOS and Linux. The code for each system is in `src/sample/{macos,linux}.rs`, `src/procinfo.rs`, `src/control.rs`, `src/timing.rs` and `src/install/service.rs`. Other code uses only POSIX calls.

## Code

- Rust 2024 edition. Async code runs on tokio.
- Write comments, identifiers, commit messages and docs in English.
- Handle a failure where it can happen and where something needs the handling; add fallbacks, retries or extra validation only for a named requirement or a reachable failure.
- Keep `domain` and `runtime` free of I/O. `sched` depends only on domain values. Store operations use domain types and own their transactions; API handlers call application methods rather than accessing the store or coordination locks.
- Tests are table-driven where several cases share a shape. Tests use temporary directories and never touch the user's real ats data, agent configuration or the network. Set `ATS_HOME` to point a daemon at a separate data directory.

## Checks

Run before every commit:

```sh
scripts/check.sh
```

It runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` and the DSH adapter tests (`node --test`). Node.js 22 or newer is required for these checks. The optional installed-DSH runtime test is documented in `plugins/dsh/README.md`.
