#!/usr/bin/env python3
"""Isolated package-copy lifecycle, NOT an OS installer or graceful drain test.

Uses only freshly built binaries at this checkout's fixed debug output paths.
No downloads, arbitrary update input, OS enrollment, models or real credentials.
The authenticated compatibility endpoint/client integration must already exist;
missing integration fails the gate, never becomes a skipped/pass result.
"""
from __future__ import annotations

import argparse
from contextlib import closing
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import socket
import sqlite3
import subprocess
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parent.parent
COMPATIBILITY = "/api/v1/compatibility"
MAX_HANDSHAKE = 512
class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *_args, **_kwargs):
        return None


OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
NAMES = ("harness-daemon", "self-agent-cli", "harness-gateway")
WIRE = {"handshake_schema": 1, "application": "self-agent", "protocol_current": 1,
        "protocol_min": 1, "lifecycle": "running", "capabilities": {"bounded_history": 1}}


def digest(path: Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(128 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def source_inputs(root: Path) -> list[Path]:
    inputs = [root / "Cargo.toml", root / "Cargo.lock"]
    for name in ("harness-core", "harness-daemon", "harness-cli", "harness-gateway"):
        package = root / "crates" / name
        inputs.append(package / "Cargo.toml")
        inputs.extend((package / "src").rglob("*.rs"))
        build = package / "build.rs"
        if build.exists():
            inputs.append(build)
    return sorted(path for path in inputs if path.is_file())


def known_binaries(root: Path = ROOT) -> tuple[dict[str, Path], str]:
    inputs = source_inputs(root)
    if not inputs:
        raise RuntimeError("缺少本仓库构建输入")
    source_digest = hashlib.sha256()
    for path in inputs:
        source_digest.update(str(path.relative_to(root)).encode())
        source_digest.update(bytes.fromhex(digest(path)))
    binaries = {}
    dependencies={"harness-daemon":{"harness-core","harness-daemon"},
                  "self-agent-cli":{"harness-core","harness-daemon","harness-cli"},
                  "harness-gateway":{"harness-core","harness-gateway"}}
    output = root / "target" / "debug"
    if (root / "target").is_symlink() or output.is_symlink():
        raise RuntimeError("构建输出目录不能是链接")
    for name in NAMES:
        relevant=[p for p in inputs if p.parent==root or p.relative_to(root).parts[1] in dependencies[name]]
        newest=max(p.stat().st_mtime_ns for p in relevant)
        path = root / "target" / "debug" / (name + (".exe" if os.name == "nt" else ""))
        if path.is_symlink() or not path.is_file() or path.resolve().parent != (root / "target" / "debug").resolve():
            raise RuntimeError("只接受当前仓库固定 debug 目录的普通已构建二进制")
        if path.stat().st_mtime_ns < newest:
            raise RuntimeError("构建产物早于源码；请完成当前树的 daemon/CLI/gateway 构建后再运行")
        binaries[name] = path
    return binaries, source_digest.hexdigest()


def copy_installation(binaries: dict[str, Path], destination: Path) -> dict[str, Path]:
    destination.mkdir(parents=True, exist_ok=False)
    installed = {}
    for name, source in binaries.items():
        if name not in NAMES or source.is_symlink() or not source.is_file():
            raise RuntimeError("安装输入不是已确认的本地构建产物")
        target = destination / source.name
        before = digest(source)
        shutil.copy2(source, target)
        if digest(target) != before or digest(source) != before:
            raise RuntimeError("复制时构建产物发生变化")
        installed[name] = target
    return installed


def uninstall_binaries(installed: dict[str, Path], installation: Path, data: Path) -> None:
    # No recursive removal: exactly the known copies and their now-empty directory.
    if data.resolve().is_relative_to(installation.resolve()):
        raise RuntimeError("数据不能放在可移除的安装目录中")
    for name, path in installed.items():
        if name not in NAMES or path.parent != installation or path.is_symlink():
            raise RuntimeError("卸载目标不是本次夹具的普通二进制副本")
    for path in installed.values():
        path.unlink()
    installation.rmdir()
    if not data.is_dir():
        raise AssertionError("移除二进制不得删除专用数据")


def available_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def environment(root: Path) -> dict[str, str]:
    env = {key: value for key, value in os.environ.items()
           if not key.startswith(("HARNESS_", "OPENAI_", "ANTHROPIC_", "CODEX_"))}
    home = root / "home"
    home.mkdir(exist_ok=True)
    env.update(HOME=str(home), USERPROFILE=str(home), CODEX_HOME=str(home / ".codex"),
               XDG_CONFIG_HOME=str(home / "config"), XDG_DATA_HOME=str(home / "data"),
               APPDATA=str(home / "appdata"), LOCALAPPDATA=str(home / "localappdata"),
               HARNESS_BASE_URL="http://127.0.0.1:1/v1", HARNESS_MODEL="", HARNESS_API_KEY="")
    return env


def http(base: str, path: str, token: str | None = None, *, method="GET", body=None,
         extra=None, limit=1024 * 1024):
    headers = {"Content-Type": "application/json"}
    if token is not None:
        headers["Authorization"] = "Bearer " + token
        headers["X-Self-Agent-Protocol"] = "1"
    headers.update(extra or {})
    request = urllib.request.Request(base + path, method=method, headers=headers,
                                     data=None if body is None else json.dumps(body).encode())
    try:
        response = OPENER.open(request, timeout=5)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        data = response.read(limit + 1)
        if len(data) > limit:
            raise AssertionError("响应超过固定边界")
        return response.status, response.headers, data


def await_condition(check, description: str, timeout=20):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            result = check()
            if result:
                return result
        except (OSError, ValueError, urllib.error.URLError):
            pass
        time.sleep(0.05)
    raise AssertionError("等待失败：" + description)


def stop_process(process: subprocess.Popen) -> None:
    """Phase 1 process termination only. Never report this as confirmed drain."""
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=8)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=8)


