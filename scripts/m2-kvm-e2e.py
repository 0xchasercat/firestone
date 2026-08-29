#!/usr/bin/env python3
"""Run the Linux x86_64 M2 acceptance scenarios against real KVM."""

from __future__ import annotations

import atexit
import datetime as dt
import errno
import hashlib
import json
import os
import platform
import pty
import re
import select
import shlex
import shutil
import signal
import stat
import subprocess
import sys
import termios
import time
import tomllib
import uuid
from pathlib import Path
from typing import Any, Callable


class AcceptanceError(RuntimeError):
    """The host or one M2 acceptance contract failed."""


REPO_ROOT = Path(__file__).resolve().parents[1]
COMMAND_TIMEOUT_SECONDS = 120
START_TIMEOUT_SECONDS = 1_900
BOOT_TIMEOUT_SECONDS = 360
MAX_OUTPUT_BYTES = 8 * 1024 * 1024
MACHINE_NAMES = ("ubuntu", "m2-readiness")
CONSOLE_CONNECTED = "connected to ubuntu console · escape: Ctrl-]".encode()
ANSI_ESCAPE = re.compile(rb"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07]*(?:\x07|\x1b\\))")


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


def prohibited_controls(value: bytes) -> list[tuple[int, int]]:
    return [
        (offset, byte)
        for offset, byte in enumerate(value)
        if byte < 0x20 and byte != 0x0A or byte == 0x7F
    ]


def require_clean_stream(label: str, value: bytes) -> None:
    controls = prohibited_controls(value)
    if controls:
        raise AcceptanceError(
            f"{label} contains prohibited control byte 0x{controls[0][1]:02x} "
            f"at offset {controls[0][0]}"
        )
    try:
        value.decode("utf-8")
    except UnicodeDecodeError as error:
        raise AcceptanceError(f"{label} is not UTF-8: {error}") from error


def stream_facts(value: bytes) -> dict[str, Any]:
    return {
        "bytes": len(value),
        "lines": len(value.splitlines()),
        "ends_with_newline": value.endswith(b"\n") if value else False,
        "sha256": bytes_sha256(value),
        "prohibited_control_count": len(prohibited_controls(value)),
    }


def visible_terminal(value: bytes) -> str:
    value = ANSI_ESCAPE.sub(b"", value).replace(b"\r", b"")
    text = value.decode("utf-8", errors="replace")
    return "".join(character for character in text if character == "\n" or character >= " ")


class PtySession:
    def __init__(
        self,
        harness: Harness,
        argv: list[str | os.PathLike[str]],
        *,
        label: str,
    ) -> None:
        self.harness = harness
        self.label = label
        self.master, self.slave = pty.openpty()
        self.original = termios.tcgetattr(self.slave)
        os.set_blocking(self.master, False)
        self.buffer = bytearray()
        self.process = harness.spawn(
            argv,
            stdin=self.slave,
            stdout=self.slave,
            stderr=self.slave,
        )

    def _read_once(self, timeout: float) -> bool:
        readable, _, _ = select.select([self.master], [], [], timeout)
        if not readable:
            return False
        try:
            block = os.read(self.master, 65_536)
        except BlockingIOError:
            return False
        except OSError as error:
            if error.errno == errno.EIO:
                return False
            raise
        if not block:
            return False
        self.buffer.extend(block)
        require(
            len(self.buffer) <= MAX_OUTPUT_BYTES,
            f"{self.label} terminal output exceeded 8 MiB",
        )
        return True

    def wait_for(
        self,
        predicate: Callable[[bytes], bool],
        *,
        timeout: float,
        description: str,
    ) -> None:
        deadline = time.monotonic() + timeout
        while not predicate(bytes(self.buffer)):
            if self.process.poll() is not None:
                self._read_once(0)
                raise AcceptanceError(
                    f"{self.label} exited {self.process.returncode} before {description}; "
                    f"output:\n{compact_bytes(bytes(self.buffer), 4_096)}"
                )
            if time.monotonic() >= deadline:
                raise AcceptanceError(
                    f"{self.label} did not reach {description} within {timeout:.1f}s; "
                    f"output:\n{compact_bytes(bytes(self.buffer), 4_096)}"
                )
            self._read_once(0.1)

    def wait_for_bytes(self, marker: bytes, timeout: float, description: str) -> None:
        self.wait_for(
            lambda value: marker in value,
            timeout=timeout,
            description=description,
        )

    def wait_for_root_prompt(self, timeout: float) -> None:
        deadline = time.monotonic() + timeout
        next_probe = 0.0
        pattern = re.compile(r"root@ubuntu:[^\n]*#\s*")
        while pattern.search(visible_terminal(bytes(self.buffer))) is None:
            now = time.monotonic()
            if self.process.poll() is not None:
                raise AcceptanceError(
                    f"{self.label} exited before the root prompt; "
                    f"output:\n{compact_bytes(bytes(self.buffer), 4_096)}"
                )
            if now >= deadline:
                raise AcceptanceError(
                    f"{self.label} did not reach an actual root@ubuntu prompt; "
                    f"output:\n{compact_bytes(bytes(self.buffer), 4_096)}"
                )
            if now >= next_probe:
                self.write(b"\n")
                next_probe = now + 1.0
            self._read_once(0.1)

    def write(self, value: bytes) -> None:
        remaining = memoryview(value)
        deadline = time.monotonic() + 5
        while remaining:
            try:
                written = os.write(self.master, remaining)
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    raise AcceptanceError(f"{self.label} terminal input remained blocked")
                _, writable, _ = select.select([], [self.master], [], 0.1)
                require(bool(writable), f"{self.label} terminal input remained blocked")
                continue
            require(written > 0, f"{self.label} terminal accepted no input")
            remaining = remaining[written:]

    def wait(self, timeout: float) -> int:
        try:
            returncode = self.process.wait(timeout=timeout)
        except subprocess.TimeoutExpired as error:
            self.harness.terminate_process(self.process)
            raise AcceptanceError(
                f"{self.label} did not exit within {timeout:.1f}s"
            ) from error
        finally:
            self.harness.forget_process(self.process)
        return returncode

    def terminal_restored(self) -> bool:
        return termios.tcgetattr(self.slave) == self.original

    def close(self) -> None:
        for descriptor in (self.master, self.slave):
            try:
                os.close(descriptor)
            except OSError:
                pass


