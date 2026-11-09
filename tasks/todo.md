# Refactoring tasks

These entries record pre-release checkpoints. References to uncommitted work
describe the state when that checkpoint was completed.

- [x] 1. Whole-request deadlines and delivery-safe transport fallback; timeout regression tests.
- [x] 2. Shared process identities and pure family reconciliation; identity-reuse tests.
- [x] 3. Schema v4 process membership, startup restoration and recovery regression tests.
- [x] 4. Shared membership for measurement and pressure; escaped-worker measurement regression tests.
- [x] Checkpoint: all three reported failures fixed and repository checks pass.
- [x] 5. Typed, transactional lifecycle transitions and report idempotency tests.
- [x] 6. Planner and application services; narrow state access and dependency checks.
- [x] 7. Typed protocol messages, documentation and final integration checks.
- [x] Checkpoint: final diff reviewed; changes remain uncommitted and unpushed.

## Follow-up review fixes

- [x] Reproduce recovery invalidation and unverified-parent descendant adoption with regression tests.
- [x] Pin one committed observation per recovery pass and confirm parent identities before discovering descendants.
- [x] Run full repository checks, cross-build Linux x86_64/Arm64 and macOS Intel test targets, and review the complete uncommitted refactor again.

## Recovery transaction and QoS follow-up

- [x] Add permanent regressions and confirm the two reported failures.
- [x] Reject stale recovery premises in the lifecycle transaction; cover concurrent lifecycle and membership changes.
- [x] Preserve unreadable process identities and QoS restoration obligations, including independent restoration and restart.
- [x] Run full checks, cross-target builds, final architecture review and update verification records.

## Finalization and inherited QoS

- [x] Add and reproduce the native completion and inherited-QoS regressions.
- [x] Persist completion reports and finalize only after confirmed ownership permits it; verify restart and cancellation.
- [x] Persist job-level QoS restoration ownership until a later observation confirms restoration; verify newly discovered descendants and restart.
- [x] Run full checks in an independent build directory, cross-build supported targets and complete a final review.

## Cross-version and installed-runtime acceptance

- [x] Build the committed schema-3 baseline and current source in separate target directories.
- [x] Verify old running and queued executors against the upgraded daemon, including lock ownership and cancellation.
- [x] Verify old-daemon refusal of schema 5 and rollback from a consistent pre-upgrade backup.
- [x] Run the installed DSH runtime with the current binary and verify reminder/session behavior.
- [x] Run repository checks, review the acceptance harness and record evidence and limits.

## Platform runtime, concurrency and architecture gates

- [x] Inspect available platforms; record unavailable Linux and native Intel runners.
- [x] Run macOS x86_64 tests under Rosetta and identify the evidence as translated execution.
- [x] Add and run graduated isolated concurrency/process-tree acceptance with latency observations.
- [x] Add production dependency checks with negative fixtures and connect them to repository checks.
- [x] Run final checks, review the changes and record results and remaining platform limits.

## Local Linux acceptance and release readiness

- [x] Provision isolated Arm64 and x86_64 Linux execution environments without host-directory mounts.
- [x] Run Linux repository checks, cross-version and concurrent process-tree acceptance.
- [x] Build and execute the actual musl release packages; verify archive contents and checksums.
- [x] Review the candidate and current local deployment prerequisites; record a release/deployment recommendation.
