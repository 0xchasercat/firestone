#!/usr/bin/env python3
"""Run the M6 OCI loop (E2E 13) against a real KVM host and a real registry.

The scenarios are the OCI path end to end: an anonymous Docker Hub pull with
progress and an `oci` sidecar, the pinned `mkfs.ext4`, `firestone-init` and
direct-boot kernel materialized on first use, a direct-kernel boot whose PID 1
is `firestone-init`, the SSH surfaces an OCI guest refuses, a served nginx
reached through a port forward, the documented force-path stop, a cached
re-pull, and a digest-pinned re-reference of the same manifest.
"""

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
    """The host or one M6 OCI contract failed."""


REPO_ROOT = Path(__file__).resolve().parents[1]
ALPINE_REFERENCE = "docker.io/library/alpine:3.20"
NGINX_REFERENCE = "docker.io/library/nginx:latest"
ALPINE_MACHINE = "oci-alpine"
PINNED_MACHINE = "oci-pinned"
NGINX_MACHINE = "oci-nginx"
COMMAND_TIMEOUT_SECONDS = 120
PULL_TIMEOUT_SECONDS = 1_800
START_TIMEOUT_SECONDS = 900
DOCTOR_TIMEOUT_SECONDS = 900
STOP_TIMEOUT_SECONDS = 15
STOP_BUDGET_SECONDS = 90
CONSOLE_WAIT_SECONDS = 180
FORWARD_WAIT_SECONDS = 120
MAX_OUTPUT_BYTES = 8 * 1024 * 1024
MAX_EVIDENCE_BYTES = 1024 * 1024
ALPINE_DISK = "6G"
NGINX_DISK = "8G"
INIT_PREFIX = "firestone-init: "
GROWN_PATTERN = "root filesystem grown to "
DIRECT_BOOT_CMDLINE = "console=hvc0 console=ttyS0 root=/dev/vda rw init=/sbin/firestone-init"


def require(condition: bool, message: str) -> None:
    if not condition:
        raise AcceptanceError(message)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while block := stream.read(1024 * 1024):
            digest.update(block)
    return digest.hexdigest()


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


def image_progress(records: list[dict[str, Any]]) -> list[tuple[int, int | None]]:
    """The `(done, total)` pairs of one pull's `image` progress events."""
    pairs: list[tuple[int, int | None]] = []
    for record in records:
        if record.get("type") != "Progress" or record.get("id") != "image":
            continue
        done, total = record.get("done"), record.get("total")
        require(isinstance(done, int), f"a progress event has no byte count: {record!r}")
        require(
            total is None or isinstance(total, int),
            f"a progress event has a non-integer total: {record!r}",
        )
        pairs.append((done, total))
    return pairs


def require_monotonic_progress(pairs: list[tuple[int, int | None]], label: str) -> None:
    """Progress never goes backwards and never passes its declared total."""
    require(bool(pairs), f"{label} emitted no progress events")
    previous = -1
    for done, total in pairs:
        require(done >= previous, f"{label} progress went backwards at {done}")
        if total is not None:
            require(done <= total, f"{label} reported {done} of {total} bytes")
        previous = done


def step_reasons(records: list[dict[str, Any]], step: str) -> list[str]:
    """Every `StepSkip` reason emitted for one step id."""
    return [
        str(record.get("reason", ""))
        for record in records
        if record.get("type") == "StepSkip" and record.get("id") == step
    ]


