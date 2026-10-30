#!/usr/bin/env python3
"""Bounded, local-only history benchmark. Never reads or seeds user data.

Example:
  python3 scripts/history_benchmark.py --daemon /private/pinned/harness-daemon \
    --cli /private/pinned/self-agent-cli --source-commit <sha> \
    --artifact-dir /private/new-results --profile small --keep-data

The supplied binaries are not built or replaced. HTTP is exclusively loopback.
All tasks are synthetic completed records; no model, tool, vault or QQ is invoked.
"""

import argparse
from contextlib import closing
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import sqlite3
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import uuid


MIB = 1024 * 1024
CLIENT_CAP = 16 * MIB
MAX_RESPONSE = 96 * MIB
MAX_DB = 128 * MIB
WINDOW_CAP = 12 * MIB
PAGE_CAP = 2 * MIB
DETAIL_CAP = 8 * MIB
PROFILES = {
    "small": (10, 10, 512, 1024),
    "long_session": (1, 1000, 1024, 4096),
    "over_snapshot": (2, 750, 1024, 4096),
    "large_history": (100, 50, 512, 2048),
}
NAMESPACE = uuid.UUID("45b5f8e2-e4ad-4605-9728-e2631fcb5395")
STAMP = "2020-01-01T00:00:00.000Z"
SOURCE = {"channel_id": "cli", "actor_id": "local", "conversation_id": "history-benchmark"}
OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def compact(value):
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"))


def identifier(kind, index):
    return str(uuid.uuid5(NAMESPACE, f"{kind}:{index}"))


def payload(size, label):
    prefix = f"SYNTHETIC_{label}:"
    assert len(prefix) <= size
    return prefix + "x" * (size - len(prefix))


def digest_file(path):
    result = hashlib.sha256()
    with Path(path).open("rb") as file:
        for block in iter(lambda: file.read(MIB), b""):
            result.update(block)
    return result.hexdigest()


def percentile(values, probability):
    """Nearest-rank percentile; small-n P95 often equals max, not a tail SLA."""
    if not values or not 0 < probability <= 1:
        raise ValueError("percentile requires samples and 0 < probability <= 1")
    ordered = sorted(values)
    return ordered[math.ceil(probability * len(ordered)) - 1]


def summarize(samples):
    successful = [sample["elapsed_ms"] for sample in samples if sample["status"] == "passed"]
    return {
        "status": "not_run" if not samples else ("passed" if len(successful) == len(samples) else "failed"),
        "samples": samples, "n": len(samples), "passed": len(successful),
        "failed": len(samples) - len(successful),
        "p50_ms": percentile(successful, 0.5) if successful else None,
        "p95_ms": percentile(successful, 0.95) if successful else None,
        "min_ms": min(successful) if successful else None,
        "max_ms": max(successful) if successful else None,
        "percentile_population": "successful samples only; failures retained separately",
    }


def clean_environment(home):
    # Do not inherit provider, proxy, Codex, vault or adapter configuration.
    keep = {key: os.environ[key] for key in ("PATH", "SystemRoot", "WINDIR", "LD_LIBRARY_PATH") if key in os.environ}
    keep.update(HOME=str(home), USERPROFILE=str(home), XDG_CONFIG_HOME=str(home / "config"),
                XDG_DATA_HOME=str(home / "data"), HARNESS_DEMO_STEP_MS="1", RUST_LOG="error")
    return keep


def proc_stats(pid):
    if sys.platform != "linux":
        return {"status": "not_measured", "reason": "Linux /proc only"}
    try:
        status = Path(f"/proc/{pid}/status").read_text()
        fields = {}
        for line in status.splitlines():
            name, _, value = line.partition(":")
            if name in ("VmRSS", "VmHWM"):
                fields[name + "_bytes"] = int(value.split()[0]) * 1024
        stat = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        fields["cpu_seconds"] = (int(stat[11]) + int(stat[12])) / os.sysconf("SC_CLK_TCK")
        fields["status"] = "measured"
        return fields
    except (OSError, ValueError, IndexError):
        return {"status": "not_measured", "reason": "process /proc unavailable"}


def stop_process(process):
    if process is None or process.poll() is not None:
        return
    if os.name == "posix":
        os.killpg(process.pid, signal.SIGTERM)
    else:
        process.terminate()
    try:
        process.wait(timeout=3)
    except subprocess.TimeoutExpired:
        if os.name == "posix":
            os.killpg(process.pid, signal.SIGKILL)
        else:
            process.kill()
        process.wait(timeout=3)


def deadline_read(response, count, deadline):
    """One socket read with the remaining total budget, not a resettable timeout."""
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise TimeoutError("HTTP/SSE operation total deadline")
    # CPython HTTPResponse owns this socket even for Connection: close. Fail
    # rather than silently substitute an unbounded trickle-read on another API.
    # urllib HTTPError wraps the actual HTTPResponse in fp; successful
    # responses already are HTTPResponse. Preserve the same deadline for both.
    stream = response.fp if isinstance(response, urllib.error.HTTPError) else response
    if stream is None or stream.fp is None:
        return b""
    stream.fp.raw._sock.settimeout(remaining)
    return stream.read1(count)


