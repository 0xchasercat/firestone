#!/usr/bin/env python3
"""Run the Linux x86_64 M3 acceptance scenarios against real KVM."""

from __future__ import annotations

import atexit
import datetime as dt
import fcntl
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
import uuid
from pathlib import Path
from typing import Any


class AcceptanceError(RuntimeError):
    """The host or one M3 acceptance contract failed."""


REPO_ROOT = Path(__file__).resolve().parents[1]
COMMAND_TIMEOUT_SECONDS = 120
START_TIMEOUT_SECONDS = 1_900
TEARDOWN_TIMEOUT_SECONDS = 30
MAX_OUTPUT_BYTES = 8 * 1024 * 1024
MAX_FILE_BYTES = 16 * 1024 * 1024
MACHINE_NAMES = ("m3-main", "m3-tap")
PASST_VERSION = "2025_02_17.a1e48a0"
PASST_COMMIT = "a1e48a02ff3550eb7875a7df6726086e9b3a1213"
TUNSETIFF = 0x400454CA
IFF_TAP = 0x0002
IFF_NO_PI = 0x1000


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
    return {
        "bytes": len(value),
        "lines": len(value.splitlines()),
        "ends_with_newline": value.endswith(b"\n") if value else False,
        "sha256": bytes_sha256(value),
    }


def read_bounded(path: Path, limit: int = MAX_FILE_BYTES) -> bytes:
    try:
        metadata = path.lstat()
        require(
            stat.S_ISREG(metadata.st_mode) and not stat.S_ISLNK(metadata.st_mode),
            f"expected a real regular file: {path}",
        )
        require(metadata.st_uid == os.getuid(), f"file has the wrong owner: {path}")
        require(metadata.st_size <= limit, f"file exceeds {limit} bytes: {path}")
        return path.read_bytes()
    except OSError as error:
        raise AcceptanceError(f"cannot read {path}: {error}") from error


def read_json(path: Path, limit: int = MAX_FILE_BYTES) -> dict[str, Any]:
    try:
        value = json.loads(read_bounded(path, limit))
    except (json.JSONDecodeError, UnicodeDecodeError) as error:
        raise AcceptanceError(f"invalid JSON in {path}: {error}") from error
    require(isinstance(value, dict), f"JSON root is not an object: {path}")
    return value


def write_private(path: Path, value: bytes, mode: int = 0o600) -> None:
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.{os.getpid()}.{uuid.uuid4().hex}.tmp")
    descriptor = os.open(
        temporary,
        os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC,
        mode,
    )
    try:
        with os.fdopen(descriptor, "wb", closefd=False) as stream:
            stream.write(value)
            stream.flush()
            os.fsync(stream.fileno())
    finally:
        os.close(descriptor)
    os.replace(temporary, path)
    os.chmod(path, mode)


def process_start_ticks(pid: int) -> int | None:
    try:
        value = (Path("/proc") / str(pid) / "stat").read_text(encoding="utf-8")
    except (FileNotFoundError, PermissionError, ProcessLookupError):
        return None
    close = value.rfind(")")
    require(close >= 0, f"malformed /proc/{pid}/stat")
    fields = value[close + 2 :].split()
    require(len(fields) > 19, f"short /proc/{pid}/stat")
    try:
        return int(fields[19])
    except ValueError as error:
        raise AcceptanceError(f"invalid start time in /proc/{pid}/stat") from error


def process_gone(pid: int, start_ticks: int | None) -> tuple[bool, bool]:
    current = process_start_ticks(pid)
    if current is None:
        return True, False
    if start_ticks is not None and current != start_ticks:
        return True, True
    return False, False


def wait_for_processes_gone(
    inventory: dict[str, dict[str, Any]], timeout: float
) -> dict[str, Any]:
    started = time.monotonic()
    deadline = started + timeout
    pending = set(inventory)
    reused: list[str] = []
    while pending and time.monotonic() < deadline:
        for label in list(pending):
            item = inventory[label]
            gone, was_reused = process_gone(item["pid"], item["start_time_ticks"])
            if gone:
                pending.remove(label)
                if was_reused:
                    reused.append(label)
        if pending:
            time.sleep(0.02)
    require(not pending, f"processes survived teardown: {sorted(pending)!r}")
    return {
        "all_gone": True,
        "labels": sorted(inventory),
        "pid_reuse_observed": sorted(reused),
        "elapsed_ms": round((time.monotonic() - started) * 1000, 3),
    }


def decode_argv_hex(values: Any, label: str) -> list[str]:
    require(isinstance(values, list), f"{label} argv_hex is not a list")
    result: list[str] = []
    for value in values:
        require(isinstance(value, str), f"{label} argv_hex contains a non-string")
        try:
            raw = bytes.fromhex(value)
        except ValueError as error:
            raise AcceptanceError(f"{label} argv_hex is invalid") from error
        result.append(raw.decode("utf-8", errors="backslashreplace"))
    return result


def decode_launch_args(values: Any, label: str) -> list[str]:
    require(isinstance(values, list), f"{label} args is not a list")
    result: list[str] = []
    for value in values:
        require(
            isinstance(value, dict) and set(value) == {"Unix"},
            f"{label} args contains an invalid OsString",
        )
        raw = value["Unix"]
        require(
            isinstance(raw, list)
            and all(
                isinstance(byte, int)
                and not isinstance(byte, bool)
                and 0 <= byte <= 255
                for byte in raw
            ),
            f"{label} args contains invalid Unix bytes",
        )
        result.append(os.fsdecode(bytes(raw)))
    return result


def status_fields(pid: int) -> dict[str, str]:
    try:
        lines = (Path("/proc") / str(pid) / "status").read_text(
            encoding="utf-8"
        ).splitlines()
    except (FileNotFoundError, PermissionError, ProcessLookupError) as error:
        raise AcceptanceError(f"cannot read status for pid {pid}: {error}") from error
    fields: dict[str, str] = {}
    for line in lines:
        key, separator, value = line.partition(":")
        if separator:
            fields[key] = value.strip()
    return fields


def self_capabilities() -> dict[str, Any]:
    fields = status_fields(os.getpid())
    effective = fields.get("CapEff")
    require(effective is not None, "harness status omitted CapEff")
    require(int(effective, 16) == 0, "acceptance user has effective capabilities")
    return {"pid": os.getpid(), "uid": os.getuid(), "cap_eff": effective}


