#!/usr/bin/env python3
"""Exercise real old executors against a new daemon in temporary ATS homes."""

import argparse
import contextlib
import hashlib
import json
from pathlib import Path
import shlex
import sqlite3
import subprocess
import tempfile

from acceptance import Home, until

NEW_SCHEMA = 6


def upgrade(old, new, case):
    cancel = case == "cancel"
    offline = case == "offline-report"
    with tempfile.TemporaryDirectory(prefix="ats-upgrade-") as directory:
        root = Path(directory)
        home = Home(root / "home")
        gate = root / "release"
        try:
            daemon = home.daemon(old)
            marker = root / "started"
            script = (f"from pathlib import Path; import time; Path({str(marker)!r}).touch()\n"
                      f"while not Path({str(gate)!r}).exists(): time.sleep(.05)\n"
                      "print('old executor result'); raise SystemExit(7)")
            running, executor = home.submit(old, "exec python3 -c " + shlex.quote(script))
            until("old running job", lambda: marker.exists() and home.job(running)["state"] == "running")
            waiting, waiter = home.submit(old, "printf 'queued executor result\\n'")
            until("old queued executor registration", lambda: home.job(waiting)["executor_pid"] == waiter.pid)
            assert home.job(waiting)["state"] == "queued"
            old_schema = home.sql("PRAGMA user_version")[0][0]
            assert old_schema in (3, 5), old_schema
            original = home.sql("SELECT executor_pid, executor_start, created_at FROM jobs WHERE id=?",
                                (running["job"],))
            home.conflict()
            daemon.kill()
            daemon.wait(timeout=5)
            if offline:
                gate.touch()
                until("old report pending during daemon outage", lambda: home.pending_report(
                    f"/v1/jobs/{running['job']}/main-result"))
            home.daemon(new)
            assert home.sql("PRAGMA user_version") == [(NEW_SCHEMA,)]
            assert home.job(running)["executor_pid"] == executor.pid
            assert home.sql("SELECT executor_pid, executor_start, created_at FROM jobs WHERE id=?",
                            (running["job"],)) == original
            if not offline:
                assert home.job(waiting)["state"] == "queued"
                assert home.sql("SELECT reserved_private FROM jobs WHERE id=?", (running["job"],))[0][0] > 0
                home.conflict()
            if cancel:
                assert home.request("POST", f"/v1/submissions/{running['submission']}/cancel", {})[0] == 204
            else:
                gate.touch()
            expected_state, expected_code = ("canceled", None) if cancel else ("completed", 7)
            until("old running report", lambda: home.job(running)["state"] == expected_state)
            until("old queued executor admitted", lambda: home.job(waiting)["state"] == "completed")
            assert home.job(running)["exit_code"] == expected_code
            assert home.job(waiting)["exit_code"] == 0
            assert executor.wait(timeout=5) == 0 and waiter.wait(timeout=5) == 0
            assert home.cli_job(old, running) == home.cli_job(new, running)
            if not cancel:
                assert (Path(home.job(running)["log_path"]) / "stdout.log").read_text() == "old executor result\n"
            for created in [running, waiting]:
                assert home.sql("SELECT COUNT(*) FROM job_events WHERE job_id=? AND kind='started'",
                                (created["job"],)) == [(1,)]
            logs = Path(home.job(waiting)["log_path"])
            assert (logs / "stdout.log").read_text() == "queued executor result\n"
            state, code, rss = home.sql("SELECT finish_state, finish_exit_code, finish_max_rss FROM jobs WHERE id=?",
                                       (running["job"],))[0]
            assert state == expected_state and code == expected_code
            payload = {"state": state, "exit_code": code, "max_rss": rss}
            count = home.sql("SELECT COUNT(*) FROM job_events")
            assert home.request("POST", f"/v1/jobs/{running['job']}/finished", payload)[0] == 204
            assert home.request("POST", f"/v1/jobs/{running['job']}/finished",
                                {**payload, "exit_code": 99})[0] == 409
            assert home.sql("SELECT COUNT(*) FROM job_events") == count
            print(json.dumps({"case": case,
                              "result": "PASS", "schema": [old_schema, NEW_SCHEMA], "old_state": state,
                              "old_exit_code": code, "queued_exit_code": 0, "started_once": True,
                              "lock_conflict": 409, "matching_replay": 204, "conflicting_replay": 409}), flush=True)
        finally:
            gate.touch()
            home.close()


