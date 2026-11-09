# Architecture

ats is one Rust crate and one binary with three process roles. The CLI submits work and starts a detached executor with the caller's environment. The daemon decides when that executor may run. The executor runs the command, retains output and reports results.

## Module responsibilities

| Module | Responsibility |
| --- | --- |
| `domain` | Job states and transitions, process identities, submission plans, leases, resource locks and measurement values. No I/O. |
| `runtime` | Reconcile confirmed process membership against an observation. No database or OS calls. |
| `planner` | Recognize commands, derive filesystem locks and predict demand from supplied history. |
| `sched` | Pure admission and pressure policy. |
| `store` | SQLite queries, migrations and transactional changes using domain values. |
| `app` | Submission, lifecycle and query operations; scheduling, monitoring, recovery and pressure orchestration. Coordination state and store access are private here. |
| `api` | Decode HTTP, call application methods, map errors and encode protocol responses. |
| `protocol` | Shared wire messages and explicit conversion from domain records. |
| `client`, `channel` | Unix-socket and request-file transport with bounded waiting and delivery-safe fallback. |
| `daemon` | Assemble components and own startup, shutdown and background-task lifetimes. |
| `executor` | Detached command execution, local process control, logs and reliable reports. |
| `sample`, `procinfo`, `control` | Memory measurement and OS-specific process operations. |
| `hook`, `install`, `plugins/dsh` | Agent reminders, instructions and login-service installation. |

The intended source dependencies are:

```text
api                     -> app, protocol, domain
cli / executor / hook   -> client, protocol, domain
app                     -> domain, planner, sched, runtime, store, OS adapters
planner                 -> domain, recognize, profile
runtime / sched         -> domain
store                   -> domain, time utilities
protocol                -> domain
daemon                  -> component assembly
```

The executor also uses the shared family algorithm and OS adapter for local control. HTTP handlers do not access the database or coordination locks. Small wire values may be shared directly; stored job and submission records are explicitly converted to protocol views.

## State ownership

Job state, lease admission, main results and final results are durable facts. `domain::job` defines allowed transitions. Store operations read the current state, validate the transition, update data and record its event inside one transaction. Application methods publish change notifications after the commit.

Repeated executor, start, main-result and terminal reports must agree with the recorded payload. A matching replay succeeds without another event. A conflicting replay leaves the transaction unchanged. Cancellation of a waiting submission and its resulting terminal transition are committed together.

Recovery commits through the same lifecycle transaction with the task facts and member identities used by its decision. The transaction compares current state, executor and launcher identities, process group, main result, cancellation, pending completion report and the durable member set before applying the event. Changed premises cause a conflict without changing state, members or events; the next recovery pass reads fresh facts. A legal state transition alone does not authorize a stale recovery decision. The store accepts domain values and events, without depending on application recovery policy.

The main result is distinct from the job result. A main process may have exited and closed its output while owned descendants still run. Those descendants continue to hold the job's reservation and locks.

An executor's `/finished` report is a durable completion claim, including state, exit code and fallback memory usage. Its acknowledgement lets the executor exit; it does not authorize deletion of confirmed daemon members. The application can finalize immediately only when probes confirm every persisted identity is absent or replaced, and the guarded transaction still sees the same facts and membership. Read errors defer finalization. Recovery can also finalize the claim after a committed empty observation, including while the executor is still awaiting its acknowledgement. Live members retain ownership across executor exit and daemon restart. Cancellation of the remaining work takes precedence over the pending result; a reported spawn failure can also settle after takeover. Pending reports prevent new executor ownership, starts and leases.

## Process ownership and recovery

A process identity is its PID and start time. The daemon monitor is the single owner of its process registry. Each observation follows this order:

1. Read process facts and reconcile them with the last accepted membership.
2. Commit changed members to `job_processes`.
3. Accept and publish the observation.
4. Use that committed observation for recovery, measurement and pressure response.

Membership survives a process changing its group or being reparented. Startup reloads confirmed members and validates identities before recovery decisions or new admission. Recovery waits for an observation of the job's registered process group, so an earlier snapshot cannot conclude that a newly started job is empty.

Enumeration failure invalidates the current observation and blocks admission and subsequent recovery passes. Unreadable individual identities remain owned until a later observation resolves them. The last accepted registry and durable membership are retained.

Each recovery pass captures one immutable committed observation for readiness, membership and termination decisions. A pass that starts without an observation makes no recovery decision. A failure during a pass cannot turn its confirmed membership into an empty family; the next pass captures the latest availability again. Retained but unreadable identities cannot establish ownership of new descendants. Only parents whose PID and start time match the current process table can extend the family.