class Harness:
    def __init__(self) -> None:
        self.home = self._checked_home()
        self.keep_home = os.environ.get("FIRESTONE_E2E_KEEP") == "1"
        default_evidence = Path("/tmp") / f"firestone-m3-evidence-{os.getpid()}.json"
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
        self.command_records: list[dict[str, Any]] = []
        self.evidence: dict[str, Any] = {
            "schema": 1,
            "result": "running",
            "started_at": dt.datetime.now(dt.UTC).isoformat(),
            "commands": self.commands,
            "command_records": self.command_records,
            "host": {},
            "host_setup": {},
            "pins": {},
            "artifacts": {},
            "image": {},
            "scenarios": {},
        }
        self.tap_name: str | None = None
        self.userns_restore_value: str | None = None
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
        command = self.record_command(argv) if record else [os.fspath(v) for v in argv]
        rendered = shlex.join(command)
        started_at = dt.datetime.now(dt.UTC).isoformat()
        started = time.monotonic()
        try:
            completed = subprocess.run(
                command,
                cwd=REPO_ROOT,
                env=os.environ.copy(),
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=timeout,
                check=False,
            )
        except subprocess.TimeoutExpired as error:
            self.command_records.append(
                {
                    "argv": command,
                    "started_at": started_at,
                    "elapsed_ms": round((time.monotonic() - started) * 1000, 3),
                    "timeout_seconds": timeout,
                    "timed_out": True,
                }
            )
            raise AcceptanceError(
                f"command timed out after {timeout:.1f}s: {rendered}"
            ) from error
        elapsed_ms = round((time.monotonic() - started) * 1000, 3)
        require(
            len(completed.stdout) <= MAX_OUTPUT_BYTES,
            f"command stdout exceeded 8 MiB: {rendered}",
        )
        require(
            len(completed.stderr) <= MAX_OUTPUT_BYTES,
            f"command stderr exceeded 8 MiB: {rendered}",
        )
        self.command_records.append(
            {
                "argv": command,
                "started_at": started_at,
                "elapsed_ms": elapsed_ms,
                "timeout_seconds": timeout,
                "timed_out": False,
                "exit_code": completed.returncode,
                "stdout": stream_facts(completed.stdout),
                "stderr": stream_facts(completed.stderr),
            }
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
        timeout: float = COMMAND_TIMEOUT_SECONDS,
        expected_code: int = 0,
        action: str | None = None,
        error_kind: str | None = None,
    ) -> tuple[list[dict[str, Any]], dict[str, Any], subprocess.CompletedProcess[bytes]]:
        completed = self.run(
            [self.binary, "--json", *arguments], timeout=timeout, check=False
        )
        require(
            completed.returncode == expected_code,
            f"{shlex.join(arguments)} exited {completed.returncode}, expected {expected_code}; "
            f"stdout:\n{compact_bytes(completed.stdout)}\n"
            f"stderr:\n{compact_bytes(completed.stderr)}",
        )
        require(not completed.stderr, f"{shlex.join(arguments)} JSON command wrote stderr")
        require(completed.stdout.endswith(b"\n"), "JSON output lacks a final newline")
        raw_lines = completed.stdout.splitlines()
        require(raw_lines and all(raw_lines), "JSON output contains empty records")
        try:
            records = [json.loads(line) for line in raw_lines]
        except (json.JSONDecodeError, UnicodeDecodeError) as error:
            raise AcceptanceError(
                f"invalid NDJSON from {shlex.join(arguments)}: "
                f"{compact_bytes(completed.stdout)}"
            ) from error
        require(all(isinstance(record, dict) for record in records), "NDJSON record is not an object")
        terminal_indexes = [
            index
            for index, record in enumerate(records)
            if record.get("type") == "Result"
            or ("error" in record and "type" not in record)
        ]
        require(
            terminal_indexes == [len(records) - 1],
            f"{shlex.join(arguments)} has invalid terminal framing",
        )
        terminal = records[-1]
        if expected_code == 0:
            require(terminal.get("type") == "Result", "successful command did not end in Result")
            if action is not None:
                require(
                    terminal.get("action") == action,
                    f"terminal action was {terminal.get('action')!r}, expected {action!r}",
                )
            terminal_kind = "Result"
            terminal_value = terminal.get("action")
        else:
            error = terminal.get("error")
            require(isinstance(error, dict), "failed command lacks an error object")
            if error_kind is not None:
                require(
                    error.get("kind") == error_kind,
                    f"error kind was {error.get('kind')!r}, expected {error_kind!r}",
                )
            terminal_kind = "error"
            terminal_value = error.get("kind")
        frame = {
            "exit_code": completed.returncode,
            "record_count": len(records),
            "terminal_kind": terminal_kind,
            "terminal_value": terminal_value,
            "stdout": stream_facts(completed.stdout),
            "stderr": stream_facts(completed.stderr),
        }
        return records, frame, completed

    @staticmethod
    def result_payload(records: list[dict[str, Any]], action: str) -> Any:
        terminal = records[-1]
        require(terminal.get("type") == "Result", f"{action} did not end in Result")
        require(terminal.get("action") == action, f"expected {action} Result")
        require("payload" in terminal, f"{action} Result has no payload")
        return terminal["payload"]

    def state(self, name: str) -> dict[str, Any]:
        return read_json(self.home / "data" / "machines" / name / "state.json")

    def wait_for_state(
        self, name: str, expected: set[str], timeout: float
    ) -> tuple[dict[str, Any], float]:
        started = time.monotonic()
        deadline = started + timeout
        last = "missing"
        while time.monotonic() < deadline:
            try:
                state = self.state(name)
                status_value = state.get("status")
                last = status_value if isinstance(status_value, str) else "invalid"
            except AcceptanceError:
                state = {}
                last = "missing"
            if last in expected:
                return state, time.monotonic() - started
            time.sleep(0.01)
        raise AcceptanceError(
            f"{name} did not reach {sorted(expected)!r} within {timeout:.1f}s; "
            f"last status was {last!r}"
        )

    def start(self, name: str, *, wait: bool = True) -> tuple[dict[str, Any], dict[str, Any]]:
        arguments = ["start", name, "--timeout", "600s"]
        if not wait:
            arguments.append("--no-wait")
        records, frame, _ = self.json_command(
            *arguments, timeout=START_TIMEOUT_SECONDS, action="start"
        )
        payload = self.result_payload(records, "start")
        require(payload["status"] == "running", f"{name} did not start")
        return payload, frame

    def stop(self, name: str, *, force: bool = False) -> tuple[dict[str, Any], dict[str, Any]]:
        arguments = ["stop", name, "--timeout", "90s"]
        if force:
            arguments.append("--force")
        records, frame, _ = self.json_command(
            *arguments, timeout=150, action="stop"
        )
        payload = self.result_payload(records, "stop")
        require(payload["status"] == "stopped", f"{name} did not stop")
        return payload, frame

    def remove(self, name: str) -> dict[str, Any]:
        records, _, _ = self.json_command("rm", name, "--force", action="rm")
        payload = self.result_payload(records, "rm")
        require(name in payload["removed"], f"{name} was not removed")
        return payload

    def shell(
        self,
        name: str,
        command: list[str],
        *,
        expected_code: int = 0,
        timeout: float = COMMAND_TIMEOUT_SECONDS,
    ) -> subprocess.CompletedProcess[bytes]:
        completed = self.run(
            [self.binary, "shell", name, "--", *command],
            timeout=timeout,
            check=False,
        )
        require(
            completed.returncode == expected_code,
            f"shell {name} exited {completed.returncode}, expected {expected_code}; "
            f"stdout:\n{compact_bytes(completed.stdout)}\n"
            f"stderr:\n{compact_bytes(completed.stderr)}",
        )
        return completed

    def guest_script(
        self,
        name: str,
        script: str,
        *,
        expected_code: int = 0,
        timeout: float = COMMAND_TIMEOUT_SECONDS,
    ) -> subprocess.CompletedProcess[bytes]:
        return self.shell(
            name,
            ["sh", "-c", shlex.quote(script)],
            expected_code=expected_code,
            timeout=timeout,
        )

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
        if sys.platform != "linux" or not Path("/proc").is_dir():
            return []
        home = os.fsencode(self.home)
        ancestors = {os.getpid()}
        parent = os.getppid()
        while parent > 1 and parent not in ancestors:
            ancestors.add(parent)
            try:
                fields = status_fields(parent)
                parent = int(fields["PPid"].split()[0])
            except (AcceptanceError, KeyError, ValueError):
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

    def remove_tap(self) -> list[str]:
        if self.tap_name is None:
            return []
        name = self.tap_name
        errors: list[str] = []
        try:
            completed = self.run(
                ["sudo", "-n", "ip", "tuntap", "del", "dev", name, "mode", "tap"],
                timeout=15,
                check=False,
            )
            if completed.returncode != 0:
                errors.append(
                    f"tap cleanup exited {completed.returncode}: {compact_bytes(completed.stderr)}"
                )
            elif (Path("/sys/class/net") / name).exists():
                errors.append(f"tap {name} survived cleanup")
            else:
                setup = self.evidence["host_setup"].get("tap")
                if isinstance(setup, dict):
                    setup["cleanup"] = {"removed": True, "exit_code": 0}
                self.tap_name = None
        except (AcceptanceError, OSError) as error:
            errors.append(f"tap cleanup failed: {error}")
        return errors

    def restore_userns_policy(self) -> list[str]:
        if self.userns_restore_value is None:
            return []
        value = self.userns_restore_value
        errors: list[str] = []
        try:
            completed = self.run(
                [
                    "sudo",
                    "-n",
                    "sysctl",
                    "-w",
                    f"kernel.apparmor_restrict_unprivileged_userns={value}",
                ],
                timeout=15,
                check=False,
            )
            if completed.returncode != 0:
                errors.append(
                    "user-namespace policy restore failed: "
                    f"{compact_bytes(completed.stderr)}"
                )
            else:
                current = Path(
                    "/proc/sys/kernel/apparmor_restrict_unprivileged_userns"
                ).read_text(encoding="utf-8").strip()
                if current != value:
                    errors.append(
                        f"user-namespace policy restored to {current!r}, expected {value!r}"
                    )
                else:
                    setup = self.evidence["host_setup"].get("user_namespaces")
                    if isinstance(setup, dict):
                        setup["cleanup"] = {"restored": True, "value": current}
                    self.userns_restore_value = None
        except (AcceptanceError, OSError) as error:
            errors.append(f"user-namespace policy restore failed: {error}")
        return errors

    def cleanup(self) -> list[str]:
        if self._cleanup_started:
            return []
        self._cleanup_started = True
        errors: list[str] = []
        if hasattr(self, "binary") and self.binary.is_file():
            for name in reversed(MACHINE_NAMES):
                for arguments in (
                    [self.binary, "--json", "stop", name, "--force", "--timeout", "5s"],
                    [self.binary, "--json", "rm", name, "--force"],
                ):
                    try:
                        subprocess.run(
                            [os.fspath(value) for value in arguments],
                            cwd=REPO_ROOT,
                            env=os.environ.copy(),
                            stdin=subprocess.DEVNULL,
                            stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL,
                            timeout=20,
                            check=False,
                        )
                    except (OSError, subprocess.TimeoutExpired) as error:
                        errors.append(f"cleanup command failed for {name}: {error}")
        errors.extend(self.remove_tap())
        errors.extend(self.restore_userns_policy())
        live = self._live_home_processes()
        if live:
            errors.append(f"live processes still reference FIRESTONE_HOME: {live}")
        if not self.keep_home and not live:
            try:
                shutil.rmtree(self.home, ignore_errors=False)
            except OSError as error:
                errors.append(f"cannot remove FIRESTONE_HOME: {error}")
        return errors