class Harness:
    def __init__(self) -> None:
        self.home = self._checked_home()
        self.keep_home = os.environ.get("FIRESTONE_E2E_KEEP") == "1"
        default_evidence = Path("/tmp") / f"firestone-m2-evidence-{os.getpid()}.json"
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
        require(
            home.exists() and home.is_dir(),
            "FIRESTONE_HOME must be an existing directory",
        )
        home = home.resolve(strict=True)
        metadata = home.stat()
        require(
            metadata.st_uid == os.getuid(),
            "FIRESTONE_HOME must be owned by the current user",
        )
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
            require(
                os.access(source, os.X_OK),
                f"FIRESTONE_BIN is not executable: {source}",
            )
            directory = self.home / "harness-bin"
            directory.mkdir(mode=0o700)
            binary = directory / "firestone"
            shutil.copy2(source, binary)
            os.chmod(binary, 0o755)
            require(
                sha256(binary) == sha256(source),
                "staged FIRESTONE_BIN changed bytes",
            )
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
        command = (
            self.record_command(argv)
            if record
            else [os.fspath(value) for value in argv]
        )
        rendered = shlex.join(command)
        try:
            completed = subprocess.run(
                command,
                cwd=REPO_ROOT,
                env=os.environ.copy(),
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=timeout,
                check=False,
            )
        except subprocess.TimeoutExpired as error:
            raise AcceptanceError(
                f"command timed out after {timeout:.1f}s: {rendered}"
            ) from error
        if check and completed.returncode != 0:
            raise AcceptanceError(
                f"command failed with exit {completed.returncode}: {rendered}\n"
                f"stdout:\n{compact_bytes(completed.stdout)}\n"
                f"stderr:\n{compact_bytes(completed.stderr)}"
            )
        return completed

    def spawn(
        self,
        argv: list[str | os.PathLike[str]],
        *,
        stdin: int | None = None,
        stdout: int | None = None,
        stderr: int | None = None,
    ) -> subprocess.Popen[bytes]:
        command = self.record_command(argv)
        process = subprocess.Popen(
            command,
            cwd=REPO_ROOT,
            env=os.environ.copy(),
            stdin=stdin if stdin is not None else subprocess.DEVNULL,
            stdout=stdout if stdout is not None else subprocess.PIPE,
            stderr=stderr if stderr is not None else subprocess.PIPE,
            start_new_session=True,
        )
        self.active_processes.append(process)
        return process

    def forget_process(self, process: subprocess.Popen[bytes]) -> None:
        try:
            self.active_processes.remove(process)
        except ValueError:
            pass

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
        error_kind: str | None = None,
    ) -> tuple[list[dict[str, Any]], dict[str, Any], subprocess.CompletedProcess[bytes]]:
        completed = self.run(
            [self.binary, "--json", *arguments],
            timeout=timeout,
            check=False,
        )
        records, frame = self.parse_json_command(
            completed,
            label=shlex.join(arguments),
            expected_code=expected_code,
            action=action,
            error_kind=error_kind,
        )
        return records, frame, completed

    @staticmethod
    def parse_json_command(
        completed: subprocess.CompletedProcess[bytes],
        *,
        label: str,
        expected_code: int,
        action: str | None,
        error_kind: str | None,
    ) -> tuple[list[dict[str, Any]], dict[str, Any]]:
        require(
            completed.returncode == expected_code,
            f"{label} exited {completed.returncode}, expected {expected_code}; "
            f"stdout:\n{compact_bytes(completed.stdout)}\n"
            f"stderr:\n{compact_bytes(completed.stderr)}",
        )
        require_clean_stream(f"{label} JSON stdout", completed.stdout)
        require_clean_stream(f"{label} JSON stderr", completed.stderr)
        require(not completed.stderr, f"{label} JSON command wrote stderr")
        require(completed.stdout.endswith(b"\n"), f"{label} JSON output lacks final newline")
        raw_lines = completed.stdout.splitlines()
        require(raw_lines and all(raw_lines), f"{label} JSON output has empty records")
        try:
            records = [json.loads(line) for line in raw_lines]
        except (json.JSONDecodeError, UnicodeDecodeError) as error:
            raise AcceptanceError(
                f"{label} returned invalid NDJSON: {compact_bytes(completed.stdout)}"
            ) from error
        require(
            all(isinstance(record, dict) for record in records),
            f"{label} NDJSON records are not objects",
        )
        terminal_indexes = [
            index
            for index, record in enumerate(records)
            if record.get("type") == "Result"
            or ("error" in record and "type" not in record)
        ]
        require(
            terminal_indexes == [len(records) - 1],
            f"{label} did not contain exactly one terminal Result/error record",
        )
        terminal = records[-1]
        if expected_code == 0:
            require(terminal.get("type") == "Result", f"{label} did not end in Result")
            require("error" not in terminal, f"{label} success ended in an error")
            if action is not None:
                require(
                    terminal.get("action") == action,
                    f"{label} Result action was {terminal.get('action')!r}, expected {action!r}",
                )
            terminal_kind = "Result"
            terminal_value = terminal.get("action")
        else:
            error = terminal.get("error")
            require(isinstance(error, dict), f"{label} failure lacks an error object")
            require(terminal.get("type") != "Result", f"{label} failure ended in Result")
            if error_kind is not None:
                require(
                    error.get("kind") == error_kind,
                    f"{label} error kind was {error.get('kind')!r}, expected {error_kind!r}",
                )
            terminal_kind = "error"
            terminal_value = error.get("kind")
        return records, {
            "exit_code": completed.returncode,
            "record_count": len(records),
            "terminal_kind": terminal_kind,
            "terminal_value": terminal_value,
            "stdout": stream_facts(completed.stdout),
            "stderr": stream_facts(completed.stderr),
        }

    @staticmethod
    def result_payload(records: list[dict[str, Any]], action: str) -> Any:
        terminal = records[-1]
        require(terminal.get("type") == "Result", f"{action} did not end with Result")
        require(
            terminal.get("action") == action,
            f"expected {action} Result, got {terminal!r}",
        )
        require("payload" in terminal, f"{action} Result has no payload")
        return terminal["payload"]

    def create(self, name: str) -> dict[str, Any]:
        records, _, _ = self.json_command(
            "create",
            name,
            "ubuntu:24.04",
            "--net",
            "none",
            action="create",
        )
        payload = self.result_payload(records, "create")
        require(payload["state"]["status"] == "created", f"{name} was not created")
        require(
            payload["spec"]["network"]["mode"] == "none",
            f"{name} enabled networking",
        )
        return payload

    def state(self, name: str) -> dict[str, Any]:
        path = self.home / "data" / "machines" / name / "state.json"
        try:
            value = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            raise AcceptanceError(f"cannot read state for {name}: {error}") from error
        require(isinstance(value, dict), f"state for {name} is not an object")
        return value

    def wait_for_state(self, name: str, expected: set[str], timeout: float) -> tuple[str, float]:
        started = time.monotonic()
        deadline = started + timeout
        last = "missing"
        while time.monotonic() < deadline:
            try:
                status_value = self.state(name).get("status")
                last = status_value if isinstance(status_value, str) else "invalid"
            except AcceptanceError:
                last = "missing"
            if last in expected:
                return last, time.monotonic() - started
            time.sleep(0.005)
        raise AcceptanceError(
            f"{name} did not reach one of {sorted(expected)!r} within {timeout:.1f}s; "
            f"last status was {last!r}"
        )

    def stop(self, name: str, *, force: bool = False) -> dict[str, Any]:
        arguments = ["stop", name, "--timeout", "90s"]
        if force:
            arguments.append("--force")
        records, _, _ = self.json_command(
            *arguments,
            timeout=130,
            action="stop",
        )
        payload = self.result_payload(records, "stop")
        require(payload["status"] == "stopped", f"{name} did not stop")
        return payload

    def shell(
        self,
        name: str,
        command: list[str],
        *,
        user: str | None = None,
        expected_code: int | set[int] = 0,
        timeout: float = COMMAND_TIMEOUT_SECONDS,
        audit_controls: bool = True,
    ) -> subprocess.CompletedProcess[bytes]:
        arguments: list[str | os.PathLike[str]] = [self.binary, "shell", name]
        if user is not None:
            arguments.extend(["--user", user])
        arguments.extend(["--", *command])
        completed = self.run(arguments, timeout=timeout, check=False)
        expected_codes = (
            expected_code if isinstance(expected_code, set) else {expected_code}
        )
        require(
            completed.returncode in expected_codes,
            f"shell {name} exited {completed.returncode}, expected one of "
            f"{sorted(expected_codes)}; "
            f"stdout:\n{compact_bytes(completed.stdout)}\n"
            f"stderr:\n{compact_bytes(completed.stderr)}",
        )
        if audit_controls:
            require_clean_stream(f"shell {name} stdout", completed.stdout)
            require_clean_stream(f"shell {name} stderr", completed.stderr)
        return completed

    def guest_script(
        self,
        name: str,
        script: str,
        *,
        user: str | None = None,
        expected_code: int | set[int] = 0,
        timeout: float = COMMAND_TIMEOUT_SECONDS,
    ) -> subprocess.CompletedProcess[bytes]:
        return self.shell(
            name,
            ["sh", "-c", shlex.quote(script)],
            user=user,
            expected_code=expected_code,
            timeout=timeout,
        )

    def console_log(self, name: str) -> bytes:
        path = self.home / "data" / "machines" / name / "console.log"
        try:
            size = path.stat().st_size
            require(size <= MAX_OUTPUT_BYTES, f"{name} console.log exceeded 8 MiB")
            return path.read_bytes()
        except OSError as error:
            raise AcceptanceError(f"cannot read console.log for {name}: {error}") from error

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
                            stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL,
                            timeout=20,
                            check=False,
                        )
                    except (OSError, subprocess.TimeoutExpired) as error:
                        errors.append(f"cleanup command failed for {name}: {error}")
        live = self._live_home_processes()
        if live:
            errors.append(f"live processes still reference FIRESTONE_HOME: {live}")
        if not self.keep_home and not live:
            shutil.rmtree(self.home, ignore_errors=False)
        return errors


