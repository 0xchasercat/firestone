#!/usr/bin/env python3
"""Run the M6 feature loop (E2E 12) against a real KVM host.

The scenarios are the M6 feature surfaces end to end on one live machine:
metrics, live resize, disk grow, `cp`, cold and warm snapshots, clone, pending
forwards, system prune, cloud-init authentication, and the two WebSocket
terminal transports. Every command, HTTP exchange, WebSocket read, and guest
interaction is bounded, and every machine is removed on success or failure.
"""

from __future__ import annotations

import atexit
import base64
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
import struct
import subprocess
import sys
import time
import tomllib
from pathlib import Path
from typing import Any


class AcceptanceError(RuntimeError):
    """The host or one M6 feature contract failed."""


REPO_ROOT = Path(__file__).resolve().parents[1]
MACHINE = "m6"
CLONE = "m6-clone"
AUTH_MACHINE = "m6-auth"
DEFAULT_IMAGE = "ubuntu:24.04"
COMMAND_TIMEOUT_SECONDS = 120
START_TIMEOUT_SECONDS = 1_200
STOP_TIMEOUT_SECONDS = 300
DOCTOR_TIMEOUT_SECONDS = 900
SERVER_START_TIMEOUT_SECONDS = 20
GUEST_TIMEOUT_SECONDS = 120
GUEST_SETTLE_SECONDS = 60
WEBSOCKET_READ_TIMEOUT_SECONDS = 30
MAX_OUTPUT_BYTES = 8 * 1024 * 1024
MAX_EVIDENCE_BYTES = 1024 * 1024
SATURATING_THRESHOLD = 1 << 63
WEBSOCKET_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
MARKER_PATH = "/root/e2e-marker"
USER_DATA_MARKER_PATH = "/root/userdata-marker"
USER_DATA_MARKER = "m6-user-data"
AUTH_PASSWORD = "m6-e2e-password"
WARM_PAGE = "M6-WARM-OK"
CONSOLE_BANNER = "M6-CONSOLE-BANNER"
GUEST_HTTP_PORT = 8080
GUEST_SECOND_PORT = 8081
GROWN_DISK = "24G"
BASE_DISK = "20G"


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


def require_clean_stream(label: str, value: bytes) -> None:
    controls = [
        (offset, byte)
        for offset, byte in enumerate(value)
        if byte < 0x20 and byte != 0x0A or byte == 0x7F
    ]
    if controls:
        raise AcceptanceError(
            f"{label} contains prohibited control byte 0x{controls[0][1]:02x} "
            f"at offset {controls[0][0]}"
        )
    try:
        value.decode("utf-8")
    except UnicodeDecodeError as error:
        raise AcceptanceError(f"{label} is not UTF-8: {error}") from error


# --------------------------------------------------------------------------
# Pure helpers. Everything below this line is unit-tested without a host.
# --------------------------------------------------------------------------


def saturating_numbers(value: Any, path: str = "$") -> list[str]:
    """Returns the JSON pointers of every integer at or above 2**63.

    Cloud Hypervisor reports an unexercised counter as a `u64::MAX`-family
    sentinel; SPEC §25.3 requires Firestone to publish `null` instead, so any
    such number reaching a client is a defect.
    """
    found: list[str] = []
    if isinstance(value, bool):
        return found
    if isinstance(value, int):
        if value >= SATURATING_THRESHOLD:
            found.append(path)
        return found
    if isinstance(value, dict):
        for key, member in value.items():
            found.extend(saturating_numbers(member, f"{path}.{key}"))
        return found
    if isinstance(value, list):
        for index, member in enumerate(value):
            found.extend(saturating_numbers(member, f"{path}[{index}]"))
    return found


def websocket_accept(key: str) -> str:
    """RFC 6455 `Sec-WebSocket-Accept` for one client key."""
    digest = hashlib.sha1((key + WEBSOCKET_GUID).encode("ascii")).digest()
    return base64.b64encode(digest).decode("ascii")


def encode_websocket_frame(opcode: int, payload: bytes, mask: bytes) -> bytes:
    """Encodes one final, masked client frame."""
    require(len(mask) == 4, "a client frame mask is four bytes")
    require(0 <= opcode <= 0xF, "a WebSocket opcode is four bits")
    header = bytearray()
    header.append(0x80 | opcode)
    length = len(payload)
    if length < 126:
        header.append(0x80 | length)
    elif length <= 0xFFFF:
        header.append(0x80 | 126)
        header.extend(struct.pack("!H", length))
    else:
        header.append(0x80 | 127)
        header.extend(struct.pack("!Q", length))
    header.extend(mask)
    masked = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
    return bytes(header) + masked


def decode_websocket_frame(buffer: bytes) -> tuple[bool, int, bytes, int] | None:
    """Decodes the first frame in `buffer`.

    Returns `(fin, opcode, payload, consumed)`, or `None` when the buffer does
    not hold a whole frame yet. A server frame is never masked; a masked one is
    a protocol violation and is refused.
    """
    if len(buffer) < 2:
        return None
    first, second = buffer[0], buffer[1]
    fin = bool(first & 0x80)
    require(first & 0x70 == 0, "server frame set a reserved WebSocket bit")
    opcode = first & 0x0F
    masked = bool(second & 0x80)
    require(not masked, "server frame was masked")
    length = second & 0x7F
    offset = 2
    if length == 126:
        if len(buffer) < offset + 2:
            return None
        length = struct.unpack("!H", buffer[offset : offset + 2])[0]
        offset += 2
    elif length == 127:
        if len(buffer) < offset + 8:
            return None
        length = struct.unpack("!Q", buffer[offset : offset + 8])[0]
        offset += 8
        require(length < SATURATING_THRESHOLD, "server frame declared an absurd length")
    if len(buffer) < offset + length:
        return None
    return fin, opcode, bytes(buffer[offset : offset + length]), offset + length


def http_response_complete(response: bytes) -> bool:
    """True when `response` already holds one whole HTTP message.

    The Cloud Hypervisor API socket keeps its connection open after a reply, so
    a client that waits for end of stream hangs forever. Framing decides when a
    response is finished; end of stream is only the fallback.
    """
    header_end = response.find(b"\r\n\r\n")
    if header_end < 0:
        return False
    try:
        head = response[:header_end].decode("ascii")
    except UnicodeDecodeError:
        return False
    body = response[header_end + 4 :]
    headers: dict[str, str] = {}
    for line in head.split("\r\n")[1:]:
        name, separator, value = line.partition(":")
        if separator == ":":
            headers[name.strip().lower()] = value.strip()
    if headers.get("transfer-encoding", "").lower() == "chunked":
        return b"\r\n0\r\n\r\n" in body or body.startswith(b"0\r\n\r\n")
    length = headers.get("content-length")
    if length is not None and length.isdigit():
        return len(body) >= int(length)
    return False


def parse_http_response(response: bytes) -> "HttpResponse":
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
    return HttpResponse(status, headers, body, header_end + 4)


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


def canonical_forwards(values: list[str]) -> list[str]:
    """Normalizes forward specs into the comparable multiset of SPEC §12.5."""
    canonical: list[str] = []
    for value in values:
        parts = value.split(":")
        proto = "tcp"
        if parts and parts[0] in {"tcp", "udp"}:
            proto = parts[0]
            parts = parts[1:]
        require(len(parts) >= 2, f"forward '{value}' has no host and guest port")
        host, guest = parts[-2], parts[-1]
        bind = ":".join(parts[:-2])
        canonical.append(f"{proto}:{bind}:{host}:{guest}")
    return sorted(canonical)


