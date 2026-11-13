#!/usr/bin/env python3
"""Measure isolated concurrent lifecycle and process-tree acceptance."""

import argparse
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
import hashlib
import http.client
import json
import math
from pathlib import Path
import shlex
import subprocess
import tempfile
import threading
import time

from acceptance import Home, until


def distribution(values):
    ordered = sorted(values)
    return {"count": len(ordered), "p50_ms": round(ordered[(len(ordered) - 1) // 2], 3),
            "p95_ms": round(ordered[math.ceil(len(ordered) * .95) - 1], 3),
            "max_ms": round(ordered[-1], 3)} if ordered else {"count": 0}


def timed(action):
    started = time.monotonic()
    result = action()
    return result, (time.monotonic() - started) * 1000


def exited(pids):
    rows = subprocess.check_output(["ps", "-axo", "pid=,stat="], text=True).splitlines()
    alive = {int(parts[0]) for row in rows if len(parts := row.split()) == 2 and not parts[1].startswith("Z")}
    return not set(pids) & alive


class Metrics:
    def __init__(self, home):
        self.home = home
        self.phase = "starting"
        self.stop = threading.Event()
        self.latencies = {}
        self.errors = []
        self.observations = {}
        self.last = {}
        self.gaps = []
        self.thread = threading.Thread(target=self.run)

    def run(self):
        while not self.stop.is_set():
            for route in ["/v1/jobs?limit=1000", "/v1/measurements"]:
                phase = self.phase
                started = time.monotonic()
                try:
                    status, body = self.home.request("GET", route)
                    assert status == 200, (status, body)
                    if phase == self.phase and phase != "restart":
                        self.latencies.setdefault((phase, route), []).append((time.monotonic() - started) * 1000)
                        if route == "/v1/measurements" and body["at"]:
                            previous = self.last.get(phase)
                            if previous is None or previous[0] != body["at"]:
                                now = time.monotonic()
                                if previous:
                                    self.gaps.append((phase, (now - previous[1]) * 1000))
                                self.last[phase] = (body["at"], now)
                                self.observations[phase] = self.observations.get(phase, 0) + 1
                except (OSError, http.client.HTTPException, AssertionError, ValueError) as error:
                    if phase != "restart" and self.phase != "restart":
                        self.errors.append(str(error))
            self.stop.wait(.05)

    def finish(self):
        self.stop.set()
        self.thread.join(timeout=10)
        assert not self.thread.is_alive(), "metrics probe did not stop"


def exercise(binary, jobs, children):
    with tempfile.TemporaryDirectory(prefix="ats-stress-") as directory:
        root = Path(directory)
        home = Home(root / "home")
        gate = root / "release"
        starts = root / "starts"
        metrics = Metrics(home)
        try:
            daemon = home.daemon(binary)
            metrics.thread.start()
            child_source = f"from pathlib import Path; import time\nwhile not Path({str(gate)!r}).exists(): time.sleep(.05)"
            sources = []
            for index in range(jobs):
                script = ("import os,pathlib,subprocess,sys,time,json\n"
                          f"children=[subprocess.Popen([sys.executable,'-c',{child_source!r}],start_new_session=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL) for _ in range({children})]\n"
                          f"pathlib.Path({str(root / str(index))!r}).write_text(json.dumps([os.getpid()]+[c.pid for c in children]))\n"
                          f"with open({str(starts)!r},'a') as f: f.write('{index}\\n')\n"
                          f"while not pathlib.Path({str(gate)!r}).exists(): time.sleep(.05)\n"
                          "for c in children: c.wait()\n"
                          f"print('result {index}'); sys.exit({index % 3})")
                sources.append("exec python3 -c " + shlex.quote(script))
            with ThreadPoolExecutor(max_workers=jobs) as pool:
                submitted = list(pool.map(lambda pair: timed(lambda: home.create(pair[1], "stress-shared" if pair[0] == 0 else f"job-{pair[0]}")), enumerate(sources)))
            created = [row for row, _ in submitted]
            metrics.latencies[("submitting", "POST /v1/jobs")] = [elapsed for _, elapsed in submitted]
            executors = [home.start_executor(binary, row, source) for row, source in zip(created, sources)]
            def started():
                try:
                    return all(len(json.loads((root / str(i)).read_text())) == children + 1 for i in range(jobs))
                except (FileNotFoundError, ValueError):
                    return False

            until("all process trees started", started, 45)
            owned = {row["job"]: json.loads((root / str(i)).read_text()) for i, row in enumerate(created)}

            def confirmed():
                status, snapshot = home.request("GET", "/v1/measurements")
                assert status == 200
                return all(set(pids) <= set(snapshot["jobs"].get(job, {}).get("processes", [])) for job, pids in owned.items())

            until("all escaped children confirmed", confirmed, 30)
            metrics.phase = "running"
            waiter, waiter_process = home.submit(binary, "printf 'waiter result\\n'", "stress-shared")
            until("lock waiter registered", lambda: home.job(waiter)["executor_pid"] == waiter_process.pid)
            assert home.job(waiter)["state"] == "queued"
            conflict = {"entry": "sh", "source": "echo must-not-run", "cwd": str(root), "lock": ["stress-shared"], "now": True}
            assert home.request("POST", "/v1/jobs", conflict)[0] == 409
            until("three loaded observations", lambda: metrics.observations.get("running", 0) >= 3)
            metrics.phase = "restart"
            daemon.kill()
            daemon.wait(timeout=5)
            home.daemon(binary)
            metrics.phase = "recovered"
            assert home.request("POST", "/v1/jobs", conflict)[0] == 409
            for row, executor in zip(created, executors):
                assert home.job(row)["executor_pid"] == executor.pid
            canceled = created[1::2]
            with ThreadPoolExecutor(max_workers=jobs) as pool:
                cancellations = list(pool.map(lambda row: timed(lambda: home.request(
                    "POST", f"/v1/submissions/{row['submission']}/cancel", {})), canceled))
            assert all(code == 204 for (code, _), _ in cancellations)
            metrics.latencies[("canceling", "POST /v1/submissions/:id/cancel")] = [elapsed for _, elapsed in cancellations]
            until("concurrent cancellation", lambda: all(home.job(row)["state"] == "canceled" for row in canceled), 30)
            until("canceled process trees exited", lambda: exited(
                pid for row in canceled for pid in owned[row["job"]]), 10)
            assert home.request("POST", "/v1/jobs", conflict)[0] == 409
            until("three recovered observations", lambda: metrics.observations.get("recovered", 0) >= 3)
            gate.touch()
            until("concurrent completion", lambda: all(home.job(row)["state"] in ("completed", "canceled") for row in created), 30)
            until("waiter completed", lambda: home.job(waiter)["state"] == "completed", 30)
            assert home.job(waiter)["exit_code"] == 0
            assert (Path(home.job(waiter)["log_path"]) / "stdout.log").read_text() == "waiter result\n"
            assert Counter(starts.read_text().splitlines()) == Counter(str(i) for i in range(jobs))
            for index, row in enumerate(created):
                result = home.job(row)
                assert result["state"] == ("canceled" if index % 2 else "completed")
                if index % 2 == 0:
                    assert result["exit_code"] == index % 3
                    assert (Path(result["log_path"]) / "stdout.log").read_text() == f"result {index}\n"
            for row in [*created, waiter]:
                assert home.sql("SELECT COUNT(*) FROM job_events WHERE job_id=? AND kind='started'", (row["job"],)) == [(1,)]
                assert home.sql("SELECT COUNT(*) FROM job_events WHERE job_id=? AND kind IN ('completed','canceled')", (row["job"],)) == [(1,)]
            assert home.sql("SELECT COUNT(*) FROM job_processes") == [(0,)]
            assert all(child.wait(timeout=5) == 0 for child in [*executors, waiter_process])
            until("all controlled processes exited", lambda: exited(
                pid for pids in owned.values() for pid in pids), 10)
            assert home.sql("PRAGMA integrity_check") == [("ok",)]
            metrics.finish()
            assert not metrics.errors, metrics.errors
            return {"jobs": jobs, "children_per_job": children, "confirmed_processes": jobs * (children + 1),
                    "result": "PASS", "api": {f"{phase} {route}": distribution(values) for (phase, route), values in metrics.latencies.items()},
                    "observed_sample_gap": distribution([gap for _, gap in metrics.gaps]),
                    "canceled": len(canceled), "completed": jobs - len(canceled) + 1}
        finally:
            gate.touch()
            try:
                if metrics.thread.ident is not None:
                    metrics.finish()
            finally:
                home.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--levels", type=int, nargs="+", default=[2, 4, 8])
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    assert all(2 <= size <= 16 for size in args.levels), "levels must be between 2 and 16"
    binary = args.binary.resolve()
    results = []
    for level in args.levels:
        result = exercise(binary, level, level)
        results.append(result)
        print(json.dumps(result), flush=True)
    if args.output:
        args.output.write_text(json.dumps({"binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
                                           "results": results}, indent=2) + "\n")


if __name__ == "__main__":
    main()
