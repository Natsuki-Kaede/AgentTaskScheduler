# Changelog

## 0.1.4 - 2026-11-11

### Fixed

- Redact sensitive arguments inside nested shell command strings, with bounded
  recursion and conservative display text for unparseable source.
- Return a nonzero terminal CLI result for canceled work even when its main
  process previously exited successfully.
- Apply the executor's visible cgroup v2 memory limits and ancestor budgets on
  Linux, accounting for reservations shared by sibling groups. Wait when the
  execution resource domain cannot be observed safely.
- Exclude failed commands from the history used to predict resource demand.
- Derive Cargo output locks from workspace roots and explicit manifest or target
  directory arguments, including explicit package workspace paths.
- Synchronize macOS QoS test startup and inspect base thread priority instead
  of dynamic scheduler priority, including inherited policy after fork.

### Compatibility and limits

The database remains at schema 5. Existing task data does not need a new schema
migration when upgrading from 0.1.3. Upgrade while no jobs are active and QoS
restoration has settled.

Linux resource-domain checks require visible cgroup v2 membership and mounts;
cgroup v1 and hidden ancestor limits are not covered. These admission checks do
not impose per-job hard memory limits. Cargo target-directory overrides through
environment variables or Cargo configuration are not fully resolved; use an
explicit `--target-dir` or `--lock` for those shared output directories.

## 0.1.3 - 2026-11-09

### Fixed

- Keep confirmed job processes, memory reservations and resource locks owned
  across daemon restarts, executor loss and completion reports. A job becomes
  terminal only after its owned processes are confirmed to have exited.
- Continue measuring reparented and escaped workers, including later memory
  growth, without adopting descendants through an unverified process identity.
- Reject recovery decisions whose task facts or persisted process members changed
  concurrently, without partially changing state or releasing resources.
- Preserve macOS background-QoS restoration responsibilities across failed
  identity reads, daemon restarts and inherited policy in later child processes.
- Bound the complete socket or request-file operation, including response reads,
  so an unresponsive daemon cannot leave an agent hook waiting indefinitely.
  A possibly delivered submission is not replayed through another transport.
- Exclude macOS metadata from portable release archives.

### Changed

- Separate application operations, pure domain and process-membership logic,
  submission planning and wire messages. Automated dependency checks keep HTTP
  handlers away from database transactions and coordination locks.
- Add isolated cross-version and concurrent process-tree acceptance scripts.

### Upgrade notes

The database upgrades transactionally from schema 3 or 4 to schema 5. Existing
CLI commands and `/v1` protocol messages remain compatible. A completion-report
acknowledgement means the report is durable; job queries and terminal waits
reflect final confirmation that owned work has ended.

Upgrade while no jobs are active, after macOS QoS restoration has settled.
Schema 3 cannot reconstruct escaped workers that were never recorded. Back up
the database consistently with SQLite's backup API, together with the old
binary and service configuration, before replacing the daemon.

An older daemon refuses a schema-5 database. Rollback requires the old binary
and the pre-upgrade database backup together; restoring that backup discards
records written after it was taken.
