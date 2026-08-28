#!/usr/bin/env python3
"""Run the Linux x86_64 M1 acceptance scenarios against real KVM."""

from __future__ import annotations

import atexit
import base64
import hashlib
import json
import os
import platform
import re
import shlex
import shutil
import signal
import select
import socket
import stat
import subprocess
import sys
import time
import tomllib
import uuid
from pathlib import Path
from typing import Any


class AcceptanceError(RuntimeError):
    """The host or one M1 acceptance contract failed."""


REPO_ROOT = Path(__file__).resolve().parents[1]
COMMAND_TIMEOUT_SECONDS = 120
START_TIMEOUT_SECONDS = 1_900
BOOT_TIMEOUT_SECONDS = 300
MAX_CONSOLE_BYTES = 8 * 1024 * 1024
MACHINE_NAMES = (
    "m1-graceful",
    "m1-convert",
    "m1-vmm-crash",
    "m1-shim-crash",
)


def require(condition: bool, message: str) -> None:
    if not condition:
        raise AcceptanceError(message)


def compact_output(value: str, limit: int = 8_192) -> str:
    if len(value) <= limit:
        return value
    return f"[... {len(value) - limit} bytes omitted ...]\n{value[-limit:]}"


class Harness:
    def __init__(self) -> None:
        self.home = self._checked_home()
        self.keep_home = os.environ.get("FIRESTONE_E2E_KEEP") == "1"
        default_evidence = Path("/tmp") / f"firestone-m1-evidence-{os.getpid()}.json"
        self.evidence_path = Path(
            os.environ.get("FIRESTONE_E2E_EVIDENCE", default_evidence)
        ).expanduser()
        if not self.evidence_path.is_absolute():
            self.evidence_path = (Path.cwd() / self.evidence_path).resolve()
        self.commands: list[str] = []
        self.created_machines: list[str] = []
        self.evidence: dict[str, Any] = {
            "schema": 1,
            "result": "running",
            "commands": self.commands,
            "host": {},
            "artifacts": {},
            "scenarios": {},
        }
        self.binary = self._binary_path()
        self._cleanup_started = False
        atexit.register(self.cleanup)

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
        require(home.exists() and home.is_dir(), "FIRESTONE_HOME must be an existing directory")
        home = home.resolve(strict=True)
        metadata = home.stat()
        require(metadata.st_uid == os.getuid(), "FIRESTONE_HOME must be owned by the current user")
        require(
            stat.S_IMODE(metadata.st_mode) == 0o700,
            "FIRESTONE_HOME must have mode 0700",
        )
        require(not any(home.iterdir()), "FIRESTONE_HOME must be empty")
        return home

    def _binary_path(self) -> Path:
        configured = os.environ.get("FIRESTONE_BIN")
        if configured:
            binary = Path(configured).expanduser()
            if not binary.is_absolute():
                binary = (Path.cwd() / binary).resolve()
            require(binary.is_file(), f"FIRESTONE_BIN is not a file: {binary}")
            require(os.access(binary, os.X_OK), f"FIRESTONE_BIN is not executable: {binary}")
            return binary
        return REPO_ROOT / "target" / "debug" / "firestone"

    def run(
        self,
        argv: list[str | os.PathLike[str]],
        *,
        timeout: int = COMMAND_TIMEOUT_SECONDS,
        check: bool = True,
        record: bool = True,
    ) -> subprocess.CompletedProcess[str]:
        command = [os.fspath(value) for value in argv]
        rendered = shlex.join(command)
        if record:
            self.commands.append(rendered)
        print(f"+ {rendered}", flush=True)
        try:
            completed = subprocess.run(
                command,
                cwd=REPO_ROOT,
                env=os.environ.copy(),
                capture_output=True,
                text=True,
                timeout=timeout,
                check=False,
            )
        except subprocess.TimeoutExpired as error:
            raise AcceptanceError(f"command timed out after {timeout}s: {rendered}") from error
        if check and completed.returncode != 0:
            raise AcceptanceError(
                f"command failed with exit {completed.returncode}: {rendered}\n"
                f"stdout:\n{compact_output(completed.stdout)}\n"
                f"stderr:\n{compact_output(completed.stderr)}"
            )
        return completed

    def firestone(
        self,
        *arguments: str,
        timeout: int = COMMAND_TIMEOUT_SECONDS,
        check: bool = True,
        record: bool = True,
    ) -> list[dict[str, Any]]:
        completed = self.run(
            [self.binary, "--json", *arguments],
            timeout=timeout,
            check=check,
            record=record,
        )
        if not check and completed.returncode != 0:
            return []
        require(not completed.stderr, f"JSON command wrote stderr: {compact_output(completed.stderr)}")
        try:
            records = [json.loads(line) for line in completed.stdout.splitlines() if line]
        except json.JSONDecodeError as error:
            raise AcceptanceError(
                f"command returned invalid NDJSON: {compact_output(completed.stdout)}"
            ) from error
        require(records, "JSON command returned no records")
        return records

    @staticmethod
    def result_payload(records: list[dict[str, Any]], action: str) -> Any:
        terminal = records[-1]
        require(terminal.get("type") == "Result", f"{action} did not end with Result")
        require(terminal.get("action") == action, f"expected {action} Result, got {terminal!r}")
        require("payload" in terminal, f"{action} Result has no payload")
        return terminal["payload"]

    def create(self, name: str, image: Path | str, *extra: str) -> dict[str, Any]:
        records = self.firestone(
            "create",
            name,
            os.fspath(image),
            "--net",
            "none",
            *extra,
        )
        payload = self.result_payload(records, "create")
        self.created_machines.append(name)
        require(payload["state"]["status"] == "created", f"{name} was not created")
        require(payload["spec"]["network"]["mode"] == "none", f"{name} enabled networking")
        return payload

    def start(self, name: str) -> tuple[dict[str, Any], list[dict[str, Any]]]:
        records = self.firestone(
            "start",
            name,
            "--no-wait",
            "--timeout",
            "600s",
            timeout=START_TIMEOUT_SECONDS,
        )
        payload = self.result_payload(records, "start")
        require(payload["status"] == "running", f"{name} did not reach running")
        return payload, records

    def stop(self, name: str, *, force: bool = False) -> tuple[dict[str, Any], list[dict[str, Any]]]:
        arguments = ["stop", name, "--timeout", "90s"]
        if force:
            arguments.append("--force")
        records = self.firestone(*arguments, timeout=130)
        payload = self.result_payload(records, "stop")
        require(payload["status"] == "stopped", f"{name} did not stop")
        return payload, records

    def state(self, name: str) -> dict[str, Any]:
        path = self.home / "data" / "machines" / name / "state.json"
        try:
            return json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            raise AcceptanceError(f"cannot read state for {name}: {error}") from error

    def vmconfig(self, name: str) -> dict[str, Any]:
        path = self.home / "data" / "machines" / name / "vmconfig.json"
        try:
            return json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            raise AcceptanceError(f"cannot read VmConfig for {name}: {error}") from error

    def list_status(self, name: str) -> str:
        records = self.firestone("ls")
        payload = self.result_payload(records, "list")
        matches = [machine for machine in payload if machine.get("name") == name]
        require(len(matches) == 1, f"list did not return exactly one {name} machine")
        status_value = matches[0].get("status")
        require(isinstance(status_value, str), f"list returned an invalid status for {name}")
        return status_value

    def wait_for_status(self, name: str, expected: str, timeout: float) -> float:
        started = time.monotonic()
        deadline = started + timeout
        last = ""
        while time.monotonic() < deadline:
            last = self.list_status(name)
            if last == expected:
                return time.monotonic() - started
            time.sleep(0.025)
        raise AcceptanceError(
            f"{name} did not report {expected!r} within {timeout:.3f}s; last status was {last!r}"
        )

    def console_tail(self, name: str) -> str:
        path = self.home / "data" / "machines" / name / "console.log"
        try:
            with path.open("rb") as stream:
                size = stream.seek(0, os.SEEK_END)
                stream.seek(max(0, size - MAX_CONSOLE_BYTES))
                return stream.read().decode("utf-8", errors="replace")
        except OSError as error:
            raise AcceptanceError(f"cannot read console log for {name}: {error}") from error

    def wait_console_match(
        self,
        name: str,
        patterns: tuple[re.Pattern[str], ...],
        *,
        timeout: int = BOOT_TIMEOUT_SECONDS,
        minimum_matches: int = 1,
    ) -> str:
        deadline = time.monotonic() + timeout
        last_tail = ""
        while time.monotonic() < deadline:
            last_tail = self.console_tail(name)
            matches: list[str] = []
            for line in last_tail.splitlines():
                if any(pattern.search(line) for pattern in patterns):
                    matches.append(line.strip())
            if len(matches) >= minimum_matches:
                return matches[-1]
            time.sleep(0.25)
        raise AcceptanceError(
            f"{name} console did not contain the required marker within {timeout}s; "
            f"tail:\n{compact_output(last_tail, 4_096)}"
        )

    def unix_http(self, name: str, method: str, path: str) -> tuple[int, bytes]:
        api_socket = self.home / "run" / name / "api.sock"
        request = (
            f"{method} {path} HTTP/1.1\r\n"
            "Host: localhost\r\n"
            "Accept: application/json\r\n"
            "Content-Length: 0\r\n"
            "\r\n"
        ).encode("ascii")
        client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        client.settimeout(5)
        try:
            client.connect(os.fspath(api_socket))
            client.sendall(request)
            response = bytearray()
            while b"\r\n\r\n" not in response:
                block = client.recv(4_096)
                require(block != b"", f"{path} closed before its response headers")
                response.extend(block)
                require(len(response) <= 16 * 1_024, f"{path} response headers exceeded 16 KiB")
            raw_headers, body = bytes(response).split(b"\r\n\r\n", 1)
            lines = raw_headers.split(b"\r\n")
            status_match = re.fullmatch(rb"HTTP/1\.1 ([0-9]{3}) .*", lines[0])
            require(status_match is not None, f"{path} returned a malformed status line")
            lengths = []
            for line in lines[1:]:
                name_bytes, separator, value = line.partition(b":")
                require(separator == b":", f"{path} returned a malformed header")
                if name_bytes.lower() == b"content-length":
                    lengths.append(int(value.strip()))
            require(len(lengths) == 1, f"{path} did not return one Content-Length")
            while len(body) < lengths[0]:
                block = client.recv(min(65_536, lengths[0] - len(body)))
                require(block != b"", f"{path} closed before its response body")
                body += block
            require(len(body) == lengths[0], f"{path} response body length was inconsistent")
            return int(status_match.group(1)), body
        finally:
            client.close()

    def write_evidence(self) -> None:
        self.evidence_path.parent.mkdir(parents=True, exist_ok=True)
        temporary = self.evidence_path.with_name(f".{self.evidence_path.name}.tmp")
        temporary.write_text(
            json.dumps(self.evidence, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        os.chmod(temporary, 0o600)
        os.replace(temporary, self.evidence_path)

    def cleanup(self) -> None:
        if self._cleanup_started:
            return
        self._cleanup_started = True
        for name in reversed(self.created_machines):
            self.run(
                [self.binary, "--json", "stop", name, "--force", "--timeout", "5s"],
                timeout=20,
                check=False,
                record=False,
            )
            self.run(
                [self.binary, "--json", "rm", name, "--force"],
                timeout=20,
                check=False,
                record=False,
            )
        if not self.keep_home:
            shutil.rmtree(self.home, ignore_errors=True)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while block := stream.read(1024 * 1024):
            digest.update(block)
    return digest.hexdigest()


def parse_ldd(output: str) -> tuple[list[Path], Path]:
    libraries: list[Path] = []
    loader: Path | None = None
    for line in output.splitlines():
        line = line.strip()
        if not line or line.startswith("linux-vdso"):
            continue
        if "=> not found" in line:
            raise AcceptanceError(f"fio has an unavailable shared library: {line}")
        path_text: str | None = None
        if "=>" in line:
            candidate = line.split("=>", 1)[1].strip().split(" ", 1)[0]
            if candidate.startswith("/"):
                path_text = candidate
        elif line.startswith("/"):
            path_text = line.split(" ", 1)[0]
        if path_text is None:
            continue
        path = Path(path_text)
        if "ld-linux" in path.name or path.name.startswith("ld-musl"):
            loader = path
        else:
            libraries.append(path)
    require(loader is not None, "cannot identify fio's ELF loader")
    return libraries, loader


def prepare_fio_disk(harness: Harness) -> tuple[Path, str, str]:
    fio = shutil.which("fio")
    require(fio is not None, "fio is required on the validation host")
    fio_version = harness.run([fio, "--version"]).stdout.strip()
    require(fio_version.startswith("fio-"), f"unexpected fio binary: {fio_version!r}")
    ldd = harness.run(["ldd", fio]).stdout
    libraries, loader = parse_ldd(ldd)

    fixtures = harness.home / "fixtures"
    bundle = fixtures / "fio-root"
    bundle.mkdir(parents=True, mode=0o700)
    copied: dict[str, str] = {}
    for source in [Path(fio), loader, *libraries]:
        require(source.is_file(), f"fio runtime file is unavailable: {source}")
        target = bundle / source.name
        source_hash = sha256(source)
        previous = copied.get(target.name)
        require(
            previous is None or previous == source_hash,
            f"fio runtime has conflicting library basename {target.name}",
        )
        if previous is None:
            shutil.copy2(source, target)
            copied[target.name] = source_hash

    disk = fixtures / "fio.raw"
    harness.run(["truncate", "-s", "512M", disk])
    harness.run(
        [
            "mke2fs",
            "-q",
            "-t",
            "ext4",
            "-L",
            "FIRESTONE_FIO",
            "-d",
            bundle,
            "-F",
            disk,
        ]
    )
    os.chmod(disk, 0o600)
    return disk, loader.name, fio_version


class ConsoleSession:
    def __init__(self, path: Path, timeout: int = BOOT_TIMEOUT_SECONDS) -> None:
        self.path = path
        deadline = time.monotonic() + timeout
        while True:
            try:
                self.fd = os.open(path, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
                break
            except FileNotFoundError:
                if time.monotonic() >= deadline:
                    raise AcceptanceError(f"console PTY did not appear: {path}")
                time.sleep(0.1)
        self.buffer = bytearray()

    def close(self) -> None:
        os.close(self.fd)

    def _sendall(self, data: bytes) -> None:
        remaining = memoryview(data)
        while remaining:
            try:
                written = os.write(self.fd, remaining)
            except BlockingIOError:
                _, writable, _ = select.select([], [self.fd], [], 0.2)
                require(writable, "console PTY remained blocked while writing")
                continue
            require(written > 0, "console PTY accepted no bytes")
            remaining = remaining[written:]

    def _receive(self) -> bool:
        readable, _, _ = select.select([self.fd], [], [], 0.2)
        if not readable:
            return False
        try:
            block = os.read(self.fd, 65_536)
        except BlockingIOError:
            return False
        if not block:
            return False
        self.buffer.extend(block)
        require(len(self.buffer) <= MAX_CONSOLE_BYTES, "console session exceeded 8 MiB")
        return True

    def _wait_for(self, marker: bytes, timeout: float) -> bytes:
        deadline = time.monotonic() + timeout
        while marker not in self.buffer:
            if time.monotonic() >= deadline:
                raise AcceptanceError(
                    f"console did not return marker {marker.decode('ascii', errors='replace')!r}"
                )
            self._receive()
        before, _, after = bytes(self.buffer).partition(marker)
        self.buffer = bytearray(after)
        return before

    def wait_for_shell(self, timeout: int = BOOT_TIMEOUT_SECONDS) -> None:
        token = f"__FIRESTONE_READY_{uuid.uuid4().hex}__".encode("ascii")
        marker = b"\r\n" + token + b"\r\n"
        command = b"\x03\nprintf '" + token + b"\\n'\n"
        deadline = time.monotonic() + timeout
        next_probe = 0.0
        while marker not in self.buffer:
            now = time.monotonic()
            if now >= deadline:
                raise AcceptanceError("guest console did not reach the root autologin shell")
            if now >= next_probe:
                self._sendall(command)
                next_probe = now + 2.0
            self._receive()
        _, _, after = bytes(self.buffer).partition(marker)
        self.buffer = bytearray(after)
        self._sendall(b"\nstty -echo\n")

    def run(self, command: str, timeout: int) -> tuple[int, str]:
        identifier = uuid.uuid4().hex
        begin = f"__FIRESTONE_BEGIN_{identifier}__".encode("ascii")
        end_prefix = f"__FIRESTONE_END_{identifier}:".encode("ascii")
        encoded = base64.b64encode(command.encode("utf-8"))
        wrapper = (
            b"printf '\n"
            + begin
            + b"\\n'; printf '%s' '"
            + encoded
            + b"' | base64 -d | /bin/sh; rc=$?; printf '\n"
            + end_prefix
            + b"%s__\\n' \"$rc\"\n"
        )
        self._sendall(wrapper)
        self._wait_for(begin + b"\r\n", 10)
        deadline = time.monotonic() + timeout
        end_pattern = re.compile(re.escape(end_prefix) + rb"([0-9]+)__\r?\n")
        while True:
            match = end_pattern.search(self.buffer)
            if match is not None:
                output = bytes(self.buffer[: match.start()]).replace(b"\r", b"")
                self.buffer = self.buffer[match.end() :]
                return int(match.group(1)), output.decode("utf-8", errors="replace").strip()
            if time.monotonic() >= deadline:
                raise AcceptanceError(f"guest command timed out after {timeout}s")
            self._receive()


def fio_summary(document: dict[str, Any]) -> dict[str, Any]:
    jobs = document.get("jobs")
    require(isinstance(jobs, list) and len(jobs) == 1, "fio output did not contain one job")
    job = jobs[0]
    read = job.get("read")
    write = job.get("write")
    require(isinstance(read, dict) and isinstance(write, dict), "fio output lacks I/O results")
    result: dict[str, Any] = {
        "read_bw_bytes": read.get("bw_bytes"),
        "read_iops": read.get("iops"),
        "write_bw_bytes": write.get("bw_bytes"),
        "write_iops": write.get("iops"),
    }
    for operation_name, operation in (("read", read), ("write", write)):
        clat = operation.get("clat_ns")
        if isinstance(clat, dict):
            percentile = clat.get("percentile")
            if isinstance(percentile, dict):
                result[f"{operation_name}_clat_p99_ns"] = percentile.get("99.000000")
    require(
        all(isinstance(result[key], (int, float)) for key in ("read_bw_bytes", "read_iops", "write_bw_bytes", "write_iops")),
        "fio output contains non-numeric throughput results",
    )
    return result


def parse_fio_documents(output: str) -> tuple[dict[str, Any], dict[str, Any]]:
    overlay_match = re.search(
        r"FIO_OVERLAY_BEGIN\n(.*?)\nFIO_OVERLAY_END",
        output,
        flags=re.DOTALL,
    )
    raw_match = re.search(
        r"FIO_RAW_BEGIN\n(.*?)\nFIO_RAW_END",
        output,
        flags=re.DOTALL,
    )
    require(overlay_match is not None and raw_match is not None, "guest fio output markers are missing")
    try:
        overlay = json.loads(overlay_match.group(1))
        raw = json.loads(raw_match.group(1))
    except json.JSONDecodeError as error:
        raise AcceptanceError("guest fio returned invalid JSON") from error
    return fio_summary(overlay), fio_summary(raw)


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
    return result


def image_evidence(harness: Harness, name: str) -> dict[str, Any]:
    state = harness.state(name)
    image_id = state["image"]["id"]
    require(isinstance(image_id, str), f"{name} did not pin an image id")
    sidecar_path = harness.home / "data" / "images" / f"{image_id}.json"
    sidecar = json.loads(sidecar_path.read_text(encoding="utf-8"))
    return {
        "id": image_id,
        "source_ref": sidecar["source_ref"],
        "source_url": sidecar["source_url"],
        "source_sha256": sidecar["source_sha256"],
        "stored_sha256": sidecar["stored_sha256"],
        "source_format": sidecar["source_format"],
        "stored_format": sidecar["stored_format"],
        "firmware": sidecar["firmware"],
    }


def pty_console_path(info: dict[str, Any]) -> Path:
    config = info.get("config")
    require(isinstance(config, dict), "vm.info did not return VmConfig")
    console = config.get("console")
    require(isinstance(console, dict), "vm.info did not return console configuration")
    require(console.get("mode") == "Pty", "vm.info did not report a PTY console")
    path_value = console.get("file")
    require(isinstance(path_value, str) and path_value, "vm.info did not return the console PTY path")
    path = Path(path_value)
    require(path.is_absolute(), "vm.info returned a relative console PTY path")
    return path


def require_default_vmconfig(harness: Harness, name: str) -> dict[str, Any]:
    config = harness.vmconfig(name)
    payload = config.get("payload")
    disks = config.get("disks")
    require(isinstance(payload, dict), "VmConfig payload is missing")
    require("firmware" in payload and "kernel" not in payload, "accepted boot did not map edk2 to payload.firmware")
    require(isinstance(disks, list) and len(disks) >= 2, "VmConfig disks are missing")
    require(disks[0].get("image_type") == "Qcow2", "root disk is not Qcow2")
    require(disks[0].get("backing_files") is True, "root disk did not enable backing files")
    require(disks[1].get("image_type") == "Raw", "CIDATA disk is not Raw")
    require(disks[1].get("readonly") is True, "CIDATA disk is not read-only")
    require(config.get("console") == {"mode": "Pty"}, "VmConfig did not select the supported PTY console")
    require("net" not in config, "network.mode=none produced a net device")
    return {
        "payload": payload,
        "root_disk": disks[0],
        "seed_disk": disks[1],
        "console": config["console"],
        "net_present": "net" in config,
    }


def run_acceptance(harness: Harness) -> None:
    require(sys.platform == "linux", "M1 acceptance requires Linux")
    require(platform.machine() == "x86_64", "M1 acceptance requires x86_64")
    require(os.access("/dev/kvm", os.R_OK | os.W_OK), "/dev/kvm is not readable and writable")
    for program in ("cargo", "qemu-img", "fio", "ldd", "truncate", "mke2fs"):
        require(shutil.which(program) is not None, f"required host program is missing: {program}")

    commit = harness.run(["git", "rev-parse", "HEAD"]).stdout.strip()
    harness.evidence["commit"] = commit
    harness.evidence["host"] = {
        "system": platform.system(),
        "release": platform.release(),
        "architecture": platform.machine(),
        "kvm_read_write": True,
        "qemu_img": harness.run(["qemu-img", "--version"]).stdout.splitlines()[0],
    }

    if "FIRESTONE_BIN" not in os.environ:
        harness.run(["cargo", "build", "--locked", "--bin", "firestone"], timeout=1_200)
    require(harness.binary.is_file() and os.access(harness.binary, os.X_OK), "firestone binary was not built")

    harness.firestone("doctor", "--fix", timeout=900)
    doctor_records = harness.firestone("doctor")
    doctor = harness.result_payload(doctor_records, "doctor")
    checks = doctor.get("checks")
    require(isinstance(checks, list) and len(checks) == 13, "doctor did not return 13 checks")
    failures = [check for check in checks if check.get("status") == "fail"]
    require(not failures, f"doctor failed checks: {failures!r}")
    harness.evidence["scenarios"]["e2e_1_doctor"] = {"checks": checks}

    artifacts = installed_artifacts(harness)
    harness.evidence["artifacts"] = artifacts
    cloud_hypervisor = harness.home / "data" / "bin" / artifacts["cloud-hypervisor"]["install_name"]
    help_output = harness.run([cloud_hypervisor, "--help"]).stdout
    for option in ("--api-socket", "--kernel", "--firmware"):
        require(option in help_output, f"pinned cloud-hypervisor help omits {option}")

    login_patterns = (
        re.compile(r"Ubuntu 24\.04(?:\.\d+)? .* login:"),
        re.compile(r"\b[a-zA-Z0-9_.-]+ login:"),
    )
    shutdown_patterns = (
        re.compile(r"reboot: Power down"),
        re.compile(r"Reached target .*System Power Off"),
    )

    graceful = "m1-graceful"
    harness.create(graceful, "ubuntu:24.04")
    start_result, start_records = harness.start(graceful)
    login_line = harness.wait_console_match(graceful, login_patterns)
    config_evidence = require_default_vmconfig(harness, graceful)
    image = image_evidence(harness, graceful)
    require(image["source_format"] == "qcow2", "Ubuntu catalog source was not qcow2")
    require(image["stored_format"] == "qcow2", "Ubuntu catalog base was not stored as qcow2")
    require(image["firmware"] == "edk2", "Ubuntu x86_64 did not select edk2")

    ping_status, ping_body = harness.unix_http(graceful, "GET", "/api/v1/vmm.ping")
    info_status, info_body = harness.unix_http(graceful, "GET", "/api/v1/vm.info")
    require(ping_status == 200, f"vmm.ping returned HTTP {ping_status}")
    require(info_status == 200, f"vm.info returned HTTP {info_status}")
    info = json.loads(info_body)
    require(info.get("state") == "Running", f"vm.info did not report Running: {info!r}")

    console = ConsoleSession(pty_console_path(info))
    try:
        console.wait_for_shell()
        cloud_rc, cloud_status = console.run(
            "cloud-init status --wait --long; rc=$?; "
            "printf 'datasource='; cloud-id; "
            "printf 'cidata_label='; blkid -s LABEL -o value /dev/vdb; "
            "exit \"$rc\"",
            timeout=300,
        )
    finally:
        console.close()
    require(cloud_rc == 0, f"cloud-init status failed: {cloud_status}")
    require(re.search(r"(?m)^status: done$", cloud_status) is not None, f"cloud-init was not done: {cloud_status}")
    require(re.search(r"(?m)^datasource=(?:nocloud|NoCloud)$", cloud_status) is not None, f"NoCloud was not consumed: {cloud_status}")
    require(re.search(r"(?m)^cidata_label=CIDATA$", cloud_status) is not None, f"CIDATA label was not observed: {cloud_status}")

    stop_result, stop_records = harness.stop(graceful)
    state = harness.state(graceful)
    require(state["last_exit"]["reason"] == "guest shutdown", "graceful stop reason was not guest shutdown")
    shutdown_line = harness.wait_console_match(graceful, shutdown_patterns, timeout=30)
    harness.evidence["scenarios"]["e2e_5_graceful_stop"] = {
        "start_result": start_result,
        "start_steps": [record.get("id") for record in start_records if record.get("type") == "StepDone"],
        "login_console_line": login_line,
        "cloud_init_status": cloud_status.splitlines(),
        "api": {
            "vmm_ping": {"status": ping_status, "body_bytes": len(ping_body)},
            "vm_info": {"status": info_status, "state": info.get("state"), "pid": info.get("pid")},
        },
        "vmconfig": config_evidence,
        "image": image,
        "stop_result": stop_result,
        "stop_detail": next(
            (record.get("detail") for record in stop_records if record.get("id") == "stop" and record.get("type") == "StepDone"),
            None,
        ),
        "last_exit": state["last_exit"],
        "shutdown_console_line": shutdown_line,
    }

    base = harness.home / "data" / "images" / f"{image['id']}.qcow2"
    fixtures = harness.home / "fixtures"
    fixtures.mkdir(exist_ok=True, mode=0o700)
    converted_raw = fixtures / "ubuntu.raw"
    harness.run(["qemu-img", "convert", "-f", "qcow2", "-O", "raw", base, converted_raw], timeout=900)
    os.chmod(converted_raw, 0o600)
    fio_disk, loader_name, fio_version = prepare_fio_disk(harness)

    converted = "m1-convert"
    machine_dir = harness.home / "data" / "machines" / converted
    overlay = {
        "disks": [
            {
                "path": os.fspath(machine_dir / "disk.qcow2"),
                "image_type": "Qcow2",
                "backing_files": True,
            },
            {
                "path": os.fspath(machine_dir / "seed.img"),
                "readonly": True,
                "image_type": "Raw",
            },
            {"path": os.fspath(fio_disk), "image_type": "Raw"},
        ]
    }
    harness.create(
        converted,
        converted_raw,
        "--vmm-firmware",
        "edk2",
        "--vmm-config",
        json.dumps(overlay, separators=(",", ":")),
    )
    harness.start(converted)
    converted_login = harness.wait_console_match(converted, login_patterns)
    converted_image = image_evidence(harness, converted)
    require(converted_image["source_format"] == "raw", "raw source was not classified as raw")
    require(converted_image["stored_format"] == "qcow2", "raw source was not converted to qcow2")
    converted_config = require_default_vmconfig(harness, converted)
    require(len(harness.vmconfig(converted)["disks"]) == 3, "fio raw disk was not attached")

    fio_command = f"""
set -eu
mkdir -p /mnt/firestone-fio
mount -t ext4 /dev/vdc /mnt/firestone-fio
loader=/mnt/firestone-fio/{shlex.quote(loader_name)}
fio=/mnt/firestone-fio/fio
common='--size=64m --rw=randrw --rwmixread=70 --bs=4k --ioengine=psync --iodepth=1 --direct=1 --runtime=10 --time_based --group_reporting --randseed=20260829 --unlink=1 --output-format=json'
$loader --library-path /mnt/firestone-fio $fio --name=overlay --filename=/var/tmp/firestone-fio-overlay.bin $common --output=/tmp/fio-overlay.json
$loader --library-path /mnt/firestone-fio $fio --name=raw --filename=/mnt/firestone-fio/firestone-fio-raw.bin $common --output=/tmp/fio-raw.json
printf 'FIO_OVERLAY_BEGIN\n'
cat /tmp/fio-overlay.json
printf '\nFIO_OVERLAY_END\nFIO_RAW_BEGIN\n'
cat /tmp/fio-raw.json
printf '\nFIO_RAW_END\n'
umount /mnt/firestone-fio
""".strip()
    converted_info_status, converted_info_body = harness.unix_http(converted, "GET", "/api/v1/vm.info")
    require(converted_info_status == 200, f"converted vm.info returned HTTP {converted_info_status}")
    converted_info = json.loads(converted_info_body)
    fio_console = ConsoleSession(pty_console_path(converted_info))
    try:
        fio_console.wait_for_shell()
        fio_rc, fio_output = fio_console.run(fio_command, timeout=180)
    finally:
        fio_console.close()
    require(fio_rc == 0, f"guest fio command failed: {compact_output(fio_output)}")
    overlay_fio, raw_fio = parse_fio_documents(fio_output)
    harness.stop(converted)
    harness.evidence["scenarios"]["verify_4_5_conversion_overlay_fio"] = {
        "login_console_line": converted_login,
        "image": converted_image,
        "vmconfig": converted_config,
        "fio_version": fio_version,
        "workload": {
            "size": "64m",
            "rw": "randrw",
            "rwmixread": 70,
            "block_size": "4k",
            "ioengine": "psync",
            "iodepth": 1,
            "direct": 1,
            "runtime_seconds": 10,
            "randseed": 20260829,
        },
        "overlay": overlay_fio,
        "raw_auxiliary_disk": raw_fio,
        "threshold_applied": False,
    }

    vmm_crash = "m1-vmm-crash"
    harness.create(vmm_crash, "ubuntu:24.04")
    harness.start(vmm_crash)
    harness.wait_console_match(vmm_crash, login_patterns)
    vmm_pid = harness.state(vmm_crash).get("vmm_pid")
    require(isinstance(vmm_pid, int) and vmm_pid > 0, "running state has no VMM pid")
    os.kill(vmm_pid, signal.SIGKILL)
    failed_seconds = harness.wait_for_status(vmm_crash, "failed", 2.0)
    failed_state = harness.state(vmm_crash)
    restart_result, _ = harness.start(vmm_crash)
    restart_login = harness.wait_console_match(
        vmm_crash,
        login_patterns,
        minimum_matches=2,
    )
    harness.stop(vmm_crash)
    harness.evidence["scenarios"]["e2e_6_vmm_sigkill_restart"] = {
        "vmm_pid": vmm_pid,
        "failed_after_ms": round(failed_seconds * 1_000, 3),
        "failed_last_exit": failed_state.get("last_exit"),
        "restart_result": restart_result,
        "restart_login_console_line": restart_login,
    }

    shim_crash = "m1-shim-crash"
    harness.create(shim_crash, "ubuntu:24.04")
    harness.start(shim_crash)
    harness.wait_console_match(shim_crash, login_patterns)
    shim_pid = harness.state(shim_crash).get("shim_pid")
    require(isinstance(shim_pid, int) and shim_pid > 0, "running state has no shim pid")
    os.kill(shim_pid, signal.SIGKILL)
    unsupervised_seconds = harness.wait_for_status(shim_crash, "running (unsupervised)", 2.0)
    unsupervised_stop, _ = harness.stop(shim_crash)
    unsupervised_state = harness.state(shim_crash)
    require(
        unsupervised_state["last_exit"]["reason"] == "guest shutdown",
        "unsupervised stop reason was not guest shutdown",
    )
    harness.evidence["scenarios"]["e2e_7_shim_sigkill_stop"] = {
        "shim_pid": shim_pid,
        "unsupervised_after_ms": round(unsupervised_seconds * 1_000, 3),
        "listed_status": "running (unsupervised)",
        "stop_result": unsupervised_stop,
        "last_exit": unsupervised_state["last_exit"],
    }


def main() -> int:
    if os.environ.get("FIRESTONE_E2E") != "1":
        print("skipped M1 KVM acceptance; set FIRESTONE_E2E=1 to run")
        return 0

    harness: Harness | None = None
    try:
        harness = Harness()
        run_acceptance(harness)
        harness.evidence["result"] = "passed"
        harness.write_evidence()
        print(f"M1 KVM acceptance passed; evidence: {harness.evidence_path}")
        return 0
    except (AcceptanceError, OSError, ValueError, KeyError, TypeError) as error:
        if harness is not None:
            harness.evidence["result"] = "failed"
            harness.evidence["error"] = str(error)
            try:
                harness.write_evidence()
            except OSError:
                pass
        print(f"M1 KVM acceptance failed: {error}", file=sys.stderr)
        return 1
    finally:
        if harness is not None:
            harness.cleanup()


if __name__ == "__main__":
    raise SystemExit(main())