def prune_rows(payload: dict[str, Any]) -> list[tuple[str, str, int]]:
    """Projects a prune result into comparable `(kind, id, bytes)` rows."""
    removed = payload.get("removed")
    require(isinstance(removed, list), "prune payload has no removed list")
    rows: list[tuple[str, str, int]] = []
    for entry in removed:
        require(isinstance(entry, dict), "prune row is not an object")
        kind, identifier, size = entry.get("kind"), entry.get("id"), entry.get("bytes")
        require(
            isinstance(kind, str) and isinstance(identifier, str) and isinstance(size, int),
            f"prune row is malformed: {entry!r}",
        )
        rows.append((kind, identifier, size))
    return sorted(rows)


def machine_row(rows: Any, name: str) -> dict[str, Any]:
    require(isinstance(rows, list), "machine list payload is not an array")
    for row in rows:
        if isinstance(row, dict) and row.get("name") == name:
            return row
    raise AcceptanceError(f"machine list has no row for '{name}'")


def parse_free_total_bytes(output: str) -> int:
    """Reads the total column of `free -b`'s `Mem:` row."""
    for line in output.splitlines():
        if line.startswith("Mem:"):
            fields = line.split()
            require(len(fields) >= 2, "free -b Mem: row has no total column")
            return int(fields[1])
    raise AcceptanceError("free -b printed no Mem: row")


def parse_single_number(output: str) -> int:
    """Reads the one number a `df --output=size` or `nproc` line prints."""
    values = [line.strip() for line in output.splitlines() if line.strip()]
    require(bool(values), "expected one numeric line, got nothing")
    last = values[-1]
    require(last.isdigit(), f"expected a number, got {last!r}")
    return int(last)


# --------------------------------------------------------------------------


class HttpResponse:
    def __init__(
        self,
        status: int,
        headers: dict[str, str],
        body: bytes,
        header_bytes: int,
    ) -> None:
        self.status = status
        self.headers = headers
        self.body = body
        self.header_bytes = header_bytes


class Endpoint:
    """Where one HTTP client connects and what Host header it sends."""

    def __init__(
        self,
        *,
        unix_path: Path | None = None,
        address: tuple[str, int] | None = None,
        token: str | None = None,
    ) -> None:
        require(
            (unix_path is None) != (address is None),
            "an endpoint is either a Unix path or a TCP address",
        )
        self.unix_path = unix_path
        self.address = address
        self.token = token

    @property
    def host_header(self) -> str:
        if self.address is None:
            return "firestone"
        return f"{self.address[0]}:{self.address[1]}"

    @property
    def origin(self) -> str:
        return f"http://{self.host_header}"

    def connect(self, timeout: float) -> socket.socket:
        if self.unix_path is not None:
            client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            client.settimeout(timeout)
            client.connect(os.fspath(self.unix_path))
            return client
        assert self.address is not None
        client = socket.create_connection(self.address, timeout=timeout)
        client.settimeout(timeout)
        return client

    def headers(self) -> list[str]:
        lines = [f"Host: {self.host_header}"]
        if self.token is not None:
            lines.append(f"Authorization: Bearer {self.token}")
            lines.append("Sec-Fetch-Site: same-origin")
            lines.append(f"Origin: {self.origin}")
        return lines


def http_request(
    endpoint: Endpoint,
    method: str,
    path: str,
    body: bytes = b"",
    *,
    timeout: float = COMMAND_TIMEOUT_SECONDS,
) -> HttpResponse:
    request = [f"{method} {path} HTTP/1.1", *endpoint.headers(), "Connection: close"]
    request.append(f"Content-Length: {len(body)}")
    if body:
        request.append("Content-Type: application/json")
    wire = ("\r\n".join(request) + "\r\n\r\n").encode() + body
    deadline = time.monotonic() + timeout
    response = bytearray()
    with endpoint.connect(min(10.0, timeout)) as client:
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
            if http_response_complete(bytes(response)):
                break
    return parse_http_response(bytes(response))


class WebSocketClient:
    """The smallest RFC 6455 client that can prove the transports work."""

    def __init__(self, endpoint: Endpoint, path: str, *, timeout: float) -> None:
        self.endpoint = endpoint
        self.path = path
        self.buffer = bytearray()
        self.key = base64.b64encode(os.urandom(16)).decode("ascii")
        self.socket = endpoint.connect(timeout)
        request = [
            f"GET {path} HTTP/1.1",
            f"Host: {endpoint.host_header}",
            "Upgrade: websocket",
            "Connection: Upgrade",
            f"Sec-WebSocket-Key: {self.key}",
            "Sec-WebSocket-Version: 13",
            f"Origin: {endpoint.origin}",
        ]
        if endpoint.token is not None:
            request.append(f"Authorization: Bearer {endpoint.token}")
        self.socket.sendall(("\r\n".join(request) + "\r\n\r\n").encode())
        self.response = self._read_handshake(timeout)

    def _read_handshake(self, timeout: float) -> HttpResponse:
        deadline = time.monotonic() + timeout
        while b"\r\n\r\n" not in self.buffer:
            remaining = deadline - time.monotonic()
            require(remaining > 0, f"WebSocket handshake for {self.path} timed out")
            self.socket.settimeout(min(1.0, remaining))
            try:
                block = self.socket.recv(65_536)
            except TimeoutError:
                continue
            require(bool(block), f"WebSocket handshake for {self.path} closed early")
            self.buffer.extend(block)
            require(len(self.buffer) <= MAX_OUTPUT_BYTES, "handshake response exceeded 8 MiB")
        header_end = self.buffer.find(b"\r\n\r\n") + 4
        head = bytes(self.buffer[:header_end])
        del self.buffer[:header_end]
        response = parse_http_response(head)
        if response.status != 101:
            body = bytearray(self.buffer)
            length = int(response.headers.get("content-length", "0") or 0)
            deadline = time.monotonic() + min(5.0, timeout)
            while len(body) < length and time.monotonic() < deadline:
                self.socket.settimeout(1.0)
                try:
                    block = self.socket.recv(65_536)
                except TimeoutError:
                    continue
                if not block:
                    break
                body.extend(block)
            response.body = bytes(body[:length]) if length else bytes(body)
        return response

    def require_upgraded(self) -> None:
        require(
            self.response.status == 101,
            f"{self.path} answered {self.response.status}, expected 101: "
            f"{compact_bytes(self.response.body)}",
        )
        require(
            self.response.headers.get("upgrade", "").lower() == "websocket",
            f"{self.path} did not answer Upgrade: websocket",
        )
        require(
            self.response.headers.get("sec-websocket-accept") == websocket_accept(self.key),
            f"{self.path} returned the wrong Sec-WebSocket-Accept",
        )
        require(
            "sec-websocket-protocol" not in self.response.headers,
            f"{self.path} negotiated a subprotocol",
        )

    def send(self, opcode: int, payload: bytes) -> None:
        self.socket.sendall(encode_websocket_frame(opcode, payload, os.urandom(4)))

    def read_frame(self, timeout: float) -> tuple[bool, int, bytes]:
        deadline = time.monotonic() + timeout
        while True:
            decoded = decode_websocket_frame(bytes(self.buffer))
            if decoded is not None:
                fin, opcode, payload, consumed = decoded
                del self.buffer[:consumed]
                return fin, opcode, payload
            remaining = deadline - time.monotonic()
            require(remaining > 0, f"{self.path} produced no frame within {timeout:.1f}s")
            self.socket.settimeout(min(1.0, remaining))
            try:
                block = self.socket.recv(65_536)
            except TimeoutError:
                continue
            require(bool(block), f"{self.path} closed before a frame arrived")
            self.buffer.extend(block)
            require(len(self.buffer) <= MAX_OUTPUT_BYTES, "WebSocket buffer exceeded 8 MiB")

    def read_binary_until(self, needle: bytes, timeout: float) -> bytes:
        """Reads binary frames until `needle` appears in the accumulated bytes."""
        collected = bytearray()
        deadline = time.monotonic() + timeout
        while needle not in collected:
            remaining = deadline - time.monotonic()
            require(
                remaining > 0,
                f"{self.path} never carried {needle!r}; saw "
                f"{compact_bytes(bytes(collected))}",
            )
            _, opcode, payload = self.read_frame(remaining)
            if opcode == 0x8:
                raise AcceptanceError(
                    f"{self.path} closed before {needle!r} arrived: "
                    f"{compact_bytes(payload)}"
                )
            if opcode in {0x1, 0x2, 0x0}:
                collected.extend(payload)
            require(len(collected) <= MAX_OUTPUT_BYTES, "WebSocket payload exceeded 8 MiB")
        return bytes(collected)

    def close(self) -> None:
        try:
            self.send(0x8, struct.pack("!H", 1000))
        except OSError:
            pass
        try:
            self.socket.close()
        except OSError:
            pass