def oci_sidecar_problems(
    sidecar: dict[str, Any],
    reference: str,
) -> list[str]:
    """Everything SPEC §8.5 fixes about an OCI sidecar that this one gets wrong."""
    problems: list[str] = []
    if sidecar.get("kind") != "oci":
        problems.append(f"kind is {sidecar.get('kind')!r}, expected 'oci'")
    if sidecar.get("source_ref") != reference:
        problems.append(f"source_ref is {sidecar.get('source_ref')!r}, expected {reference!r}")
    if sidecar.get("source_url") is not None:
        problems.append("source_url is not null")
    if sidecar.get("firmware") is not None:
        problems.append("firmware is not null")
    if sidecar.get("source_format") != "raw":
        problems.append(f"source_format is {sidecar.get('source_format')!r}, expected 'raw'")
    if sidecar.get("verification_algorithm") != "sha256":
        problems.append("verification_algorithm is not sha256")
    if sidecar.get("verification_digest") != sidecar.get("source_sha256"):
        problems.append("verification_digest does not equal source_sha256")
    oci = sidecar.get("oci")
    if not isinstance(oci, dict):
        problems.append("the sidecar carries no oci object")
        return problems
    required = {
        "registry_ref",
        "manifest_digest",
        "config_digest",
        "entrypoint",
        "cmd",
        "env",
        "workdir",
        "user",
        "boot",
    }
    if set(oci) != required:
        problems.append(f"the oci object keys are {sorted(oci)}, expected {sorted(required)}")
        return problems
    if oci["registry_ref"] != reference:
        problems.append(f"oci.registry_ref is {oci['registry_ref']!r}, expected {reference!r}")
    if oci["boot"] != "firestone-init":
        problems.append(f"oci.boot is {oci['boot']!r}, expected 'firestone-init'")
    for key in ("entrypoint", "cmd", "env"):
        if not isinstance(oci[key], list) or not all(
            isinstance(entry, str) for entry in oci[key]
        ):
            problems.append(f"oci.{key} is not an array of strings")
    for key in ("workdir", "user"):
        if oci[key] is not None and not isinstance(oci[key], str):
            problems.append(f"oci.{key} is neither a string nor null")
    digest = oci["manifest_digest"]
    if not isinstance(digest, str) or not digest.startswith("sha256:") or len(digest) != 71:
        problems.append(f"oci.manifest_digest is not a sha256 digest: {digest!r}")
    elif sidecar.get("source_sha256") != digest[len("sha256:") :]:
        problems.append("source_sha256 is not the manifest digest's hex")
    return problems


def grown_blocks(console: str) -> int | None:
    """The block count `firestone-init` reported growing the root filesystem to."""
    marker = INIT_PREFIX + GROWN_PATTERN
    for line in console.splitlines():
        index = line.find(marker)
        if index < 0:
            continue
        rest = line[index + len(marker) :].split()
        if rest and rest[0].isdigit():
            return int(rest[0])
    return None


def init_lines(console: str) -> list[str]:
    """Every `firestone-init` line the guest console carries, in order."""
    found: list[str] = []
    for line in console.splitlines():
        index = line.find(INIT_PREFIX)
        if index >= 0:
            found.append(line[index:].rstrip("\r"))
    return found


def parse_http_response(response: bytes) -> tuple[int, dict[str, str], bytes]:
    header_end = response.find(b"\r\n\r\n")
    require(header_end >= 0, "HTTP response has no header terminator")
    try:
        lines = response[:header_end].decode("ascii").split("\r\n")
    except UnicodeDecodeError as error:
        raise AcceptanceError(f"HTTP headers are not ASCII: {error}") from error
    parts = lines[0].split()
    require(len(parts) >= 2 and parts[1].isdigit(), "HTTP response has no status code")
    headers: dict[str, str] = {}
    for line in lines[1:]:
        name, separator, value = line.partition(":")
        require(separator == ":", f"malformed HTTP header: {line!r}")
        headers[name.lower()] = value.strip()
    body = response[header_end + 4 :]
    if "content-length" in headers:
        length = int(headers["content-length"])
        require(len(body) >= length, "HTTP body is shorter than Content-Length")
        body = body[:length]
    return int(parts[1]), headers, body


# --------------------------------------------------------------------------