def rollback(old, new):
    with tempfile.TemporaryDirectory(prefix="ats-rollback-") as directory:
        root = Path(directory)
        home = Home(root / "home")
        backup = root / "before-upgrade.db"
        try:
            daemon = home.daemon(old)
            created, executor = home.submit(old, "printf 'preserved history\\n'; exit 9")
            until("old completed history", lambda: home.job(created)["state"] == "completed")
            assert executor.wait(timeout=5) == 0
            original = home.cli_job(old, created)
            assert original["exit_code"] == 9
            old_schema = home.sql("PRAGMA user_version")[0][0]
            assert old_schema in (3, 5), old_schema
            daemon.terminate()
            daemon.wait(timeout=5)
            columns = ",".join(row[1] for row in home.sql("PRAGMA table_info(jobs)"))
            query = f"SELECT {columns} FROM jobs ORDER BY id"
            baseline = home.sql(query)
            with contextlib.closing(sqlite3.connect(home.root / "ats.db")) as source:
                with contextlib.closing(sqlite3.connect(backup)) as destination:
                    source.backup(destination)
            daemon = home.daemon(new)
            assert home.sql("PRAGMA user_version") == [(NEW_SCHEMA,)]
            assert home.sql(query) == baseline
            assert home.cli_job(new, created)["exit_code"] == 9
            extra, executor = home.submit(new, "exit 0")
            until("new history", lambda: home.job(extra)["state"] == "completed")
            assert executor.wait(timeout=5) == 0
            daemon.terminate()
            daemon.wait(timeout=5)
            records = home.sql("SELECT * FROM jobs ORDER BY id")
            refused = home.spawn(old, "daemon")
            assert refused.wait(timeout=5) != 0
            diagnostic = (root / f"process-{len(home.processes) - 1}.log").read_text()
            assert f"database schema version {NEW_SCHEMA} is newer than this ats ({old_schema})" in diagnostic, diagnostic
            assert home.sql("PRAGMA user_version") == [(NEW_SCHEMA,)]
            assert home.sql("SELECT * FROM jobs ORDER BY id") == records
            assert home.sql("PRAGMA integrity_check") == [("ok",)]
            with contextlib.closing(sqlite3.connect(backup)) as source:
                with contextlib.closing(sqlite3.connect(home.root / "ats.db")) as destination:
                    source.backup(destination)
            home.daemon(old)
            assert home.sql("PRAGMA user_version") == [(old_schema,)]
            assert home.sql("PRAGMA integrity_check") == [("ok",)]
            assert home.sql(query) == baseline
            assert home.cli_job(old, created)["exit_code"] == 9
            assert home.sql("SELECT COUNT(*) FROM jobs") == [(1,)]
            logs = Path(original["log_path"])
            assert (logs / "stdout.log").read_text() == "preserved history\n"
            print(json.dumps({"case": "refuse-new-schema-and-restore-backup", "result": "PASS",
                              "schema": [old_schema, NEW_SCHEMA, old_schema], "old_daemon_refused_schema_6": True,
                              "history_exit_code": 9, "integrity": "ok"}), flush=True)
        finally:
            home.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--old-binary", type=Path, required=True)
    parser.add_argument("--new-binary", type=Path, required=True)
    parser.add_argument("--case", choices=["upgrade", "cancel", "offline-report", "rollback"], action="append")
    args = parser.parse_args()
    old, new = args.old_binary.resolve(), args.new_binary.resolve()
    assert old != new, "old and new binaries must be distinct"
    for label, path in [("old", old), ("new", new)]:
        print(json.dumps({"binary": label, "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}))
    for case in args.case or ["upgrade", "cancel", "offline-report", "rollback"]:
        if case == "rollback":
            rollback(old, new)
        else:
            upgrade(old, new, case)


if __name__ == "__main__":
    main()