class Daemon:
    def __init__(self, executable, directory, timeout):
        self.executable, self.directory, self.timeout = executable, directory, timeout
        self.process = self.log = None
        self.base = self.token = None
        self.home = directory.parent / "isolated-home"
        self.home.mkdir(exist_ok=True, mode=0o700)

    def start(self):
        started = time.monotonic()
        self.directory.mkdir(mode=0o700, exist_ok=True)
        env = clean_environment(self.home)
        env.update(HARNESS_DATA_DIR=str(self.directory), HARNESS_BIND="127.0.0.1:0")
        self.log = (self.directory.parent / "daemon.log").open("ab")
        self.process = subprocess.Popen([str(self.executable)], env=env, cwd=self.home,
                                        stdout=self.log, stderr=self.log, start_new_session=True)
        deadline = started + self.timeout
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise RuntimeError(f"daemon exited {self.process.returncode}; see private daemon.log")
            try:
                info = json.loads((self.directory / "daemon.json").read_text())
                if info["pid"] == self.process.pid:
                    assert info["base_url"].startswith("http://127.0.0.1:")
                    self.base, self.token = info["base_url"], info["token"]
                    _, health, _ = self.request("/health", timeout=min(1, deadline - time.monotonic()))
                    if json.loads(health)["status"] == "ok":
                        return {"status": "passed", "elapsed_ms": (time.monotonic() - started) * 1000}
            except (OSError, ValueError, urllib.error.URLError):
                pass
            time.sleep(0.01)
        raise TimeoutError("daemon startup hard deadline")

    def stop(self):
        stop_process(self.process)
        if self.log:
            self.log.close()
        self.process = self.log = None

    def request(self, path, body=None, timeout=None, max_bytes=MAX_RESPONSE, allow_http_error=False):
        started = time.monotonic()
        request = urllib.request.Request(self.base + path,
            data=compact(body).encode() if body is not None else None,
            headers={"Authorization": "Bearer " + self.token, "Content-Type": "application/json"})
        timeout = timeout or self.timeout
        try:
            response = OPENER.open(request, timeout=timeout)
        except urllib.error.HTTPError as error:
            if not allow_http_error:
                raise
            response = error
        with response:
            data = bytearray()
            while True:
                if time.monotonic() - started > timeout:
                    raise TimeoutError("HTTP operation total deadline")
                block = deadline_read(response, 64 * 1024, started + timeout)
                if not block:
                    break
                if len(data) + len(block) > max_bytes:
                    raise ValueError(f"HTTP response exceeds {max_bytes} byte safety ceiling; not truncated as success")
                data.extend(block)
        elapsed = (time.monotonic() - started) * 1000
        return elapsed, bytes(data), response.status

    def replay(self, after, count):
        started = time.monotonic()
        self.replay_progress = {"events_received": 0, "last_sequence": after, "bytes_received": 0}
        request = urllib.request.Request(self.base + f"/api/v1/events?after={after}",
                                        headers={"Authorization": "Bearer " + self.token})
        total = 0
        cursor = after
        first_ms = None
        with OPENER.open(request, timeout=self.timeout) as response:
            pending = bytearray()
            while cursor < after + count:
                if time.monotonic() - started > self.timeout:
                    raise TimeoutError("SSE replay total deadline")
                fields, frame_bytes = {}, 0
                while True:
                    while b"\n" not in pending:
                        block = deadline_read(response, 64 * 1024, started + self.timeout)
                        if not block:
                            raise ValueError("SSE closed before requested retained events")
                        pending.extend(block)
                        if len(pending) > 512 * 1024:
                            raise ValueError("SSE line safety ceiling")
                    end = pending.index(b"\n") + 1
                    line = bytes(pending[:end])
                    del pending[:end]
                    frame_bytes += len(line)
                    total += len(line)
                    if frame_bytes > 512 * 1024 or total > MAX_RESPONSE:
                        raise ValueError("SSE benchmark byte budget exceeded")
                    if line in (b"\n", b"\r\n"):
                        if fields:
                            break
                        continue
                    key, _, value = line.decode().strip().partition(":")
                    fields[key] = value.lstrip()
                event = json.loads(fields["data"])
                assert fields["event"] == "run_event" and int(fields["id"]) == cursor + 1
                assert event["sequence"] == cursor + 1
                cursor += 1
                self.replay_progress = {"events_received": cursor - after, "last_sequence": cursor, "bytes_received": total}
                if first_ms is None:
                    first_ms = (time.monotonic() - started) * 1000
        return {"status": "passed", "elapsed_ms": (time.monotonic() - started) * 1000,
                "first_event_ms": first_ms, "bytes": total, "events": count, "last_sequence": cursor}