class InstalledDaemon:
    def __init__(self, binary: Path, data: Path, root: Path, env: dict[str, str]):
        self.binary, self.data, self.root, self.env = binary, data, root, env
        self.process = None
        self.log = None
        self.base = self.token = ""

    def start(self):
        port = available_port()
        self.base = f"http://127.0.0.1:{port}"
        env = dict(self.env, HARNESS_DATA_DIR=str(self.data), HARNESS_BIND=f"127.0.0.1:{port}")
        self.log = (self.root / "daemon.log").open("ab")
        self.process = subprocess.Popen([str(self.binary)], cwd=self.binary.parent, env=env,
                                        stdout=self.log, stderr=self.log)

        def ready():
            if self.process.poll() is not None:
                raise RuntimeError("复制后的后台提前退出；日志仅保留在私有夹具目录")
            info = json.loads((self.data / "daemon.json").read_text(encoding="utf-8"))
            if info["pid"] != self.process.pid or info["base_url"] != self.base:
                return False
            status, _, raw = http(self.base, "/health", limit=1024)
            if status != 200 or json.loads(raw).get("status") != "ok":
                return False
            self.token = info["token"]
            return True
        await_condition(ready, "复制后台启动")
        status, _, raw = http(self.base, COMPATIBILITY, self.token, limit=MAX_HANDSHAKE)
        if status != 200 or json.loads(raw) != WIRE:
            raise AssertionError("认证兼容握手尚未接线或不是协议1 Running；不能继续操作")
        status, _, _ = http(self.base, COMPATIBILITY, limit=1024)
        assert status == 401, "兼容握手必须认证"

    def stop(self):
        if self.process is not None:
            stop_process(self.process)
        if self.log is not None:
            self.log.close()


def cli(binary: Path, data: Path, env: dict[str, str], *arguments: str, expect_success=True):
    result = subprocess.run([str(binary), "--data-dir", str(data), *arguments], cwd=binary.parent,
                            env=env, capture_output=True, timeout=15)
    if expect_success:
        if result.returncode != 0:
            raise AssertionError("复制后的CLI操作失败；不回显潜在敏感错误正文")
        return json.loads(result.stdout)
    assert result.returncode == 1, "不兼容后台必须通用非零退出，不能执行动作"
    return result