def configure_timeouts(harness: Harness) -> dict[str, Any]:
    path = harness.home / "config" / "config.toml"
    contents = (
        b'[start]\ntimeout_first_boot = "600s"\ntimeout = "600s"\n\n'
        b'[stop]\ntimeout = "90s"\n'
    )
    write_private(path, contents)
    require(stat.S_IMODE(path.stat().st_mode) == 0o600, "config.toml is not mode 0600")
    return {"path": str(path), "mode": "0600", "sha256": bytes_sha256(contents)}


def setup_user_namespaces(harness: Harness) -> dict[str, Any]:
    probe_command = ["unshare", "--user", "--map-root-user", "true"]
    probe = harness.run(probe_command, timeout=10, check=False)
    result: dict[str, Any] = {
        "probe_command": probe_command,
        "before": {
            "exit_code": probe.returncode,
            "stdout": stream_facts(probe.stdout),
            "stderr": compact_bytes(probe.stderr, 2_048),
        },
        "adjusted": False,
        "cleanup": {"restored": False, "not_required": True},
    }
    harness.evidence["host_setup"]["user_namespaces"] = result
    if probe.returncode == 0:
        result["after"] = result["before"]
        return result

    restriction = Path("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
    before = restriction.read_text(encoding="utf-8").strip() if restriction.exists() else None
    result["apparmor_restrict_unprivileged_userns_before"] = before
    require(
        os.environ.get("FIRESTONE_E2E_ALLOW_USERNS_SETUP") == "1",
        "unshare root mapping failed; verify 16 requires rootless user namespaces. On this "
        "disposable test host, set FIRESTONE_E2E_ALLOW_USERNS_SETUP=1 to allow the "
        "harness to temporarily set kernel.apparmor_restrict_unprivileged_userns=0; "
        "the harness records and restores the prior value",
    )
    require(
        before is not None,
        "unshare root mapping failed and the AppArmor user-namespace policy sysctl is absent",
    )
    completed = harness.run(
        [
            "sudo",
            "-n",
            "sysctl",
            "-w",
            "kernel.apparmor_restrict_unprivileged_userns=0",
        ],
        timeout=15,
        check=False,
    )
    require(
        completed.returncode == 0,
        "test-host setup could not enable user namespaces with passwordless sudo: "
        f"{compact_bytes(completed.stderr)}",
    )
    harness.userns_restore_value = before
    result["adjusted"] = True
    result["cleanup"] = {"restored": False, "not_required": False}
    result["adjustment_command"] = (
        "sudo -n sysctl -w kernel.apparmor_restrict_unprivileged_userns=0"
    )
    after = harness.run(probe_command, timeout=10, check=False)
    result["after"] = {
        "exit_code": after.returncode,
        "stdout": stream_facts(after.stdout),
        "stderr": compact_bytes(after.stderr, 2_048),
        "apparmor_restrict_unprivileged_userns": restriction.read_text(
            encoding="utf-8"
        ).strip(),
    }
    require(
        after.returncode == 0,
        "unshare root mapping still fails after explicit test-host setup",
    )
    return result


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
            "commit": dependency.get("commit"),
            "install_name": artifact["install_name"],
            "path": str(path),
            "sha256": actual,
            "url": artifact["url"],
        }
    require(result["cloud-hypervisor"]["version"] == "v53.0", "Cloud Hypervisor pin changed")
    require(result["virtiofsd"]["version"] == "v1.14.0", "virtiofsd pin changed")
    for name, expected in (("cloud-hypervisor", "v53"), ("virtiofsd", "1.14.0")):
        completed = harness.run([result[name]["path"], "--version"], check=False)
        combined = completed.stdout + completed.stderr
        require(completed.returncode == 0 and expected.encode() in combined, f"{name} version probe changed")
        result[name]["version_probe"] = compact_bytes(combined, 2_048).strip()
        result[name]["version_probe_sha256"] = bytes_sha256(combined)
    return result


def passt_evidence(harness: Harness) -> dict[str, Any]:
    selected = shutil.which("passt")
    require(selected is not None, "required host program is missing: passt")
    path = Path(selected).resolve(strict=True)
    completed = harness.run([path, "--version"], check=False)
    combined = completed.stdout + completed.stderr
    require(completed.returncode == 0, "passt --version failed")
    version_lines = combined.splitlines()
    require(
        version_lines and version_lines[0] == f"passt {PASST_VERSION}".encode(),
        f"acceptance requires exact passt {PASST_VERSION}",
    )
    return {
        "version": PASST_VERSION,
        "commit": PASST_COMMIT,
        "path": str(path),
        "sha256": sha256(path),
        "version_output": compact_bytes(combined, 2_048).strip(),
        "version_output_sha256": bytes_sha256(combined),
    }


def doctor_evidence(harness: Harness) -> dict[str, Any]:
    started = time.monotonic()
    fix_records, fix_frame, _ = harness.json_command(
        "doctor", "--fix", timeout=900, action="doctor"
    )
    records, frame, _ = harness.json_command("doctor", action="doctor")
    report = harness.result_payload(records, "doctor")
    checks = report.get("checks")
    require(isinstance(checks, list) and len(checks) == 13, "doctor did not return 13 checks")
    failures = [check for check in checks if check.get("status") == "fail"]
    require(not failures, f"doctor failed checks: {failures!r}")
    userns = next(
        (check for check in checks if check.get("id") == "user_namespaces"), None
    )
    require(isinstance(userns, dict), "doctor omitted user_namespaces")
    require(
        userns.get("status") == "ok",
        "verify 16 requires doctor to report rootless user namespace support; "
        f"doctor returned {userns!r}",
    )
    return {
        "elapsed_ms": round((time.monotonic() - started) * 1000, 3),
        "fix_frame": fix_frame,
        "frame": frame,
        "check_count": len(checks),
        "user_namespaces": userns,
        "fix_result": harness.result_payload(fix_records, "doctor"),
    }


def generate_key(harness: Harness, directory: Path, name: str) -> tuple[Path, Path]:
    private = directory / name
    public = directory / f"{name}.pub"
    harness.run(
        [
            "ssh-keygen",
            "-q",
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            f"firestone-m3-{name}",
            "-f",
            private,
        ]
    )
    require(stat.S_IMODE(private.stat().st_mode) == 0o600, f"{name} private key mode changed")
    require(stat.S_IMODE(public.stat().st_mode) == 0o644, f"{name} public key mode changed")
    return private, public


def user_data(version: int) -> bytes:
    return f"""#cloud-config
hostname: m3-user-v{version}
write_files:
  - path: /etc/firestone-user-version
    permissions: "0644"
    content: |
      v{version}
runcmd:
  - [sh, -c, "printf 'v{version}-runcmd\\n' > /var/tmp/firestone-user-runcmd"]
""".encode()


def network_data(metric: int) -> bytes:
    return f"""version: 2
ethernets:
  passt0:
    match:
      name: "e*"
    dhcp4: true
    dhcp6: false
    dhcp4-overrides:
      route-metric: {metric}
    optional: true
""".encode()


def create_fixtures(harness: Harness) -> dict[str, Any]:
    root = harness.home / "harness-fixtures"
    root.mkdir(mode=0o700)
    rw = root / "rw"
    ro = root / "ro"
    keys = root / "keys"
    for directory in (rw, ro, keys):
        directory.mkdir(mode=0o700)
    readonly_marker = b"FIRESTONE_M3_READONLY_SOURCE\n"
    write_private(ro / "host.txt", readonly_marker, 0o600)
    key1, key1_public = generate_key(harness, keys, "key1")
    key2, key2_public = generate_key(harness, keys, "key2")
    active_public = keys / "active.pub"
    write_private(active_public, read_bounded(key1_public), 0o600)
    user_path = root / "user-data.yaml"
    network_path = root / "network-config.yaml"
    write_private(user_path, user_data(1), 0o600)
    write_private(network_path, network_data(111), 0o600)
    return {
        "root": root,
        "rw": rw.resolve(strict=True),
        "ro": ro.resolve(strict=True),
        "readonly_marker": readonly_marker,
        "key1": key1,
        "key2": key2,
        "key1_public": key1_public,
        "key2_public": key2_public,
        "active_public": active_public,
        "user_data": user_path,
        "network_data": network_path,
    }


