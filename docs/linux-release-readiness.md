# Local Linux acceptance and release readiness

This report records the pre-release verification checkpoint on 2026-11-09.
The candidate was the uncommitted refactor reporting version 0.1.2, with database
schema 5. The committed baseline was
`ea287ee44603dc7124050a7ab805fb4396f27c29`, version 0.1.2 with schema 3.
No commit, push, tag, publication or replacement of the installed host binary
was performed at that checkpoint. Subsequent 0.1.3 release preparation updates
the version and fixes metadata exclusion in the release script; the measurements
and archived candidate identities below remain unchanged. See the
[0.1.3 changelog](../CHANGELOG.md) for the release and upgrade contract.

## Execution environments

Both Linux guests ran locally on the Arm64 Mac, with 2 CPUs, 4 GiB RAM,
20 GiB disks, no host-directory mounts and no container runtime. Only one guest
ran at a time. Both used Ubuntu 24.04.5 LTS, kernel 6.8.0-146-generic,
glibc 2.39, Python 3.12.3 and Node 24.19.0.

| Target | Execution and build method | Result |
| --- | --- | --- |
| Linux Arm64 | VZ virtual machine, Arm64 kernel and binaries; Rust 1.98.1 installed in the guest. | Full `scripts/check.sh`, upgrade, concurrency and packaged musl acceptance passed. |
| Linux x86_64 | QEMU 11.1.3 TCG full-system emulation, x86_64 kernel and binaries. Rust executables were cross-built with Cargo/zigbuild on macOS, then executed inside the guest. | All Rust suites, Node adapter tests, upgrade, concurrency and packaged musl acceptance passed. |
| macOS Arm64 | Native host. | Optimized archive passed all four upgrade/rollback cases. The unchanged source had already passed full native checks. |
| macOS x86_64 | Rosetta on the Arm64 host. | Optimized archive passed all four upgrade/rollback cases. The unchanged source had already passed 153 translated Rust tests. |

The Linux image downloads were verified against their published SHA-256 values.
Their dated URLs, hashes, guest facts and binary identities are in the
[Arm64 evidence](validation/2026-11-09-linux-arm64.json) and
[x86_64 evidence](validation/2026-11-09-linux-x86_64.json).
Lima CLI 2.3.0 was used after installing the additional guest agents; the VM
configuration records retain their original Lima 2.2.1 creation version.
Provisioning installed QEMU and the extra agents through Homebrew, which also
upgraded Lima and required dependencies. Both task-owned guests were stopped
after acceptance and remain available for reuse.

These results establish Linux runtime behavior in the named environments.
QEMU and Rosetta results are recorded as emulated/translated execution, without
claiming physical x86 hardware or native-performance measurements.

## Repository and lifecycle acceptance

Each Linux architecture passed 134 unit tests, 4 architecture gates, 5 hook
integration tests and 7 recovery integration tests: 150 Rust tests in total.
Three Node adapter tests passed on each; the installed-DSH case remained opt-in
and skipped. The earlier real installed-DSH verification remains documented in
[compatibility-verification.md](compatibility-verification.md).

Arm64 ran the complete repository check script, including formatting and Clippy
with warnings denied. x86_64 ran every cross-compiled test executable with
`--test-threads=1`, followed by the Node tests. Test executables' compiled source
and CLI paths were resolved through a guest-only path link to the copied source;
no host directory was mounted into the guest.

Both GNU debug builds and the extracted musl release binaries passed all four
cases in `scripts/check-upgrade.py`:

- old running and queued executors survived daemon replacement with unchanged
  identities; the commands started once and retained their expected exit codes;
- cancellation through the upgraded daemon ended the old executor and released
  its queued lock contender;
- a result reported during the daemon outage was recovered without another start;
- an old daemon refused schema 5 without changing history; restoring a consistent
  pre-upgrade SQLite backup restored schema 3, the original logs and exit code 9.

Lock conflicts returned 409, matching completion replays returned 204 and
conflicting replays returned 409. SQLite integrity checks returned `ok`.

The real systemd user-service smoke also passed on both Linux architectures:
installation and health, unchanged PID on repeated installation, actual queued
command completion with exit code 7 and preserved stdout, a new healthy daemon
PID after killing the main process, preserved history, and successful uninstall.
It used the packaged musl binary and an isolated `ATS_HOME` in the disposable
guest. No guest ats process or installed ats unit remained after cleanup.

## Concurrent process-tree acceptance

Both guests passed all three levels of `scripts/check-stress.py`: 2, 4 and
8 concurrent jobs, with respectively 2, 4 and 8 escaped children per job. The
largest level confirmed 72 controlled processes, plus its separate lock waiter.
Restart, concurrent cancellation, actual process exit, eventual waiter admission,
output/exit-code preservation, exactly-once events and empty durable membership
were verified. No unexpected probe request error occurred.

