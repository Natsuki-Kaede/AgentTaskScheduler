"""Isolated process and Unix-socket fixtures for acceptance scripts."""

import contextlib
import http.client
import json
import os
import socket
import sqlite3
import subprocess
import time


def until(label, check, seconds=15):
    deadline = time.monotonic() + seconds
    while True:
        value = check()
        if value:
            return value
        assert time.monotonic() < deadline, f"timed out: {label}"
        time.sleep(0.05)


class Home:
    def __init__(self, root):
        self.root = root
        self.env = {**os.environ, "ATS_HOME": str(root)}
        self.env.pop("ATS_JOB_ID", None)
        self.processes = []
        self.executors = []

    def spawn(self, binary, *args):
        log = open(self.root.parent / f"process-{len(self.processes)}.log", "wb")
        try:
            child = subprocess.Popen([str(binary), *args], env=self.env,
                                     cwd=self.root.parent, stdout=log, stderr=log)
        finally:
            log.close()
        self.processes.append(child)
        return child

    def request(self, method, path, body=None):
        with socket.socket(socket.AF_UNIX) as stream:
            stream.settimeout(3)
            stream.connect(str(self.root / "ats.sock"))
            payload = json.dumps(body).encode() if body is not None else b""
            stream.sendall(f"{method} {path} HTTP/1.1\r\nHost: ats\r\n"
                           f"Content-Type: application/json\r\nContent-Length: {len(payload)}\r\n"
                           "Connection: close\r\n\r\n".encode() + payload)
            response = http.client.HTTPResponse(stream)
            response.begin()
            raw = response.read()
            return response.status, json.loads(raw) if raw else None

    def daemon(self, binary):
        child = self.spawn(binary, "daemon")

        def ready():
            assert child.poll() is None, f"daemon exited: {binary}"
            try:
                status, body = self.request("GET", "/v1/health")
                return status == 200 and body["pid"] == child.pid
            except (OSError, http.client.HTTPException):
                return False

        until("daemon health", ready)
        return child

    def create(self, source, lock="upgrade"):
        status, created = self.request("POST", "/v1/jobs", {
            "entry": "sh", "source": source, "cwd": str(self.root.parent), "lock": [lock],
        })
        assert status == 201, (status, created)
        return created

    def start_executor(self, binary, created, source):
        child = self.spawn(binary, "__executor", "--job", created["job"], "--spec",
                           json.dumps({"entry": "sh", "source": source, "shell": "/bin/sh"}))
        self.executors.append((created, child))
        return child

    def submit(self, binary, source, lock="upgrade"):
        created = self.create(source, lock)
        return created, self.start_executor(binary, created, source)

    def job(self, created):
        status, row = self.request("GET", f"/v1/jobs/{created['job']}")
        assert status == 200, (status, row)
        return row

    def sql(self, query, params=()):
        with contextlib.closing(sqlite3.connect(self.root / "ats.db")) as db:
            return db.execute(query, params).fetchall()

    def pending_report(self, path):
        for request in (self.root / "requests").glob("*.req"):
            try:
                if json.loads(request.read_text())["path"] == path:
                    return True
            except FileNotFoundError:
                # The old client can withdraw another request while it times out.
                continue
        return False

    def close(self):
        # Cancel through the owning executor before shutting down its daemon.
        for created, child in self.executors:
            if child.poll() is None:
                with contextlib.suppress(OSError, http.client.HTTPException):
                    self.request("POST", f"/v1/submissions/{created['submission']}/cancel", {})
                with contextlib.suppress(subprocess.TimeoutExpired):
                    child.wait(timeout=3)
        for child in reversed(self.processes):
            if child.poll() is None:
                child.terminate()
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=5)

    def conflict(self):
        status, body = self.request("POST", "/v1/jobs", {
            "entry": "sh", "source": "echo must-not-run", "cwd": str(self.root.parent),
            "lock": ["upgrade"], "now": True,
        })
        assert status == 409 and "lock" in body["error"], (status, body)

    def cli_job(self, binary, created):
        output = subprocess.run([str(binary), "status", created["job"], "--json"],
                                env=self.env, capture_output=True, text=True, timeout=5, check=True)
        return json.loads(output.stdout)