def fixture_hashes(fixtures: dict[str, Any]) -> dict[str, Any]:
    return {
        "user_data": {
            "bytes": fixtures["user_data"].stat().st_size,
            "sha256": sha256(fixtures["user_data"]),
        },
        "network_data": {
            "bytes": fixtures["network_data"].stat().st_size,
            "sha256": sha256(fixtures["network_data"]),
        },
        "active_ssh_public_key": {
            "bytes": fixtures["active_public"].stat().st_size,
            "sha256": sha256(fixtures["active_public"]),
        },
    }


def choose_host_port(kind: int, preferred: int) -> tuple[int, bool]:
    def available(port: int) -> int | None:
        candidate = socket.socket(socket.AF_INET, kind)
        try:
            candidate.bind(("127.0.0.1", port))
            return int(candidate.getsockname()[1])
        except OSError:
            return None
        finally:
            candidate.close()

    selected = available(preferred)
    if selected is not None:
        return selected, True
    selected = available(0)
    require(selected is not None and selected > 0, "cannot allocate a free host port")
    return selected, False


def image_evidence(harness: Harness, name: str) -> dict[str, Any]:
    state = harness.state(name)
    image_id = state["image"]["id"]
    require(isinstance(image_id, str), f"{name} did not pin an image id")
    sidecar = read_json(harness.home / "data" / "images" / f"{image_id}.json")
    require(sidecar["source_ref"] == "ubuntu:24.04", "image ref changed")
    require(sidecar["architecture"] == "x86_64", "image architecture changed")
    require(sidecar["firmware"] == "edk2", "Ubuntu x86_64 did not select edk2")
    require(sidecar["verification_algorithm"] == "sha256", "image was not SHA-256 verified")
    require(
        sidecar["verification_digest"] == sidecar["source_sha256"],
        "image verifier digest does not match source SHA-256",
    )
    stored = harness.home / "data" / "images" / f"{image_id}.qcow2"
    actual = sha256(stored)
    require(actual == sidecar["stored_sha256"], "stored image checksum changed")
    return {
        "id": image_id,
        "generation": sidecar["generation"],
        "source_ref": sidecar["source_ref"],
        "source_url": sidecar["source_url"],
        "source_sha256": sidecar["source_sha256"],
        "stored_sha256": actual,
        "size": sidecar["size"],
        "source_format": sidecar["source_format"],
        "stored_format": sidecar["stored_format"],
        "architecture": sidecar["architecture"],
        "firmware": sidecar["firmware"],
        "verification_algorithm": sidecar["verification_algorithm"],
        "verification_digest": sidecar["verification_digest"],
    }


def capture_process(
    label: str, record: dict[str, Any], expected_pid: int
) -> dict[str, Any]:
    pid = record.get("pid")
    require(pid == expected_pid and isinstance(pid, int), f"{label} pid disagrees with state")
    start_ticks = process_start_ticks(pid)
    require(start_ticks is not None, f"{label} pid {pid} is not live")
    recorded_ticks = record.get("start_time_ticks")
    if recorded_ticks is not None:
        require(start_ticks == recorded_ticks, f"{label} pid identity changed")
    fields = status_fields(pid)
    uid_fields = fields.get("Uid", "").split()
    require(uid_fields and int(uid_fields[0]) == os.getuid(), f"{label} has the wrong uid")
    cap_eff = fields.get("CapEff")
    require(cap_eff is not None, f"{label} status omitted CapEff")
    if label in {"shim", "vmm"}:
        require(int(cap_eff, 16) == 0, f"{label} has effective host capabilities")
    recorded_executable = record.get("executable")
    require(
        isinstance(recorded_executable, str) and Path(recorded_executable).is_absolute(),
        f"{label} identity omitted an absolute executable",
    )
    proc_exe = Path("/proc") / str(pid) / "exe"
    proc_exe_access_error: str | None = None
    try:
        executable_link = os.readlink(proc_exe)
        executable_metadata = proc_exe.stat()
        executable_hash = sha256(proc_exe)
    except PermissionError as error:
        executable = Path(recorded_executable)
        try:
            executable_metadata = executable.stat()
            executable_hash = sha256(executable)
        except OSError as executable_error:
            raise AcceptanceError(
                f"cannot inventory {label} recorded executable: {executable_error}"
            ) from executable_error
        executable_link = str(executable)
        proc_exe_access_error = str(error)
    except OSError as error:
        raise AcceptanceError(f"cannot inventory {label} pid {pid}: {error}") from error
    proc_cmdline_access_error: str | None = None
    try:
        cmdline: bytes | None = (Path("/proc") / str(pid) / "cmdline").read_bytes()
    except PermissionError as error:
        cmdline = None
        proc_cmdline_access_error = str(error)
    except OSError as error:
        raise AcceptanceError(f"cannot inventory {label} pid {pid}: {error}") from error
    require(cmdline is None or len(cmdline) <= 1024 * 1024, f"{label} cmdline exceeds 1 MiB")
    require(
        executable_metadata.st_dev == record.get("executable_dev")
        and executable_metadata.st_ino == record.get("executable_ino"),
        f"{label} executable identity disagrees with identity.json",
    )
    require(
        executable_link == recorded_executable,
        f"{label} executable path disagrees with identity.json",
    )
    process_group = os.getpgid(pid)
    require(
        process_group == record.get("process_group"),
        f"{label} process group disagrees with identity.json",
    )
    argv_hex = record.get("argv_hex")
    launch_argv_hex = record.get("launch_argv_hex")
    return {
        "pid": pid,
        "process_group": process_group,
        "uid": int(uid_fields[0]),
        "gid": int(fields.get("Gid", "-1").split()[0]),
        "cap_eff": cap_eff,
        "start_time_ticks": start_ticks,
        "executable": executable_link,
        "executable_dev": executable_metadata.st_dev,
        "executable_ino": executable_metadata.st_ino,
        "executable_sha256": executable_hash,
        "proc_exe_accessible": proc_exe_access_error is None,
        "proc_exe_access_error": proc_exe_access_error,
        "cmdline_sha256": bytes_sha256(cmdline) if cmdline is not None else None,
        "cmdline_hex": (
            [part.hex() for part in cmdline.rstrip(b"\0").split(b"\0")]
            if cmdline is not None
            else None
        ),
        "proc_cmdline_accessible": proc_cmdline_access_error is None,
        "proc_cmdline_access_error": proc_cmdline_access_error,
        "argv_hex": argv_hex,
        "argv": decode_argv_hex(argv_hex, label),
        "launch_artifact": record.get("launch_artifact"),
        "launch_argv_hex": launch_argv_hex,
        "launch_argv": (
            decode_argv_hex(launch_argv_hex, f"{label} launch")
            if launch_argv_hex is not None
            else None
        ),
        "launch_binding": record.get("launch_binding"),
        "launch_sha256": record.get("launch_sha256"),
    }


def runtime_inventory(harness: Harness, name: str) -> dict[str, Any]:
    runtime = harness.home / "run" / name
    identity_path = runtime / "identity.json"
    identity = read_json(identity_path)
    state = harness.state(name)
    require(state["status"] == "running", f"{name} is not running for inventory")
    result: dict[str, dict[str, Any]] = {}
    result["shim"] = capture_process("shim", identity["shim"], state["shim_pid"])
    result["vmm"] = capture_process("vmm", identity["vmm"], state["vmm_pid"])
    sidecars = identity.get("sidecars")
    require(isinstance(sidecars, dict), "identity sidecars is not an object")
    require(sidecars.keys() == state["sidecar_pids"].keys(), "sidecar identity labels disagree with state")
    for label, record in sidecars.items():
        result[label] = capture_process(label, record, state["sidecar_pids"][label])
    return {
        "identity_sha256": sha256(identity_path),
        "launch_plan_sha256": sha256(runtime / "launch.json"),
        "processes": result,
    }


