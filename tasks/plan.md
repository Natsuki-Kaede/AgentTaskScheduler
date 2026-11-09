# Architecture refactoring plan

This document records the completed refactoring and its historical verification
checkpoints. Those phases kept changes local and uncommitted. On 2026-11-09 the
user authorized the subsequent 0.1.3 release closeout; the
[changelog](../CHANGELOG.md) describes the release and upgrade contract.

## Boundaries

- `domain`: job transitions, process identities, resource values and lease terms.
- `planner`: recognition, history-based prediction and submission planning.
- `sched`: pure admission policy.
- `runtime`: process membership reconciliation, independent of system calls and persistence.
- `app`: submission, lifecycle, query and background-operation services.
- `store`: SQLite queries, migrations and transactional updates using domain types.
- `protocol`: shared request and response types.
- `api`: HTTP decoding, service calls and response encoding.
- `client` and `channel`: transport, delivery status and whole-request deadlines.
- `daemon`: composition, startup, shutdown and background loops.
- `executor`: detached execution, local process control and reliable reports.
- `sample`, `procinfo`, `control`: measurement and operating-system adapters.

## Delivery order

1. Bound socket and request-file operations with one deadline. Never replay a possibly delivered submission through another transport.
2. Extract process identities and family reconciliation.
3. Persist confirmed process members in schema version 4 and restore them before recovery decisions.
4. Use the same committed membership for recovery, measurement and pressure response.
5. Centralize typed lifecycle transitions and idempotent reports inside store transactions.
6. Extract submission planning and application services; remove API-owned daemon state.
7. Share typed protocol messages and complete dependency cleanup.

## Acceptance

- A daemon restart followed by executor loss retains a live escaped worker and its resource ownership.
- A reparented worker remains measured; later memory growth contributes to its peak.
- A socket that accepts without replying cannot hang a hook past its request budget.
- Existing CLI, JSON, exit-code, dependency, cancellation and reminder behavior remains compatible.
- Temporary test homes isolate databases and configuration. Run focused checks after each phase and the complete repository checks at integration checkpoints.
- No commit, push, installation or service replacement is part of this work.

## Recovery boundary

Recovery retains confirmed and persisted identities. A version 3 database cannot reconstruct members that escaped before they were recorded. Unknown process observations must not be interpreted as proof that the task ended.

## Verification completed

- `scripts/check.sh`: passed, including formatting and clippy with warnings denied.
- Rust: 127 unit tests, 5 hook integration tests and 5 recovery integration tests passed.
- DSH: 3 adapter tests passed; the installed-runtime test remained opt-in and was skipped.
- Linux x86_64 and Arm64: `cargo zigbuild --locked --target x86_64-unknown-linux-gnu --target aarch64-unknown-linux-gnu --tests` passed. Linux binaries were built, not executed.
- macOS Intel: `cargo build --locked --target x86_64-apple-darwin --tests` passed. Intel binaries were built, not executed; runtime tests ran on the native macOS Arm64 host.
- Dependency inspection confirmed that domain/runtime have no I/O dependencies, scheduling depends on domain values, persistence has no API/policy dependency, and HTTP handlers do not access store or coordination locks.
- The escaped-worker regressions use an explicit parent-release signal and count total private memory, including compressed pages. Lock retention/release is checked at the submission boundary independently of pressure and reconnect admission gates.
- `git diff --check` passed. No failed escaped-worker test executors remained after cleanup.
- All changes remain local and uncommitted.

## Follow-up review fixes and verification

- Regression tests first reproduced premature completion during observation invalidation and descendant adoption through an unreadable parent. The other 119 unit tests passed before the fixes.
- Each recovery pass now owns one immutable committed observation. The watcher captures a fresh observation every round; unavailable rounds retain ownership, and later confirmed exits can complete the job.
- Family reconciliation retains unreadable members but discovers new descendants only through identities confirmed in the current process table. Confirmed children can still discover their own descendants independently of an unreadable ancestor.
- Full repository checks and the three cross-target builds above passed after the fixes. The complete uncommitted refactor was reviewed again for state ownership, dependency direction, persistence, lifecycle reports, process control, transport deadlines and wire compatibility; no additional actionable findings remained.