The sampler measures the members it is given; it does not reconstruct parent trees. Its memory cache is keyed by PID and start time. Incomplete memory readings do not lower reservations. Pressure response validates committed identities before changing QoS and records restoration obligations separately in `background_processes`.

QoS identity probes distinguish a matching identity, confirmed absence or replacement, and a read error. Read errors preserve restoration obligations and do not block restoration of other processes. An obligation is removed only after restoration succeeds or the original identity is confirmed gone. Identity checks before applying background QoS also fail closed on read errors; an intent already committed before a second failed probe remains available for later cleanup.

Background QoS can be inherited by children before their first observation. Before applying it, the store atomically records the process intent and a job-level restoration responsibility. After pressure subsides, every current confirmed member of that job receives a restoration intent, including children first seen in that round. Restoring or losing the original parent does not clear the job responsibility. A later unchanged observation, with all current members restored and no pending process intents, closes it. This allows another generation forked before restoration to be observed and restored. The job marker and process association survive restart; the in-memory set of restored identities is only a bounded cache. Legacy per-process intents are associated with a job when current confirmed membership establishes ownership.

The executor keeps a local family for execution and cancellation while the daemon is unavailable. It uses the same family algorithm, without opening SQLite.

## Request deadlines

A request has one sleep-aware deadline covering connection, HTTP handshake, response headers and body, or request-file waiting. Long polls supply a larger budget. Dropping a timed-out socket request also aborts its HTTP driver.

The client falls back to request files only when socket connection establishment fails in a way that proves the request was not sent. After connection establishment it does not automatically replay the request through another transport: a timed-out submission may already have created a job. Executor reports can retry because their payloads are checked for idempotency. Reminder failures let the agent continue.

## Database and wire compatibility

Schema version 4 adds `job_processes(job_id, pid, start)`. Additions are applied in the same migration transaction as the schema-version update. Membership writes apply only to jobs still holding a lease; stale observations cannot revive terminal ownership. Job pruning removes member rows before deleting their jobs.

Schema version 5 adds `jobs.finish_state`, `finish_exit_code` and `finish_max_rss`, a nullable job association on `background_processes`, and `background_jobs` for inherited restoration responsibility. QoS obligations are independent of job pruning. Migration preserves existing jobs, reservations, member identities and legacy QoS intents; failure rolls back the additions and version update together.

Existing `/v1` routes and message representations are retained, including flat lease terms. Old executors can finish reporting over the existing protocol. Completion acknowledgement now explicitly means the report is recorded; job queries and terminal waits reflect finalization. An older daemon refuses a version 5 database; rollback requires a pre-migration backup.

Recovery covers confirmed, persisted identities. A version 3 database cannot reconstruct descendants that escaped before ownership was recorded. An upgrade should therefore switch daemon versions without active work. Polling cannot guarantee discovery of a process that fully detaches before any observer sees it.

## Verification

Run `scripts/check.sh` for formatting, clippy, Rust tests and DSH adapter tests. Tests use temporary homes and do not open the user's installed database or configuration. Important regressions exercise:

- daemon restart followed by executor loss while an escaped worker retains a lock;
- reparented-worker measurement and later memory growth;
- a connected but unresponsive daemon during a Stop hook;
- socket header/body timeout, file fallback, long polling and prevention of submission replay;
- migration, failed process observations, reused identities, transactional transitions and conflicting reports;
- observation failure between recovery readiness and membership checks, followed by a fresh unavailable pass and eventual confirmed exit;
- unreadable and reused parent identities, with continued descendant discovery through independently confirmed children;
- concurrent lifecycle or membership changes invalidating a recovery decision without partial writes;
- unreadable QoS identities, independent restoration of other processes, retained obligations across restart and identity loss without affecting reused PIDs;
- normal completion reports with a daemon-confirmed escaped worker, retained lock ownership over restart, eventual completion and late cancellation;
- native macOS background-policy inheritance across a real fork, restoration after reopening the store, and job responsibility retained until a later quiet observation;
- multi-generation restoration after parent exit and restart, plus transactional schema version 5 migration and rollback on failure.

The installed-DSH runtime test remains opt-in as described in `plugins/dsh/README.md`.

Cross-version executor, database rollback and installed DSH runtime acceptance
results and reproduction commands are in [compatibility-verification.md](compatibility-verification.md).

Platform availability, translated x86_64 execution, graduated concurrency and
automatic dependency gates are documented in
[runtime-and-capacity-verification.md](runtime-and-capacity-verification.md).

Local Linux kernel/runtime acceptance, release archive checks and deployment
prerequisites are recorded in [linux-release-readiness.md](linux-release-readiness.md).
