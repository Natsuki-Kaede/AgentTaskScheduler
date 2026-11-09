# Cross-version and DSH runtime verification

Verified locally on 2026-11-09, on macOS Arm64. All daemon data, shell work,
database backups and restored databases used temporary directories. No installed
ats binary, login service, DSH profile or user session was changed.

## Cross-version acceptance

The old binary was built from committed revision
`ea287ee44603dc7124050a7ab805fb4396f27c29` (ats 0.1.2, schema 3).
The new binary was built from the current uncommitted refactor (schema 5).
Source trees and Cargo target directories were separate.

| Case | Observed result |
| --- | --- |
| Running and queued old executors survive daemon replacement | Schema 3 upgraded to 5; executor identities and creation facts stayed intact. The running command completed with exit code 7; the queued command then ran with exit code 0. Each command started once. |
| Cancel an old executor through the new daemon | The old running executor accepted cancellation and reported `canceled`; its queued lock contender then completed. |
| Command finishes while no daemon is running | The old executor's main-result request was observed waiting in the request-file channel before the new daemon started. The new daemon received the result and completion without a duplicate start. |
| Reject schema 5, then restore an upgrade backup | The old daemon refused schema 5 without changing job records. Restoring a consistent schema-3 SQLite backup allowed it to start again, with the original history, exit code 9 and stdout log. Integrity checks returned `ok`. |

Both active-upgrade cases confirmed that a conflicting explicit `--now` lock
request returned 409 before and after replacement. Matching completion replays
returned 204; changed exit codes returned 409, without additional events. Old and
new CLI job views agreed after completion. The outage case also checked the lock
before shutdown and the same report replay contract after recovery.

Binary SHA-256 values for this run:

```text
old: 604ea4db847acbf7b68ae0708270498ba434b6fe412af6be7291395f108941e4
new: 40bd040b7b23c4a3675211660dafb20540b3a5ab7b584192a2257f07a24384f3
```

To reproduce from the repository root without switching the working tree:

```sh
compat_root=$(mktemp -d)
mkdir "$compat_root/old-source"
git archive --format=tar --output="$compat_root/old.tar" ea287ee44603dc7124050a7ab805fb4396f27c29
tar -xf "$compat_root/old.tar" -C "$compat_root/old-source"
CARGO_TARGET_DIR="$compat_root/old-target" ats run -- rtk cargo build --locked --manifest-path "$compat_root/old-source/Cargo.toml"
CARGO_TARGET_DIR="$compat_root/new-target" ats run -- rtk cargo build --locked
ats run -- rtk proxy python3 scripts/check-upgrade.py \
  --old-binary "$compat_root/old-target/debug/ats" \
  --new-binary "$compat_root/new-target/debug/ats"
```

Wait for each build to complete before advancing if ats returns a background
submission. The script emits JSON results and binary hashes, launches its own
daemons and executors, and reaps them before removing each temporary home.
Use `--case upgrade`, `--case cancel`, `--case offline-report` or `--case rollback`
to run one acceptance case.

## Installed DSH runtime acceptance

The installed desktop application supplied DSH `0.2.2-rc.1`, Electron `44.7.0`
and its Node `v24.21.0` runtime. The test loaded the real Cordis services, agent
loop, native Bash tool, shell-environment registry and local subprocess runtime.
The model was scripted; no provider request or desktop UI interaction was used.

Verification passed with both the repository plugin and the installed plugin at
`~/dsh-plugins/agent-task-scheduler/index.js`, configured to use the new binary.
The installed plugin matched source after normalizing its installed binary path.

- Native Bash returned exit codes `[75, 0]` for background submission and detach.
- The submission belonged to the DSH agent's session, overriding an intentionally
  inherited foreign `CODEX_THREAD_ID`.
- A running reminder continued the same turn, without creating another turn.
- Completion was delivered on the next turn and acknowledged once.
- The command completed with exit code 0. Agent handles, daemon, temporary home
  and test environment overrides were disposed or restored during cleanup.

Run the [DSH runtime test](../plugins/dsh/README.md#verification) with `ATS_BINARY`
set to the new binary. Set `DSH_ATS_PLUGIN` to the installed entry point for the
second variant. Normal `scripts/check.sh` keeps this host-specific test opt-in;
the explicit Electron run executed it with zero skips.

## Repository checks and review

- `scripts/check.sh` passed formatting, Clippy with warnings denied, 136 unit
  tests, 5 hook integration tests, 7 recovery integration tests and 3 Node
  adapter tests. Its opt-in runtime case was skipped in the standard run.
- The explicit Electron run passed all 4 Node tests, including the installed
  DSH runtime case, with zero skips. A separate installed-plugin variant also
  passed its runtime case.
- Final review checked temporary-home isolation, direct-child executor ownership,
  bounded waits, cancellation before daemon teardown, SQLite backup/restore,
  session attribution, reminder consumption and environment restoration. No
  additional production defects were identified.
- A negative control supplied schema 5 as the old baseline. The harness rejected
  it, then canceled and reaped its owned processes; no validation daemon or
  executor remained. The installed plugin's SHA-256 was unchanged after testing.

## Scope and remaining work

These tests establish compatibility for the named committed baseline and the
current native macOS build. They do not extend schema-3 recovery to escaped
processes that were never persisted, and do not authorize arbitrary active-work
upgrades. Database rollback restores an idle pre-upgrade snapshot, discarding
post-upgrade records; it is not a reverse migration of schema 5.

The later [platform and capacity verification](runtime-and-capacity-verification.md)
adds Rosetta x86_64 execution, bounded concurrent process-tree measurements and
automated module-dependency gates. Subsequent [local Linux acceptance](linux-release-readiness.md)
also passed on Arm64 VZ and x86_64 QEMU guests, including packaged musl binaries
and systemd services. Physical Intel macOS runtime acceptance still requires a
suitable runner; the local macOS x86_64 results use Rosetta.