## Recovery transaction and QoS validation

- The two permanent regressions first failed on the preceding implementation, while the other 121 unit tests passed.
- Recovery now passes its expected domain task facts and observed member identities into `Store::recover`. The same lifecycle transaction rejects changed facts or member sets before updating state, membership or events. The recovery process interface supplies member identities directly, so the liveness decision and transaction guard use the same frozen observation.
- Ten recovery-related tests passed after the transaction change. Table-driven cases cover concurrent registration, admission, start, main result, cancellation, member addition/removal/replacement and completion. Positive cases cover retained reservations, set-based identity comparison and eventual completion after a fresh observation.
- QoS probes now use `procinfo::probe` and preserve read errors. Six pressure tests passed, covering independent restoration, unreadable obligations across restart, confirmed absence/reuse, restoration failure and read errors before or after committing intent.
- `scripts/check.sh` passed after both fixes: 127 unit tests, 5 hook integration tests, 5 recovery integration tests and 3 DSH adapter tests; the opt-in installed-DSH test was skipped. Formatting and clippy passed with warnings denied. The unconditional finish convenience method is now test-only; production recovery uses the guarded transaction.
- Linux x86_64/Arm64 and macOS Intel program and test targets were built again successfully with the commands above. Runtime tests ran on native macOS Arm64.
- Final review confirmed that transactions contain no process probes, no store-to-application dependency was introduced, and existing lifecycle reports, cancellation, process control, measurement and transport contracts remain covered. No additional actionable findings were identified. Changes remain uncommitted and unpushed.

## Completion authority and inherited QoS validation

- Native regressions first reproduced both remaining failures: an executor completion report released a daemon-confirmed escaped worker and its lock, and a child first observed after pressure subsided retained its parent's inherited background policy.
- Schema version 5 persists completion reports and job-level QoS restoration responsibility. Completion acknowledgements retain known live or unreadable members. Immediate finalization and recovery both use the guarded lifecycle transaction; the guard includes the pending report. Replays preserve the original payload, reported jobs cannot execute again, late cancellation takes precedence, and spawn failures can finalize after takeover.
- The completion integration tests pause only the isolated executor while the daemon confirms an escaped worker, then allow normal reporting. A conflicting explicit lock remains rejected while the worker lives, including after a daemon restart. Separate cases confirm eventual completion and cancellation of the remaining work after restart.
- QoS restoration includes newly confirmed descendants and keeps job responsibility through a later unchanged observation. The native macOS regression uses a real fork and reopens the store before restoration: parent and child return from background priority 4 to normal priority 31. A separate regression covers parent exit, another generation after restart, and cleanup after the episode ends. Identity read errors and failed restoration remain retryable.
- Migration tests preserve version 4 job state, member identities and legacy QoS intents, then verify pending reports and associated obligations survive reopening. An induced migration failure leaves the old schema, version and records intact.
- Independent verification used a new dedicated `CARGO_TARGET_DIR` for the actual repository source. `scripts/check.sh` passed: 136 unit tests, 5 hook integration tests, 7 recovery integration tests, 3 DSH adapter tests, formatting and clippy with warnings denied. The installed-DSH runtime test remained opt-in and was skipped. Executor tests now wait for confirmed terminal state after the completion acknowledgement.
- Linux x86_64/Arm64 and macOS Intel program and test targets built successfully in the independent directory using the cross-target commands above. Runtime tests ran on native macOS Arm64; the cross-built targets were not executed.
- Final review traced report acceptance, guarded finalization, recovery, cancellation, QoS restoration, migration, process observation, measurement and transport deadlines. Production dependency inspection confirmed pure domain/runtime/scheduling boundaries, no store-to-application dependency, and no database or coordination-lock access in HTTP handlers. No additional actionable findings remained. Polling still covers confirmed identities and cannot guarantee discovery of work that fully detaches before observation.
- The API description and architecture document now explain completion acknowledgement and schema version 5. All changes remain local, uncommitted and unpushed; no installed binary, service or user database was changed.
- A final process check found that the explicit-dependency test left its blocked executor waiting after daemon shutdown. The fixture now cancels that submission and confirms executor exit while its daemon is available. Task-owned leftovers from earlier validation runs were removed.