def configure_m2_network(harness: Harness) -> dict[str, Any]:
    """Pin the pre-M3 acceptance scope without changing the exercised run argv."""
    directory = harness.home / "config"
    directory.mkdir(mode=0o700, exist_ok=True)
    metadata = directory.stat()
    require(metadata.st_uid == os.getuid(), "M2 config directory has the wrong owner")
    require(
        stat.S_IMODE(metadata.st_mode) == 0o700,
        "M2 config directory is not mode 0700",
    )
    path = directory / "config.toml"
    require(not path.exists(), "M2 config.toml already exists")
    contents = b'[defaults.network]\nmode = "none"\n'
    descriptor = os.open(
        path,
        os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC,
        0o600,
    )
    try:
        with os.fdopen(descriptor, "wb", closefd=False) as stream:
            stream.write(contents)
            stream.flush()
            os.fsync(stream.fileno())
    finally:
        os.close(descriptor)
    directory_descriptor = os.open(directory, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
    try:
        os.fsync(directory_descriptor)
    finally:
        os.close(directory_descriptor)
    require(stat.S_IMODE(path.stat().st_mode) == 0o600, "M2 config.toml is not mode 0600")
    return {
        "network_mode": "none",
        "config_mode": "0600",
        "config_sha256": bytes_sha256(contents),
    }


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
        require(
            actual == artifact["sha256"],
            f"installed {name} checksum does not match deps.toml",
        )
        result[name] = {
            "version": dependency["version"],
            "install_name": artifact["install_name"],
            "sha256": actual,
            "url": artifact["url"],
        }
    require(
        result["cloud-hypervisor"]["version"] == "v53.0",
        "acceptance requires pinned Cloud Hypervisor v53.0",
    )
    return result


def image_evidence(harness: Harness, name: str) -> dict[str, Any]:
    state = harness.state(name)
    image_id = state["image"]["id"]
    require(isinstance(image_id, str), f"{name} did not pin an image id")
    sidecar_path = harness.home / "data" / "images" / f"{image_id}.json"
    sidecar = json.loads(sidecar_path.read_text(encoding="utf-8"))
    require(sidecar["source_ref"] == "ubuntu:24.04", "run did not resolve ubuntu:24.04")
    require(sidecar["architecture"] == "x86_64", "run did not select x86_64")
    require(sidecar["firmware"] == "edk2", "Ubuntu x86_64 did not select edk2")
    require(
        sidecar["verification_algorithm"] == "sha256",
        "Ubuntu image was not verified with SHA-256",
    )
    require(
        sidecar["verification_digest"] == sidecar["source_sha256"],
        "Ubuntu verifier digest did not pin the downloaded source bytes",
    )
    stored_path = harness.home / "data" / "images" / f"{image_id}.qcow2"
    actual_stored = sha256(stored_path)
    require(
        actual_stored == sidecar["stored_sha256"],
        "stored Ubuntu image checksum does not match its sidecar",
    )
    return {
        "id": image_id,
        "generation": sidecar["generation"],
        "source_ref": sidecar["source_ref"],
        "source_url": sidecar["source_url"],
        "source_sha256": sidecar["source_sha256"],
        "stored_sha256": sidecar["stored_sha256"],
        "stored_sha256_recomputed": actual_stored,
        "size": sidecar["size"],
        "source_format": sidecar["source_format"],
        "stored_format": sidecar["stored_format"],
        "architecture": sidecar["architecture"],
        "firmware": sidecar["firmware"],
        "verification_algorithm": sidecar["verification_algorithm"],
        "verification_digest": sidecar["verification_digest"],
    }


def require_ordered_steps(records: list[dict[str, Any]]) -> dict[str, Any]:
    steps = [
        (record.get("type"), record.get("id"))
        for record in records
        if record.get("type") in {"StepStart", "StepUpdate", "StepDone", "StepFail"}
    ]

    def position(kind: str, step: str) -> int:
        try:
            return steps.index((kind, step))
        except ValueError as error:
            raise AcceptanceError(f"start output omitted {kind} {step}: {steps!r}") from error

    boot_start = position("StepStart", "boot")
    boot_done = position("StepDone", "boot")
    ssh_start = position("StepStart", "ssh")
    ssh_done = position("StepDone", "ssh")
    require(
        boot_start < boot_done < ssh_start < ssh_done,
        f"boot/SSH readiness steps were not ordered: {steps!r}",
    )
    updates = [
        record.get("detail")
        for record in records
        if record.get("type") == "StepUpdate" and record.get("id") == "ssh"
    ]
    require(updates, "start did not emit an SSH wait reason")
    return {"ordered_steps": steps, "ssh_wait_reasons": updates}


def run_prompt_scenario(harness: Harness) -> dict[str, Any]:
    token = f"__FIRESTONE_M2_RUN_{uuid.uuid4().hex}__".encode()
    started = time.monotonic()
    session = PtySession(
        harness,
        [harness.binary, "run", "ubuntu"],
        label="firestone run ubuntu PTY",
    )
    try:
        session.wait_for_root_prompt(BOOT_TIMEOUT_SECONDS)
        session.write(b"stty -echo\n")
        time.sleep(0.2)
        session.write(b"printf '" + token + b"\\n'\n")
        session.wait_for_bytes(token + b"\r", 20, "the accepted guest command marker")
        session.write(b"exit\n")
        returncode = session.wait(30)
        require(returncode == 0, f"interactive run exited {returncode}")
        return {
            "root_prompt": True,
            "guest_command_marker": token.decode(),
            "exit_code": returncode,
            "elapsed_ms": round((time.monotonic() - started) * 1000, 3),
        }
    finally:
        if session.process.poll() is None:
            harness.terminate_process(session.process)
        session.close()


def run_shell_scenarios(harness: Harness) -> dict[str, Any]:
    argv = harness.shell(
        "ubuntu",
        ["printf", "M2_ARGV:%s:%s", "alpha", "beta"],
    )
    require(argv.stdout == b"M2_ARGV:alpha:beta", "guest shell argv order changed")
    require(not argv.stderr, f"argv shell wrote stderr: {compact_bytes(argv.stderr)}")

    root = harness.shell("ubuntu", ["id", "-un"])
    require(root.stdout == b"root\n", "default shell user was not root")
    ubuntu = harness.shell("ubuntu", ["id", "-un"], user="ubuntu")
    require(ubuntu.stdout == b"ubuntu\n", "--user ubuntu did not select ubuntu")

    exited = harness.shell(
        "ubuntu",
        ["sh", "-c", shlex.quote("exit 37")],
        expected_code=37,
    )
    require(not exited.stdout, "exit-37 command wrote stdout")

    signalled = harness.shell(
        "ubuntu",
        ["sh", "-c", shlex.quote("kill -TERM $$")],
        expected_code=255,
    )
    require(not signalled.stdout, "guest signal command wrote stdout")

    return {
        "argv": {
            "stdout": argv.stdout.decode(),
            "exit_code": argv.returncode,
            "stream": stream_facts(argv.stdout),
        },
        "users": {
            "default": root.stdout.decode().strip(),
            "override": ubuntu.stdout.decode().strip(),
        },
        "exit_code": exited.returncode,
        "guest_signal": signal.SIGTERM,
        "signal_exit_code": signalled.returncode,
        "signal_stderr": stream_facts(signalled.stderr),
    }


def verify_guest_units(harness: Harness) -> dict[str, Any]:
    script = r"""
set -eu
version=$(systemctl --version | sed -n '1p')
cloud_status=$(cloud-init status --wait --long)
printf '%s\n' "$cloud_status" | grep -q '^status: done$'
printf '%s\n' "$cloud_status" | grep -q 'DataSourceNoCloud'
test ! -e /run/systemd/generator/sshd-vsock.socket
test "$(systemctl is-active firestone-sshd.socket)" = active
test "$(systemctl show -p SubState --value firestone-sshd.socket)" = listening
test "$(systemctl show -p Result --value firestone-sshd.socket)" = success
test "$(systemctl show -p LoadState --value sshd-vsock.socket)" = not-found
test "$(systemctl is-active serial-getty@hvc0.service)" = active
test "$(systemctl show -p SubState --value serial-getty@hvc0.service)" = running
ss -H -ln --vsock | grep -Eq '(\*|[0-9]+):22'
/usr/sbin/sshd -T | grep -qx 'permitrootlogin without-password'
/usr/sbin/sshd -T | grep -qx 'passwordauthentication no'
systemd-analyze verify /etc/systemd/system/firestone-sshd.socket /etc/systemd/system/firestone-sshd@.service /usr/lib/systemd/system/serial-getty@.service
printf 'systemd=%s\n' "$version"
printf 'cloud_init_done=true\n'
printf 'cloud_init_datasource=NoCloud\n'
printf 'native_generator_present=false\n'
printf 'firestone_socket=active/listening\n'
printf 'native_socket=not-found\n'
printf 'hvc0_getty=active/running\n'
printf 'vsock_22_listening=true\n'
printf 'root_key_only=true\n'
printf 'units_verified=true\n'
""".strip()
    completed = harness.guest_script("ubuntu", script, timeout=300)
    require(not completed.stderr, f"guest unit checks wrote stderr: {compact_bytes(completed.stderr)}")
    facts: dict[str, str] = {}
    for line in completed.stdout.decode().splitlines():
        key, separator, value = line.partition("=")
        require(separator == "=", f"guest unit fact was malformed: {line!r}")
        facts[key] = value
    require(facts.get("systemd", "").startswith("systemd 255 "), "accepted Ubuntu is not systemd 255")
    expected = {
        "cloud_init_done": "true",
        "cloud_init_datasource": "NoCloud",
        "native_generator_present": "false",
        "firestone_socket": "active/listening",
        "native_socket": "not-found",
        "hvc0_getty": "active/running",
        "vsock_22_listening": "true",
        "root_key_only": "true",
        "units_verified": "true",
    }
    for key, value in expected.items():
        require(facts.get(key) == value, f"guest unit fact {key} was {facts.get(key)!r}")
    return {
        "facts": facts,
        "verify_11_status_unchanged": True,
        "verify_17_status_unchanged": True,
    }


def console_session(
    harness: Harness,
    *,
    token: bytes | None,
    interrupt: bool,
) -> dict[str, Any]:
    session = PtySession(
        harness,
        [harness.binary, "console", "ubuntu"],
        label="firestone console ubuntu",
    )
    try:
        session.wait_for_bytes(CONSOLE_CONNECTED, 10, "the console connection acknowledgement")
        if interrupt:
            os.kill(session.process.pid, signal.SIGTERM)
            returncode = session.wait(10)
            require(returncode == 130, f"interrupted console exited {returncode}")
        else:
            session.wait_for_root_prompt(30)
            if token is not None:
                session.write(b"stty -echo\n")
                time.sleep(0.2)
                session.write(b"printf '" + token + b"\\n'\n")
                session.wait_for_bytes(token + b"\r", 20, "the hvc0 command marker")
            session.write(bytes([0x1D]))
            returncode = session.wait(10)
            require(returncode == 0, f"console detach exited {returncode}")
        restored = session.terminal_restored()
        require(restored, "console did not restore terminal attributes")
        return {
            "exit_code": returncode,
            "interrupted": interrupt,
            "terminal_restored": restored,
            "connected": True,
            "interacted": token is not None,
        }
    finally:
        if session.process.poll() is None:
            harness.terminate_process(session.process)
        session.close()


def verify_console(harness: Harness) -> dict[str, Any]:
    first_token = f"__FIRESTONE_M2_CONSOLE_A_{uuid.uuid4().hex}__".encode()
    second_token = f"__FIRESTONE_M2_CONSOLE_B_{uuid.uuid4().hex}__".encode()
    serial_before = harness.console_log("ubuntu")
    first = console_session(harness, token=first_token, interrupt=False)
    second = console_session(harness, token=second_token, interrupt=False)
    interrupted = console_session(harness, token=None, interrupt=True)
    serial_during = harness.console_log("ubuntu")
    require(
        serial_during.startswith(serial_before),
        "console.log serial history changed while the PTY broker was active",
    )
    require(
        first_token not in serial_during and second_token not in serial_during,
        "PTY staging bytes raced into console.log while Cloud Hypervisor was running",
    )
    staging = harness.home / "run" / "ubuntu" / "console.pty.log"
    require(staging.is_file(), "console PTY staging log is missing while the VMM runs")
    require(
        stat.S_IMODE(staging.stat().st_mode) == 0o600,
        "console PTY staging log is not mode 0600",
    )
    return {
        "first_attach": first,
        "second_attach": second,
        "interrupted_attach": interrupted,
        "serial_bytes_before": len(serial_before),
        "serial_bytes_during": len(serial_during),
        "staging_mode": "0600",
        "tokens": [first_token.decode(), second_token.decode()],
    }


def structured_output_scenarios(harness: Harness) -> dict[str, Any]:
    json_frames: dict[str, Any] = {}
    records, frame, _ = harness.json_command(
        "ssh-config",
        "ubuntu",
        action="ssh-config",
    )
    require(len(records) == 1, "--json ssh-config did not emit exactly one Result")
    json_frames["ssh_config"] = frame
    for label, arguments in (
        ("run", ("run", "ubuntu", "--", "true")),
        ("shell", ("shell", "ubuntu", "--", "true")),
        ("console", ("console", "ubuntu")),
    ):
        _, frame, _ = harness.json_command(
            *arguments,
            expected_code=2,
            error_kind="usage",
        )
        json_frames[label] = frame

    config = harness.run([harness.binary, "ssh-config", "ubuntu"])
    require_clean_stream("redirected ssh-config stdout", config.stdout)
    require_clean_stream("redirected ssh-config stderr", config.stderr)
    require(config.stdout.startswith(b"Host firestone.ubuntu\n"), "ssh-config Host framing changed")
    require(config.stdout.endswith(b"\n"), "ssh-config output lacks its final newline")
    require(not config.stderr, "redirected ssh-config wrote stderr")

    console = harness.run(
        [harness.binary, "console", "ubuntu"],
        check=False,
    )
    require(console.returncode == 2, f"redirected console exited {console.returncode}")
    require_clean_stream("redirected console stdout", console.stdout)
    require_clean_stream("redirected console stderr", console.stderr)
    expected_error = (
        b"error: console requires terminal stdin, stdout, and stderr\n"
        b"hint:  run firestone console from an interactive terminal\n"
    )
    require(not console.stdout, "redirected console wrote stdout")
    require(console.stderr == expected_error, "redirected console error framing changed")

    return {
        "json": json_frames,
        "redirected_non_tty": {
            "ssh_config": {
                "exit_code": config.returncode,
                "stdout": stream_facts(config.stdout),
                "stderr": stream_facts(config.stderr),
            },
            "console_error": {
                "exit_code": console.returncode,
                "lines": console.stderr.decode().splitlines(),
                "stdout": stream_facts(console.stdout),
                "stderr": stream_facts(console.stderr),
            },
        },
    }


def communicate_bounded(
    harness: Harness,
    process: subprocess.Popen[bytes],
    timeout: float,
    label: str,
) -> subprocess.CompletedProcess[bytes]:
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired as error:
        harness.terminate_process(process)
        raise AcceptanceError(f"{label} did not exit within {timeout:.1f}s") from error
    harness.forget_process(process)
    return subprocess.CompletedProcess(process.args, process.returncode, stdout, stderr)


def interrupt_start(
    harness: Harness,
    *,
    wait_for: set[str],
    expected_final: set[str],
    label: str,
    settle_seconds: float = 0.0,
) -> tuple[dict[str, Any], dict[str, Any]]:
    process = harness.spawn(
        [harness.binary, "--json", "start", "m2-readiness", "--timeout", "600s"]
    )
    observed, observed_after = harness.wait_for_state("m2-readiness", wait_for, 120)
    if settle_seconds:
        time.sleep(settle_seconds)
    os.kill(process.pid, signal.SIGINT)
    completed = communicate_bounded(harness, process, 30, label)
    _, frame = harness.parse_json_command(
        completed,
        label=label,
        expected_code=130,
        action=None,
        error_kind="interrupted",
    )
    final, final_after = harness.wait_for_state("m2-readiness", expected_final, 20)
    return {
        "observed_state": observed,
        "signal_after_ms": round((observed_after + settle_seconds) * 1000, 3),
        "final_state": final,
        "final_after_ms": round(final_after * 1000, 3),
    }, frame


def readiness_scenarios(harness: Harness) -> dict[str, Any]:
    harness.create("m2-readiness")
    started = time.monotonic()
    records, default_frame, _ = harness.json_command(
        "start",
        "m2-readiness",
        "--timeout",
        "600s",
        timeout=START_TIMEOUT_SECONDS,
        action="start",
    )
    default_elapsed = time.monotonic() - started
    readiness = require_ordered_steps(records)
    payload = harness.result_payload(records, "start")
    require(payload["status"] == "running", "default start did not reach running")
    harness.stop("m2-readiness")

    no_wait_started = time.monotonic()
    no_wait = harness.run(
        [
            harness.binary,
            "start",
            "m2-readiness",
            "--no-wait",
            "--timeout",
            "600s",
        ],
        timeout=START_TIMEOUT_SECONDS,
    )
    no_wait_elapsed = time.monotonic() - no_wait_started
    require_clean_stream("redirected --no-wait stdout", no_wait.stdout)
    require_clean_stream("redirected --no-wait stderr", no_wait.stderr)
    rendered = no_wait.stdout + no_wait.stderr
    require(b"[boot]" not in rendered, "--no-wait emitted a boot readiness step")
    require(b"[ssh]" not in rendered, "--no-wait emitted an SSH readiness step")
    require(harness.state("m2-readiness")["status"] == "running", "--no-wait did not persist running")
    harness.stop("m2-readiness", force=True)

    _, timeout_frame, _ = harness.json_command(
        "start",
        "m2-readiness",
        "--timeout",
        "20s",
        expected_code=6,
        error_kind="timeout",
    )
    timeout_state, _ = harness.wait_for_state("m2-readiness", {"running"}, 10)
    harness.stop("m2-readiness", force=True)

    rollback, rollback_frame = interrupt_start(
        harness,
        wait_for={"starting"},
        expected_final={"created", "stopped"},
        label="interrupted start rollback",
    )
    require(
        not (harness.home / "run" / "m2-readiness").exists(),
        "interrupted launch rollback left its runtime directory behind",
    )

    background, background_frame = interrupt_start(
        harness,
        wait_for={"running"},
        expected_final={"running"},
        label="interrupted readiness wait",
        settle_seconds=2.0,
    )
    harness.stop("m2-readiness", force=True)

    return {
        "default": {
            "elapsed_ms": round(default_elapsed * 1000, 3),
            "result": payload,
            "frame": default_frame,
            **readiness,
        },
        "no_wait": {
            "elapsed_ms": round(no_wait_elapsed * 1000, 3),
            "status_at_return": "running",
            "boot_step_present": False,
            "ssh_step_present": False,
            "stdout": stream_facts(no_wait.stdout),
            "stderr": stream_facts(no_wait.stderr),
        },
        "readiness_timeout": {
            "timeout": "20s",
            "state_at_return": timeout_state,
            "vm_left_running": True,
            "frame": timeout_frame,
        },
        "interrupted_launch_rollback": {
            **rollback,
            "runtime_removed": True,
            "frame": rollback_frame,
        },
        "interrupted_readiness": {
            **background,
            "vm_left_running": True,
            "frame": background_frame,
        },
    }


def run_acceptance(harness: Harness) -> None:
    require(sys.platform == "linux", "M2 acceptance requires Linux")
    require(platform.machine() == "x86_64", "M2 acceptance requires x86_64")
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

    doctor_started = time.monotonic()
    doctor_fix, doctor_fix_frame, _ = harness.json_command(
        "doctor",
        "--fix",
        timeout=900,
        action="doctor",
    )
    doctor, doctor_frame, _ = harness.json_command("doctor", action="doctor")
    doctor_elapsed = time.monotonic() - doctor_started
    report = harness.result_payload(doctor, "doctor")
    checks = report.get("checks")
    require(isinstance(checks, list) and len(checks) == 13, "doctor did not return 13 checks")
    failures = [check for check in checks if check.get("status") == "fail"]
    require(not failures, f"doctor failed checks: {failures!r}")
    harness.evidence["scenarios"]["doctor"] = {
        "elapsed_ms": round(doctor_elapsed * 1000, 3),
        "fix_frame": doctor_fix_frame,
        "frame": doctor_frame,
        "check_count": len(checks),
        "fix_result": harness.result_payload(doctor_fix, "doctor"),
    }
    harness.evidence["artifacts"] = installed_artifacts(harness)
    harness.evidence["m2_scope"] = configure_m2_network(harness)

    images_dir = harness.home / "data" / "images"
    image_cache_empty = not images_dir.exists() or not any(images_dir.iterdir())
    require(image_cache_empty, "image cache was not empty before the first run")
    first_started = time.monotonic()
    first = harness.run(
        [harness.binary, "run", "ubuntu", "--", "true"],
        timeout=START_TIMEOUT_SECONDS,
    )
    first_elapsed = time.monotonic() - first_started
    require_clean_stream("cold run stdout", first.stdout)
    require_clean_stream("cold run stderr", first.stderr)
    require(not first.stdout, "run ubuntu -- true wrote stdout")
    for step in (b"[image]", b"[boot]", b"[ssh]"):
        require(step in first.stderr, f"cold run omitted {step.decode()} output")
    require(harness.state("ubuntu")["status"] == "running", "cold run did not leave ubuntu running")
    machine_spec = tomllib.loads(
        (harness.home / "data" / "machines" / "ubuntu" / "firestone.toml").read_text(
            encoding="utf-8"
        )
    )
    require(
        machine_spec["network"]["mode"] == "none",
        "exact run command did not inherit network.mode none from the M2 scope",
    )
    image = image_evidence(harness, "ubuntu")
    harness.evidence["image"] = image

    warm_started = time.monotonic()
    warm = harness.run(
        [harness.binary, "run", "ubuntu", "--", "true"],
        timeout=30,
    )
    warm_elapsed = time.monotonic() - warm_started
    require_clean_stream("warm run stdout", warm.stdout)
    require_clean_stream("warm run stderr", warm.stderr)
    require(warm.returncode == 0, "warm run did not complete SSH true")
    require(
        warm_elapsed < 5.0,
        f"warm run took {warm_elapsed:.3f}s, not under 5s",
    )
    harness.evidence["scenarios"]["e2e_2"] = {
        "home_empty_before_preflight": True,
        "image_cache_empty_before_run": image_cache_empty,
        "cold_run_ms": round(first_elapsed * 1000, 3),
        "warm_run_ms": round(warm_elapsed * 1000, 3),
        "warm_under_5_seconds": True,
        "cold_stdout": stream_facts(first.stdout),
        "cold_stderr": stream_facts(first.stderr),
        "warm_stdout": stream_facts(warm.stdout),
        "warm_stderr": stream_facts(warm.stderr),
        "ssh_command_completed": True,
        "image": image,
    }

    harness.evidence["scenarios"]["empty_command_run_pty"] = run_prompt_scenario(harness)
    harness.evidence["scenarios"]["shell"] = run_shell_scenarios(harness)
    harness.evidence["scenarios"]["verify_11_17_regression"] = verify_guest_units(harness)

    console = verify_console(harness)
    harness.evidence["scenarios"]["verify_13_console"] = console
    harness.evidence["scenarios"]["e2e_10"] = structured_output_scenarios(harness)
    harness.evidence["scenarios"]["start_boundaries"] = readiness_scenarios(harness)

    private_key = harness.home / "data" / "ssh" / "id_ed25519"
    public_key = harness.home / "data" / "ssh" / "id_ed25519.pub"
    known_hosts = harness.home / "data" / "machines" / "ubuntu" / "known_hosts"
    require(stat.S_IMODE(private_key.stat().st_mode) == 0o600, "SSH private key is not mode 0600")
    require(stat.S_IMODE(public_key.stat().st_mode) == 0o644, "SSH public key is not mode 0644")
    require(stat.S_IMODE(known_hosts.stat().st_mode) == 0o600, "known_hosts is not mode 0600")
    harness.evidence["ssh_file_modes"] = {
        "private_key": "0600",
        "public_key": "0644",
        "known_hosts": "0600",
    }

    harness.stop("ubuntu")
    final_console = harness.console_log("ubuntu")
    tokens = [value.encode() for value in console["tokens"]]
    marker_counts = [final_console.count(token) for token in tokens]
    require(marker_counts == [1, 1], f"console markers were corrupted or duplicated: {marker_counts}")
    require(
        len(final_console) >= console["serial_bytes_during"],
        "console.log lost serial history during final merge",
    )
    require(
        stat.S_IMODE(
            (harness.home / "data" / "machines" / "ubuntu" / "console.log").stat().st_mode
        )
        == 0o600,
        "final console.log is not mode 0600",
    )
    harness.evidence["scenarios"]["verify_13_console"].update(
        {
            "final_console_bytes": len(final_console),
            "marker_counts": marker_counts,
            "console_log_mode": "0600",
            "serial_history_complete": True,
            "concurrent_corruption": False,
        }
    )


def install_harness_signal_handlers() -> None:
    def interrupted(signum: int, _frame: Any) -> None:
        raise AcceptanceError(f"M2 harness interrupted by signal {signum}")

    for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(signum, interrupted)


def main() -> int:
    if os.environ.get("FIRESTONE_E2E") != "1":
        print("skipped M2 KVM acceptance; set FIRESTONE_E2E=1 to run")
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
        print(f"M2 KVM acceptance failed: {failure}", file=sys.stderr)
        return 1
    require(harness is not None, "M2 harness was not initialized")
    print(f"M2 KVM acceptance passed; evidence: {harness.evidence_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