def seed_database(path, project, profile):
    sessions, per_session, prompt_bytes, reply_bytes = profile
    tasks = sessions * per_session
    event_total = tasks * 20
    retained = min(event_total, 20_000)
    first_sequence = event_total - retained + 1
    prompt, reply = payload(prompt_bytes, "PROMPT"), payload(reply_bytes, "REPLY")
    db = sqlite3.connect(path)
    try:
        with db:
            # The schema itself must have been initialized by the supplied daemon.
            for table in ("sessions", "tasks", "events", "commands"):
                if db.execute(f"SELECT COUNT(*) FROM {table}").fetchone()[0]:
                    raise ValueError("Refusing to seed a nonempty history table")
            for session_index in range(sessions):
                session_id = identifier("session", session_index)
                messages = []
                for offset in range(per_session):
                    index = session_index * per_session + offset
                    task_id = identifier("task", index)
                    task = {"id": task_id, "session_id": session_id, "title": f"合成历史任务 {index}",
                        "objective": prompt, "status": "completed", "execution_mode": "demo",
                        "task_kind": "main", "main_sequence": offset + 1, "created_at": STAMP,
                        "updated_at": STAMP, "source": SOURCE, "result": reply, "stream_output": reply,
                        "stream_output_truncated": False, "steps": [], "evidence": [], "supplements": [],
                        "agent": {"id": "coder", "name": "合成夹具", "instructions": "", "executor": "native", "model": ""}}
                    db.execute("INSERT INTO tasks(id,body) VALUES(?,?)", (task_id, compact(task)))
                    command_id = identifier("command", index)
                    envelope = {"command_id": command_id, "source": SOURCE,
                        "command": {"type": "run_agent", "session_id": session_id, "content": prompt}}
                    receipt = {"command_id": command_id, "status": "accepted", "task_id": task_id, "session_id": session_id}
                    db.execute("INSERT INTO commands(id,envelope,receipt) VALUES(?,?,?)",
                               (command_id, compact(envelope), compact(receipt)))
                    for role, content in (("user", prompt), ("assistant", reply)):
                        messages.append({"id": identifier(role, index), "role": role, "content": content,
                                         "created_at": STAMP, "task_id": task_id})
                session = {"id": session_id, "title": f"合成长会话 {session_index}", "project_path": str(project),
                           "created_at": STAMP, "updated_at": STAMP, "messages": messages}
                db.execute("INSERT INTO sessions(id,body) VALUES(?,?)", (session_id, compact(session)))
            for sequence in range(first_sequence, event_total + 1):
                index = (sequence - 1) // 20
                terminal = sequence % 20 == 0
                event = {"id": identifier("event", sequence), "sequence": sequence,
                    "session_id": identifier("session", index // per_session), "task_id": identifier("task", index),
                    "kind": "task_completed" if terminal else "assistant_delta", "created_at": STAMP,
                    "message": "合成终态事件" if terminal else "合成历史增量",
                    "data": None if terminal else {"kind": "assistant_delta", "text": payload(128, "DELTA")}}
                db.execute("INSERT INTO events(sequence,id,body) VALUES(?,?,?)", (sequence, event["id"], compact(event)))
        assert db.execute("PRAGMA integrity_check").fetchone()[0] == "ok"
        assert not db.execute("PRAGMA foreign_key_check").fetchall()
        db.execute("PRAGMA wal_checkpoint(TRUNCATE)")
        size = db.execute("PRAGMA page_count").fetchone()[0] * db.execute("PRAGMA page_size").fetchone()[0]
        if size >= MAX_DB:
            raise ValueError("Dataset exceeds strict <128 MiB benchmark database limit")
        return {"sessions": sessions, "tasks": tasks, "messages": tasks * 2, "commands": tasks,
                "prompt_utf8_bytes": prompt_bytes, "reply_utf8_bytes": reply_bytes,
                "task_result_and_stream_output_each_equal_reply": True,
                "events_total_sequence": event_total, "events_retained": retained, "first_event_sequence": first_sequence,
                "database_bytes": size, "schema_version": db.execute("PRAGMA user_version").fetchone()[0],
                "seed_sha256": digest_file(path)}
    finally:
        db.close()


def inspect_fixture_history(path):
    """Read-only logical fingerprint; includes immutable messages and receipts."""
    digest = hashlib.sha256()
    counts = {}
    with closing(sqlite3.connect(path.as_uri() + "?mode=ro", uri=True)) as db, db:
        for table, columns in (("sessions", "id,body"), ("tasks", "id,body"),
                               ("commands", "id,envelope,receipt"), ("events", "sequence,id,body")):
            count = 0
            for row in db.execute(f"SELECT {columns} FROM {table} ORDER BY id"):
                digest.update((table + ":" + compact(row) + "\n").encode())
                count += 1
            counts[table] = count
        counts["messages"] = db.execute("SELECT coalesce(sum(json_array_length(json_extract(body,'$.messages'))),0) FROM sessions").fetchone()[0]
        counts["not_completed_tasks"] = db.execute("SELECT count(*) FROM tasks WHERE json_extract(body,'$.status')!='completed'").fetchone()[0]
        counts["event_cursor"] = db.execute("SELECT coalesce(max(sequence),0) FROM events").fetchone()[0]
        return {"counts": counts, "history_sha256": digest.hexdigest(),
                "schema_version": db.execute("PRAGMA user_version").fetchone()[0],
                "database_bytes": db.execute("PRAGMA page_count").fetchone()[0] * db.execute("PRAGMA page_size").fetchone()[0]}


def archived_seeds(result_paths):
    seeds = {}
    for path in result_paths:
        path = path.resolve(strict=True)
        if path.stat().st_size > 8 * MIB:
            raise ValueError("Seed result metadata exceeds 8 MiB")
        document = json.loads(path.read_text())
        for case in document["cases"]:
            name = case["profile"]
            if name in seeds:
                raise ValueError(f"Duplicate archived seed profile: {name}")
            dataset = case["dataset"]
            seed = path.parent / name / "seed.sqlite"
            if seed.is_symlink() or not seed.is_file() or seed.stat().st_size >= MAX_DB:
                raise ValueError("Archived seed must be an ordinary <128 MiB file")
            if digest_file(seed) != dataset["seed_sha256"]:
                raise ValueError("Archived seed SHA-256 mismatch; never repair or overwrite it")
            seeds[name] = {"path": seed, "dataset": dataset, "source_results": str(path)}
    return seeds


def cli_run(executable, data, arguments, timeout, home, ndjson=False):
    started = time.monotonic()
    with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
        process = subprocess.Popen([str(executable), "--data-dir", str(data), *arguments],
            cwd=home, env=clean_environment(home), stdout=stdout, stderr=stderr, start_new_session=True)
        try:
            process.wait(timeout=timeout)
            elapsed = (time.monotonic() - started) * 1000
        except subprocess.TimeoutExpired:
            return {"status": "failed", "elapsed_ms": (time.monotonic() - started) * 1000, "reason": "CLI hard deadline"}, None
        finally:
            stop_process(process)
        size = stdout.tell()
        stderr.seek(0)
        error = stderr.read(8192).decode("utf-8", errors="replace")
        sample = {"status": "passed" if process.returncode == 0 else "failed", "elapsed_ms": elapsed,
                  "exit_code": process.returncode, "stdout_bytes": size, "stderr": error}
        if size > MAX_RESPONSE:
            sample.update(status="failed", reason="CLI stdout exceeds benchmark budget")
            return sample, None
        stdout.seek(0)
        data = stdout.read()
        sample["stdout_sha256"] = hashlib.sha256(data).hexdigest()
        value = ([json.loads(line) for line in data.splitlines() if line] if ndjson else json.loads(data)) if process.returncode == 0 else None
        return sample, value


def http_sample(daemon, path, validator, max_bytes=MAX_RESPONSE):
    started = time.monotonic()
    try:
        elapsed, data, status = daemon.request(path, max_bytes=max_bytes)
        parsed = json.loads(data)
        validator(parsed)
        return {"status": "passed", "elapsed_ms": elapsed, "http_status": status,
                "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}
    except (AssertionError, KeyError, TypeError, OSError, ValueError, urllib.error.URLError) as error:
        return {"status": "failed", "elapsed_ms": (time.monotonic() - started) * 1000,
                "reason": str(error)[:512] or type(error).__name__}



def assert_subset(actual, expected, field="record"):
    """Legacy-compatible serializers may add defaults, never alter seeded fields."""
    if isinstance(expected, dict):
        assert isinstance(actual, dict), f"{field}: object required"
        for key, value in expected.items():
            assert key in actual, f"{field}.{key}: seeded field missing"
            assert_subset(actual[key], value, f"{field}.{key}")
    elif isinstance(expected, list):
        assert isinstance(actual, list) and len(actual) == len(expected), f"{field}: array shortened"
        for index, value in enumerate(expected):
            assert_subset(actual[index], value, f"{field}[{index}]")
    else:
        assert actual == expected, f"{field}: seeded value changed/truncated"


class SeedOracle:
    """Read-only original seed, one session's messages at a time (not all history)."""
    def __init__(self, path):
        self.db = sqlite3.connect(path.resolve().as_uri() + "?mode=ro", uri=True)
        self.session_id = self.session = None

    def close(self):
        self.db.close()

    def row(self, table, identifier):
        assert table in {"sessions", "tasks", "events"}
        row = self.db.execute(f"SELECT body FROM {table} WHERE id=?", (identifier,)).fetchone()
        assert row is not None, f"unknown synthetic {table} ID"
        return json.loads(row[0])

    def check_task(self, task):
        assert_subset(task, self.row("tasks", task["id"]), "task")

    def check_event(self, event):
        expected = self.row("events", event["id"])
        # RunEvent.data explicitly uses skip_serializing_if=Option::is_none.
        # Missing and null terminal data are equivalent; non-null payloads must
        # remain exact, including every text byte.
        if expected.get("data") is None:
            assert event.get("data") is None
            expected.pop("data", None)
        assert_subset(event, expected, "event")

    def session_row(self, identifier):
        if self.session_id != identifier:
            self.session_id, self.session = identifier, self.row("sessions", identifier)
        return self.session

    def check_session(self, session):
        expected = self.session_row(session["id"])
        assert_subset(session, {key: value for key, value in expected.items() if key != "messages"}, "session")

    def check_message(self, session_id, message):
        offset = message["history_offset"]
        expected = self.session_row(session_id)["messages"]
        assert type(offset) is int and 0 <= offset < len(expected)
        assert_subset(message, expected[offset], "message")

    def task_ids(self):
        return {row[0] for row in self.db.execute("SELECT id FROM tasks")}

    def session_ids(self):
        return [row[0] for row in self.db.execute("SELECT id FROM sessions ORDER BY rowid")]


def valid_anchor(value):
    assert isinstance(value, str) and str(uuid.UUID(value)) == value, "invalid history epoch UUID"


def check_collection(items, metadata, total):
    assert isinstance(items, list) and isinstance(metadata, dict)
    assert metadata["total"] == total and metadata["returned"] == len(items)
    assert 0 <= len(items) <= total
    assert metadata["omitted"] == total - len(items)
    assert metadata["has_more"] is (total > len(items))
    ids = [item["id"] for item in items]
    assert len(ids) == len(set(ids)), "duplicate returned IDs"
    missing = metadata["unavailable"]
    assert len(missing) <= metadata["omitted"]
    missing_ids = [item["id"] for item in missing]
    assert len(missing_ids) == len(set(missing_ids)) and not set(ids).intersection(missing_ids)
    assert all(item["reason"] == "record_too_large" for item in missing)
    assert metadata.get("pending", 0) == 0, "completed-only fixture cannot have pending work"


def bounded_snapshot(value, dataset, oracle):
    history = value["history"]
    assert history["version"] == 1
    valid_anchor(history["anchor"])
    assert value["event_cursor"] == dataset["events_total_sequence"]
    totals = {"sessions": dataset["sessions"], "tasks": dataset["tasks"], "events": dataset["events_retained"],
              "plans": 0, "plan_changes": 0, "deliveries": 0, "execution_requests": 0, "context_states": 0, "selections": 0}
    assert set(history["collections"]) == set(totals)
    for kind, total in totals.items():
        check_collection(value[kind], history["collections"][kind], total)
        assert len(value[kind]) <= 80
    for task in value["tasks"]:
        oracle.check_task(task)
    for session in value["sessions"]:
        oracle.check_session(session)
        meta = history["session_messages"][session["id"]]
        expected = oracle.session_row(session["id"])["messages"]
        assert meta["total"] == len(expected) and meta["returned"] == len(session["messages"])
        offset = meta["offset"]
        missing = meta["unavailable"]
        assert 0 <= offset <= len(expected)
        assert meta["has_more_before"] is (offset > 0)
        shown_positions = [item["history_offset"] for item in session["messages"]]
        assert shown_positions == sorted(shown_positions), "message window order changed"
        for item in missing:
            assert item["reason"] == "record_too_large" and item["id"] == expected[item["offset"]]["id"]
        positions = shown_positions + [item["offset"] for item in missing]
        assert sorted(positions) == list(range(offset, len(expected))), "unreported message window gap"
        for item in session["messages"]:
            oracle.check_message(session["id"], item)
    event_sequences = [event["sequence"] for event in value["events"]]
    assert event_sequences == sorted(set(event_sequences)), "window event order changed"
    for event in value["events"]:
        oracle.check_event(event)


def task_detail(value, task_id, dataset, oracle):
    assert value["task"]["id"] == task_id
    oracle.check_task(value["task"])
    assert value["event_cursor"] == dataset["events_total_sequence"]
    total = oracle.db.execute("SELECT count(*) FROM events WHERE json_extract(body,'$.task_id')=?", (task_id,)).fetchone()[0]
    check_collection(value["events"], value["history"]["events"], total)
    check_collection(value["execution_requests"], value["history"]["execution_requests"], 0)
    sequences = []
    for event in value["events"]:
        assert event["task_id"] == task_id
        oracle.check_event(event)
        sequences.append(event["sequence"])
    assert sequences == sorted(set(sequences))


def check_page(value, kind, dataset, oracle, session_id=None, expected_anchor=None):
    valid_anchor(value["anchor"])
    if expected_anchor is not None:
        assert value["anchor"] == expected_anchor, "epoch changed during traversal"
    assert value["event_cursor"] == dataset["events_total_sequence"]
    items, missing = value["items"], value["unavailable"]
    assert value["returned"] == len(items) and len(items) + len(missing) <= 80
    # The canonical fixture records are small enough to fit a page; an omission
    # must fail this full-field regression rather than being treated as recovery.
    assert not missing, "small synthetic records unexpectedly unavailable"
    assert len({item["id"] for item in items}) == len(items)
    assert type(value["has_more"]) is bool
    cursor = value["next_cursor"]
    assert value["has_more"] is (cursor is not None)
    if cursor is not None:
        assert isinstance(cursor, str) and re.fullmatch(r"[A-Za-z0-9_-]{1,2048}", cursor)
        assert items, "non-progressing empty page"
    if kind == "tasks":
        assert value["total"] == dataset["tasks"]
        for item in items:
            oracle.check_task(item)
    elif kind == "messages":
        total = len(oracle.session_row(session_id)["messages"])
        assert value["total"] == total
        offset = value["offset"]
        assert type(offset) is int and 0 <= offset <= total
        assert value["has_more"] is (offset > 0)
        assert [item["history_offset"] for item in items] == list(range(offset, offset + len(items)))
        for item in items:
            oracle.check_message(session_id, item)
    return {"returned": len(items), "total": value["total"], "has_more": value["has_more"], "anchor": value["anchor"]}


def page_traversal(daemon, kind, dataset, oracle, check_deadline):
    """Each HTTP request retains 30s budget; the multi-page sweep uses case budget.
    The Python verifier's ID sets are NOT evidence for GUI/daemon memory bounds.
    """
    started = time.monotonic()
    samples, seen_ids, restart_cursor = [], set(), None
    sessions = oracle.session_ids() if kind == "messages" else [None]
    anchor = None
    try:
        for session_id in sessions:
            cursor, cursors, offsets = None, set(), set()
            base = f"/api/v1/history/sessions/{session_id}/messages" if kind == "messages" else "/api/v1/history/tasks"
            previous_offset = None
            while True:
                check_deadline()
                path = base + (f"?cursor={cursor}" if cursor is not None else "")
                elapsed, data, status = daemon.request(path, max_bytes=PAGE_CAP)
                value = json.loads(data)
                meta = check_page(value, kind, dataset, oracle, session_id, anchor)
                anchor = value["anchor"]
                if kind == "messages":
                    if previous_offset is not None:
                        assert value["offset"] + len(value["items"]) == previous_offset, "message page gap"
                    else:
                        assert value["offset"] + len(value["items"]) == value["total"]
                    previous_offset = value["offset"]
                for item in value["items"]:
                    assert item["id"] not in seen_ids, "duplicate ID across pages"
                    seen_ids.add(item["id"])
                    if kind == "messages":
                        assert item["history_offset"] not in offsets
                        offsets.add(item["history_offset"])
                samples.append({"status": "passed", "elapsed_ms": elapsed, "bytes": len(data), "http_status": status,
                                "sha256": hashlib.sha256(data).hexdigest(), "session_id": session_id, **meta})
                cursor = value["next_cursor"]
                if cursor is None:
                    break
                assert cursor not in cursors, "repeated pagination cursor"
                cursors.add(cursor)
                if restart_cursor is None:
                    restart_cursor = {"path": base + "?cursor=" + cursor, "anchor": anchor}
            if kind == "messages":
                assert offsets == set(range(len(oracle.session_row(session_id)["messages"])))
        expected_count = dataset["tasks"] if kind == "tasks" else dataset["messages"]
        assert len(seen_ids) == expected_count
        if kind == "tasks":
            assert seen_ids == oracle.task_ids(), "task page IDs differ from seed"
        sample = {"status": "passed", "elapsed_ms": (time.monotonic() - started) * 1000,
                  "pages": len(samples), "bytes": sum(item["bytes"] for item in samples), "items": len(seen_ids),
                  "max_page_bytes": max((item["bytes"] for item in samples), default=0),
                  "page_requests": summarize(samples), "anchor": anchor,
                  "request_budget_scope": "each page; full sweep bounded by dataset deadline"}
    except (AssertionError, KeyError, TypeError, OSError, ValueError, urllib.error.URLError) as error:
        sample = {"status": "failed", "elapsed_ms": (time.monotonic() - started) * 1000,
                  "reason": str(error)[:512] or type(error).__name__, "pages": len(samples),
                  "items": len(seen_ids), "page_requests": summarize(samples)}
    return sample, restart_cursor


def legacy_snapshot_sample(daemon, dataset, validator):
    started = time.monotonic()
    try:
        elapsed, data, status = daemon.request("/api/v1/snapshot", allow_http_error=True)
        value = json.loads(data)
        if status == 200:
            assert "history" not in value, "legacy route silently returned a partial window"
            validator(value)
            assert len(data) <= CLIENT_CAP
            outcome = "complete_legacy_snapshot"
        else:
            assert status == 413 and value["code"] == "record_too_large", "unexpected legacy status"
            assert len(data) <= 8192
            outcome = "intentional_legacy_capacity_rejection_not_new_client_failure"
        return {"status": "passed", "elapsed_ms": elapsed, "http_status": status, "bytes": len(data), "outcome": outcome}
    except (AssertionError, KeyError, TypeError, OSError, ValueError, urllib.error.URLError) as error:
        return {"status": "failed", "elapsed_ms": (time.monotonic() - started) * 1000,
                "reason": str(error)[:512] or type(error).__name__}


def benchmark_case(args, name, case):
    started = time.monotonic()
    deadline = started + args.case_timeout
    data, project = case / "data", case / "project"
    project.mkdir(mode=0o700)
    daemon = Daemon(args.daemon, data, args.timeout)
    result = {"profile": name, "status": "running", "metrics": {}}
    oracle = None
    original_history = None
    restart_cursors = []
    if args.bounded_history:
        result["bounded_history_mode"] = True

    def record_metric(name, samples):
        result["metrics"][name] = summarize(samples)
        (case / "progress.json").write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")
        print(compact({"profile": result["profile"], "metric_finished": name,
                       "n": len(samples), "failed": result["metrics"][name]["failed"]}), flush=True)

    previous_alarm = None
    if hasattr(signal, "setitimer"):
        def expired(*_args):
            raise TimeoutError("dataset total hard deadline")
        previous_alarm = signal.signal(signal.SIGALRM, expired)
        signal.setitimer(signal.ITIMER_REAL, args.case_timeout)

    def check_deadline():
        if time.monotonic() >= deadline:
            raise TimeoutError("dataset total deadline")

    def legacy_snapshots(value):
        assert len(value["sessions"]) == dataset["sessions"]
        assert len(value["tasks"]) == dataset["tasks"]
        assert sum(len(session["messages"]) for session in value["sessions"]) == dataset["messages"]
        assert all(task["status"] == "completed" for task in value["tasks"])
        assert value["event_cursor"] == dataset["events_total_sequence"]
        assert len(value["events"]) == min(300, dataset["events_retained"])

    def snapshots(value):
        if args.bounded_history:
            bounded_snapshot(value, dataset, oracle)
        else:
            legacy_snapshots(value)

    def diagnostics(value):
        assert value["counts"]["sessions"] == dataset["sessions"]
        assert value["counts"]["tasks_completed"] == dataset["tasks"]
        assert value["counts"]["tasks_running"] == value["counts"]["tasks_pending"] == 0
        assert value["counts"]["events_retained"] == dataset["events_retained"]
        assert value["vault"] == "not_checked"

    try:
        if args.seeds:
            source = args.seeds[name]
            data.mkdir(mode=0o700)
            shutil.copyfile(source["path"], data / "harness.sqlite")
            dataset = dict(source["dataset"])
            result["dataset"] = dataset
            before_migration = inspect_fixture_history(data / "harness.sqlite")
            result["reused_seed"] = {"path": str(source["path"]), "source_results": source["source_results"],
                "source_sha256": dataset["seed_sha256"], "before_migration": before_migration}
            if args.keep_data:
                shutil.copyfile(data / "harness.sqlite", case / "seed.sqlite")
            record_metric("migration_first_start", [daemon.start()])
            daemon.stop()
            after_migration = inspect_fixture_history(data / "harness.sqlite")
            result["reused_seed"]["after_migration"] = after_migration
            assert before_migration["history_sha256"] == after_migration["history_sha256"], "Migration changed synthetic immutable history"
            assert before_migration["counts"] == after_migration["counts"]
            assert after_migration["counts"]["not_completed_tasks"] == 0
            assert after_migration["database_bytes"] < MAX_DB
            dataset["source_schema_version"] = dataset["schema_version"]
            dataset["schema_version"] = after_migration["schema_version"]
            dataset["migrated_database_bytes"] = after_migration["database_bytes"]
        else:
            daemon.start()  # Real schema initialization, not a benchmark startup sample.
            daemon.stop()
            dataset = seed_database(data / "harness.sqlite", project, PROFILES[name])
            result["dataset"] = dataset
            if args.keep_data:
                shutil.copyfile(data / "harness.sqlite", case / "seed.sqlite")
        if args.bounded_history:
            oracle_path = args.seeds[name]["path"] if args.seeds else data / "harness.sqlite"
            oracle = SeedOracle(oracle_path)
            original_history = inspect_fixture_history(data / "harness.sqlite")
        startup = []
        for _ in range(args.startup_samples):
            check_deadline()
            startup.append(daemon.start())
            daemon.stop()
        record_metric("startup_to_health", startup)
        daemon.start()
        time.sleep(0.5)
        before = proc_stats(daemon.process.pid)
        idle_start = time.monotonic()
        time.sleep(2)
        after = proc_stats(daemon.process.pid)
        result["idle"] = {"before": before, "after": after, "wall_seconds": time.monotonic() - idle_start}
        if "cpu_seconds" in before and "cpu_seconds" in after:
            result["idle"]["process_cpu_percent_one_core"] = 100 * (after["cpu_seconds"] - before["cpu_seconds"]) / result["idle"]["wall_seconds"]
        (case / "progress.json").write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")
        print(compact({"profile": name, "idle": result["idle"]}), flush=True)
        for metric, path, validator in (
            ("http_snapshot", "/api/v1/snapshot?window=1" if args.bounded_history else "/api/v1/snapshot", snapshots),
            ("http_diagnostics", "/api/v1/reliability/diagnostics", diagnostics),
        ):
            samples = []
            for _ in range(args.samples):
                check_deadline()
                samples.append(http_sample(daemon, path, validator, WINDOW_CAP if args.bounded_history and metric == "http_snapshot" else MAX_RESPONSE))
            record_metric(metric, samples)
        result["resources_after_http"] = proc_stats(daemon.process.pid)
        snapshot_sizes = [sample["bytes"] for sample in result["metrics"]["http_snapshot"]["samples"] if "bytes" in sample]
        snapshot_cap = WINDOW_CAP if args.bounded_history else CLIENT_CAP
        result["snapshot_client_budget"] = {"limit_bytes": snapshot_cap, "max_observed_bytes": max(snapshot_sizes, default=0),
            "status": "failed" if len(snapshot_sizes) != args.samples or any(size > snapshot_cap for size in snapshot_sizes) else "passed"}
        selected_task = identifier("task", 0)
        if args.bounded_history:
            record_metric("http_legacy_snapshot", [legacy_snapshot_sample(daemon, dataset, legacy_snapshots) for _ in range(args.samples)])
            # Select an actual absent ID, not an assumption about window ordering.
            _, raw_window, _ = daemon.request("/api/v1/snapshot?window=1", max_bytes=WINDOW_CAP)
            window = json.loads(raw_window)
            snapshots(window)
            visible = {task["id"] for task in window["tasks"]}
            outside = sorted(oracle.task_ids() - visible)
            assert outside, "fixture must have an off-window task for this regression"
            selected_task = outside[0]
            result["bounded_history"] = {"snapshot_history": window["history"], "snapshot_wire_bytes": len(raw_window),
                "off_window_task_id": selected_task, "off_window_verified": selected_task not in visible,
                "page_limit_bytes": PAGE_CAP, "task_detail_limit_bytes": DETAIL_CAP,
                "verifier_memory_scope": "Python ID sets/session oracle are not app memory evidence"}
            record_metric("http_task_detail", [http_sample(daemon, f"/api/v1/tasks/{selected_task}",
                lambda value: task_detail(value, selected_task, dataset, oracle), DETAIL_CAP) for _ in range(args.samples)])
            record_metric("http_history_tasks_page", [http_sample(daemon, "/api/v1/history/tasks",
                lambda value: check_page(value, "tasks", dataset, oracle), PAGE_CAP) for _ in range(args.samples)])
            for kind in ("tasks", "messages"):
                traversal, saved_cursor = page_traversal(daemon, kind, dataset, oracle, check_deadline)
                record_metric(f"http_history_{kind}_full", [traversal])
                if saved_cursor:
                    restart_cursors.append(saved_cursor)
            result["resources_after_pagination"] = proc_stats(daemon.process.pid)
        cli_metrics = [("cli_status", ["status"]), ("cli_status_one", ["status", selected_task]), ("cli_diagnostics", ["diagnostics"])]
        if args.bounded_history:
            cli_metrics += [("cli_follow_completed", ["--wait", str(int(args.timeout)), "follow", selected_task]),
                            ("cli_history_tasks_page", ["history", "tasks"])]
        for metric, arguments in cli_metrics:
            samples = []
            for _ in range(args.samples):
                check_deadline()
                sample, value = cli_run(args.cli, data, arguments, args.timeout, daemon.home,
                                        ndjson=(metric == "cli_follow_completed"))
                if sample["status"] == "passed":
                    try:
                        if metric == "cli_status":
                            snapshots(value)
                        elif metric == "cli_status_one":
                            assert value["id"] == selected_task and value["status"] == "completed"
                            if oracle:
                                oracle.check_task(value)
                        elif metric == "cli_follow_completed":
                            assert len(value) == 1 and value[0]["type"] == "task_snapshot" and value[0]["reset"] is True
                            assert value[0]["task"]["id"] == selected_task and value[0]["event_cursor"] == dataset["events_total_sequence"]
                            oracle.check_task(value[0]["task"])
                        elif metric == "cli_history_tasks_page":
                            check_page(value, "tasks", dataset, oracle)
                            sample["returned"] = value["returned"]
                            sample["total"] = value["total"]
                            assert value["has_more"], "large fixture page unexpectedly claims complete"
                        else:
                            diagnostics(value)
                    except (AssertionError, KeyError, ValueError, TypeError) as error:
                        sample.update(status="failed", reason=str(error)[:512] or type(error).__name__)
                samples.append(sample)
            record_metric(metric, samples)
        for metric, count in (("replay_128", min(128, dataset["events_retained"])),
                              ("replay_all_retained", dataset["events_retained"])):
            samples = []
            for _ in range(args.samples):
                check_deadline()
                replay_start = time.monotonic()
                try:
                    samples.append(daemon.replay(dataset["first_event_sequence"] - 1, count))
                except (OSError, ValueError, urllib.error.URLError, AssertionError) as error:
                    samples.append({"status": "failed", "elapsed_ms": (time.monotonic() - replay_start) * 1000,
                                    "reason": str(error)[:512], **daemon.replay_progress})
            record_metric(metric, samples)
        if dataset["first_event_sequence"] > 1:
            try:
                daemon.request("/api/v1/events?after=0")
                result["pruned_cursor"] = {"status": "failed", "reason": "old cursor accepted"}
            except urllib.error.HTTPError as error:
                result["pruned_cursor"] = {"status": "passed" if error.code == 409 else "failed", "http_status": error.code}
        backups = case / "backups"
        backups.mkdir(mode=0o700)
        backup_samples, inspect_samples = [], []
        for index in range(args.backup_samples):
            check_deadline()
            operation_id = str(uuid.uuid4())
            backup_name = f"sample-{index}"
            backup_start = time.monotonic()
            _, response, _ = daemon.request("/api/v1/reliability/backups", {"id": operation_id, "parent": str(backups), "name": backup_name})
            receipt = json.loads(response)
            while receipt["state"] in {"pending", "running"}:
                if time.monotonic() - backup_start > args.timeout + 5:
                    raise TimeoutError("backup observation hard deadline; original intent not resent")
                time.sleep(0.02)
                _, response, _ = daemon.request(f"/api/v1/reliability/backups/{operation_id}")
                receipt = json.loads(response)
            sample = {"status": "passed" if receipt["state"] == "completed" else "failed",
                      "elapsed_ms": (time.monotonic() - backup_start) * 1000, "receipt": receipt}
            backup_samples.append(sample)
            if sample["status"] == "passed":
                bundle = backups / backup_name
                sample["database_bytes"] = (bundle / "database.sqlite").stat().st_size
                sample["database_sha256"] = digest_file(bundle / "database.sqlite")
                inspect, manifest = cli_run(args.cli, data, ["backup-inspect", str(bundle)], args.timeout, daemon.home)
                inspect_samples.append(inspect)
                if index == 0 and inspect["status"] == "passed":
                    restored = case / "restore-validation"
                    validation, _ = cli_run(args.cli, restored, ["restore", str(bundle), "--confirm-sha256", manifest["sha256"]],
                                             args.timeout, daemon.home)
                    result["fixture_typed_restore_validation"] = validation
                    if restored.exists():
                        shutil.rmtree(restored)  # Only this fresh fixture directory.
                shutil.rmtree(bundle)  # Keep hashes, not N duplicate backup images.
        record_metric("online_backup", backup_samples)
        record_metric("cli_backup_inspect", inspect_samples)
        result["resources_after_all"] = proc_stats(daemon.process.pid)
        if args.bounded_history:
            # Restart is a separate observation after lifetime HWM sampling. Old
            # cursor failure must not retry any command or mutate the seed.
            daemon.stop()
            result["pagination_restart_startup"] = daemon.start()
            reset_samples = []
            assert len(restart_cursors) == 2, "missing tasks/messages pagination reset witness"
            for saved in restart_cursors:
                elapsed, raw, status = daemon.request(saved["path"], max_bytes=8192, allow_http_error=True)
                value = json.loads(raw)
                reset_samples.append({"status": "passed" if status == 409 and value.get("code") == "history_reset" else "failed",
                    "elapsed_ms": elapsed, "http_status": status, "bytes": len(raw), "old_anchor": saved["anchor"]})
            record_metric("history_cursor_after_restart", reset_samples)
            _, fresh, _ = daemon.request("/api/v1/history/tasks", max_bytes=PAGE_CAP)
            fresh = json.loads(fresh)
            check_page(fresh, "tasks", dataset, oracle)
            assert all(fresh["anchor"] != saved["anchor"] for saved in restart_cursors)
            result["bounded_history"]["new_anchor_after_restart"] = fresh["anchor"]
        result["status"] = "failed" if any(metric["failed"] for metric in result["metrics"].values()) or result["snapshot_client_budget"]["status"] == "failed" else "passed"
        if result.get("fixture_typed_restore_validation", {}).get("status") != "passed":
            result["status"] = "failed"
        if result.get("pruned_cursor", {}).get("status") == "failed":
            result["status"] = "failed"
    except Exception as error:
        result.update(status="failed", error=f"{type(error).__name__}: {error}"[:1024])
    finally:
        if previous_alarm is not None:
            signal.setitimer(signal.ITIMER_REAL, 0)
            signal.signal(signal.SIGALRM, previous_alarm)
        daemon.stop()
        if oracle is not None:
            oracle.close()
        if args.bounded_history and original_history is not None:
            final_history = inspect_fixture_history(data / "harness.sqlite")
            unchanged_history = original_history["history_sha256"] == final_history["history_sha256"] and original_history["counts"] == final_history["counts"]
            result["immutable_history_after_reads_backups_restart"] = {"status": "passed" if unchanged_history else "failed",
                "before": original_history, "after": final_history}
            if not unchanged_history:
                result.update(status="failed", error="Immutable seeded history changed during read-only measurement")
        if args.seeds:
            unchanged = digest_file(args.seeds[name]["path"]) == args.seeds[name]["dataset"]["seed_sha256"]
            result["original_seed_unchanged"] = unchanged
            if not unchanged:
                result.update(status="failed", error="Original seed hash changed")
        for metric in ("startup_to_health", "http_snapshot", "http_diagnostics", "cli_status",
                       "cli_status_one", "cli_diagnostics", "replay_128", "replay_all_retained",
                       "online_backup", "cli_backup_inspect"):
            result["metrics"].setdefault(metric, summarize([]))
        result["elapsed_seconds"] = time.monotonic() - started
        if (data / "harness.sqlite").exists():
            result["final_database_sha256"] = digest_file(data / "harness.sqlite")
    return result


def environment():
    result = {"os": platform.system(), "release": platform.release(), "machine": platform.machine(),
              "python": platform.python_version(), "sqlite_python": sqlite3.sqlite_version,
              "logical_cpus": os.cpu_count(), "affinity_cpus": len(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else None}
    for key, path in (("cgroup_memory_max", "/sys/fs/cgroup/memory.max"), ("cgroup_cpu_max", "/sys/fs/cgroup/cpu.max")):
        try:
            result[key] = Path(path).read_text().strip()
        except OSError:
            result[key] = "unmeasured"
    if sys.platform == "linux":
        result["cpu_model"] = next((line.split(":", 1)[1].strip() for line in Path("/proc/cpuinfo").read_text().splitlines() if line.startswith("model name")), "unknown")
    return result


def self_test():
    assert percentile(list(range(1, 21)), 0.5) == 10
    assert percentile(list(range(1, 21)), 0.95) == 19
    assert percentile([4], 0.95) == 4
    stats = summarize([{"status": "passed", "elapsed_ms": 2}, {"status": "failed", "elapsed_ms": 99}])
    assert stats["failed"] == 1 and stats["p95_ms"] == 2
    from types import SimpleNamespace
    class FakeBody:
        fp = SimpleNamespace(raw=SimpleNamespace(_sock=SimpleNamespace(settimeout=lambda timeout: None)))
        def read1(self, count):
            return b"fixture"[:count]
        def close(self):
            pass
    assert deadline_read(FakeBody(), 4, time.monotonic() + 1) == b"fixt"
    error_response = urllib.error.HTTPError("http://127.0.0.1/fixture", 413, "fixture", {}, FakeBody())
    try:
        assert deadline_read(error_response, 4, time.monotonic() + 1) == b"fixt"
    finally:
        error_response.close()
    with tempfile.TemporaryDirectory(prefix="self-agent-bench-test-") as directory:
        path = Path(directory) / "fixture.sqlite"
        with closing(sqlite3.connect(path)) as db, db:
            db.executescript("CREATE TABLE sessions(id TEXT PRIMARY KEY,body TEXT NOT NULL);"
                "CREATE TABLE tasks(id TEXT PRIMARY KEY,body TEXT NOT NULL);"
                "CREATE TABLE commands(id TEXT PRIMARY KEY,envelope TEXT NOT NULL,receipt TEXT NOT NULL);"
                "CREATE TABLE events(sequence INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT UNIQUE NOT NULL,body TEXT NOT NULL);")
        summary = seed_database(path, Path(directory), (2, 3, 32, 64))
        assert summary["tasks"] == 6 and summary["messages"] == 12 and summary["events_retained"] == 120
        with closing(sqlite3.connect(path)) as db, db:
            assert all(json.loads(row[0])["status"] == "completed" for row in db.execute("SELECT body FROM tasks"))
            assert db.execute("SELECT seq FROM sqlite_sequence WHERE name='events'").fetchone()[0] == 120
        original = inspect_fixture_history(path)
        copy = Path(directory) / "copy.sqlite"
        shutil.copyfile(path, copy)
        assert inspect_fixture_history(copy) == original
        with closing(sqlite3.connect(copy)) as db, db:
            db.execute("UPDATE tasks SET body=json_set(body,'$.result','changed') WHERE id=?", (identifier("task", 0),))
        assert inspect_fixture_history(copy)["history_sha256"] != original["history_sha256"]
        oracle = SeedOracle(path)
        try:
            terminal = oracle.row("events", identifier("event", 20))
            terminal.pop("data")
            oracle.check_event(terminal)
            delta = oracle.row("events", identifier("event", 1))
            delta["data"]["text"] = "truncated"
            try:
                oracle.check_event(delta)
            except AssertionError:
                pass
            else:
                raise AssertionError("non-null event payload truncation accepted")
            anchor = "9d21c749-4c1d-431c-9f88-d638a4b71c4f"
            tasks = [oracle.row("tasks", identifier("task", i)) for i in range(6)]
            ids = oracle.session_ids()
            def metadata(total, returned):
                return {"total": total, "returned": returned, "omitted": total - returned,
                        "has_more": total > returned, "pending": 0, "unavailable": []}
            session = dict(oracle.session_row(ids[0]))
            session["messages"] = [dict(message, history_offset=i) for i, message in enumerate(session["messages"]) if i >= 4]
            totals = {"sessions": 2, "tasks": 6, "events": 120, "plans": 0, "plan_changes": 0,
                      "deliveries": 0, "execution_requests": 0, "context_states": 0, "selections": 0}
            window = {kind: [] for kind in totals}
            window.update(sessions=[session], tasks=tasks[-2:], event_cursor=120,
                history={"version": 1, "anchor": anchor,
                    "collections": {kind: metadata(total, len(window[kind])) for kind, total in totals.items()},
                    "session_messages": {ids[0]: {"offset": 4, "total": 6, "returned": 2, "has_more_before": True, "unavailable": []}}})
            window["history"]["collections"]["sessions"] = metadata(2, 1)
            window["history"]["collections"]["tasks"] = metadata(6, 2)
            bounded_snapshot(window, summary, oracle)
            window["history"]["collections"]["tasks"]["omitted"] = 0
            try:
                bounded_snapshot(window, summary, oracle)
            except AssertionError:
                pass
            else:
                raise AssertionError("false full-window metadata accepted")
            window["history"]["collections"]["tasks"]["omitted"] = 4
            damaged = dict(tasks[0], result=tasks[0]["result"][:-1])
            try:
                oracle.check_task(damaged)
            except AssertionError:
                pass
            else:
                raise AssertionError("truncated task result accepted")

            class FakePages:
                def request(self, route, **_kwargs):
                    second = "?cursor=" in route
                    if route.startswith("/api/v1/history/tasks"):
                        items = tasks[:3] if second else tasks[3:]
                        value = {"items": items, "total": 6, "returned": 3, "has_more": not second,
                                 "next_cursor": None if second else "next_tasks", "unavailable": [], "anchor": anchor, "event_cursor": 120}
                    else:
                        selected = route.split("/")[5]
                        offset = 0 if second else 3
                        messages = oracle.session_row(selected)["messages"]
                        items = [dict(messages[i], history_offset=i) for i in range(offset, offset + 3)]
                        value = {"items": items, "total": 6, "returned": 3, "has_more": not second,
                                 "next_cursor": None if second else "next_messages", "offset": offset,
                                 "unavailable": [], "anchor": anchor, "event_cursor": 120}
                    return 1.0, compact(value).encode(), 200
            for kind, count in (("tasks", 6), ("messages", 12)):
                sample, saved = page_traversal(FakePages(), kind, summary, oracle, lambda: None)
                assert sample["status"] == "passed", sample
                assert sample["items"] == count and saved["anchor"] == anchor
        finally:
            oracle.close()
        try:
            seed_database(path, Path(directory), (2, 3, 32, 64))
        except ValueError:
            pass
        else:
            raise AssertionError("nonempty data was accepted")
    print("PASS percentile/failure math, closed SQLite handles, completed-only deterministic seed, window omissions, exact fields, complete task/message pagination and nonempty-data guard")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--daemon", type=Path)
    parser.add_argument("--cli", type=Path)
    parser.add_argument("--source-commit")
    parser.add_argument("--artifact-dir", type=Path)
    parser.add_argument("--profile", action="append", choices=PROFILES)
    parser.add_argument("--samples", type=int, default=10)
    parser.add_argument("--startup-samples", type=int, default=5)
    parser.add_argument("--backup-samples", type=int, default=3)
    parser.add_argument("--timeout", type=float, default=30)
    parser.add_argument("--case-timeout", type=float, default=600)
    parser.add_argument("--keep-data", action="store_true")
    parser.add_argument("--bounded-history", action="store_true",
                        help="Explicit window=1/new history stage; legacy mode and baseline criteria stay unchanged")
    parser.add_argument("--seed-results", type=Path, action="append", default=[],
                        help="Reuse archived results.json + PROFILE/seed.sqlite; copy and migrate, never modify source")
    args = parser.parse_args()
    if args.self_test:
        self_test()
        return 0
    for required in ("daemon", "cli", "source_commit", "artifact_dir"):
        if getattr(args, required) is None:
            parser.error(f"--{required.replace('_', '-')} is required")
    if not all(1 <= value <= 30 for value in (args.samples, args.startup_samples, args.backup_samples)):
        parser.error("sample counts must be 1..30")
    if not 1 <= args.timeout <= 60 or not 30 <= args.case_timeout <= 1800:
        parser.error("timeouts must be bounded: request 1..60s; case 30..1800s")
    args.daemon, args.cli = args.daemon.resolve(strict=True), args.cli.resolve(strict=True)
    args.seeds = archived_seeds(args.seed_results)
    if args.seeds and any(name not in args.seeds for name in (args.profile or PROFILES)):
        parser.error("Every requested profile must have an archived seed")
    args.artifact_dir = args.artifact_dir.resolve()
    os.umask(0o077)
    args.artifact_dir.mkdir(mode=0o700, parents=False, exist_ok=False)
    shutil.copyfile(__file__, args.artifact_dir / "history_benchmark.py")
    output = args.artifact_dir / "results.json"
    result = {"format": 1, "started_at_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "source_commit_provided": args.source_commit, "binary_commit_embedding_verified": False,
        "benchmark_script_sha256": digest_file(__file__),
        "binaries": {name: {"path": str(path), "bytes": path.stat().st_size, "sha256": digest_file(path)}
                     for name, path in (("daemon", args.daemon), ("cli", args.cli))},
        "environment": environment(), "samples": args.samples, "startup_samples": args.startup_samples,
        "seed_results": [str(path.resolve()) for path in args.seed_results],
        "bounded_history": args.bounded_history,
        "backup_samples": args.backup_samples, "percentile_method": "nearest rank",
        "limits": {"client_snapshot_bytes": WINDOW_CAP if args.bounded_history else CLIENT_CAP,
                   "history_page_bytes": PAGE_CAP if args.bounded_history else None,
                   "task_detail_bytes": DETAIL_CAP if args.bounded_history else None,
                   "benchmark_response_bytes": MAX_RESPONSE,
                   "dataset_database_bytes_exclusive": MAX_DB, "operation_timeout_seconds": args.timeout,
                   "dataset_timeout_seconds": args.case_timeout},
        "cache_policy": "No OS cache flush; repeated process starts use warm filesystem cache. No warmup samples discarded.",
        "fixture_policy": "Synthetic completed-only records, real daemon schema, private directories, loopback only; no external models/vault/QQ.",
        "cases": []}
    signal.signal(signal.SIGTERM, lambda *_args: (_ for _ in ()).throw(SystemExit(143)))
    try:
        for name in args.profile or PROFILES:
            if shutil.disk_usage(args.artifact_dir).free < 512 * MIB:
                raise RuntimeError("Less than 512 MiB free; benchmark stopped without deleting other files")
            case = args.artifact_dir / name
            case.mkdir(mode=0o700)
            measured = benchmark_case(args, name, case)
            result["cases"].append(measured)
            output.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")
            print(compact({"profile": name, "status": measured["status"], "dataset": measured.get("dataset"),
                "snapshot_budget": measured.get("snapshot_client_budget"), "idle": measured.get("idle"),
                "metrics": {key: {field: value[field] for field in ("n", "failed", "p50_ms", "p95_ms")}
                            for key, value in measured["metrics"].items()}, "error": measured.get("error")}), flush=True)
            if not args.keep_data:
                shutil.rmtree(case)  # Only the newly created synthetic case, never input paths.
    finally:
        result["finished_at_utc"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        output.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")
        (args.artifact_dir / "results.sha256").write_text(digest_file(output) + "  results.json\n")
    return 0 if result["cases"] and all(case["status"] == "passed" for case in result["cases"]) else 2


if __name__ == "__main__":
    raise SystemExit(main())