class CompatibilityFixture(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def answer(self, code, body):
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        self.server.seen.append(("GET", self.path, self.headers.get("Authorization"),
                                 self.headers.get("X-Self-Agent-Protocol")))
        if self.path == COMPATIBILITY:
            self.answer(self.server.reply_status, self.server.reply)
        elif self.path == "/health":
            self.answer(200, b'{"status":"ok"}')
        else:
            self.answer(404, b'{}')

    def do_POST(self):
        self.server.seen.append(("POST", self.path, self.headers.get("Authorization"),
                                 self.headers.get("X-Self-Agent-Protocol")))
        length = int(self.headers.get("Content-Length", "0"))
        if length > 65536:
            self.answer(413, b'{}')
            return
        body = json.loads(self.rfile.read(length))
        self.answer(200, json.dumps({"command_id": body.get("command_id"), "status": "accepted",
                                     "session_id": str(uuid.uuid4())}).encode())


def verify_client_gate(binary: Path, root: Path, env: dict[str, str]):
    good = json.dumps(WIRE).encode()
    cases = [("missing", 404, b'{}'), ("malformed", 200, b'not-json'),
             ("empty", 200, b''), ("oversized", 200, good + b' ' * 513),
             ("old", 200, json.dumps(dict(WIRE, protocol_current=0, protocol_min=0)).encode()),
             ("incompatible", 200, json.dumps(dict(WIRE, protocol_current=2, protocol_min=2)).encode()),
             ("duplicate", 200, good.replace(b'{', b'{"lifecycle":"running",', 1))]
    server = ThreadingHTTPServer(("127.0.0.1", 0), CompatibilityFixture)
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    directory = root / "incompatible-client-data"
    directory.mkdir()
    token = "package-compatibility-fixture-token"
    (directory / "daemon.json").write_text(json.dumps({"base_url": f"http://127.0.0.1:{server.server_port}",
        "token": token, "pid": os.getpid()}), encoding="utf-8")
    try:
        for _label, status, raw in cases:
            server.seen, server.reply_status, server.reply = [], status, raw
            result = cli(binary, directory, env, "create", "不能创建", str(root), expect_success=False)
            assert token.encode() not in result.stderr + result.stdout, "不能回显认证材料"
            assert not any(item[0] == "POST" for item in server.seen), "握手失败前不允许动作"
            handshake = [item for item in server.seen if item[1] == COMPATIBILITY]
            assert len(handshake) == 1, "兼容探测不得自动重试或省略"
            assert handshake[0][2] == "Bearer " + token, "握手必须绑定认证后台"
        server.seen, server.reply_status, server.reply = [], 200, good
        cli(binary, directory, env, "create", "合法夹具", str(root))
        assert [entry[0] for entry in server.seen if entry[1] != "/health"] == ["GET", "POST"]
        assert all(entry[3] == "1" for entry in server.seen if entry[0] == "POST"), "新CLI动作必须携带协议代际"
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=3)


def gateway_roundtrip(binary: Path, database: Path, root: Path, env: dict[str, str]):
    enrollment = subprocess.run([str(binary), "--db", str(database), "enroll", "安装夹具"],
        cwd=binary.parent, env=env, capture_output=True, timeout=10)
    assert enrollment.returncode == 0, "复制gateway注册夹具失败"
    device = json.loads(enrollment.stdout)["device"]["device_id"]
    for _ in range(2):
        port = available_port()
        origin = f"http://127.0.0.1:{port}"
        with (root / "gateway.log").open("ab") as log:
            process = subprocess.Popen([str(binary), "--db", str(database), "serve", "--origin", origin,
                "--bind", f"127.0.0.1:{port}", "--fixture"], cwd=binary.parent, env=env,
                stdout=log, stderr=log)
            try:
                def ready():
                    if process.poll() is not None:
                        raise RuntimeError("复制gateway提前退出")
                    status, _, raw = http(origin, "/gateway/api/session", extra={"Sec-Fetch-Site": "same-origin"})
                    return status == 200 and json.loads(raw).get("code") == "ok"
                await_condition(ready, "复制gateway启动")
            finally:
                stop_process(process)
        rows = subprocess.run([str(binary), "--db", str(database), "list"], cwd=binary.parent,
            env=env, capture_output=True, timeout=10)
        assert rows.returncode == 0 and json.loads(rows.stdout)[0]["device_id"] == device