class Server:
    """One `firestone serve` process, on a Unix socket or a loopback port."""

    def __init__(
        self,
        harness: "Harness",
        *,
        listen: str | None = None,
        token_file: Path | None = None,
    ) -> None:
        self.harness = harness
        self.listen = listen
        argv: list[str | os.PathLike[str]] = [harness.binary, "serve"]
        if listen is not None:
            argv.extend(["--listen", listen])
        if token_file is not None:
            argv.extend(["--token", os.fspath(token_file)])
        self.token_file = token_file
        self.process = harness.spawn(argv)
        self.endpoint = self._wait_ready()

    def _wait_ready(self) -> Endpoint:
        deadline = time.monotonic() + SERVER_START_TIMEOUT_SECONDS
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                completed = self.harness.collect_process(self.process, 5, "serve startup")
                raise AcceptanceError(
                    f"serve exited {completed.returncode} before readiness; "
                    f"stdout:\n{compact_bytes(completed.stdout)}\n"
                    f"stderr:\n{compact_bytes(completed.stderr)}"
                )
            endpoint = self._candidate_endpoint()
            if endpoint is None:
                time.sleep(0.05)
                continue
            try:
                response = http_request(endpoint, "GET", "/v1/version", timeout=5)
            except (AcceptanceError, OSError):
                time.sleep(0.05)
                continue
            if response.status != 200:
                time.sleep(0.05)
                continue
            return endpoint
        raise AcceptanceError(f"serve did not become ready ({self.listen or 'unix default'})")

    def _candidate_endpoint(self) -> Endpoint | None:
        if self.listen is None or self.listen.startswith("unix:"):
            path = (
                Path(self.listen[len("unix:") :])
                if self.listen is not None
                else self.harness.socket_path
            )
            try:
                metadata = path.lstat()
            except FileNotFoundError:
                return None
            require(stat.S_ISSOCK(metadata.st_mode), "serve published a non-socket node")
            require(metadata.st_uid == os.getuid(), "serve socket has the wrong owner")
            require(
                stat.S_IMODE(metadata.st_mode) == 0o600,
                "serve socket was visible outside mode 0600",
            )
            return Endpoint(unix_path=path)
        require(self.listen.startswith("tcp:"), "unsupported serve listener")
        host, _, port = self.listen[len("tcp:") :].rpartition(":")
        require(self.token_file is not None, "a TCP listener needs a token file")
        assert self.token_file is not None
        if not self.token_file.exists():
            return None
        metadata = self.token_file.stat()
        require(
            stat.S_IMODE(metadata.st_mode) == 0o600 and metadata.st_uid == os.getuid(),
            "serve token file is not a current-user mode-0600 file",
        )
        token = self.token_file.read_text(encoding="utf-8").strip()
        if len(token) != 64:
            return None
        return Endpoint(address=(host, int(port)), token=token)

    def stop(self) -> None:
        if self.process.poll() is None:
            self.harness.terminate_process(self.process)
        else:
            self.harness.forget_process(self.process)