def assert_main_runtime(
    harness: Harness,
    fixtures: dict[str, Any],
    tcp_port: int,
    udp_port: int,
    pins: dict[str, Any],
) -> dict[str, Any]:
    name = "m3-main"
    runtime = harness.home / "run" / name
    machine = harness.home / "data" / "machines" / name
    plan = read_json(runtime / "launch.json")
    vmconfig = read_json(machine / "vmconfig.json")
    network = plan["network"]
    require(network["mode"] == "passt", "launch plan did not select passt")
    expected_passt_args = [
        "--foreground",
        "--one-off",
        "--vhost-user",
        "--socket",
        str(runtime / "net.sock"),
        "--log-file",
        str(machine / "passt.log"),
        "-t",
        f"127.0.0.1/{tcp_port}:80",
        "-u",
        f"127.0.0.1/{udp_port}:5353",
        "--repair-path",
        "none",
    ]
    actual_passt_args = decode_launch_args(network["args"], "passt")
    require(actual_passt_args == expected_passt_args, f"passt argv changed: {actual_passt_args!r}")
    require(network["forwards"] == [
        f"127.0.0.1:{tcp_port}:80",
        f"udp:127.0.0.1:{udp_port}:5353",
    ], "launch-plan forwards changed")
    filesystems = plan["filesystems"]
    require(isinstance(filesystems, list) and len(filesystems) == 2, "expected two virtiofsd plans")
    expected_mounts = [
        (0, "share0", fixtures["rw"], "/work", False),
        (1, "share1", fixtures["ro"], "/readonly", True),
    ]
    for item, (index, tag, host, guest, readonly) in zip(filesystems, expected_mounts, strict=True):
        require(item["index"] == index and item["tag"] == tag, "virtiofs tag/index changed")
        require(item["host"] == str(host) and item["guest"] == guest, "virtiofs path changed")
        require(item["readonly"] is readonly, "virtiofs readonly mapping changed")
        require(item["sandbox"] == "namespace", "virtiofsd did not use --sandbox namespace")
        expected_args = [
            "--socket-path",
            str(runtime / f"fs{index}.sock"),
            "--shared-dir",
            str(host),
            "--sandbox",
            "namespace",
            "--cache",
            "auto",
            "--announce-submounts",
        ]
        if readonly:
            expected_args.append("--readonly")
        expected_args.extend(["--log-level", "warn"])
        actual_args = decode_launch_args(item["args"], f"virtiofsd-{index}")
        require(actual_args == expected_args, f"virtiofsd-{index} argv changed: {actual_args!r}")
        pid_file = runtime / f"fs{index}.sock.pid"
        require(stat.S_IMODE(pid_file.stat().st_mode) == 0o600, f"virtiofsd-{index} pid mode changed")
    net = vmconfig.get("net")
    require(isinstance(net, list) and len(net) == 1, "VmConfig did not contain one net device")
    require(net[0].get("vhost_user") is True, "VmConfig omitted vhost_user=true")
    require(net[0].get("vhost_mode") == "Client", "VmConfig vhost_mode is not Client")
    require(net[0].get("vhost_socket") == str(runtime / "net.sock"), "VmConfig net socket changed")
    require("tap" not in net[0] and "ip" not in net[0] and "mask" not in net[0], "passt VmConfig has tap addressing")
    fs_devices = vmconfig.get("fs")
    require(isinstance(fs_devices, list) and len(fs_devices) == 2, "VmConfig fs count changed")
    for index, tag in enumerate(("share0", "share1")):
        require(
            fs_devices[index] == {
                "num_queues": 1,
                "queue_size": 1024,
                "socket": str(runtime / f"fs{index}.sock"),
                "tag": tag,
            },
            f"VmConfig fs{index} changed",
        )
    inventory = runtime_inventory(harness, name)
    processes = inventory["processes"]
    require(set(processes) == {"shim", "vmm", "passt", "virtiofsd-0", "virtiofsd-1"}, "process inventory labels changed")
    require(processes["vmm"]["launch_sha256"] == pins["cloud-hypervisor"]["sha256"], "VMM launch hash changed")
    require(processes["passt"]["launch_sha256"] == pins["passt"]["sha256"], "passt launch hash changed")
    for label in ("virtiofsd-0", "virtiofsd-1"):
        require(processes[label]["launch_sha256"] == pins["virtiofsd"]["sha256"], f"{label} launch hash changed")
    return {
        "launch_plan_sha256": sha256(runtime / "launch.json"),
        "vmconfig_sha256": sha256(machine / "vmconfig.json"),
        "passt_args": expected_passt_args,
        "filesystems": filesystems,
        "vmconfig_net": net,
        "vmconfig_fs": fs_devices,
        "inventory": inventory,
    }


def seed_snapshot(harness: Harness, name: str) -> dict[str, Any]:
    seed = harness.home / "data" / "machines" / name / "seed"
    user = read_bounded(seed / "user-data")
    network = read_bounded(seed / "network-config")
    digest = hashlib.sha256()
    digest.update(b"firestone-instance-v1\0")
    digest.update(len(user).to_bytes(8, "big"))
    digest.update(user)
    digest.update(len(network).to_bytes(8, "big"))
    digest.update(network)
    expected = f"iid-{name}-{digest.hexdigest()[:12]}"
    metadata = read_bounded(seed / "meta-data")
    expected_metadata = (
        f"instance-id: {json.dumps(expected)}\n"
        f"local-hostname: {json.dumps(name)}\n"
    ).encode()
    require(metadata == expected_metadata, "seed meta-data does not contain the exact expected instance-id")
    state_id = harness.state(name).get("instance_id")
    require(state_id == expected, f"state instance-id {state_id!r} differs from seed {expected!r}")
    return {
        "instance_id": expected,
        "identity_digest": digest.hexdigest(),
        "meta_data": stream_facts(metadata),
        "user_data": stream_facts(user),
        "network_config": stream_facts(network),
    }


def trust_snapshot(harness: Harness, name: str) -> dict[str, Any]:
    path = harness.home / "data" / "machines" / name / "known_hosts"
    value = read_bounded(path, 1024 * 1024)
    require(value, f"{name} known_hosts is empty")
    require(stat.S_IMODE(path.stat().st_mode) == 0o600, f"{name} known_hosts mode changed")
    return {"mode": "0600", "bytes": len(value), "sha256": bytes_sha256(value)}


def direct_ssh(
    harness: Harness,
    name: str,
    private_key: Path,
    trust_path: Path,
    marker: str,
) -> dict[str, Any]:
    write_private(trust_path, b"", 0o600)
    proxy = shlex.join([str(harness.binary), "_vsock-proxy", name, "22"])
    completed = harness.run(
        [
            "ssh",
            "-o",
            f"ProxyCommand={proxy}",
            "-o",
            f"IdentityFile={private_key}",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            f"UserKnownHostsFile={trust_path}",
            "-o",
            "StrictHostKeyChecking=accept-new",
            "-o",
            "LogLevel=ERROR",
            f"root@firestone.{name}",
            "printf",
            marker,
        ],
        timeout=60,
    )
    require(completed.stdout == marker.encode(), "configured user SSH key returned the wrong marker")
    require(not completed.stderr, f"configured user SSH key wrote stderr: {compact_bytes(completed.stderr)}")
    return {
        "marker": marker,
        "exit_code": completed.returncode,
        "stdout": stream_facts(completed.stdout),
        "known_hosts": {
            "mode": f"{stat.S_IMODE(trust_path.stat().st_mode):04o}",
            "sha256": sha256(trust_path),
        },
    }


def verify_cloud_merge(harness: Harness, version: int, metric: int) -> dict[str, Any]:
    network_probe = (
        "import json; "
        "d=json.load(open('/run/cloud-init/network-config.json')); "
        "v=d['ethernets']['passt0']['dhcp4-overrides']['route-metric']; "
        "print('network_metric='+str(v))"
    )
    script = f"""
set -eu
cloud-init status --wait --long >/tmp/firestone-cloud-status
[ "$(hostname)" = "m3-user-v{version}" ]
[ "$(cat /etc/firestone-user-version)" = "v{version}" ]
[ "$(cat /var/tmp/firestone-user-runcmd)" = "v{version}-runcmd" ]
test -f /etc/systemd/system/firestone-sshd.socket
test "$(systemctl is-active firestone-sshd.socket)" = active
test "$(systemctl show -p SubState --value firestone-sshd.socket)" = listening
python3 -c {shlex.quote(network_probe)}
printf 'hostname=m3-user-v{version}\n'
printf 'user_write_files=v{version}\n'
printf 'user_runcmd=v{version}-runcmd\n'
printf 'firestone_socket=active/listening\n'
printf 'cloud_init=done\n'
""".strip()
    completed = harness.guest_script("m3-main", script, timeout=300)
    require(not completed.stderr, f"merged cloud-init check wrote stderr: {compact_bytes(completed.stderr)}")
    facts: dict[str, str] = {}
    for line in completed.stdout.decode().splitlines():
        key, separator, value = line.partition("=")
        require(separator == "=", f"malformed cloud-init fact: {line!r}")
        facts[key] = value
    expected = {
        "network_metric": str(metric),
        "hostname": f"m3-user-v{version}",
        "user_write_files": f"v{version}",
        "user_runcmd": f"v{version}-runcmd",
        "firestone_socket": "active/listening",
        "cloud_init": "done",
    }
    require(facts == expected, f"merged cloud-init facts changed: {facts!r}")
    return {"facts": facts, "verify_10_merged_user_and_firestone_parts": True}