## Cross-version and installed-runtime acceptance

- Added `scripts/check-upgrade.py`, a dependency-free acceptance harness with temporary ATS homes and directly owned/reaped daemons and executors. It receives distinct old/new binaries and records their hashes.
- Built the committed schema-3 baseline `ea287ee44603dc7124050a7ab805fb4396f27c29` and current schema-5 source in separate source/target directories. Four acceptance cases passed: running/queued old executors continue after upgrade, cancellation reaches an old executor, an old result waiting during daemon outage reaches the new daemon, and old-daemon schema refusal followed by consistent-backup restoration. Lock conflicts, one start per job, stdout/exit codes, matching/conflicting completion replays, preserved records and database integrity were verified.
- The installed DSH 0.2.2-rc.1 runtime passed with the current binary using both source and installed plugin entry points. The stronger test verifies native Bash session ownership over an inherited foreign Codex session, exit codes 75/0, same-turn continuation, detach and once-only next-turn completion. Runtime handles and environment changes are cleaned up through the owning lifecycle.
- Repository checks passed: formatting, Clippy with warnings denied, 136 unit tests, 5 hook integration tests, 7 recovery integration tests and 3 standard Node adapter tests. Explicit Electron execution passed all 4 Node tests with zero skips; the installed-plugin variant passed separately. Binary hashes, scope and reproduction commands are in `docs/compatibility-verification.md`.
- Final review found no additional production defects. The installed plugin remained byte-for-byte unchanged, and validation daemons/executors were reaped. Current results are native macOS Arm64 evidence; non-native runtime checks, concurrency measurements and automated dependency rules remain follow-up work. No commit, push, installation or service replacement was performed.

## Platform runtime, concurrency and dependency gates

- Local inventory found no Lima instances or container runtime; the user confirmed no external Linux or physical Intel runner was available. These native runtime targets remain `NOT_RUN`. Rosetta was available. Its first run exposed an xcrun/Python launcher architecture mismatch; using the existing standalone Python on PATH resolved the fixture environment issue.
- All 153 Rust tests passed natively and as x86_64 binaries through Rosetta: 137 unit, 4 architecture, 5 hook and 7 recovery tests. Formatting, Clippy with warnings denied and 3 Node adapter tests passed. Linux x86_64/Arm64 program and test targets cross-compiled successfully, without execution.
- Added isolated graduated stress acceptance at 2/4/8 jobs with 6/20/72 confirmed controlled processes. All levels passed ownership, lock, restart, cancellation, actual process-exit, result, duplicate-event and database-integrity checks. Final measured data and its binary hash are retained in `docs/validation/2026-11-09-stress.json`; the readout describes percentile and sample-gap limits.
- Added a real macOS 16-process QoS batch test with an on-disk store. The explicit native run measured 1.706 ms for backgrounding and 1.705 ms for restoration, with all obligations cleared. No system-wide memory pressure was induced.
- Added Rust AST dependency gates to normal `cargo test`, using the existing locked `syn` version as a development dependency. Positive and negative fixtures cover grouped/renamed imports, relative paths, raw identifiers, test conditions and external module files. Extracted shared acceptance fixtures and reran all four cross-version cases successfully.
- Final review found no additional production defects. Native Linux and physical Intel runtime acceptance still require runners; longer load studies are optional. No commit, push, installation or service replacement was performed.
