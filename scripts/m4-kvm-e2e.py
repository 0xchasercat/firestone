#!/usr/bin/env python3
"""Run M4 E2E 9 against Linux x86_64 KVM and the real Unix REST server."""

from __future__ import annotations

import atexit
import datetime as dt
import hashlib
import json
import os
import platform
import shlex
import shutil
import signal
import socket
import stat
import subprocess
import sys
import time
import tomllib
from pathlib import Path
from typing import Any


class AcceptanceError(RuntimeError):
    """The host or one E2E 9 contract failed."""


REPO_ROOT = Path(__file__).resolve().parents[1]
MACHINE = "ubuntu"
COMMAND_TIMEOUT_SECONDS = 120
START_TIMEOUT_SECONDS = 1_900
SERVER_START_TIMEOUT_SECONDS = 10
MAX_OUTPUT_BYTES = 8 * 1024 * 1024


def require(condition: bool, message: str) -> None:
    if not condition:
        raise AcceptanceError(message)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while block := stream.read(1024 * 1024):
            digest.update(block)
    return digest.hexdigest()


def bytes_sha256(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def compact_bytes(value: bytes, limit: int = 8_192) -> str:
    if len(value) > limit:
        value = value[-limit:]
        prefix = f"[... output truncated to {limit} bytes ...]\n"
    else:
        prefix = ""
    rendered = []
    for byte in value:
        if byte == 0x0A:
            rendered.append("\n")
        elif 0x20 <= byte <= 0x7E:
            rendered.append(chr(byte))
        else:
            rendered.append(f"\\x{byte:02x}")
    return prefix + "".join(rendered)


def stream_facts(value: bytes) -> dict[str, Any]:
    prohibited = [
        (offset, byte)
        for offset, byte in enumerate(value)
        if byte < 0x20 and byte != 0x0A or byte == 0x7F
    ]
    return {
        "bytes": len(value),
        "lines": len(value.splitlines()),
        "ends_with_newline": value.endswith(b"\n") if value else False,
        "sha256": bytes_sha256(value),
        "prohibited_control_count": len(prohibited),
    }


def require_clean_stream(label: str, value: bytes) -> None:
    facts = stream_facts(value)
    require(
        facts["prohibited_control_count"] == 0,
        f"{label} contains a prohibited control byte",
    )
    try:
        value.decode("utf-8")
    except UnicodeDecodeError as error:
        raise AcceptanceError(f"{label} is not UTF-8: {error}") from error


def process_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except (ProcessLookupError, PermissionError):
        return False
    status = Path("/proc") / str(pid) / "status"
    try:
        state = next(
            line for line in status.read_text(encoding="utf-8").splitlines()
            if line.startswith("State:")
        )
    except (FileNotFoundError, PermissionError, StopIteration):
        return False
    return " Z " not in f" {state} " and "(zombie)" not in state


class HttpResponse:
    def __init__(
        self,
        status: int,
        headers: dict[str, str],
        body: bytes,
        *,
        first_body_ms: float | None,
        completed_ms: float,
    ) -> None:
        self.status = status
        self.headers = headers
        self.body = body
        self.first_body_ms = first_body_ms
        self.completed_ms = completed_ms


class Server:
    def __init__(self, harness: Harness) -> None:
        self.harness = harness
        self.process = harness.spawn([harness.binary, "serve"])
        self.publication_observations: list[dict[str, Any]] = []
        self.socket_identity = self._wait_ready()

    def _wait_ready(self) -> dict[str, Any]:
        deadline = time.monotonic() + SERVER_START_TIMEOUT_SECONDS
        socket_path = self.harness.socket_path
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                completed = self.harness.collect_process(self.process, 1, "serve startup")
                raise AcceptanceError(
                    f"serve exited {completed.returncode} before readiness; "
                    f"stdout:\n{compact_bytes(completed.stdout)}\n"
                    f"stderr:\n{compact_bytes(completed.stderr)}"
                )
            try:
                metadata = socket_path.lstat()
            except FileNotFoundError:
                time.sleep(0.001)
                continue
            observation = {
                "mode": f"{stat.S_IMODE(metadata.st_mode):04o}",
                "uid": metadata.st_uid,
                "is_socket": stat.S_ISSOCK(metadata.st_mode),
                "device": metadata.st_dev,
                "inode": metadata.st_ino,
            }
            self.publication_observations.append(observation)
            require(observation["is_socket"], "serve published a non-socket node")
            require(observation["uid"] == os.getuid(), "serve socket has the wrong owner")
            require(observation["mode"] == "0600", "serve socket was visible outside mode 0600")
            try:
                response = self.harness.http_request("GET", "/v1/version", timeout=2)
            except (AcceptanceError, OSError):
                time.sleep(0.005)
                continue
            require(response.status == 200, "serve readiness request did not return 200")
            runtime = self.harness.home / "run"
            lock = runtime / ".serve.lock"
            require(
                stat.S_IMODE(runtime.stat().st_mode) == 0o700,
                "serve runtime directory is not mode 0700",
            )
            require(
                stat.S_IMODE(lock.stat().st_mode) == 0o600,
                "serve lock is not mode 0600",
            )
            return observation
        raise AcceptanceError("serve did not publish its Unix socket before the deadline")

    def stop(self, signum: int) -> subprocess.CompletedProcess[bytes]:
        require(self.process.poll() is None, "serve process already exited")
        os.kill(self.process.pid, signum)
        completed = self.harness.collect_process(self.process, 10, f"serve signal {signum}")
        self.process = None
        return completed


class Harness:
    def __init__(self) -> None:
        self.home = self._checked_home()
        self.keep_home = os.environ.get("FIRESTONE_E2E_KEEP") == "1"
        default_evidence = Path("/tmp") / f"firestone-m4-evidence-{os.getpid()}.json"
        self.evidence_path = Path(
            os.environ.get("FIRESTONE_E2E_EVIDENCE", default_evidence)
        ).expanduser()
        if not self.evidence_path.is_absolute():
            self.evidence_path = (Path.cwd() / self.evidence_path).resolve()
        try:
            self.evidence_path.relative_to(self.home)
        except ValueError:
            pass
        else:
            raise AcceptanceError("FIRESTONE_E2E_EVIDENCE must be outside FIRESTONE_HOME")
        self.socket_path = self.home / "run" / "serve.sock"
        self.commands: list[str] = []
        self.active_processes: list[subprocess.Popen[bytes]] = []
        self.evidence: dict[str, Any] = {
            "schema": 1,
            "result": "running",
            "started_at": dt.datetime.now(dt.UTC).isoformat(),
            "commands": self.commands,
            "host": {},
            "artifacts": {},
            "image": {},
            "scenarios": {},
        }
        self._cleanup_started = False
        atexit.register(self.cleanup)
        self.binary = self._binary_path()

    @staticmethod
    def _checked_home() -> Path:
        raw = os.environ.get("FIRESTONE_HOME")
        require(
            raw is not None and raw != "",
            "FIRESTONE_HOME must name a new isolated directory, for example: "
            "FIRESTONE_HOME=$(mktemp -d)",
        )
        home = Path(raw).expanduser()
        require(home.is_absolute(), "FIRESTONE_HOME must be absolute")
        require(home.exists() and home.is_dir(), "FIRESTONE_HOME must exist")
        home = home.resolve(strict=True)
        metadata = home.stat()
        require(metadata.st_uid == os.getuid(), "FIRESTONE_HOME has the wrong owner")
        require(
            stat.S_IMODE(metadata.st_mode) == 0o700,
            "FIRESTONE_HOME must have mode 0700",
        )
        require(not any(home.iterdir()), "FIRESTONE_HOME must be empty")
        return home

    def _binary_path(self) -> Path:
        configured = os.environ.get("FIRESTONE_BIN")
        if configured:
            source = Path(configured).expanduser()
            if not source.is_absolute():
                source = (Path.cwd() / source).resolve()
            require(source.is_file(), f"FIRESTONE_BIN is not a file: {source}")
            require(os.access(source, os.X_OK), f"FIRESTONE_BIN is not executable: {source}")
            directory = self.home / "harness-bin"
            directory.mkdir(mode=0o700)
            binary = directory / "firestone"
            shutil.copy2(source, binary)
            os.chmod(binary, 0o755)
            require(sha256(binary) == sha256(source), "staged FIRESTONE_BIN changed bytes")
            return binary
        return REPO_ROOT / "target" / "debug" / "firestone"

    def environment(self) -> dict[str, str]:
        environment = os.environ.copy()
        environment["FIRESTONE_HOME"] = os.fspath(self.home)
        return environment

    def record_command(self, argv: list[str | os.PathLike[str]]) -> list[str]:
        command = [os.fspath(value) for value in argv]
        rendered = shlex.join(command)
        self.commands.append(rendered)
        print(f"+ {rendered}", flush=True)
        return command

    def run(
        self,
        argv: list[str | os.PathLike[str]],
        *,
        timeout: float = COMMAND_TIMEOUT_SECONDS,
        check: bool = True,
        record: bool = True,
    ) -> subprocess.CompletedProcess[bytes]:
        command = self.record_command(argv) if record else [os.fspath(value) for value in argv]
        rendered = shlex.join(command)
        try:
            completed = subprocess.run(
                command,
                cwd=REPO_ROOT,
                env=self.environment(),
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=timeout,
                check=False,
            )
        except subprocess.TimeoutExpired as error:
            raise AcceptanceError(
                f"command timed out after {timeout:.1f}s: {rendered}"
            ) from error
        require(
            len(completed.stdout) <= MAX_OUTPUT_BYTES
            and len(completed.stderr) <= MAX_OUTPUT_BYTES,
            f"command output exceeded 8 MiB: {rendered}",
        )
        if check and completed.returncode != 0:
            raise AcceptanceError(
                f"command failed with exit {completed.returncode}: {rendered}\n"
                f"stdout:\n{compact_bytes(completed.stdout)}\n"
                f"stderr:\n{compact_bytes(completed.stderr)}"
            )
        return completed

    def spawn(self, argv: list[str | os.PathLike[str]]) -> subprocess.Popen[bytes]:
        command = self.record_command(argv)
        process = subprocess.Popen(
            command,
            cwd=REPO_ROOT,
            env=self.environment(),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
        self.active_processes.append(process)
        return process

    def forget_process(self, process: subprocess.Popen[bytes]) -> None:
        try:
            self.active_processes.remove(process)
        except ValueError:
            pass

    def collect_process(
        self,
        process: subprocess.Popen[bytes],
        timeout: float,
        label: str,
    ) -> subprocess.CompletedProcess[bytes]:
        try:
            stdout, stderr = process.communicate(timeout=timeout)
        except subprocess.TimeoutExpired as error:
            self.terminate_process(process)
            raise AcceptanceError(f"{label} did not exit within {timeout:.1f}s") from error
        self.forget_process(process)
        require(
            len(stdout) <= MAX_OUTPUT_BYTES and len(stderr) <= MAX_OUTPUT_BYTES,
            f"{label} output exceeded 8 MiB",
        )
        return subprocess.CompletedProcess(process.args, process.returncode, stdout, stderr)

    def terminate_process(self, process: subprocess.Popen[bytes]) -> None:
        if process.poll() is not None:
            self.forget_process(process)
            return
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                pass
        self.forget_process(process)

    def json_command(
        self,
        *arguments: str,
        timeout: float = COMMAND_TIMEOUT_SECONDS,
        expected_code: int = 0,
        action: str | None = None,
    ) -> tuple[list[dict[str, Any]], bytes, subprocess.CompletedProcess[bytes]]:
        completed = self.run(
            [self.binary, "--json", *arguments],
            timeout=timeout,
            check=False,
        )
        require(
            completed.returncode == expected_code,
            f"{shlex.join(arguments)} exited {completed.returncode}, expected {expected_code}; "
            f"stdout:\n{compact_bytes(completed.stdout)}\n"
            f"stderr:\n{compact_bytes(completed.stderr)}",
        )
        require_clean_stream(f"{shlex.join(arguments)} stdout", completed.stdout)
        require(not completed.stderr, f"{shlex.join(arguments)} wrote JSON stderr")
        require(completed.stdout.endswith(b"\n"), "JSON output lacks its final newline")
        lines = completed.stdout.splitlines()
        require(lines and all(lines), "JSON output contains no records or an empty record")
        try:
            records = [json.loads(line) for line in lines]
        except (json.JSONDecodeError, UnicodeDecodeError) as error:
            raise AcceptanceError(
                f"invalid NDJSON: {compact_bytes(completed.stdout)}"
            ) from error
        terminals = [
            index
            for index, record in enumerate(records)
            if record.get("type") == "Result" or "error" in record
        ]
        require(
            terminals == [len(records) - 1],
            "JSON output did not end in exactly one Result or error",
        )
        terminal = records[-1]
        require(terminal.get("type") == "Result", "JSON command did not end in Result")
        if action is not None:
            require(terminal.get("action") == action, f"expected {action} Result")
        marker = b',"payload":'
        line = lines[-1]
        start = line.find(marker)
        require(start >= 0 and line.endswith(b"}"), "Result payload framing changed")
        payload_bytes = line[start + len(marker) : -1]
        require(json.loads(payload_bytes) == terminal.get("payload"), "Result payload bytes changed")
        return records, payload_bytes, completed

    def http_request(
        self,
        method: str,
        path: str,
        body: bytes = b"",
        *,
        accept: str | None = None,
        timeout: float = COMMAND_TIMEOUT_SECONDS,
    ) -> HttpResponse:
        request = [
            f"{method} {path} HTTP/1.1",
            "Host: firestone",
            "Connection: close",
            f"Content-Length: {len(body)}",
        ]
        if body:
            request.append("Content-Type: application/json")
        if accept is not None:
            request.append(f"Accept: {accept}")
        wire = ("\r\n".join(request) + "\r\n\r\n").encode() + body
        started = time.monotonic()
        deadline = started + timeout
        response = bytearray()
        header_end: int | None = None
        first_body_ms: float | None = None
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            client.settimeout(min(5.0, timeout))
            client.connect(os.fspath(self.socket_path))
            client.sendall(wire)
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise AcceptanceError(f"HTTP {method} {path} timed out after {timeout:.1f}s")
                client.settimeout(min(1.0, remaining))
                try:
                    block = client.recv(65_536)
                except TimeoutError:
                    continue
                if not block:
                    break
                response.extend(block)
                require(len(response) <= MAX_OUTPUT_BYTES, "HTTP response exceeded 8 MiB")
                if header_end is None:
                    found = response.find(b"\r\n\r\n")
                    if found >= 0:
                        header_end = found + 4
                if header_end is not None and len(response) > header_end and first_body_ms is None:
                    first_body_ms = round((time.monotonic() - started) * 1000, 3)
        completed_ms = round((time.monotonic() - started) * 1000, 3)
        return parse_http_response(
            bytes(response),
            first_body_ms=first_body_ms,
            completed_ms=completed_ms,
        )

    def state(self, name: str) -> dict[str, Any]:
        path = self.home / "data" / "machines" / name / "state.json"
        try:
            value = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            raise AcceptanceError(f"cannot read state for {name}: {error}") from error
        require(isinstance(value, dict), f"state for {name} is not an object")
        return value

    def write_evidence(self) -> None:
        self.evidence_path.parent.mkdir(parents=True, exist_ok=True)
        temporary = self.evidence_path.with_name(
            f".{self.evidence_path.name}.{os.getpid()}.tmp"
        )
        payload = (json.dumps(self.evidence, indent=2, sort_keys=True) + "\n").encode()
        descriptor = os.open(
            temporary,
            os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC,
            0o600,
        )
        try:
            with os.fdopen(descriptor, "wb", closefd=False) as stream:
                stream.write(payload)
                stream.flush()
                os.fsync(stream.fileno())
        finally:
            os.close(descriptor)
        os.replace(temporary, self.evidence_path)
        os.chmod(self.evidence_path, 0o600)
        require(
            stat.S_IMODE(self.evidence_path.stat().st_mode) == 0o600,
            "evidence file is not mode 0600",
        )

    def _live_home_processes(self) -> list[int]:
        home = os.fsencode(self.home)
        ancestors = {os.getpid()}
        parent = os.getppid()
        while parent > 1 and parent not in ancestors:
            ancestors.add(parent)
            try:
                status = (Path("/proc") / str(parent) / "status").read_text()
                parent_line = next(
                    line for line in status.splitlines() if line.startswith("PPid:")
                )
                parent = int(parent_line.split()[1])
            except (FileNotFoundError, PermissionError, StopIteration, ValueError):
                break
        result: list[int] = []
        for entry in Path("/proc").iterdir():
            if not entry.name.isdigit() or int(entry.name) in ancestors:
                continue
            try:
                if entry.stat().st_uid != os.getuid():
                    continue
                command = (entry / "cmdline").read_bytes()
                links = [entry / "cwd", entry / "exe"]
                links.extend((entry / "fd").iterdir())
                referenced = home in command
                for link in links:
                    try:
                        target = os.fsencode(os.path.realpath(link))
                    except OSError:
                        continue
                    if target == home or target.startswith(home + b"/"):
                        referenced = True
                        break
                if referenced:
                    result.append(int(entry.name))
            except (FileNotFoundError, PermissionError, ProcessLookupError):
                continue
        return sorted(result)

    def cleanup(self) -> list[str]:
        if self._cleanup_started:
            return []
        self._cleanup_started = True
        errors: list[str] = []
        for process in list(self.active_processes):
            self.terminate_process(process)
        if hasattr(self, "binary") and self.binary.is_file():
            for arguments in (
                [self.binary, "--json", "stop", MACHINE, "--force", "--timeout", "5s"],
                [self.binary, "--json", "rm", MACHINE, "--force"],
            ):
                try:
                    subprocess.run(
                        [os.fspath(value) for value in arguments],
                        cwd=REPO_ROOT,
                        env=self.environment(),
                        stdin=subprocess.DEVNULL,
                        stdout=subprocess.DEVNULL,
                        stderr=subprocess.DEVNULL,
                        timeout=20,
                        check=False,
                    )
                except (OSError, subprocess.TimeoutExpired) as error:
                    errors.append(f"cleanup command failed: {error}")
        live = self._live_home_processes()
        if live:
            errors.append(f"live processes still reference FIRESTONE_HOME: {live}")
        if not self.keep_home and not live:
            try:
                shutil.rmtree(self.home)
            except OSError as error:
                errors.append(f"cannot remove FIRESTONE_HOME: {error}")
        return errors


def parse_http_response(
    response: bytes,
    *,
    first_body_ms: float | None,
    completed_ms: float,
) -> HttpResponse:
    header_end = response.find(b"\r\n\r\n")
    require(header_end >= 0, "HTTP response has no header terminator")
    try:
        lines = response[:header_end].decode("ascii").split("\r\n")
    except UnicodeDecodeError as error:
        raise AcceptanceError(f"HTTP headers are not ASCII: {error}") from error
    status_parts = lines[0].split()
    require(len(status_parts) >= 2, "HTTP response has no status code")
    status = int(status_parts[1])
    headers: dict[str, str] = {}
    for line in lines[1:]:
        name, separator, value = line.partition(":")
        require(separator == ":", f"malformed HTTP header: {line!r}")
        headers[name.lower()] = value.strip()
    wire_body = response[header_end + 4 :]
    if headers.get("transfer-encoding", "").lower() == "chunked":
        body = decode_chunked(wire_body)
    elif "content-length" in headers:
        length = int(headers["content-length"])
        require(len(wire_body) >= length, "HTTP body is shorter than Content-Length")
        body = wire_body[:length]
    else:
        body = wire_body
    return HttpResponse(
        status,
        headers,
        body,
        first_body_ms=first_body_ms,
        completed_ms=completed_ms,
    )


def decode_chunked(value: bytes) -> bytes:
    output = bytearray()
    offset = 0
    while True:
        line_end = value.find(b"\r\n", offset)
        require(line_end >= 0, "chunked response has no size delimiter")
        size_text = value[offset:line_end].split(b";", 1)[0]
        try:
            size = int(size_text, 16)
        except ValueError as error:
            raise AcceptanceError(f"invalid chunk size {size_text!r}") from error
        offset = line_end + 2
        if size == 0:
            return bytes(output)
        require(offset + size + 2 <= len(value), "chunked response is truncated")
        output.extend(value[offset : offset + size])
        offset += size
        require(value[offset : offset + 2] == b"\r\n", "chunk lacks trailing CRLF")
        offset += 2


def ndjson_result(value: bytes, action: str) -> tuple[list[dict[str, Any]], bytes]:
    require_clean_stream(f"REST {action} NDJSON", value)
    require(value.endswith(b"\n"), f"REST {action} NDJSON lacks a final newline")
    lines = value.splitlines()
    require(lines and all(lines), f"REST {action} returned empty NDJSON")
    try:
        records = [json.loads(line) for line in lines]
    except (json.JSONDecodeError, UnicodeDecodeError) as error:
        raise AcceptanceError(f"REST {action} returned invalid NDJSON") from error
    terminals = [
        index
        for index, record in enumerate(records)
        if record.get("type") == "Result" or "error" in record
    ]
    require(
        terminals == [len(records) - 1],
        f"REST {action} did not end in exactly one terminal record",
    )
    terminal = records[-1]
    require(terminal.get("type") == "Result", f"REST {action} ended in an error")
    require(terminal.get("action") == action, f"REST Result action is not {action}")
    marker = b',"payload":'
    line = lines[-1]
    start = line.find(marker)
    require(start >= 0 and line.endswith(b"}"), f"REST {action} Result framing changed")
    payload = line[start + len(marker) : -1]
    require(json.loads(payload) == terminal.get("payload"), "REST Result payload bytes changed")
    return records, payload


def installed_artifacts(harness: Harness) -> dict[str, Any]:
    manifest = tomllib.loads((REPO_ROOT / "deps.toml").read_text(encoding="utf-8"))
    dependencies = manifest["dependency"]
    result: dict[str, Any] = {}
    for name in (
        "cloud-hypervisor",
        "rust-hypervisor-firmware",
        "cloud-hypervisor-edk2",
        "virtiofsd",
    ):
        dependency = dependencies[name]
        artifact = dependency["x86_64"]
        path = harness.home / "data" / "bin" / artifact["install_name"]
        actual = sha256(path)
        require(actual == artifact["sha256"], f"installed {name} checksum does not match deps.toml")
        result[name] = {
            "version": dependency["version"],
            "install_name": artifact["install_name"],
            "sha256": actual,
            "url": artifact["url"],
        }
    require(
        result["cloud-hypervisor"]["version"] == "v53.0",
        "E2E 9 requires pinned Cloud Hypervisor v53.0",
    )
    return result


def image_evidence(harness: Harness) -> dict[str, Any]:
    state = harness.state(MACHINE)
    image_id = state["image"].get("id")
    require(isinstance(image_id, str), "running machine did not pin an image id")
    sidecar_path = harness.home / "data" / "images" / f"{image_id}.json"
    sidecar = json.loads(sidecar_path.read_text(encoding="utf-8"))
    require(sidecar["source_ref"] == "ubuntu:24.04", "E2E 9 used the wrong image")
    require(sidecar["architecture"] == "x86_64", "E2E 9 used the wrong architecture")
    require(sidecar["firmware"] == "edk2", "Ubuntu x86_64 did not select edk2")
    require(
        sidecar["verification_algorithm"] == "sha256"
        and sidecar["verification_digest"] == sidecar["source_sha256"],
        "Ubuntu source verification is not the pinned SHA-256",
    )
    stored = harness.home / "data" / "images" / f"{image_id}.qcow2"
    stored_sha256 = sha256(stored)
    require(
        stored_sha256 == sidecar["stored_sha256"],
        "stored image checksum does not match its sidecar",
    )
    return {
        "id": image_id,
        "generation": sidecar["generation"],
        "source_ref": sidecar["source_ref"],
        "source_url": sidecar["source_url"],
        "source_sha256": sidecar["source_sha256"],
        "stored_sha256": sidecar["stored_sha256"],
        "stored_sha256_recomputed": stored_sha256,
        "size": sidecar["size"],
        "architecture": sidecar["architecture"],
        "firmware": sidecar["firmware"],
        "verification_algorithm": sidecar["verification_algorithm"],
        "verification_digest": sidecar["verification_digest"],
    }


def run_acceptance(harness: Harness) -> None:
    require(sys.platform == "linux", "M4 E2E 9 requires Linux")
    require(platform.machine() == "x86_64", "M4 E2E 9 requires x86_64")
    require(
        os.access("/dev/kvm", os.R_OK | os.W_OK),
        "/dev/kvm is not readable and writable",
    )
    for program in ("git", "qemu-img", "ssh", "ssh-keygen"):
        require(shutil.which(program) is not None, f"required host program is missing: {program}")

    commit = harness.run(["git", "rev-parse", "HEAD"]).stdout.decode().strip()
    if "FIRESTONE_BIN" not in os.environ:
        harness.run(
            ["cargo", "build", "--locked", "--bin", "firestone"],
            timeout=1_200,
        )
    require(
        harness.binary.is_file() and os.access(harness.binary, os.X_OK),
        "firestone binary was not built",
    )
    harness.evidence["commit"] = commit
    harness.evidence["host"] = {
        "system": platform.system(),
        "release": platform.release(),
        "architecture": platform.machine(),
        "kvm_read_write": True,
        "firestone_sha256": sha256(harness.binary),
        "harness_sha256": sha256(Path(__file__)),
        "initial_home_mode": "0700",
        "initial_home_empty": True,
    }

    doctor_fix, _, _ = harness.json_command(
        "doctor",
        "--fix",
        timeout=900,
        action="doctor",
    )
    doctor, _, _ = harness.json_command("doctor", action="doctor")
    checks = doctor[-1]["payload"].get("checks")
    require(isinstance(checks, list) and len(checks) == 13, "doctor did not return 13 checks")
    failures = [check for check in checks if check.get("status") == "fail"]
    require(not failures, f"doctor failed checks: {failures!r}")
    harness.evidence["scenarios"]["doctor"] = {
        "check_count": len(checks),
        "fix_result_sha256": bytes_sha256(
            json.dumps(doctor_fix[-1]["payload"], sort_keys=True).encode()
        ),
        "result_sha256": bytes_sha256(
            json.dumps(doctor[-1]["payload"], sort_keys=True).encode()
        ),
    }
    harness.evidence["artifacts"] = installed_artifacts(harness)

    create, _, _ = harness.json_command(
        "create",
        MACHINE,
        "ubuntu:24.04",
        "--net",
        "none",
        action="create",
    )
    require(create[-1]["payload"]["state"]["status"] == "created", "machine was not created")
    require(
        create[-1]["payload"]["spec"]["network"]["mode"] == "none",
        "E2E 9 unexpectedly enabled networking",
    )

    first_server = Server(harness)
    start_body = json.dumps(
        {"wait": True, "timeout_s": 600}, separators=(",", ":")
    ).encode()
    start_response = harness.http_request(
        "POST",
        f"/v1/machines/{MACHINE}/start",
        start_body,
        timeout=START_TIMEOUT_SECONDS,
    )
    require(start_response.status == 200, f"REST start returned {start_response.status}")
    require(
        start_response.headers.get("content-type") == "application/x-ndjson",
        "REST start did not return application/x-ndjson",
    )
    start_records, start_payload = ndjson_result(start_response.body, "start")
    require(len(start_records) > 1, "REST start emitted no progress before Result")
    require(start_records[-1]["payload"]["status"] == "running", "REST start did not reach running")

    listed = harness.http_request("GET", "/v1/machines")
    require(listed.status == 200, f"REST machine list returned {listed.status}")
    rows = json.loads(listed.body)
    require(
        any(row.get("name") == MACHINE and row.get("status") == "running" for row in rows),
        f"REST machine list did not report running: {rows!r}",
    )

    cli_show, cli_show_payload, _ = harness.json_command("show", MACHINE, action="show")
    rest_show = harness.http_request("GET", f"/v1/machines/{MACHINE}")
    require(rest_show.status == 200, f"REST machine show returned {rest_show.status}")
    require(json.loads(rest_show.body) == cli_show[-1]["payload"], "show payload values differ")
    require(
        rest_show.body == cli_show_payload,
        "actual CLI and REST show Result payload bytes or key ordering differ",
    )

    state = harness.state(MACHINE)
    shim_pid = state.get("shim_pid")
    vmm_pid = state.get("vmm_pid")
    require(isinstance(shim_pid, int) and process_alive(shim_pid), "shim is not alive")
    require(isinstance(vmm_pid, int) and process_alive(vmm_pid), "VMM is not alive")
    harness.evidence["image"] = image_evidence(harness)

    killed_server = first_server.stop(signal.SIGKILL)
    require(killed_server.returncode < 0, "SIGKILL did not terminate serve by signal")
    require(process_alive(shim_pid), "serve SIGKILL terminated the shim")
    require(process_alive(vmm_pid), "serve SIGKILL terminated the VMM")
    stale_socket = harness.socket_path.lstat()
    require(stat.S_ISSOCK(stale_socket.st_mode), "serve SIGKILL did not leave a stale socket")
    require(stat.S_IMODE(stale_socket.st_mode) == 0o600, "stale socket mode changed")

    restarted_server = Server(harness)
    require(process_alive(shim_pid), "serve restart terminated the shim")
    require(process_alive(vmm_pid), "serve restart terminated the VMM")
    listed_after_restart = harness.http_request("GET", "/v1/machines")
    rows_after_restart = json.loads(listed_after_restart.body)
    require(
        any(
            row.get("name") == MACHINE and row.get("status") == "running"
            for row in rows_after_restart
        ),
        f"restarted serve did not report running: {rows_after_restart!r}",
    )

    stop_body = json.dumps({"timeout_s": 90}, separators=(",", ":")).encode()
    stop_response = harness.http_request(
        "POST",
        f"/v1/machines/{MACHINE}/stop",
        stop_body,
        timeout=130,
    )
    require(stop_response.status == 200, f"REST stop returned {stop_response.status}")
    stop_records, stop_payload = ndjson_result(stop_response.body, "stop")
    require(stop_records[-1]["payload"]["status"] == "stopped", "REST stop did not stop")

    remove_response = harness.http_request("DELETE", f"/v1/machines/{MACHINE}")
    require(remove_response.status == 204, f"REST remove returned {remove_response.status}")
    require(not remove_response.body, "REST 204 remove returned a body")
    require(
        not (harness.home / "data" / "machines" / MACHINE).exists(),
        "REST remove left the machine directory",
    )

    graceful = restarted_server.stop(signal.SIGTERM)
    require(graceful.returncode == 0, f"serve SIGTERM exited {graceful.returncode}")
    require(not graceful.stdout and not graceful.stderr, "serve wrote process output")
    require(not harness.socket_path.exists(), "serve graceful shutdown left its socket")

    harness.evidence["scenarios"]["e2e_9"] = {
        "socket": {
            "path": os.fspath(harness.socket_path),
            "runtime_mode": "0700",
            "lock_mode": "0600",
            "first_publication": first_server.socket_identity,
            "first_observation_count": len(first_server.publication_observations),
            "stale_after_sigkill": {
                "mode": f"{stat.S_IMODE(stale_socket.st_mode):04o}",
                "device": stale_socket.st_dev,
                "inode": stale_socket.st_ino,
            },
            "restart_publication": restarted_server.socket_identity,
            "restart_observation_count": len(restarted_server.publication_observations),
            "atomic_mode_0600": True,
        },
        "start": {
            "status": start_response.status,
            "content_type": start_response.headers.get("content-type"),
            "record_count": len(start_records),
            "first_body_ms": start_response.first_body_ms,
            "completed_ms": start_response.completed_ms,
            "terminal_result_last": True,
            "payload_sha256": bytes_sha256(start_payload),
            "reported_running": True,
        },
        "cli_rest_show": {
            "same_parsed_payload": True,
            "byte_equal": True,
            "payload_sha256": bytes_sha256(cli_show_payload),
        },
        "serve_restart": {
            "killed_returncode": killed_server.returncode,
            "shim_pid": shim_pid,
            "vmm_pid": vmm_pid,
            "shim_survived": True,
            "vmm_survived": True,
            "reported_running_after_restart": True,
        },
        "stop": {
            "status": stop_response.status,
            "record_count": len(stop_records),
            "payload_sha256": bytes_sha256(stop_payload),
            "reported_stopped": True,
        },
        "remove": {
            "status": remove_response.status,
            "empty_body": True,
            "machine_directory_absent": True,
        },
    }


def install_harness_signal_handlers() -> None:
    def interrupted(signum: int, _frame: Any) -> None:
        raise AcceptanceError(f"M4 harness interrupted by signal {signum}")

    for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(signum, interrupted)


def main() -> int:
    if os.environ.get("FIRESTONE_E2E") != "1":
        print("skipped M4 KVM acceptance; set FIRESTONE_E2E=1 to run")
        return 0

    install_harness_signal_handlers()
    harness: Harness | None = None
    failure: str | None = None
    try:
        harness = Harness()
        run_acceptance(harness)
    except (AcceptanceError, OSError, ValueError, KeyError, TypeError) as error:
        failure = str(error)
    finally:
        if harness is not None:
            cleanup_errors = harness.cleanup()
            if cleanup_errors and failure is None:
                failure = "; ".join(cleanup_errors)
            harness.evidence["cleanup"] = {
                "completed": not cleanup_errors,
                "errors": cleanup_errors,
                "home_removed": not harness.keep_home and not harness.home.exists(),
                "home_kept_by_request": harness.keep_home,
            }
            harness.evidence["finished_at"] = dt.datetime.now(dt.UTC).isoformat()
            if failure is None:
                harness.evidence["result"] = "passed"
            else:
                harness.evidence["result"] = "failed"
                harness.evidence["error"] = failure
            try:
                harness.write_evidence()
            except (AcceptanceError, OSError) as error:
                if failure is None:
                    failure = f"cannot write evidence: {error}"

    if failure is not None:
        print(f"M4 KVM acceptance failed: {failure}", file=sys.stderr)
        return 1
    require(harness is not None, "M4 harness was not initialized")
    print(f"M4 KVM acceptance passed; evidence: {harness.evidence_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