def forwarding_scenario(
    harness: Harness, tcp_port: int, udp_port: int
) -> dict[str, Any]:
    http_marker = f"FIRESTONE_M3_HTTP_{uuid.uuid4().hex}\n"
    udp_marker = f"FIRESTONE_M3_UDP_{uuid.uuid4().hex}".encode()
    http_script = f"""
set -eu
install -d -m 0755 /run/firestone-m3-http
printf %s {shlex.quote(http_marker)} > /run/firestone-m3-http/marker.txt
nohup python3 -m http.server 80 --bind 0.0.0.0 --directory /run/firestone-m3-http </dev/null >/tmp/firestone-m3-http.log 2>&1 &
printf '%s\n' "$!"
""".strip()
    http = harness.guest_script("m3-main", http_script)
    require(http.stdout.strip().isdigit(), "guest HTTP server did not return a pid")
    udp_program = f"""import socket
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.bind(('0.0.0.0', 5353))
while True:
    data, address = s.recvfrom(65535)
    s.sendto(b'FIRESTONE_M3_UDP_REPLY:' + data, address)
"""
    udp_script = (
        "set -eu\n"
        f"nohup python3 -c {shlex.quote(udp_program)} </dev/null "
        ">/tmp/firestone-m3-udp.log 2>&1 &\n"
        "printf '%s\\n' \"$!\""
    )
    udp = harness.guest_script("m3-main", udp_script)
    require(udp.stdout.strip().isdigit(), "guest UDP server did not return a pid")

    curl = harness.run(
        [
            "curl",
            "--fail",
            "--silent",
            "--show-error",
            "--retry",
            "20",
            "--retry-all-errors",
            "--retry-connrefused",
            "--retry-delay",
            "1",
            "--retry-max-time",
            "30",
            f"http://127.0.0.1:{tcp_port}/marker.txt",
        ],
        timeout=40,
    )
    require(curl.stdout == http_marker.encode(), "curl returned the wrong forwarded HTTP marker")
    require(not curl.stderr, f"curl wrote stderr: {compact_bytes(curl.stderr)}")

    started = time.monotonic()
    deadline = started + 20
    expected_udp = b"FIRESTONE_M3_UDP_REPLY:" + udp_marker
    observed = b""
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.settimeout(0.5)
    try:
        while time.monotonic() < deadline:
            sock.sendto(udp_marker, ("127.0.0.1", udp_port))
            try:
                observed, _ = sock.recvfrom(65535)
            except TimeoutError:
                continue
            if observed == expected_udp:
                break
    finally:
        sock.close()
    require(observed == expected_udp, "UDP forwarding did not return the expected marker")

    vsock_marker = f"FIRESTONE_M3_VSOCK_{uuid.uuid4().hex}"
    vsock = harness.shell("m3-main", ["printf", vsock_marker])
    require(vsock.stdout == vsock_marker.encode(), "SSH over vsock failed while passt was active")
    require(not vsock.stderr, f"vsock shell wrote stderr: {compact_bytes(vsock.stderr)}")
    return {
        "tcp": {
            "host": f"127.0.0.1:{tcp_port}",
            "guest_port": 80,
            "curl_marker": http_marker.rstrip(),
            "curl": stream_facts(curl.stdout),
        },
        "udp": {
            "host": f"127.0.0.1:{udp_port}",
            "guest_port": 5353,
            "request_sha256": bytes_sha256(udp_marker),
            "response_sha256": bytes_sha256(observed),
            "elapsed_ms": round((time.monotonic() - started) * 1000, 3),
        },
        "ssh_over_vsock": {
            "marker": vsock_marker,
            "stdout": stream_facts(vsock.stdout),
        },
    }


def mount_scenario(harness: Harness, fixtures: dict[str, Any]) -> dict[str, Any]:
    marker = f"FIRESTONE_M3_MOUNT_{uuid.uuid4().hex}\n"
    script = f"""
set -eu
[ "$(findmnt -n -o FSTYPE /work)" = "virtiofs" ]
[ "$(findmnt -n -o FSTYPE /readonly)" = "virtiofs" ]
findmnt -n -o OPTIONS /work | tr ',' '\n' | grep -qx rw
findmnt -n -o OPTIONS /readonly | tr ',' '\n' | grep -qx ro
printf %s {shlex.quote(marker)} > /work/guest.txt
sync
[ "$(cat /readonly/host.txt)" = "FIRESTONE_M3_READONLY_SOURCE" ]
if {{ printf denied > /readonly/denied; }} 2>/tmp/firestone-m3-ro-error; then
  exit 71
fi
test ! -e /readonly/denied
printf 'rw_source=%s\n' "$(findmnt -n -o SOURCE /work)"
printf 'ro_source=%s\n' "$(findmnt -n -o SOURCE /readonly)"
printf 'rw_options=%s\n' "$(findmnt -n -o OPTIONS /work)"
printf 'ro_options=%s\n' "$(findmnt -n -o OPTIONS /readonly)"
printf 'ro_denied=true\n'
""".strip()
    completed = harness.guest_script("m3-main", script)
    require(not completed.stderr, f"mount checks wrote stderr: {compact_bytes(completed.stderr)}")
    host_file = fixtures["rw"] / "guest.txt"
    value = read_bounded(host_file)
    require(value == marker.encode(), "guest write was not visible on the host")
    metadata = host_file.stat()
    require(metadata.st_uid == os.getuid(), "guest-created mount file has the wrong host uid")
    facts = {}
    for line in completed.stdout.decode().splitlines():
        key, separator, value_text = line.partition("=")
        require(separator == "=", f"malformed mount fact: {line!r}")
        facts[key] = value_text
    require(facts.get("rw_source") == "share0", "rw mount did not use share0")
    require(facts.get("ro_source") == "share1", "ro mount did not use share1")
    require(facts.get("ro_denied") == "true", "readonly mount accepted a guest write")
    return {
        "guest_write_marker": marker.rstrip(),
        "host_file": {
            "path": str(host_file),
            "uid": metadata.st_uid,
            "mode": f"{stat.S_IMODE(metadata.st_mode):04o}",
            "sha256": bytes_sha256(value),
        },
        "facts": facts,
        "multiple_tags": ["share0", "share1"],
        "readonly_write_denied": True,
    }


def teardown_evidence(
    harness: Harness,
    name: str,
    inventory: dict[str, Any],
    expected_status: str,
) -> dict[str, Any]:
    started = time.monotonic()
    state, state_after = harness.wait_for_state(
        name, {expected_status}, TEARDOWN_TIMEOUT_SECONDS
    )
    runtime = harness.home / "run" / name
    deadline = time.monotonic() + TEARDOWN_TIMEOUT_SECONDS
    while runtime.exists() and time.monotonic() < deadline:
        time.sleep(0.02)
    require(not runtime.exists(), f"{name} runtime directory survived teardown")
    processes = wait_for_processes_gone(
        inventory["processes"], TEARDOWN_TIMEOUT_SECONDS
    )
    state = harness.state(name)
    require(state.get("status") == expected_status, f"{name} final state changed during teardown")
    require(state.get("vmm_pid") is None, f"{name} retained a VMM pid")
    require(state.get("shim_pid") is None, f"{name} retained a shim pid")
    require(state.get("sidecar_pids") == {}, f"{name} retained sidecar pids")
    live_home = harness._live_home_processes()
    require(not live_home, f"processes still reference FIRESTONE_HOME: {live_home}")
    return {
        "state": expected_status,
        "state_after_ms": round(state_after * 1000, 3),
        "total_elapsed_ms": round((time.monotonic() - started) * 1000, 3),
        "runtime_removed": True,
        "sockets_removed": True,
        "state_process_ids_cleared": True,
        "processes": processes,
        "last_exit": state.get("last_exit"),
    }


def assert_effective_result(
    payload: dict[str, Any], fixtures: dict[str, Any], tcp_port: int, udp_port: int
) -> None:
    require(
        payload["forwards"]
        == [
            f"127.0.0.1:{tcp_port}:80",
            f"udp:127.0.0.1:{udp_port}:5353",
        ],
        f"effective forward result changed: {payload['forwards']!r}",
    )
    require(
        payload["mounts"]
        == [
            f"{fixtures['rw']} -> /work",
            f"{fixtures['ro']} -> /readonly",
        ],
        f"effective mount result changed: {payload['mounts']!r}",
    )