class Harness:
    def __init__(self) -> None:
        self.home = self._checked_home()
        self.keep_home = os.environ.get("FIRESTONE_E2E_KEEP") == "1"
        self.image = os.environ.get("FIRESTONE_E2E_IMAGE", DEFAULT_IMAGE)
        default_evidence = Path("/tmp") / f"firestone-m6-evidence-{os.getpid()}.json"
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
        self.workspace = self.home / "harness-work"
        self.workspace.mkdir(mode=0o700)
        self.commands: list[str] = []
        self.active_processes: list[subprocess.Popen[bytes]] = []
        self.created_machines: list[str] = []
        self.evidence: dict[str, Any] = {
            "schema": 1,
            "scenario": "e2e12-m6-feature-loop",
            "result": "running",
            "started_at": dt.datetime.now(dt.UTC).isoformat(),
            "commands": self.commands,
            "host": {},
            "artifacts": {},
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
        source = (
            Path(configured).expanduser()
            if configured
            else REPO_ROOT / "target" / "debug" / "firestone"
        )
        if not source.is_absolute():
            source = (Path.cwd() / source).resolve()
        require(source.is_file(), f"the Firestone binary is missing: {source}")
        require(os.access(source, os.X_OK), f"the Firestone binary is not executable: {source}")
        directory = self.home / "harness-bin"
        directory.mkdir(mode=0o700)
        binary = directory / "firestone"
        shutil.copy2(source, binary)
        os.chmod(binary, 0o755)
        require(sha256(binary) == sha256(source), "the staged Firestone binary changed bytes")
        return binary

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
            raise AcceptanceError(f"command timed out after {timeout:.1f}s: {rendered}") from error
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
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                pass
        self.forget_process(process)

    def json_command(
        self,
        *arguments: str,
        action: str,
        timeout: float = COMMAND_TIMEOUT_SECONDS,
        expected_code: int = 0,
    ) -> tuple[list[dict[str, Any]], dict[str, Any] | list[Any]]:
        completed = self.run([self.binary, "--json", *arguments], timeout=timeout, check=False)
        label = shlex.join(arguments)
        require(
            completed.returncode == expected_code,
            f"{label} exited {completed.returncode}, expected {expected_code}:\n"
            f"stdout:\n{compact_bytes(completed.stdout)}\n"
            f"stderr:\n{compact_bytes(completed.stderr)}",
        )
        require_clean_stream(f"{label} JSON stdout", completed.stdout)
        require(not completed.stderr, f"{label} wrote stderr under --json")
        lines = completed.stdout.splitlines()
        require(bool(lines) and all(lines), f"{label} emitted no JSON records")
        try:
            records = [json.loads(line) for line in lines]
        except (json.JSONDecodeError, UnicodeDecodeError) as error:
            raise AcceptanceError(f"{label} emitted invalid NDJSON") from error
        terminal = records[-1]
        require(terminal.get("type") == "Result", f"{label} did not end with Result")
        require(terminal.get("action") == action, f"{label} Result action is not {action}")
        payload = terminal.get("payload")
        require(payload is not None, f"{label} Result carries no payload")
        return records, payload

    def object_command(
        self, *arguments: str, action: str, timeout: float = COMMAND_TIMEOUT_SECONDS
    ) -> tuple[list[dict[str, Any]], dict[str, Any]]:
        records, payload = self.json_command(*arguments, action=action, timeout=timeout)
        require(isinstance(payload, dict), f"{action} payload is not an object")
        assert isinstance(payload, dict)
        return records, payload

    def guest(
        self,
        name: str,
        command: str,
        *,
        timeout: float = GUEST_TIMEOUT_SECONDS,
        check: bool = True,
    ) -> str:
        completed = self.run(
            [self.binary, "shell", name, "--", "sh", "-c", command],
            timeout=timeout,
            check=check,
        )
        return completed.stdout.decode("utf-8", "replace")

    def guest_soft(self, name: str, command: str) -> str:
        """One guest command that may fail, for use inside a polling loop."""
        return self.guest(name, command, check=False)

    def guest_number(self, name: str, command: str) -> int | None:
        """One numeric guest reading, or `None` while the guest cannot answer."""
        try:
            return parse_single_number(self.guest_soft(name, command))
        except AcceptanceError:
            return None

    def state(self, name: str) -> dict[str, Any]:
        path = self.home / "data" / "machines" / name / "state.json"
        try:
            value = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            raise AcceptanceError(f"cannot read state for {name}: {error}") from error
        require(isinstance(value, dict), f"state for {name} is not an object")
        return value

    def machine_dir(self, name: str) -> Path:
        return self.home / "data" / "machines" / name

    def vmm_info(self, name: str) -> dict[str, Any]:
        socket_path = self.home / "run" / name / "api.sock"
        require(socket_path.exists(), f"machine {name} has no api.sock at {socket_path}")
        endpoint = Endpoint(unix_path=socket_path)
        response = http_request(endpoint, "GET", "/api/v1/vm.info", timeout=15)
        require(response.status == 200, f"vm.info answered {response.status}")
        try:
            document = json.loads(response.body)
        except json.JSONDecodeError as error:
            raise AcceptanceError("vm.info returned invalid JSON") from error
        require(isinstance(document, dict), "vm.info is not an object")
        return document

    def start(self, name: str, *, timeout_seconds: int = 900) -> dict[str, Any]:
        _, payload = self.object_command(
            "start",
            name,
            "--timeout",
            f"{timeout_seconds}s",
            action="start",
            timeout=START_TIMEOUT_SECONDS,
        )
        require(payload.get("status") == "running", f"{name} did not reach running")
        return payload

    def stop(self, name: str) -> dict[str, Any]:
        _, payload = self.object_command(
            "stop", name, "--timeout", "120s", action="stop", timeout=STOP_TIMEOUT_SECONDS
        )
        require(payload.get("status") == "stopped", f"{name} did not stop")
        return payload

    def write_evidence(self) -> None:
        parent = self.evidence_path.parent
        require(parent.is_dir(), "evidence parent directory does not exist")
        payload = (json.dumps(self.evidence, indent=2, sort_keys=True) + "\n").encode()
        require(len(payload) <= MAX_EVIDENCE_BYTES, "evidence exceeds 1 MiB")
        temporary = parent / f".{self.evidence_path.name}.{os.getpid()}.partial"
        flags = os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_CLOEXEC
        if hasattr(os, "O_NOFOLLOW"):
            flags |= os.O_NOFOLLOW
        descriptor = os.open(temporary, flags, 0o600)
        try:
            with os.fdopen(descriptor, "wb", closefd=False) as stream:
                stream.write(payload)
                stream.flush()
                os.fsync(stream.fileno())
        finally:
            os.close(descriptor)
        os.replace(temporary, self.evidence_path)
        os.chmod(self.evidence_path, 0o600)

    def cleanup(self) -> list[str]:
        if self._cleanup_started:
            return []
        self._cleanup_started = True
        errors: list[str] = []
        for process in list(self.active_processes):
            self.terminate_process(process)
        if getattr(self, "binary", None) is not None and self.binary.is_file():
            for name in reversed(self.created_machines):
                for arguments in (
                    [self.binary, "--json", "stop", name, "--force", "--timeout", "15s"],
                    [self.binary, "--json", "rm", name, "--force"],
                ):
                    try:
                        subprocess.run(
                            [os.fspath(value) for value in arguments],
                            cwd=REPO_ROOT,
                            env=self.environment(),
                            stdin=subprocess.DEVNULL,
                            stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL,
                            timeout=120,
                            check=False,
                        )
                    except (OSError, subprocess.TimeoutExpired) as error:
                        errors.append(f"cleanup command failed: {error}")
        if not self.keep_home:
            try:
                shutil.rmtree(self.home)
            except FileNotFoundError:
                pass
            except OSError as error:
                errors.append(f"cannot remove FIRESTONE_HOME: {error}")
        return errors


def free_loopback_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.bind(("127.0.0.1", 0))
        return int(probe.getsockname()[1])


def dependency_artifact(name: str, architecture: str = "x86_64") -> dict[str, Any]:
    manifest = tomllib.loads((REPO_ROOT / "deps.toml").read_text(encoding="utf-8"))
    dependency = manifest["dependency"].get(name)
    require(isinstance(dependency, dict), f"deps.toml has no [dependency.{name}]")
    artifact = dependency.get(architecture)
    require(isinstance(artifact, dict), f"deps.toml pins no {architecture} {name}")
    return {"version": dependency["version"], **artifact}


def helper_cache_dir() -> Path:
    configured = os.environ.get("FIRESTONE_E2E_HELPER_CACHE")
    cache = (
        Path(configured).expanduser()
        if configured
        else Path.home() / ".cache" / "firestone-e2e-helpers"
    )
    cache.mkdir(parents=True, exist_ok=True)
    return cache


def stage_pinned_helper(harness: Harness, name: str) -> dict[str, Any]:
    """Installs one `deps.toml` helper a standalone release would embed.

    `doctor --fix` downloads only the three vendored dependencies; `passt` and
    `qemu-img` reach a release through the embedded-helper payload, which a
    plain `cargo build` does not carry. The harness therefore stages the exact
    pinned bytes into the run's own `data/bin`, which is where `doctor` looks
    first, so the run measures the pinned helper rather than whatever the host
    happens to carry.
    """
    import urllib.request

    artifact = dependency_artifact(name)
    cached = helper_cache_dir() / artifact["install_name"]
    if not cached.is_file() or sha256(cached) != artifact["sha256"]:
        partial = cached.with_name(f".{cached.name}.{os.getpid()}.partial")
        request = urllib.request.Request(  # noqa: S310 - a pinned HTTPS release asset
            artifact["url"], headers={"Accept-Encoding": "identity"}
        )
        require(artifact["url"].startswith("https://"), f"{name} is not pinned over HTTPS")
        with urllib.request.urlopen(request, timeout=120) as stream:  # noqa: S310
            partial.write_bytes(stream.read(256 * 1024 * 1024))
        os.replace(partial, cached)
        require(
            sha256(cached) == artifact["sha256"],
            f"the downloaded {name} does not match its deps.toml checksum",
        )
    bin_dir = harness.home / "data" / "bin"
    bin_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
    for directory in (harness.home / "data", bin_dir):
        os.chmod(directory, 0o700)
    target = bin_dir / artifact["install_name"]
    shutil.copy2(cached, target)
    os.chmod(target, 0o755)
    require(sha256(target) == artifact["sha256"], f"the staged {name} changed bytes")
    return {
        "version": artifact["version"],
        "install_name": artifact["install_name"],
        "sha256": artifact["sha256"],
        "staged_from": artifact["url"],
    }


def installed_artifacts(harness: Harness) -> dict[str, Any]:
    manifest = tomllib.loads((REPO_ROOT / "deps.toml").read_text(encoding="utf-8"))
    dependencies = manifest["dependency"]
    result: dict[str, Any] = {}
    for name in ("cloud-hypervisor", "rust-hypervisor-firmware", "cloud-hypervisor-edk2", "passt"):
        dependency = dependencies.get(name)
        if dependency is None:
            continue
        artifact = dependency.get("x86_64")
        if artifact is None:
            continue
        path = harness.home / "data" / "bin" / artifact["install_name"]
        if not path.is_file():
            continue
        actual = sha256(path)
        require(
            actual == artifact["sha256"],
            f"installed {name} checksum differs from deps.toml",
        )
        result[name] = {
            "version": dependency["version"],
            "install_name": artifact["install_name"],
            "sha256": actual,
        }
    require("cloud-hypervisor" in result, "doctor --fix did not install cloud-hypervisor")
    return result


def rest_json(endpoint: Endpoint, method: str, path: str, body: bytes = b"") -> Any:
    response = http_request(endpoint, method, path, body)
    require(
        response.status in {200, 201},
        f"{method} {path} answered {response.status}: {compact_bytes(response.body)}",
    )
    try:
        return json.loads(response.body)
    except json.JSONDecodeError as error:
        raise AcceptanceError(f"{method} {path} returned invalid JSON") from error


def wait_for(predicate: Any, timeout: float, message: str) -> Any:
    deadline = time.monotonic() + timeout
    last: Any = None
    while time.monotonic() < deadline:
        last = predicate()
        if last:
            return last
        time.sleep(1.0)
    raise AcceptanceError(f"{message} (last observation: {last!r})")


# --------------------------------------------------------------------------
# Scenarios
# --------------------------------------------------------------------------


def scenario_metrics(harness: Harness, endpoint: Endpoint, name: str) -> dict[str, Any]:
    first = rest_json(endpoint, "GET", f"/v1/machines/{name}/metrics")
    require(isinstance(first, dict), "metrics did not return one JSON object")
    harness.guest(
        name,
        "end=$(( $(date +%s) + 3 )); while [ $(date +%s) -lt $end ]; do :; done; exit 0",
    )
    second = rest_json(endpoint, "GET", f"/v1/machines/{name}/metrics")
    require(isinstance(second, dict), "the second metrics sample is not an object")

    for label, sample in (("first", first), ("second", second)):
        require(
            set(sample) == {"sampled_at", "cpu", "memory", "block", "net"},
            f"the {label} metrics sample has the wrong key set: {sorted(sample)}",
        )
        offenders = saturating_numbers(sample)
        require(
            not offenders,
            f"the {label} metrics sample surfaced a u64::MAX sentinel at {offenders}",
        )
        block = sample["block"]
        require(isinstance(block, list) and block, f"the {label} sample reported no block device")
        for device in block:
            require(
                set(device)
                == {"device", "read_bytes", "written_bytes", "read_ops", "write_ops"},
                f"block entry key set changed: {sorted(device)}",
            )
            require(isinstance(device["device"], str), "a block device id is not a string")
        require(
            any(device["read_bytes"] for device in block),
            f"the {label} sample reported no block reads at all",
        )
        require(
            isinstance(sample["cpu"].get("vcpus"), int),
            f"the {label} sample has no vCPU count",
        )
    before = first["cpu"]["cpu_time_ns"]
    after = second["cpu"]["cpu_time_ns"]
    require(
        isinstance(before, int) and isinstance(after, int),
        "cpu_time_ns was null on Linux, where /proc is readable",
    )
    require(after > before, f"cumulative cpu_time_ns did not increase: {before} -> {after}")
    require(
        second["sampled_at"] > first["sampled_at"],
        "the second sample is not later than the first",
    )
    _, cli = harness.object_command("metrics", name, action="metrics")
    require(set(cli) == set(first), "the CLI metrics payload has a different key set than REST")
    return {
        "first_cpu_time_ns": before,
        "second_cpu_time_ns": after,
        "block_devices": [device["device"] for device in first["block"]],
        "net": first["net"],
        "sentinels": [],
        "cli_matches_rest_shape": True,
    }


def scenario_resize(harness: Harness, name: str) -> dict[str, Any]:
    before_nproc = parse_single_number(harness.guest(name, "nproc"))
    before_memory = parse_free_total_bytes(harness.guest(name, "free -b"))
    _, payload = harness.object_command(
        "resize", name, "--cpus", "2", "--memory", "2G", action="resize"
    )
    require(payload.get("applied_live") is True, "the live resize did not apply live")
    require(payload.get("cpus") == 2, "the resize result did not report two vCPUs")
    require(payload.get("memory") == "2G", "the resize result did not report 2G")

    info = harness.vmm_info(name)
    config = info.get("config", {})
    boot_vcpus = config.get("cpus", {}).get("boot_vcpus")
    require(boot_vcpus == 2, f"vm.info still reports {boot_vcpus} boot vCPUs after the resize")
    actual = info.get("memory_actual_size")
    require(
        isinstance(actual, int) and actual >= 2 * 1024**3,
        f"vm.info memory_actual_size did not reach 2 GiB: {actual!r}",
    )

    wait_for(
        lambda: harness.guest_number(name, "nproc") == 2,
        GUEST_SETTLE_SECONDS,
        "the guest never onlined the hotplugged vCPU",
    )
    def memory_grew() -> bool:
        try:
            return parse_free_total_bytes(harness.guest_soft(name, "free -b")) > before_memory
        except AcceptanceError:
            return False

    wait_for(
        memory_grew,
        GUEST_SETTLE_SECONDS,
        "the guest never onlined the hotplugged memory",
    )
    after_memory = parse_free_total_bytes(harness.guest(name, "free -b"))
    return {
        "applied_live": True,
        "before_nproc": before_nproc,
        "after_nproc": 2,
        "before_memory_bytes": before_memory,
        "after_memory_bytes": after_memory,
        "vm_info_boot_vcpus": boot_vcpus,
        "vm_info_memory_actual_size": actual,
    }


def scenario_cp(harness: Harness, name: str) -> dict[str, Any]:
    source = harness.workspace / "cp-source.bin"
    payload = os.urandom(64 * 1024)
    source.write_bytes(payload)
    harness.run([harness.binary, "cp", source, f"{name}:/root/cp-source.bin"], timeout=180)
    returned = harness.workspace / "cp-returned.bin"
    harness.run([harness.binary, "cp", f"{name}:/root/cp-source.bin", returned], timeout=180)
    require(returned.read_bytes() == payload, "firestone cp did not round-trip the bytes")

    tree = harness.workspace / "cp-tree"
    (tree / "nested").mkdir(parents=True)
    (tree / "top.txt").write_text("top\n", encoding="utf-8")
    (tree / "nested" / "leaf.txt").write_text("leaf\n", encoding="utf-8")
    harness.run([harness.binary, "cp", "-r", tree, f"{name}:/root/cp-tree"], timeout=180)
    listing = harness.guest(name, "cat /root/cp-tree/top.txt /root/cp-tree/nested/leaf.txt")
    require(listing.split() == ["top", "leaf"], f"recursive cp did not land the tree: {listing!r}")
    returned_tree = harness.workspace / "cp-tree-back"
    harness.run(
        [harness.binary, "cp", "-r", f"{name}:/root/cp-tree", returned_tree],
        timeout=180,
    )
    require(
        (returned_tree / "nested" / "leaf.txt").read_text(encoding="utf-8") == "leaf\n",
        "recursive cp did not return the tree",
    )
    return {
        "file_bytes": len(payload),
        "file_sha256": bytes_sha256(payload),
        "round_trip_equal": True,
        "recursive_round_trip_equal": True,
    }


def scenario_terminals(harness: Harness, name: str) -> dict[str, Any]:
    port = free_loopback_port()
    token_file = harness.workspace / "serve-token"
    server = Server(
        harness,
        listen=f"tcp:127.0.0.1:{port}",
        token_file=token_file,
    )
    try:
        endpoint = server.endpoint
        rows = rest_json(endpoint, "GET", "/v1/machines")
        require(
            machine_row(rows, name)["status"] == "running",
            "the TCP listener did not report the running machine",
        )
        console = WebSocketClient(
            endpoint, f"/v1/machines/{name}/console/ws", timeout=WEBSOCKET_READ_TIMEOUT_SECONDS
        )
        try:
            console.require_upgraded()
            console.send(0x1, json.dumps({"resize": {"rows": 40, "cols": 120}}).encode())
            console.send(0x2, b"\n")
            harness.guest(name, f"printf '{CONSOLE_BANNER}\\n' > /dev/hvc0")
            banner = console.read_binary_until(
                CONSOLE_BANNER.encode(), WEBSOCKET_READ_TIMEOUT_SECONDS
            )
            busy = WebSocketClient(
                endpoint,
                f"/v1/machines/{name}/console/ws",
                timeout=WEBSOCKET_READ_TIMEOUT_SECONDS,
            )
            try:
                require(
                    busy.response.status == 409,
                    f"a second console client answered {busy.response.status}, expected 409",
                )
                envelope = json.loads(busy.response.body)
                require(
                    envelope.get("error", {}).get("kind") == "busy",
                    f"the busy console error is not kind busy: {envelope!r}",
                )
            finally:
                busy.close()
        finally:
            console.close()

        shell = WebSocketClient(
            endpoint, f"/v1/machines/{name}/shell/ws", timeout=WEBSOCKET_READ_TIMEOUT_SECONDS
        )
        try:
            shell.require_upgraded()
            shell.send(0x1, json.dumps({"resize": {"rows": 24, "cols": 80}}).encode())
            shell.send(0x2, b"printf 'M6-SHELL-%s\\n' OK\n")
            shell_bytes = shell.read_binary_until(b"M6-SHELL-OK", WEBSOCKET_READ_TIMEOUT_SECONDS)
        finally:
            shell.close()
    finally:
        server.stop()
    return {
        "listen": f"tcp:127.0.0.1:{port}",
        "console_status": 101,
        "console_banner_bytes": len(banner),
        "console_second_client_status": 409,
        "shell_status": 101,
        "shell_bytes": len(shell_bytes),
    }


def guest_boot_identity(harness: Harness, name: str) -> tuple[str, float]:
    boot_id = harness.guest(name, "cat /proc/sys/kernel/random/boot_id").strip()
    uptime = float(harness.guest(name, "cut -d' ' -f1 /proc/uptime").strip())
    require(bool(boot_id), "the guest reported no boot id")
    return boot_id, uptime


def read_marker(harness: Harness, name: str) -> str:
    return harness.guest_soft(name, f"cat {MARKER_PATH}").strip()


def write_marker(harness: Harness, name: str, value: str) -> None:
    harness.guest(name, f"printf '{value}' > {MARKER_PATH}; sync")


def http_get_forward(port: int, timeout: float) -> bytes:
    deadline = time.monotonic() + timeout
    last: str = "no attempt"
    while time.monotonic() < deadline:
        try:
            endpoint = Endpoint(address=("127.0.0.1", port))
            response = http_request(endpoint, "GET", "/", timeout=10)
        except (AcceptanceError, OSError) as error:
            last = str(error)
            time.sleep(1.0)
            continue
        if response.status == 200:
            return response.body
        last = f"status {response.status}"
        time.sleep(1.0)
    raise AcceptanceError(f"the forwarded port {port} never answered 200: {last}")


def scenario_warm_snapshot(harness: Harness, name: str, forward_port: int) -> dict[str, Any]:
    write_marker(harness, name, "base")
    harness.guest(
        name,
        "mkdir -p /root/www && printf '"
        + WARM_PAGE
        + "' > /root/www/index.html && cd /root/www && "
        "setsid python3 -m http.server "
        f"{GUEST_HTTP_PORT} > /root/www/http.log 2>&1 < /dev/null & sleep 2; exit 0",
    )
    page = http_get_forward(forward_port, 60)
    require(page.decode().strip() == WARM_PAGE, f"the forward served {page!r}")
    boot_id, uptime = guest_boot_identity(harness, name)

    _, created = harness.object_command(
        "snapshot", "create", name, "warm1", action="snapshot-create", timeout=600
    )
    require(created.get("kind") == "warm", f"a running machine yielded {created.get('kind')}")
    require(isinstance(created.get("memory_bytes"), int), "the warm snapshot has no memory_bytes")

    resumed_boot_id, resumed_uptime = guest_boot_identity(harness, name)
    require(resumed_boot_id == boot_id, "the guest rebooted across the warm snapshot")
    require(resumed_uptime > uptime, "the guest uptime did not advance after the resume")

    write_marker(harness, name, "warm-mutated")
    require(read_marker(harness, name) == "warm-mutated", "the guest mutation did not land")
    harness.stop(name)

    _, restored = harness.object_command(
        "snapshot",
        "restore",
        name,
        "warm1",
        "--start",
        action="snapshot-restore",
        timeout=START_TIMEOUT_SECONDS,
    )
    require(restored.get("started") is True, "a warm restore did not start the machine")
    wait_for(
        lambda: read_marker(harness, name) == "base",
        GUEST_SETTLE_SECONDS,
        "the warm restore did not roll the guest file back",
    )
    restored_boot_id, _ = guest_boot_identity(harness, name)
    require(restored_boot_id == boot_id, "the warm restore booted a fresh kernel")
    restored_page = http_get_forward(forward_port, 120)
    require(
        restored_page.decode().strip() == WARM_PAGE,
        f"the restored machine's forward served {restored_page!r}",
    )
    return {
        "kind": "warm",
        "memory_bytes": created["memory_bytes"],
        "disk_bytes": created.get("disk_bytes"),
        "boot_id_stable": True,
        "marker_rolled_back": True,
        "forward_alive_after_restore": True,
    }


def scenario_cold_snapshot(harness: Harness, name: str) -> dict[str, Any]:
    harness.stop(name)
    _, created = harness.object_command(
        "snapshot", "create", name, "cold1", action="snapshot-create", timeout=600
    )
    require(created.get("kind") == "cold", f"a stopped machine yielded {created.get('kind')}")
    require("memory_bytes" not in created or created["memory_bytes"] is None,
            "a cold snapshot reported memory bytes")
    _, listed = harness.object_command("snapshot", "list", name, action="snapshot-list")
    names = {entry["snapshot"] for entry in listed["snapshots"]}
    require(names == {"warm1", "cold1"}, f"snapshot list is {sorted(names)}")

    harness.start(name)
    write_marker(harness, name, "cold-mutated")
    harness.stop(name)
    _, restored = harness.object_command(
        "snapshot",
        "restore",
        name,
        "cold1",
        "--start",
        action="snapshot-restore",
        timeout=START_TIMEOUT_SECONDS,
    )
    require(restored.get("started") is True, "the cold restore did not honor --start")
    require(read_marker(harness, name) == "base", "the cold restore did not roll the guest back")
    return {
        "kind": "cold",
        "disk_bytes": created.get("disk_bytes"),
        "snapshots": sorted(names),
        "marker_rolled_back": True,
    }


def scenario_pending_forwards(
    harness: Harness, endpoint: Endpoint, name: str, first: int, second: int
) -> dict[str, Any]:
    configured = [f"{first}:{GUEST_HTTP_PORT}", f"{second}:{GUEST_SECOND_PORT}"]
    body = json.dumps({"network": {"forward": configured}}, separators=(",", ":")).encode()
    response = http_request(endpoint, "PATCH", f"/v1/machines/{name}", body)
    require(response.status == 200, f"PATCH answered {response.status}")
    patched = json.loads(response.body)
    warnings = patched.get("warnings", [])
    require(
        any("port forwards apply on restart" in warning for warning in warnings),
        f"PATCH did not warn about pending forwards: {warnings!r}",
    )
    _, rows = harness.json_command("ls", action="list")
    row = machine_row(rows, name)
    require(row["forwards_pending"] is True, "ls --json did not mark the forwards pending")
    require(
        canonical_forwards(row["forwards"]) == canonical_forwards([f"{first}:{GUEST_HTTP_PORT}"]),
        "the pending row stopped showing the applied forwards",
    )
    _, restarted = harness.object_command(
        "restart", name, action="restart", timeout=START_TIMEOUT_SECONDS
    )
    require(restarted.get("status") == "running", "restart did not leave the machine running")
    _, rows_after = harness.json_command("ls", action="list")
    row_after = machine_row(rows_after, name)
    require(row_after["forwards_pending"] is False, "restart did not clear the pending flag")
    require(
        canonical_forwards(row_after["forwards"]) == canonical_forwards(configured),
        f"restart applied {row_after['forwards']!r}, expected {configured!r}",
    )
    applied = harness.state(name)["forwards"]
    require(
        canonical_forwards(applied) == canonical_forwards(configured),
        "state.json did not record the newly applied forwards",
    )
    return {
        "configured": configured,
        "pending_before_restart": True,
        "pending_after_restart": False,
        "applied_after_restart": row_after["forwards"],
    }


def scenario_disk_grow(harness: Harness, endpoint: Endpoint, name: str) -> dict[str, Any]:
    before = parse_single_number(harness.guest(name, "df -B1 --output=size / | tail -1"))
    body = json.dumps({"disk": GROWN_DISK}, separators=(",", ":")).encode()
    response = http_request(endpoint, "PATCH", f"/v1/machines/{name}", body)
    require(response.status == 200, f"the disk PATCH answered {response.status}")
    records, restarted = harness.object_command(
        "restart", name, action="restart", timeout=START_TIMEOUT_SECONDS
    )
    require(restarted.get("status") == "running", "the machine did not restart")
    disk_steps = [
        record.get("detail", "")
        for record in records
        if record.get("id") == "disk" and record.get("type") == "StepDone"
    ]
    require(
        any("grown" in str(detail) for detail in disk_steps),
        f"start did not report a grown overlay: {disk_steps!r}",
    )
    def filesystem_grew() -> bool:
        measured = harness.guest_number(name, "df -B1 --output=size / | tail -1")
        return measured is not None and measured > before

    wait_for(
        filesystem_grew,
        GUEST_SETTLE_SECONDS,
        "growpart never extended the guest root filesystem",
    )
    grown = parse_single_number(harness.guest(name, "df -B1 --output=size / | tail -1"))
    return {
        "before_bytes": before,
        "after_bytes": grown,
        "spec_disk": GROWN_DISK,
        "disk_step_details": disk_steps,
    }


def scenario_clone(harness: Harness, endpoint: Endpoint, name: str, clone: str) -> dict[str, Any]:
    write_marker(harness, name, "clone-base")
    harness.guest(name, "sync")
    harness.stop(name)
    source_state = harness.state(name)
    harness.created_machines.append(clone)
    _, cloned = harness.object_command("clone", name, clone, action="clone", timeout=1_800)
    require(cloned.get("source") == name and cloned.get("dest") == clone, "clone result changed")
    require(isinstance(cloned.get("disk_bytes"), int) and cloned["disk_bytes"] > 0,
            "the clone copied no overlay")

    body = json.dumps({"network": {"forward": []}}, separators=(",", ":")).encode()
    response = http_request(endpoint, "PATCH", f"/v1/machines/{clone}", body)
    require(response.status == 200, f"clearing the clone's forwards answered {response.status}")

    harness.start(name)
    harness.start(clone)
    require(read_marker(harness, clone) == "clone-base", "the clone lost the source's guest file")
    clone_state = harness.state(clone)
    require(
        clone_state["instance_id"] != source_state["instance_id"],
        "the clone reused the source's cloud-init instance id",
    )
    require(clone_state["mac"] != source_state["mac"], "the clone reused the source's MAC")
    wait_for(
        lambda: harness.guest_soft(clone, "hostname").strip() == clone,
        GUEST_SETTLE_SECONDS,
        "the clone never re-provisioned its hostname",
    )
    return {
        "source_instance_id": source_state["instance_id"],
        "clone_instance_id": clone_state["instance_id"],
        "source_mac": source_state["mac"],
        "clone_mac": clone_state["mac"],
        "clone_hostname": clone,
        "disk_bytes": cloned["disk_bytes"],
        "marker_present": True,
    }


def scenario_cloud_init_auth(harness: Harness, name: str) -> dict[str, Any]:
    password_file = harness.workspace / "password"
    password_file.write_text(AUTH_PASSWORD + "\n", encoding="utf-8")
    os.chmod(password_file, 0o600)
    user_data = (
        "#cloud-config\n"
        "write_files:\n"
        f"  - path: {USER_DATA_MARKER_PATH}\n"
        "    permissions: '0600'\n"
        f"    content: {USER_DATA_MARKER}\n"
    )
    harness.created_machines.append(name)
    harness.object_command(
        "create",
        name,
        harness.image,
        "--net",
        "none",
        "--cpus",
        "1",
        "--memory",
        "1G",
        "--password-file",
        os.fspath(password_file),
        "--ssh-pwauth",
        "--user-data-inline",
        user_data,
        action="create",
    )
    harness.start(name)
    seed = (harness.machine_dir(name) / "seed" / "user-data").read_text(encoding="utf-8")
    require("chpasswd" in seed, "the rendered seed carries no chpasswd list")
    require(AUTH_PASSWORD in seed, "the rendered seed does not carry the configured password")
    require("ssh_pwauth: true" in seed, "the rendered seed did not enable password auth")
    seed_mode = stat.S_IMODE((harness.machine_dir(name) / "seed" / "user-data").stat().st_mode)
    require(seed_mode == 0o600, f"the rendered seed is mode {seed_mode:04o}, expected 0600")
    marker = harness.guest(name, f"cat {USER_DATA_MARKER_PATH}").strip()
    require(marker == USER_DATA_MARKER, f"the inline user-data marker is {marker!r}")
    guest_pwauth = harness.guest(
        name,
        "sshd -T 2>/dev/null | grep -i '^passwordauthentication' || "
        "grep -rhi '^ *PasswordAuthentication' /etc/ssh/sshd_config /etc/ssh/sshd_config.d/ "
        "2>/dev/null | head -1",
    ).strip()
    sshpass = shutil.which("sshpass")
    password_login: dict[str, Any] = {"attempted": False, "reason": "sshpass is not installed"}
    if sshpass is not None:
        completed = harness.run(
            [
                sshpass,
                "-p",
                AUTH_PASSWORD,
                "ssh",
                "-o",
                f"ProxyCommand={harness.binary} _vsock-proxy {name} 22",
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-o",
                "PreferredAuthentications=password",
                "-o",
                "PubkeyAuthentication=no",
                "-o",
                "LogLevel=ERROR",
                f"root@firestone.{name}",
                "id -u",
            ],
            timeout=120,
            check=False,
        )
        require(
            completed.returncode == 0 and completed.stdout.strip() == b"0",
            "the password login over the vsock proxy failed: "
            f"{compact_bytes(completed.stdout + completed.stderr)}",
        )
        password_login = {"attempted": True, "uid": 0}
    harness.stop(name)
    return {
        "seed_carries_chpasswd": True,
        "seed_mode": f"{seed_mode:04o}",
        "user_data_marker": marker,
        "guest_password_authentication": guest_pwauth,
        "password_login": password_login,
    }


def scenario_prune(harness: Harness, machines: list[str], image_ids: set[str]) -> dict[str, Any]:
    target = harness.machine_dir(machines[-1])
    rotated = target / "console.log.previous"
    rotated.write_bytes(b"rotated console history\n" * 64)
    os.chmod(rotated, 0o600)
    partial = target / "harness.partial"
    partial.write_bytes(b"interrupted artifact\n" * 64)
    os.chmod(partial, 0o600)

    _, planned = harness.object_command(
        "system", "prune", "--dry-run", action="system-prune", timeout=300
    )
    require(planned["dry_run"] is True, "the dry run did not report dry_run")
    planned_rows = prune_rows(planned)
    require(
        rotated.is_file() and partial.is_file(),
        "the dry run deleted an artifact",
    )
    _, acted = harness.object_command("system", "prune", action="system-prune", timeout=300)
    require(acted["dry_run"] is False, "the real run reported dry_run")
    acted_rows = prune_rows(acted)
    require(
        planned_rows == acted_rows,
        f"dry run and real run disagree:\n  dry: {planned_rows}\n  act: {acted_rows}",
    )
    require(
        planned["reclaimed_bytes"] == acted["reclaimed_bytes"],
        "dry run and real run reclaimed different byte totals",
    )
    require(not rotated.exists(), "prune left the rotated console log")
    require(not partial.exists(), "prune left the .partial artifact")
    kinds = {kind for kind, _, _ in acted_rows}
    require({"log", "partial"} <= kinds, f"prune reported kinds {sorted(kinds)}")

    _, images = harness.object_command(
        "system", "prune", "--images", action="system-prune", timeout=600
    )
    removed_images = {row[1] for row in prune_rows(images) if row[0] == "image"}
    require(
        not (removed_images & image_ids),
        f"prune removed a referenced base image: {sorted(removed_images & image_ids)}",
    )
    _, stored = harness.json_command("images", "ls", action="images-ls")
    require(isinstance(stored, list), "images ls payload is not an array")
    surviving = {entry["metadata"]["id"] for entry in stored}
    require(
        image_ids <= surviving,
        f"a referenced image vanished: {sorted(image_ids - surviving)}",
    )
    return {
        "dry_run_rows": [list(row) for row in planned_rows],
        "act_rows": [list(row) for row in acted_rows],
        "reclaimed_bytes": acted["reclaimed_bytes"],
        "referenced_images_survived": sorted(image_ids),
    }


def scenario_prune_machines(harness: Harness) -> dict[str, Any]:
    _, planned = harness.object_command(
        "system", "prune", "--machines", "--dry-run", action="system-prune", timeout=300
    )
    planned_names = sorted(row[1] for row in prune_rows(planned) if row[0] == "machine")
    _, acted = harness.object_command(
        "system", "prune", "--machines", "--force", action="system-prune", timeout=600
    )
    acted_names = sorted(row[1] for row in prune_rows(acted) if row[0] == "machine")
    require(
        planned_names == acted_names,
        f"the machine tier's dry run listed {planned_names}, the run removed {acted_names}",
    )
    _, rows = harness.json_command("ls", action="list")
    require(isinstance(rows, list) and not rows, f"prune --machines left machines: {rows!r}")
    for name in acted_names:
        if name in harness.created_machines:
            harness.created_machines.remove(name)
    return {"dry_run_machines": planned_names, "removed_machines": acted_names}


# --------------------------------------------------------------------------


def run_acceptance(harness: Harness) -> None:
    require(sys.platform == "linux", "the M6 feature loop requires Linux")
    require(platform.machine() == "x86_64", "the M6 feature loop requires x86_64")
    kvm = Path("/dev/kvm")
    metadata = kvm.lstat()
    require(stat.S_ISCHR(metadata.st_mode), "the M6 loop requires a real /dev/kvm character device")
    descriptor = os.open(kvm, os.O_RDWR | os.O_CLOEXEC | getattr(os, "O_NOFOLLOW", 0))
    os.close(descriptor)
    for program in ("ssh", "scp", "qemu-img"):
        require(shutil.which(program) is not None, f"required host program is missing: {program}")

    forward_port = free_loopback_port()
    second_forward_port = free_loopback_port()
    require(forward_port != second_forward_port, "the two forward ports collided")

    harness.evidence["host"] = {
        "system": platform.system(),
        "release": platform.release(),
        "architecture": platform.machine(),
        "kvm_character_device": True,
        "python": platform.python_version(),
        "firestone_sha256": sha256(harness.binary),
        "harness_sha256": sha256(Path(__file__).resolve()),
        "image": harness.image,
        "initial_home_mode": "0700",
        "initial_home_empty": True,
    }

    harness.evidence["staged_helpers"] = {
        name: stage_pinned_helper(harness, name) for name in ("passt", "qemu-img")
    }
    harness.object_command("doctor", "--fix", action="doctor", timeout=DOCTOR_TIMEOUT_SECONDS)
    _, doctor = harness.object_command("doctor", action="doctor", timeout=DOCTOR_TIMEOUT_SECONDS)
    failures = [check for check in doctor["checks"] if check.get("status") == "fail"]
    require(not failures, f"doctor failed before the M6 loop: {failures!r}")
    harness.evidence["artifacts"] = installed_artifacts(harness)

    harness.created_machines.append(MACHINE)
    harness.object_command(
        "create",
        MACHINE,
        harness.image,
        "--cpus",
        "1",
        "--cpus-max",
        "4",
        "--memory",
        "1G",
        "--memory-max",
        "4G",
        "--disk",
        BASE_DISK,
        "--net",
        "passt",
        "-p",
        f"{forward_port}:{GUEST_HTTP_PORT}",
        action="create",
    )
    harness.start(MACHINE)
    image_ids = {harness.state(MACHINE)["image"]["id"]}

    scenarios = harness.evidence["scenarios"]
    server = Server(harness)
    try:
        scenarios["metrics"] = scenario_metrics(harness, server.endpoint, MACHINE)
        scenarios["resize"] = scenario_resize(harness, MACHINE)
        scenarios["cp"] = scenario_cp(harness, MACHINE)
    finally:
        server.stop()

    scenarios["terminals"] = scenario_terminals(harness, MACHINE)
    scenarios["warm_snapshot"] = scenario_warm_snapshot(harness, MACHINE, forward_port)
    scenarios["cold_snapshot"] = scenario_cold_snapshot(harness, MACHINE)

    server = Server(harness)
    try:
        scenarios["pending_forwards"] = scenario_pending_forwards(
            harness, server.endpoint, MACHINE, forward_port, second_forward_port
        )
        scenarios["disk_grow"] = scenario_disk_grow(harness, server.endpoint, MACHINE)
        scenarios["clone"] = scenario_clone(harness, server.endpoint, MACHINE, CLONE)
    finally:
        server.stop()

    harness.stop(MACHINE)
    harness.stop(CLONE)
    scenarios["cloud_init_auth"] = scenario_cloud_init_auth(harness, AUTH_MACHINE)
    scenarios["prune"] = scenario_prune(harness, [MACHINE, CLONE, AUTH_MACHINE], image_ids)
    scenarios["prune_machines"] = scenario_prune_machines(harness)


def install_harness_signal_handlers() -> None:
    def interrupted(signum: int, _frame: Any) -> None:
        raise AcceptanceError(f"the M6 feature harness was interrupted by signal {signum}")

    for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(signum, interrupted)


def main() -> int:
    if os.environ.get("FIRESTONE_E2E") != "1":
        print("skipped the M6 feature loop; set FIRESTONE_E2E=1 to run on Linux x86_64 KVM")
        return 0

    install_harness_signal_handlers()
    harness: Harness | None = None
    failure: str | None = None
    try:
        harness = Harness()
        run_acceptance(harness)
    except (AcceptanceError, OSError, ValueError, KeyError, TypeError) as error:
        failure = f"{type(error).__name__}: {error}"
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
        print(f"the M6 feature loop failed: {failure}", file=sys.stderr)
        return 1
    require(harness is not None, "the M6 feature harness was not initialized")
    print(f"the M6 feature loop passed; evidence: {harness.evidence_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