| Guest | Jobs | Loaded job-query P95 | Recovered job-query P95 | Maximum observed sample gap |
| --- | --- | --- | --- | --- |
| Arm64 | 2 | 2.375 ms | 1.132 ms | 1044.402 ms |
| Arm64 | 4 | 1.387 ms | 1.366 ms | 1039.797 ms |
| Arm64 | 8 | 1290.192 ms | 80.388 ms | 4082.690 ms |
| x86_64 QEMU | 2 | 15.002 ms | 13.025 ms | 1057.143 ms |
| x86_64 QEMU | 4 | 23.639 ms | 21.120 ms | 1920.728 ms |
| x86_64 QEMU | 8 | 119.349 ms | 152.721 ms | 4020.884 ms |

These are bounded functional acceptance observations on a shared host. The
Arm64 eight-job run had substantial latency while host load and memory pressure
were changing. No controlled experiment isolated the cause. Neither these
figures nor the emulated x86 figures establish production latency or capacity
limits. Raw distributions and operation counts are preserved in the evidence.

## Release archive verification

All four optimized targets were built and packaged. Arm64 Linux used the normal
release script in the guest with `CC_aarch64_unknown_linux_musl=musl-gcc`.
x86_64 Linux used a temporary Cargo-to-zigbuild adapter when invoking the release
script on macOS; native Linux CI remains the authoritative native build path.
The cross linker emitted a deprecated optimization-setting warning; the build
and static executable runtime checks succeeded.

macOS-created archives initially included AppleDouble files and provenance
extended attributes. Repacking with `COPYFILE_DISABLE=1 tar --no-xattrs` removed
that metadata without changing executable bytes. Each final archive contains
exactly `ats`, `README.md` and `LICENSE` under its target directory. The repacked
Linux x86_64 archive was extracted and its CLI run again in the guest. All final
executable hashes match the binaries used in the successful lifecycle cases.

`scripts/formula.sh` generated all four checksums and the Homebrew formula.
All checksum checks passed, no substitution placeholder remained, and `ruby -c`
accepted the generated formula. Archive contents and hashes are in
[release-artifacts.json](validation/2026-11-09-release-artifacts.json); macOS
package lifecycle results are in
[macos-release.json](validation/2026-11-09-macos-release.json).

The archived candidate source has SHA-256
`9c44a17b07817826b67ed83a7aee227b7d74cae00a7acb2377f1daeb55c06b4f`.
Its [source manifest](validation/2026-11-09-source-manifest.json) records file
hashes. Subsequent repository edits in this pass are verification documents,
evidence and task records only. Candidate archives still say 0.1.2 and are local
acceptance artifacts; a new release needs a new version and regenerated assets.

## Review and deployment decision

The final review found no additional P1/P2 production defect in the uncommitted
candidate. It checked recovery premises and persisted membership in the same
transaction, durable completion claims, unreadable identity handling, inherited
QoS restoration, transport deadlines, protocol compatibility and module
dependencies. The original three failures remain covered, together with the
later recovery-concurrency and QoS regressions. Store transactions own durable
facts; application services own orchestration; pure policy and membership code
remain independent of I/O; API handlers use application methods.

One non-blocking P3 packaging improvement remains: `scripts/release.sh:14` does
not suppress macOS metadata itself. The clean candidate archives used the
explicit environment/options above. Making that behavior part of the script
would keep future macOS-generated release archives clean without a manual step.

A native macOS package upgrade attempt initially timed out while waiting for
the old baseline's command to start during host memory pressure. Both full
package acceptance runs passed after reducing contention. The disposable
service smoke also needed two fixture corrections: an explicit lock to prevent
the lightweight command's intended direct execution, and `--kill-whom=main`
because this guest rejected signaling auxiliary service processes. No product
code was changed to accommodate these fixtures.

The candidate is ready to enter release preparation. Public release still needs
a new version (recommended 0.1.3), release notes, an authorized commit/push and
green four-platform CI before tagging and publishing. Physical Intel coverage
will come from that CI; local Rosetta evidence remains labeled separately.

The latest [installed-host snapshot](validation/2026-11-09-local-readiness.json)
at 12:09:19 +08:00 still showed version 0.1.2, schema 3, integrity `ok` and one
active job. QoS obligations had cleared to zero and pressure response was inactive;
the earlier 12:05:30 snapshot retained in the same artifact had five obligations
and active pressure. The installed binary hash and LaunchAgent PID were unchanged.
Installation must recheck these facts at the maintenance window.

Local deployment should wait until active work has drained and the old daemon's
QoS restoration has settled. Then stop new submissions, back up the old binary,
LaunchAgent configuration and database consistently with SQLite's backup API,
replace the binary, restart the service, and verify schema 5 integrity, history,
health and a real queued smoke command. A schema-3 database cannot reconstruct
escaped processes never recorded by the old version; active-work acceptance
does not remove this idle-upgrade requirement. Rollback uses the old binary and
the pre-migration database backup together.

Longer load studies are optional follow-up. No additional architecture rewrite
is required by the evidence gathered in this pass.