def run_main_scenario(
    harness: Harness,
    fixtures: dict[str, Any],
    tcp_port: int,
    udp_port: int,
    pins: dict[str, Any],
) -> dict[str, Any]:
    name = "m3-main"
    create_records, create_frame, _ = harness.json_command(
        "create",
        name,
        "ubuntu:24.04",
        "-p",
        f"127.0.0.1:{tcp_port}:80",
        "-p",
        f"udp:127.0.0.1:{udp_port}:5353",
        "--mount",
        f"{fixtures['rw']}:/work",
        "--mount",
        f"{fixtures['ro']}:/readonly:ro",
        "--user-data",
        str(fixtures["user_data"]),
        "--cloud-init-network-config",
        str(fixtures["network_data"]),
        "--ssh-key",
        str(fixtures["active_public"]),
        action="create",
    )
    created = harness.result_payload(create_records, "create")
    require(created["state"]["status"] == "created", "m3-main was not created")
    require(
        created["spec"]["network"]["mode"] == "passt",
        "m3-main did not inherit the default passt mode",
    )

    initial_started = time.monotonic()
    initial_payload, initial_frame = harness.start(name)
    initial_elapsed = time.monotonic() - initial_started
    assert_effective_result(initial_payload, fixtures, tcp_port, udp_port)
    harness.evidence["image"] = image_evidence(harness, name)
    initial_runtime = assert_main_runtime(harness, fixtures, tcp_port, udp_port, pins)
    initial_seed = seed_snapshot(harness, name)
    initial_trust = trust_snapshot(harness, name)
    known_hosts_path = harness.home / "data" / "machines" / name / "known_hosts"
    initial_trust_lines = set(read_bounded(known_hosts_path).splitlines())
    require(initial_trust_lines, "initial known_hosts has no trust entries")
    forwarding = forwarding_scenario(harness, tcp_port, udp_port)
    mounts = mount_scenario(harness, fixtures)
    merged_v1 = verify_cloud_merge(harness, 1, 111)
    key1 = direct_ssh(
        harness,
        name,
        fixtures["key1"],
        fixtures["root"] / "direct-known-hosts-v1",
        f"FIRESTONE_M3_KEY1_{uuid.uuid4().hex}",
    )

    normal_stop_payload, normal_stop_frame = harness.stop(name)
    normal_teardown = teardown_evidence(
        harness, name, initial_runtime["inventory"], "stopped"
    )

    unchanged_start_payload, unchanged_start_frame = harness.start(name)
    assert_effective_result(unchanged_start_payload, fixtures, tcp_port, udp_port)
    unchanged_start_seed = seed_snapshot(harness, name)
    unchanged_start_trust = trust_snapshot(harness, name)
    require(
        unchanged_start_seed == initial_seed,
        "unchanged stop/start changed exact seed identity or rendered inputs",
    )
    require(
        unchanged_start_trust == initial_trust,
        "unchanged stop/start changed SSH host trust",
    )
    before_changed_restart = assert_main_runtime(
        harness, fixtures, tcp_port, udp_port, pins
    )

    write_private(fixtures["active_public"], read_bounded(fixtures["key2_public"]), 0o600)
    write_private(fixtures["user_data"], user_data(2), 0o600)
    write_private(fixtures["network_data"], network_data(222), 0o600)
    changed_sources = fixture_hashes(fixtures)
    changed_started = time.monotonic()
    changed_records, changed_frame, _ = harness.json_command(
        "restart", name, timeout=START_TIMEOUT_SECONDS, action="restart"
    )
    changed_elapsed = time.monotonic() - changed_started
    changed_payload = harness.result_payload(changed_records, "restart")
    assert_effective_result(changed_payload, fixtures, tcp_port, udp_port)
    replaced_on_change = wait_for_processes_gone(
        before_changed_restart["inventory"]["processes"],
        TEARDOWN_TIMEOUT_SECONDS,
    )
    changed_runtime = assert_main_runtime(harness, fixtures, tcp_port, udp_port, pins)
    changed_seed = seed_snapshot(harness, name)
    changed_trust = trust_snapshot(harness, name)
    require(
        changed_seed["instance_id"] != initial_seed["instance_id"],
        "changed SSH key/user-data/network-config preserved the instance-id",
    )
    require(
        changed_trust["sha256"] != initial_trust["sha256"],
        "seed identity change did not replace stale known_hosts trust",
    )
    changed_trust_lines = set(read_bounded(known_hosts_path).splitlines())
    require(changed_trust_lines, "changed known_hosts has no trust entries")
    require(
        initial_trust_lines.isdisjoint(changed_trust_lines),
        "stale known_hosts entries survived the seed identity change",
    )
    merged_v2 = verify_cloud_merge(harness, 2, 222)
    key2 = direct_ssh(
        harness,
        name,
        fixtures["key2"],
        fixtures["root"] / "direct-known-hosts-v2",
        f"FIRESTONE_M3_KEY2_{uuid.uuid4().hex}",
    )

    unchanged_restart_started = time.monotonic()
    unchanged_records, unchanged_frame, _ = harness.json_command(
        "restart", name, timeout=START_TIMEOUT_SECONDS, action="restart"
    )
    unchanged_restart_elapsed = time.monotonic() - unchanged_restart_started
    unchanged_payload = harness.result_payload(unchanged_records, "restart")
    assert_effective_result(unchanged_payload, fixtures, tcp_port, udp_port)
    replaced_unchanged = wait_for_processes_gone(
        changed_runtime["inventory"]["processes"], TEARDOWN_TIMEOUT_SECONDS
    )
    unchanged_runtime = assert_main_runtime(harness, fixtures, tcp_port, udp_port, pins)
    unchanged_seed = seed_snapshot(harness, name)
    unchanged_trust = trust_snapshot(harness, name)
    require(unchanged_seed == changed_seed, "unchanged restart changed exact seed identity")
    require(unchanged_trust == changed_trust, "unchanged restart changed known_hosts trust")
    verify_cloud_merge(harness, 2, 222)

    vmm = unchanged_runtime["inventory"]["processes"]["vmm"]
    gone, _ = process_gone(vmm["pid"], vmm["start_time_ticks"])
    require(not gone, "VMM disappeared before crash injection")
    crash_started = time.monotonic()
    os.kill(vmm["pid"], signal.SIGKILL)
    crash_teardown = teardown_evidence(
        harness, name, unchanged_runtime["inventory"], "failed"
    )
    crash_teardown["signal"] = signal.SIGKILL
    crash_teardown["elapsed_from_signal_ms"] = round(
        (time.monotonic() - crash_started) * 1000, 3
    )
    harness.remove(name)

    return {
        "create": {"frame": create_frame, "result": created},
        "initial_start": {
            "elapsed_ms": round(initial_elapsed * 1000, 3),
            "frame": initial_frame,
            "result": initial_payload,
            "runtime": initial_runtime,
            "seed": initial_seed,
            "known_hosts": initial_trust,
        },
        "e2e_3": forwarding,
        "e2e_4": mounts,
        "verify_10_initial_merge": merged_v1,
        "configured_key_v1": key1,
        "verify_7_normal_stop": {
            "result": normal_stop_payload,
            "frame": normal_stop_frame,
            "teardown": normal_teardown,
        },
        "unchanged_stop_start": {
            "frame": unchanged_start_frame,
            "result": unchanged_start_payload,
            "seed_preserved": True,
            "trust_preserved": True,
        },
        "e2e_8_changed_restart": {
            "elapsed_ms": round(changed_elapsed * 1000, 3),
            "frame": changed_frame,
            "result": changed_payload,
            "changed_sources": changed_sources,
            "old_processes_gone": replaced_on_change,
            "runtime": changed_runtime,
            "seed_before": initial_seed,
            "seed_after": changed_seed,
            "known_hosts_before": initial_trust,
            "known_hosts_after": changed_trust,
            "stale_known_hosts_entries_absent": True,
            "new_key": key2,
            "merged_cloud_init": merged_v2,
        },
        "e2e_8_unchanged_restart": {
            "elapsed_ms": round(unchanged_restart_elapsed * 1000, 3),
            "frame": unchanged_frame,
            "result": unchanged_payload,
            "old_processes_gone": replaced_unchanged,
            "runtime": unchanged_runtime,
            "instance_id_preserved": True,
            "known_hosts_preserved": True,
        },
        "verify_7_vmm_crash": crash_teardown,
    }


