# Platform, concurrency and architecture verification

Verified on 2026-11-09. Changes remain local and uncommitted.

## Available execution environments

| Target | Environment and result |
| --- | --- |
| macOS Arm64 | Native host; repository checks passed. |
| macOS x86_64 | Rosetta translation on the Arm64 host; all 153 Rust tests passed. This is translated execution, not physical Intel hardware evidence. |
| Linux x86_64 | Later passed 150 Rust tests, Node, upgrade, concurrency and packaged musl/service acceptance in a local QEMU Linux VM; see [Linux acceptance](linux-release-readiness.md). |
| Linux Arm64 | Later passed full repository checks and upgrade, concurrency and packaged musl/service acceptance in a local VZ Linux VM; see [Linux acceptance](linux-release-readiness.md). |
| Physical Intel macOS | `NOT_RUN`: no supplied runner. Rosetta results above are tracked separately. |

Initial inspection found Lima installed with no instances, no Docker/Podman tools
or VM application, and working Rosetta execution. The user confirmed no external
runner was available. That checkpoint did not provision an environment. The
subsequent local Linux acceptance provisioned two isolated guests and closed
both Linux runtime gaps; its methods and evidence are recorded separately.

The first translated run failed because `/usr/bin/python3` invoked xcrun, whose
installed developer library had only Arm64 slices. Using the already installed
standalone Arm64 Python on PATH allowed subprocess fixtures to run. The resulting
test commands were still x86_64 Rust executables running through Rosetta; their
Python child fixtures ran natively.

## Graduated concurrency acceptance

`scripts/check-stress.py` uses temporary ATS homes and the shared acceptance
fixture. It submits jobs concurrently, starts independent process trees whose
children leave the parent's session, and waits until all controlled PIDs appear
in daemon measurements. Each level checks:

- lock contention before and after daemon replacement;
- unchanged executor identities after restart;
- concurrent cancellation, including actual exit of the canceled process trees;
- concurrent completion with preserved exit codes and stdout;
- one command start and one terminal event per job;
- eventual execution of the lock waiter, empty persisted membership, reaped
  executors, and SQLite integrity.

Each job in a level has the same number of children as the job count. Each level
also has one lock waiter. Final measured observations are recorded in
[the JSON artifact](validation/2026-11-09-stress.json).

| Concurrent jobs | Confirmed controlled processes | Submit P95 | Loaded job-query P95 | Recovered job-query P95 | Maximum observed sample gap |
| --- | --- | --- | --- | --- | --- |
| 2 | 6 | 1.800 ms | 2.423 ms | 2.899 ms | 1007.416 ms |
| 4 | 20 | 3.235 ms | 2.312 ms | 1.639 ms | 993.210 ms |
| 8 | 72 | 8.525 ms | 3.709 ms | 1.997 ms | 1109.863 ms |

All three levels passed. No unexpected request errors occurred. The maximum
observed request time across the probe and operation measurements was 24.155 ms,
during startup. Planned restart unavailability is excluded from normal-request
errors. The starting phase includes admission waiting as well as process setup.

API measurements are end-to-end Unix-socket calls; they do not directly measure
Store mutex wait time. Probe requests run approximately every 50 ms. Sample gaps
are intervals between observations of changed published snapshots, including
startup, loaded and recovered phases but excluding the planned outage. P50/P95
use nearest-rank estimates; small operation counts are preserved in the artifact.
These are short local acceptance observations, not capacity limits or sustained
production latency guarantees. The observed scale does not justify a storage or
locking redesign.

## Controlled QoS batch

The macOS test `background_batch_restores_all_processes_and_records_latency`
uses 16 owned sleeping processes, real identity probes and QoS calls, and a
temporary on-disk SQLite store with the normal durability settings. It verifies
all processes enter background policy, all are restored, and both process and
job restoration obligations are cleared after a later observation.

The explicit native run observed 1.706 ms to apply background policy and
1.705 ms to restore the batch. This measures the response operation, not an entire
daemon sampling cycle. The test supplies the pressure flag to the policy; no
system-wide memory pressure was induced.

## Automated dependency gates

`tests/architecture.rs` parses Rust with the already locked `syn` version,
enabled only as a development dependency. Normal `cargo test` and therefore
`scripts/check.sh` run the gates automatically. Four tests cover the repository
and positive/negative fixtures:

- `domain`, `runtime` and `sched` reject I/O and inappropriate crate dependencies;
- `store` accepts domain and utility dependencies without application/policy
  dependencies;
- API handlers accept application, protocol and domain dependencies, and reject
  direct store access or coordination locking;
- grouped imports, renamed imports, raw identifiers, `self`/`super` paths and
  external module files are inspected;
- test-only modules/functions are excluded, while possibly active production
  platform branches remain inspected. Comments and string examples are not
  mistaken for dependencies.

This is a source-level gate, not macro expansion or full Rust name resolution.
Compiler privacy and review still complement it. No production crate split or
runtime dependency was introduced.

## Final verification and review

- `scripts/check.sh`: formatting and Clippy with warnings denied passed; 137
  unit tests, 4 architecture tests, 5 hook tests and 7 recovery tests passed.
  Three DSH adapter tests passed; the installed-runtime case stayed opt-in.
- The same 153 Rust tests passed as x86_64 binaries through Rosetta with the
  standalone Python PATH described above.
- Linux x86_64 and Arm64 program/test targets, including the architecture gates,
  cross-compiled successfully at this checkpoint. The subsequent
  [Linux acceptance](linux-release-readiness.md) also verified their runtime.
- Extracting `scripts/acceptance.py` preserved all four cross-version acceptance
  cases, which were rerun successfully.
- Final review covered process ownership, cleanup, normal/outage measurement
  separation, percentile computation, dependency-rule scope and test-only code.
  No additional production defects were identified. No performance-driven
  production change was made without evidence.
- An injected probe-cleanup failure verified that daemon/executor cleanup still
  runs before the error propagates. Final process inspection found no validation
  daemon or executor remaining, and the recorded measurement binary hash matched.

Reproduce the local load check with:

```sh
ats run -- rtk proxy python3 scripts/check-stress.py \
  --binary /absolute/path/to/ats --levels 2 4 8 --output /tmp/ats-stress.json
```

For translated Rust tests, use `cargo test --locked --target x86_64-apple-darwin`
with a working standalone Python directory on PATH. Linux runtime results are
now in the later report; physical Intel acceptance still requires a suitable
runner. Longer or larger load studies
are an optional extension of the bounded cases above.