class Harness:
    def __init__(self) -> None:
        self.home = self._checked_home()
        self.keep_home = os.environ.get("FIRESTONE_E2E_KEEP") == "1"
        default_evidence = Path("/tmp") / f"firestone-m6-oci-{os.getpid()}.json"
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
        self.commands: list[str] = []
        self.created_machines: list[str] = []
        self.evidence: dict[str, Any] = {
            "schema": 1,
            "scenario": "e2e13-m6-oci-loop",
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

    def json_command(
        self,
        *arguments: str,
        action: str,
        timeout: float = COMMAND_TIMEOUT_SECONDS,
        expected_code: int = 0,
    ) -> tuple[list[dict[str, Any]], Any]:
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
        if expected_code == 0:
            require(terminal.get("type") == "Result", f"{label} did not end with Result")
            require(terminal.get("action") == action, f"{label} Result action is not {action}")
            return records, terminal.get("payload")
        require("error" in terminal, f"{label} did not end with an error record")
        return records, terminal["error"]

    def object_command(
        self, *arguments: str, action: str, timeout: float = COMMAND_TIMEOUT_SECONDS
    ) -> tuple[list[dict[str, Any]], dict[str, Any]]:
        records, payload = self.json_command(*arguments, action=action, timeout=timeout)
        require(isinstance(payload, dict), f"{action} payload is not an object")
        return records, payload

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

    def console(self, name: str) -> str:
        path = self.machine_dir(name) / "console.log"
        try:
            return path.read_text(encoding="utf-8", errors="replace")
        except OSError as error:
            raise AcceptanceError(f"cannot read the console log for {name}: {error}") from error

    def sidecar(self, image_id: str) -> dict[str, Any]:
        path = self.home / "data" / "images" / f"{image_id}.json"
        try:
            value = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            raise AcceptanceError(f"cannot read the sidecar for {image_id}: {error}") from error
        require(isinstance(value, dict), f"the sidecar for {image_id} is not an object")
        return value

    def start(self, name: str) -> dict[str, Any]:
        _, payload = self.object_command(
            "start",
            name,
            "--timeout",
            f"{START_TIMEOUT_SECONDS}s",
            action="start",
            timeout=START_TIMEOUT_SECONDS + 120,
        )
        require(payload.get("status") == "running", f"{name} did not reach running")
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
    plain `cargo build` does not carry. The OCI helpers are deliberately *not*
    staged: materializing them on first use is one of the contracts under test.
    """
    import urllib.request

    artifact = dependency_artifact(name)
    require(artifact["url"].startswith("https://"), f"{name} is not pinned over HTTPS")
    cached = helper_cache_dir() / artifact["install_name"]
    if not cached.is_file() or sha256(cached) != artifact["sha256"]:
        partial = cached.with_name(f".{cached.name}.{os.getpid()}.partial")
        request = urllib.request.Request(  # noqa: S310 - a pinned HTTPS release asset
            artifact["url"], headers={"Accept-Encoding": "identity"}
        )
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
    return {
        "version": artifact["version"],
        "install_name": artifact["install_name"],
        "sha256": artifact["sha256"],
    }


def require_materialized(harness: Harness, name: str, expected_mode: int) -> dict[str, Any]:
    """One pinned artifact Firestone installed for itself, verified in place."""
    artifact = dependency_artifact(name)
    path = harness.home / "data" / "bin" / artifact["install_name"]
    require(path.is_file(), f"{name} was not materialized at {path}")
    metadata = path.stat()
    mode = stat.S_IMODE(metadata.st_mode)
    require(mode == expected_mode, f"{name} is mode {mode:04o}, expected {expected_mode:04o}")
    actual = sha256(path)
    require(actual == artifact["sha256"], f"the materialized {name} is not the pinned artifact")
    return {
        "version": artifact["version"],
        "install_name": artifact["install_name"],
        "sha256": actual,
        "mode": f"{mode:04o}",
        "bytes": metadata.st_size,
    }


def http_get(port: int, timeout: float) -> tuple[int, bytes]:
    deadline = time.monotonic() + timeout
    last = "no attempt"
    while time.monotonic() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=10) as client:
                client.settimeout(10)
                client.sendall(
                    b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n"
                    b"User-Agent: firestone-e2e\r\n\r\n"
                )
                response = bytearray()
                while True:
                    block = client.recv(65_536)
                    if not block:
                        break
                    response.extend(block)
                    require(len(response) <= MAX_OUTPUT_BYTES, "HTTP response exceeded 8 MiB")
            status, _, body = parse_http_response(bytes(response))
            if status == 200:
                return status, body
            last = f"status {status}"
        except (AcceptanceError, OSError) as error:
            last = str(error)
        time.sleep(2.0)
    raise AcceptanceError(f"the forwarded port {port} never answered 200: {last}")


def wait_for_console(harness: Harness, name: str, needle: str, timeout: float) -> str:
    deadline = time.monotonic() + timeout
    console = ""
    while time.monotonic() < deadline:
        console = harness.console(name)
        if needle in console:
            return console
        time.sleep(2.0)
    raise AcceptanceError(
        f"machine `{name}` console never carried {needle!r}; "
        f"firestone-init said {init_lines(console)!r}"
    )


# --------------------------------------------------------------------------
# Scenarios
# --------------------------------------------------------------------------


def scenario_pull(harness: Harness, reference: str) -> dict[str, Any]:
    records, payload = harness.object_command(
        "images", "pull", reference, action="images-pull", timeout=PULL_TIMEOUT_SECONDS
    )
    require(payload.get("cached") is False, f"the first pull of {reference} reported a cache hit")
    require(payload.get("firmware") is None, "an OCI image selected a firmware")
    metadata = payload.get("metadata")
    require(isinstance(metadata, dict), "the pull result carries no image metadata")
    pairs = image_progress(records)
    require_monotonic_progress(pairs, f"the {reference} pull")
    image_id = metadata["id"]
    sidecar = harness.sidecar(image_id)
    problems = oci_sidecar_problems(sidecar, reference)
    require(not problems, f"the {reference} sidecar violates SPEC 8.5: {problems}")
    stored = harness.home / "data" / "images" / f"{image_id}.qcow2"
    require(stored.is_file(), f"the published image {stored} is missing")
    require(
        sha256(stored) == sidecar["stored_sha256"],
        "the stored image does not match its sidecar checksum",
    )
    require(
        stat.S_IMODE(stored.stat().st_mode) == 0o400,
        "the published OCI base image is not mode 0400",
    )

    cached_records, cached_payload = harness.object_command(
        "images", "pull", reference, action="images-pull", timeout=PULL_TIMEOUT_SECONDS
    )
    require(cached_payload.get("cached") is True, "the second pull did not report a cache hit")
    require(
        cached_payload["metadata"]["id"] == image_id,
        "the cached pull published a different image id",
    )
    reasons = step_reasons(cached_records, "image")
    require("cached" in reasons, f"the cached pull did not skip the image step: {reasons!r}")
    return {
        "reference": reference,
        "image_id": image_id,
        "manifest_digest": sidecar["oci"]["manifest_digest"],
        "config_digest": sidecar["oci"]["config_digest"],
        "entrypoint": sidecar["oci"]["entrypoint"],
        "cmd": sidecar["oci"]["cmd"],
        "user": sidecar["oci"]["user"],
        "workdir": sidecar["oci"]["workdir"],
        "boot": sidecar["oci"]["boot"],
        "size": metadata["size"],
        "stored_sha256": sidecar["stored_sha256"],
        "progress_events": len(pairs),
        "progress_total": pairs[-1][1],
        "cached_on_repull": True,
    }


def scenario_alpine_boot(harness: Harness, name: str, reference: str) -> dict[str, Any]:
    harness.created_machines.append(name)
    harness.object_command(
        "create",
        name,
        reference,
        "--net",
        "none",
        "--cpus",
        "1",
        "--memory",
        "1G",
        "--disk",
        ALPINE_DISK,
        action="create",
    )
    harness.start(name)
    started = harness.state(name)
    vmconfig = json.loads((harness.machine_dir(name) / "vmconfig.json").read_text())
    payload = vmconfig.get("payload", {})
    require(
        payload.get("cmdline") == DIRECT_BOOT_CMDLINE,
        f"the direct-boot command line is {payload.get('cmdline')!r}",
    )
    require("firmware" not in payload, "an OCI machine published a firmware payload")
    require(
        payload.get("kernel", "").endswith(dependency_artifact("cloud-hypervisor-kernel")["install_name"]),
        f"the OCI machine did not boot the pinned kernel: {payload.get('kernel')!r}",
    )
    disks = vmconfig.get("disks", [])
    require(len(disks) == 2, f"an OCI machine published {len(disks)} disks, expected 2")
    require(
        disks[1].get("image_type") == "Raw" and disks[1].get("readonly") is True,
        f"the config disk slot is {disks[1]!r}",
    )
    require(
        Path(disks[1]["path"]).name == "config.img",
        "the seed slot does not carry config.img",
    )

    console = wait_for_console(harness, name, INIT_PREFIX, CONSOLE_WAIT_SECONDS)
    require(
        "Run /sbin/firestone-init as init process" in console,
        "the kernel did not run the injected /sbin/firestone-init",
    )
    blocks = grown_blocks(console)
    require(
        blocks is not None and blocks * 4096 > 4 * 1024**3,
        f"firestone-init did not grow the root filesystem to the machine's disk: {blocks!r}",
    )
    started_line = wait_for_console(harness, name, "started `/bin/sh` as pid", CONSOLE_WAIT_SECONDS)
    require("started `/bin/sh` as pid" in started_line, "firestone-init never started the entrypoint")

    # SPEC 11.8: an OCI guest has no sshd, so both SSH surfaces refuse up front.
    for arguments, action in (
        (["shell", name, "--", "true"], "shell"),
        (["ssh-config", name], "ssh-config"),
    ):
        refusal = harness.run([harness.binary, *arguments], check=False, timeout=60)
        require(
            refusal.returncode == 2,
            f"{action} on an OCI machine exited {refusal.returncode}, expected 2",
        )
        message = refusal.stderr.decode("utf-8", "replace")
        require(
            "sshd" in message and "console" in message,
            f"{action} did not name the missing sshd and the console: {message!r}",
        )

    # SPEC verify 25: no ACPI handler answers the power button, so the stop
    # falls through its timeout to the force path and still lands.
    began = time.monotonic()
    _, stopped = harness.object_command(
        "stop",
        name,
        "--timeout",
        f"{STOP_TIMEOUT_SECONDS}s",
        action="stop",
        timeout=STOP_BUDGET_SECONDS,
    )
    elapsed = time.monotonic() - began
    require(stopped.get("status") == "stopped", "the OCI machine did not stop")
    require(
        elapsed <= STOP_BUDGET_SECONDS,
        f"the OCI stop took {elapsed:.1f}s, beyond the force-path budget",
    )
    exit_record = harness.state(name)["last_exit"]
    require(
        exit_record["reason"] == "graceful stop timed out",
        f"the OCI stop recorded {exit_record!r}",
    )
    return {
        "machine": name,
        "cmdline": payload["cmdline"],
        "kernel": Path(payload["kernel"]).name,
        "config_disk": disks[1],
        "grown_blocks": blocks,
        "grown_bytes": blocks * 4096,
        "init_lines": init_lines(console)[:8],
        "ssh_surfaces_refused": ["shell", "ssh-config"],
        "stop_seconds": round(elapsed, 3),
        "last_exit": exit_record,
        "instance_id": started["instance_id"],
    }


def scenario_digest_reference(
    harness: Harness, name: str, tagged: dict[str, Any]
) -> dict[str, Any]:
    reference = f"docker.io/library/alpine@{tagged['manifest_digest']}"
    harness.created_machines.append(name)
    harness.object_command(
        "create",
        name,
        reference,
        "--net",
        "none",
        "--cpus",
        "1",
        "--memory",
        "1G",
        "--disk",
        ALPINE_DISK,
        action="create",
    )
    harness.start(name)
    state = harness.state(name)
    image_id = state["image"]["id"]
    sidecar = harness.sidecar(image_id)
    problems = oci_sidecar_problems(sidecar, reference)
    require(not problems, f"the digest-pinned sidecar violates SPEC 8.5: {problems}")
    require(
        sidecar["oci"]["manifest_digest"] == tagged["manifest_digest"],
        "the digest-pinned reference selected a different manifest",
    )
    require(
        sidecar["stored_sha256"] == tagged["stored_sha256"],
        "the digest-pinned pull packed different bytes than the tagged pull",
    )
    # SPEC 8.5: the stable id hashes the canonical reference too, so a digest
    # reference is its own image even when it packs identical bytes.
    require(image_id != tagged["image_id"], "the digest reference reused the tagged image id")
    console = wait_for_console(harness, name, "started `/bin/sh` as pid", CONSOLE_WAIT_SECONDS)
    require(INIT_PREFIX in console, "the digest-pinned machine did not run firestone-init")
    harness.object_command(
        "stop",
        name,
        "--timeout",
        f"{STOP_TIMEOUT_SECONDS}s",
        action="stop",
        timeout=STOP_BUDGET_SECONDS,
    )
    return {
        "reference": reference,
        "image_id": image_id,
        "tagged_image_id": tagged["image_id"],
        "manifest_digest": sidecar["oci"]["manifest_digest"],
        "stored_sha256": sidecar["stored_sha256"],
        "identical_bytes": True,
        "distinct_stable_id": True,
    }


def scenario_nginx(harness: Harness, name: str, reference: str, port: int) -> dict[str, Any]:
    pull = scenario_pull(harness, reference)
    require(
        pull["entrypoint"] or pull["cmd"],
        "the nginx image declares neither an entrypoint nor a command",
    )
    harness.created_machines.append(name)
    harness.object_command(
        "create",
        name,
        reference,
        "--net",
        "passt",
        "-p",
        f"{port}:80",
        "--cpus",
        "1",
        "--memory",
        "1G",
        "--disk",
        NGINX_DISK,
        action="create",
    )
    harness.start(name)
    console = wait_for_console(harness, name, "eth0 configured with ", CONSOLE_WAIT_SECONDS)
    address = ""
    for line in init_lines(console):
        if "eth0 configured with " in line:
            address = line.rsplit(" ", 1)[-1]
    require(bool(address), "firestone-init reported no DHCP address")
    status, body = http_get(port, FORWARD_WAIT_SECONDS)
    require(status == 200, f"the forwarded nginx answered {status}")
    text = body.decode("utf-8", "replace")
    require(
        "Welcome to nginx!" in text,
        f"the forward did not serve the nginx welcome page: {text[:200]!r}",
    )
    logs = harness.run(
        [harness.binary, "logs", name, "--source", "console", "-n", "500"],
        timeout=60,
    ).stdout.decode("utf-8", "replace")
    require(
        "/docker-entrypoint.sh" in logs,
        "the console log carries no entrypoint output",
    )
    blocks = grown_blocks(console)
    require(
        blocks is not None and blocks * 4096 > 6 * 1024**3,
        f"firestone-init did not grow nginx's root filesystem: {blocks!r}",
    )
    began = time.monotonic()
    _, stopped = harness.object_command(
        "stop",
        name,
        "--timeout",
        f"{STOP_TIMEOUT_SECONDS}s",
        action="stop",
        timeout=STOP_BUDGET_SECONDS,
    )
    elapsed = time.monotonic() - began
    require(stopped.get("status") == "stopped", "the nginx machine did not stop")
    exit_record = harness.state(name)["last_exit"]
    return {
        "pull": pull,
        "machine": name,
        "forward": f"{port}:80",
        "dhcp_address": address,
        "welcome_page": True,
        "response_bytes": len(body),
        "entrypoint_output": True,
        "grown_blocks": blocks,
        "stop_seconds": round(elapsed, 3),
        "last_exit": exit_record,
    }


def scenario_prune(harness: Harness) -> dict[str, Any]:
    _, planned = harness.object_command(
        "system", "prune", "--machines", "--dry-run", action="system-prune", timeout=300
    )
    planned_names = sorted(
        row["id"] for row in planned["removed"] if row.get("kind") == "machine"
    )
    _, acted = harness.object_command(
        "system", "prune", "--machines", "--force", action="system-prune", timeout=600
    )
    acted_names = sorted(row["id"] for row in acted["removed"] if row.get("kind") == "machine")
    require(
        planned_names == acted_names,
        f"the machine tier planned {planned_names} and removed {acted_names}",
    )
    for name in acted_names:
        if name in harness.created_machines:
            harness.created_machines.remove(name)
    _, rows = harness.json_command("ls", action="list")
    require(isinstance(rows, list) and not rows, f"prune --machines left machines: {rows!r}")
    return {"removed_machines": acted_names}


# --------------------------------------------------------------------------


def run_acceptance(harness: Harness) -> None:
    require(sys.platform == "linux", "the M6 OCI loop requires Linux")
    require(platform.machine() == "x86_64", "the M6 OCI loop requires x86_64")
    kvm = Path("/dev/kvm")
    metadata = kvm.lstat()
    require(stat.S_ISCHR(metadata.st_mode), "the M6 OCI loop requires a real /dev/kvm")
    descriptor = os.open(kvm, os.O_RDWR | os.O_CLOEXEC | getattr(os, "O_NOFOLLOW", 0))
    os.close(descriptor)

    harness.evidence["host"] = {
        "system": platform.system(),
        "release": platform.release(),
        "architecture": platform.machine(),
        "kvm_character_device": True,
        "python": platform.python_version(),
        "firestone_sha256": sha256(harness.binary),
        "harness_sha256": sha256(Path(__file__).resolve()),
        "registry": "docker.io (anonymous)",
        "initial_home_mode": "0700",
        "initial_home_empty": True,
    }
    harness.evidence["staged_helpers"] = {
        name: stage_pinned_helper(harness, name) for name in ("passt", "qemu-img")
    }
    harness.object_command("doctor", "--fix", action="doctor", timeout=DOCTOR_TIMEOUT_SECONDS)
    _, doctor = harness.object_command("doctor", action="doctor", timeout=DOCTOR_TIMEOUT_SECONDS)
    failures = [check for check in doctor["checks"] if check.get("status") == "fail"]
    require(not failures, f"doctor failed before the M6 OCI loop: {failures!r}")

    # The OCI helpers are not staged: the pull and the start must install them.
    for name in ("mkfs-ext4", "firestone-init"):
        path = harness.home / "data" / "bin" / dependency_artifact(name)["install_name"]
        require(not path.exists(), f"{name} was present before the first OCI use")

    scenarios = harness.evidence["scenarios"]
    alpine = scenario_pull(harness, ALPINE_REFERENCE)
    harness.evidence["artifacts"] = {
        # `mkfs.ext4` is run on the host, so it is published executable; the
        # `firestone-init` payload and the direct-boot kernel are data the host
        # only copies, so both are published 0644 (SPEC 17.2).
        "mkfs-ext4": require_materialized(harness, "mkfs-ext4", 0o755),
        "firestone-init": require_materialized(harness, "firestone-init", 0o644),
    }
    scenarios["alpine_pull"] = alpine
    scenarios["alpine_boot"] = scenario_alpine_boot(harness, ALPINE_MACHINE, ALPINE_REFERENCE)
    harness.evidence["artifacts"]["cloud-hypervisor-kernel"] = require_materialized(
        harness, "cloud-hypervisor-kernel", 0o644
    )
    scenarios["digest_reference"] = scenario_digest_reference(harness, PINNED_MACHINE, alpine)
    scenarios["nginx"] = scenario_nginx(
        harness, NGINX_MACHINE, NGINX_REFERENCE, free_loopback_port()
    )
    scenarios["prune"] = scenario_prune(harness)


def install_harness_signal_handlers() -> None:
    def interrupted(signum: int, _frame: Any) -> None:
        raise AcceptanceError(f"the M6 OCI harness was interrupted by signal {signum}")

    for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(signum, interrupted)


def main() -> int:
    if os.environ.get("FIRESTONE_E2E") != "1":
        print("skipped the M6 OCI loop; set FIRESTONE_E2E=1 to run on Linux x86_64 KVM")
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
        print(f"the M6 OCI loop failed: {failure}", file=sys.stderr)
        return 1
    require(harness is not None, "the M6 OCI harness was not initialized")
    print(f"the M6 OCI loop passed; evidence: {harness.evidence_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