def setup_tap(harness: Harness) -> dict[str, Any]:
    name = f"fst{os.getpid():x}{uuid.uuid4().hex[:5]}"[:15]
    sysfs = Path("/sys/class/net") / name
    require(not sysfs.exists(), f"generated tap name already exists: {name}")
    completed = harness.run(
        [
            "sudo",
            "-n",
            "ip",
            "tuntap",
            "add",
            "dev",
            name,
            "mode",
            "tap",
            "user",
            str(os.getuid()),
        ],
        timeout=15,
        check=False,
    )
    require(
        completed.returncode == 0,
        "cannot create the required user-owned tap with passwordless sudo host setup: "
        f"{compact_bytes(completed.stderr)}",
    )
    harness.tap_name = name
    harness.run(["sudo", "-n", "ip", "link", "set", "dev", name, "up"], timeout=15)
    shown = harness.run(["ip", "-details", "tuntap", "show", "dev", name])
    require(name.encode() in shown.stdout, "ip did not report the created tap")
    tun_flags_text = (sysfs / "tun_flags").read_text(encoding="utf-8").strip()
    tun_flags = int(tun_flags_text, 0)
    require(tun_flags & IFF_TAP != 0, f"{name} is not a TAP device")
    descriptor = os.open("/dev/net/tun", os.O_RDWR | os.O_CLOEXEC)
    try:
        request = struct.pack("16sH22x", name.encode(), IFF_TAP | IFF_NO_PI)
        fcntl.ioctl(descriptor, TUNSETIFF, request)
    finally:
        os.close(descriptor)
    result = {
        "name": name,
        "created_by": "sudo -n ip tuntap add (test-host setup only)",
        "owner_uid": os.getuid(),
        "ip_output": compact_bytes(shown.stdout, 4_096).strip(),
        "ip_output_sha256": bytes_sha256(shown.stdout),
        "tun_flags": tun_flags_text,
        "unprivileged_tunsetiff_probe": True,
        "cleanup": {"removed": False},
    }
    harness.evidence["host_setup"]["tap"] = result
    return result


def run_tap_scenario(harness: Harness) -> dict[str, Any]:
    setup = setup_tap(harness)
    name = "m3-tap"
    create_records, create_frame, _ = harness.json_command(
        "create",
        name,
        "ubuntu:24.04",
        "--net",
        "tap",
        "--tap",
        setup["name"],
        "--no-provisioning",
        action="create",
    )
    created = harness.result_payload(create_records, "create")
    start_payload, start_frame = harness.start(name, wait=False)
    require(start_payload["forwards"] == [], "tap start reported passt forwards")
    require(start_payload["mounts"] == [], "tap start reported mounts")
    runtime = harness.home / "run" / name
    plan = read_json(runtime / "launch.json")
    network = plan["network"]
    require(network["mode"] == "tap", "tap launch plan mode changed")
    require(network["name"] == setup["name"], "tap launch plan name changed")
    require(network.get("ip") is None and network.get("mask") is None, "tap plan set ip/mask")
    vmconfig = read_json(harness.home / "data" / "machines" / name / "vmconfig.json")
    net = vmconfig.get("net")
    require(isinstance(net, list) and len(net) == 1, "tap VmConfig net count changed")
    require(net[0].get("tap") == setup["name"], "tap VmConfig name changed")
    require("ip" not in net[0] and "mask" not in net[0], "tap VmConfig serialized ip/mask")
    require(
        "vhost_user" not in net[0]
        and "vhost_socket" not in net[0]
        and "vhost_mode" not in net[0],
        "tap VmConfig serialized vhost-user fields",
    )
    inventory = runtime_inventory(harness, name)
    require(set(inventory["processes"]) == {"shim", "vmm"}, "tap start launched sidecars")
    require(inventory["processes"]["vmm"]["cap_eff"] == "0000000000000000", "tap VMM has capabilities")
    stop_payload, stop_frame = harness.stop(name, force=True)
    teardown = teardown_evidence(harness, name, inventory, "stopped")
    harness.remove(name)
    cleanup_errors = harness.remove_tap()
    require(not cleanup_errors, "; ".join(cleanup_errors))
    return {
        "host_setup": setup,
        "create": {"frame": create_frame, "result": created},
        "start": {"frame": start_frame, "result": start_payload},
        "launch_plan_network": network,
        "vmconfig_net": net,
        "inventory": inventory,
        "firestone_user_cap_eff": self_capabilities()["cap_eff"],
        "vmm_cap_eff": inventory["processes"]["vmm"]["cap_eff"],
        "ip_and_mask_absent": True,
        "stop": {"frame": stop_frame, "result": stop_payload, "teardown": teardown},
        "tap_removed": True,
    }


def run_acceptance(harness: Harness) -> None:
    require(sys.platform == "linux", "M3 acceptance requires Linux")
    require(platform.machine() == "x86_64", "M3 acceptance requires x86_64")
    require(os.geteuid() != 0, "M3 product acceptance must run as an unprivileged user")
    require(
        os.access("/dev/kvm", os.R_OK | os.W_OK),
        "/dev/kvm is not readable and writable",
    )
    for program in (
        "cargo",
        "curl",
        "git",
        "ip",
        "qemu-img",
        "ssh",
        "ssh-keygen",
        "sudo",
        "unshare",
    ):
        require(shutil.which(program) is not None, f"required host program is missing: {program}")

    commit = harness.run(["git", "rev-parse", "HEAD"]).stdout.decode().strip()
    if "FIRESTONE_BIN" not in os.environ:
        harness.run(
            ["cargo", "build", "--locked", "--bin", "firestone"], timeout=1_200
        )
    require(
        harness.binary.is_file() and os.access(harness.binary, os.X_OK),
        "firestone binary was not built",
    )
    kvm = Path("/dev/kvm").stat()
    harness.evidence["commit"] = commit
    harness.evidence["host"] = {
        "system": platform.system(),
        "release": platform.release(),
        "architecture": platform.machine(),
        "uid": os.getuid(),
        "euid": os.geteuid(),
        "capabilities": self_capabilities(),
        "kvm": {
            "read_write": True,
            "mode": f"{stat.S_IMODE(kvm.st_mode):04o}",
            "uid": kvm.st_uid,
            "gid": kvm.st_gid,
            "major": os.major(kvm.st_rdev),
            "minor": os.minor(kvm.st_rdev),
        },
        "firestone_sha256": sha256(harness.binary),
        "harness_sha256": sha256(Path(__file__)),
        "initial_home_mode": "0700",
        "initial_home_empty": True,
    }
    harness.evidence["harness_config"] = configure_timeouts(harness)
    setup_user_namespaces(harness)
    harness.evidence["scenarios"]["doctor"] = doctor_evidence(harness)
    artifacts = installed_artifacts(harness)
    passt = passt_evidence(harness)
    harness.evidence["artifacts"] = artifacts
    harness.evidence["pins"] = {
        "cloud-hypervisor": artifacts["cloud-hypervisor"],
        "edk2": artifacts["cloud-hypervisor-edk2"],
        "virtiofsd": artifacts["virtiofsd"],
        "passt": passt,
    }
    pins = {
        "cloud-hypervisor": artifacts["cloud-hypervisor"],
        "virtiofsd": artifacts["virtiofsd"],
        "passt": passt,
    }

    fixtures = create_fixtures(harness)
    initial_sources = fixture_hashes(fixtures)
    tcp_port, exact_8080 = choose_host_port(socket.SOCK_STREAM, 8080)
    udp_port, exact_udp = choose_host_port(socket.SOCK_DGRAM, 53530)
    harness.evidence["host_ports"] = {
        "tcp": {
            "selected": tcp_port,
            "preferred": 8080,
            "preferred_available": exact_8080,
        },
        "udp": {
            "selected": udp_port,
            "preferred": 53530,
            "preferred_available": exact_udp,
        },
    }
    harness.evidence["fixture_hashes_initial"] = initial_sources
    main = run_main_scenario(harness, fixtures, tcp_port, udp_port, pins)
    harness.evidence["scenarios"].update(main)
    harness.evidence["scenarios"]["verify_8_tap"] = run_tap_scenario(harness)


def install_harness_signal_handlers() -> None:
    def interrupted(signum: int, _frame: Any) -> None:
        raise AcceptanceError(f"M3 harness interrupted by signal {signum}")

    for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(signum, interrupted)


def main() -> int:
    if os.environ.get("FIRESTONE_E2E") != "1":
        print("skipped M3 KVM acceptance; set FIRESTONE_E2E=1 to run")
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
                "tap_removed": harness.tap_name is None,
                "userns_policy_restored": harness.userns_restore_value is None,
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
        print(f"M3 KVM acceptance failed: {failure}", file=sys.stderr)
        if harness is not None and harness.evidence_path.exists():
            print(
                f"failed evidence: {harness.evidence_path} "
                f"sha256={sha256(harness.evidence_path)}",
                file=sys.stderr,
            )
        return 1
    require(harness is not None, "M3 harness was not initialized")
    evidence_sha = sha256(harness.evidence_path)
    print(
        f"M3 KVM acceptance passed; evidence: {harness.evidence_path} "
        f"sha256={evidence_sha}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