def run_package(keep_fixture=False):
    binaries, source_sha256 = known_binaries()
    root = Path(tempfile.mkdtemp(prefix="self-agent-package-")).resolve()
    installed = {}
    daemon = None
    try:
        installation = root / "installation" / "bin"
        installed = copy_installation(binaries, installation)
        data, project = root / "data", root / "project"
        data.mkdir(); project.mkdir()
        env = environment(root)
        manifest = {"source_sha256": source_sha256, "binaries": {name: digest(path) for name, path in installed.items()},
                    "scope": "local-package-copy-only-no-installer-no-graceful-drain"}
        (root / "fixture-manifest.json").write_text(json.dumps(manifest, indent=2), encoding="utf-8")
        verify_client_gate(installed["self-agent-cli"], root, env)
        print("PASS 已复制CLI：缺失/损坏/旧/不兼容认证握手先拒绝，零动作、零重试")
        daemon = InstalledDaemon(installed["harness-daemon"], data, root, env)
        daemon.start()
        created = cli(installed["self-agent-cli"], data, env, "create", "安装生命周期夹具", str(project))
        session_id = created["session_id"]
        before = cli(installed["self-agent-cli"], data, env, "sessions")
        assert session_id in json.dumps(before)
        wrong = {"command_id": str(uuid.uuid4()), "source": {"channel_id": "cli", "actor_id": "local", "conversation_id": "package"},
                 "command": {"type": "create_session", "title": "MUST_NOT_APPLY", "project_path": str(project)}}
        status, _, _ = http(daemon.base, "/api/v1/commands", daemon.token, method="POST", body=wrong,
                            extra={"X-Self-Agent-Protocol": "invalid"})
        assert 400 <= status < 500, "无效显式协议头必须拒绝"
        assert "MUST_NOT_APPLY" not in json.dumps(cli(installed["self-agent-cli"], data, env, "sessions"))
        daemon.stop()
        daemon.start()
        after = cli(installed["self-agent-cli"], data, env, "sessions")
        assert session_id in json.dumps(after), "进程终止后重启必须保留已提交会话"
        daemon.stop()
        print("PASS 已复制daemon/CLI：认证使用与进程重启保留专用数据（不是优雅drain证明）")
        gateway_roundtrip(installed["harness-gateway"], root / "gateway.sqlite", root, env)
        print("PASS 已复制gateway：loopback fixture启动/重启，设备目录保留")
        database = data / "harness.sqlite"
        assert database.is_file()
        uninstall_binaries(installed, installation, data)
        assert not installation.exists() and database.is_file()
        with closing(sqlite3.connect(database.as_uri() + "?mode=ro", uri=True)) as db:
            row = db.execute("SELECT id FROM sessions WHERE id=?", (session_id,)).fetchone()
        assert row == (session_id,), "移除安装副本不得移除持久会话"
        print("PASS 仅移除已复制二进制，专用数据和原会话仍可只读验证")
        print("未验证：OS安装器、签名/升级发布、实际优雅停止、真实凭据与公网部署")
    finally:
        if daemon is not None:
            daemon.stop()
        if keep_fixture:
            print("私有夹具目录保留：", root)
        else:
            shutil.rmtree(root)


class SafetyTests(unittest.TestCase):
    def test_uninstall_only_known_copies_keeps_data(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"; source.mkdir()
            binaries = {}
            for name in NAMES:
                path = source / name; path.write_bytes(b"fixture-not-executed"); binaries[name] = path
            destination = root / "installation"
            installed = copy_installation(binaries, destination)
            data = root / "data"; data.mkdir(); (data / "keep").write_text("retained")
            uninstall_binaries(installed, destination, data)
            self.assertEqual((data / "keep").read_text(), "retained")
            self.assertTrue(all(path.is_file() for path in binaries.values()))

    def test_unknown_name_and_nested_data_are_refused(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); source = root / "input"; source.write_bytes(b"not executable")
            with self.assertRaises(RuntimeError):
                copy_installation({"downloaded-update": source}, root / "install")
            destination = root / "owned"; destination.mkdir(); data = destination / "data"; data.mkdir()
            with self.assertRaises(RuntimeError):
                uninstall_binaries({}, destination, data)
            self.assertTrue(data.exists())

    def test_freshness_rejects_stale_known_paths(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary)
            (root / "Cargo.toml").write_text("[workspace]")
            (root / "Cargo.lock").write_text("fixture")
            output=root / "target" / "debug"; output.mkdir(parents=True)
            for name in NAMES:
                (output / (name + (".exe" if os.name=="nt" else ""))).write_bytes(b"not executed")
            self.assertEqual(len(known_binaries(root)[0]),3)
            future=time.time()+5
            os.utime(root / "Cargo.toml",(future,future))
            with self.assertRaises(RuntimeError):
                known_binaries(root)

    def test_environment_discards_live_provider_variables_and_selects_isolated_home(self):
        with tempfile.TemporaryDirectory() as temporary:
            env = environment(Path(temporary))
            self.assertEqual(env["HARNESS_BASE_URL"], "http://127.0.0.1:1/v1")
            self.assertEqual(env["HARNESS_API_KEY"], "")
            self.assertTrue(Path(env["HOME"]).is_relative_to(temporary))
            self.assertFalse(any(key.startswith(("OPENAI_", "ANTHROPIC_")) for key in env))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--keep-fixture", action="store_true", help="保留本次新建私有夹具，不使用任意已有数据目录")
    parser.add_argument("--self-test", action="store_true", help="仅测试脚本边界，不执行任何构建二进制")
    args = parser.parse_args()
    if args.self_test:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(SafetyTests)
        if not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful():
            raise SystemExit(1)
    else:
        try:
            run_package(args.keep_fixture)
        except Exception as error:
            print("FAIL 安装副本生命周期门禁未通过：", type(error).__name__)
            raise SystemExit(1) from None


if __name__ == "__main__":
    main()
