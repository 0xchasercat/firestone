#!/usr/bin/env python3
"""Run E2E 11 for the Linux x86_64 built-in catalog on a real KVM host."""

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
import stat
import subprocess
import sys
import time
import tomllib
import uuid
from pathlib import Path
from typing import Any


class AcceptanceError(RuntimeError):
    """The host or one catalog matrix contract failed."""


REPO_ROOT = Path(__file__).resolve().parents[1]
MATRIX_REFERENCES = (
    "ubuntu:24.04",
    "ubuntu:22.04",
    "debian:12",
    "debian:13",
    "fedora:44",
)
COMMAND_TIMEOUT_SECONDS = 120
START_TIMEOUT_SECONDS = 1_200
DOCTOR_TIMEOUT_SECONDS = 900
MAX_OUTPUT_BYTES = 8 * 1024 * 1024
MAX_EVIDENCE_BYTES = 1024 * 1024


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


class Harness:
    def __init__(self) -> None:
        self.home = self._checked_home()
        self.keep_home = os.environ.get("FIRESTONE_E2E_KEEP") == "1"
        default_evidence = Path("/tmp") / f"firestone-m5-catalog-{os.getpid()}.json"
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
            "scenario": "e2e11-catalog-matrix",
            "result": "running",
            "started_at": dt.datetime.now(dt.UTC).isoformat(),
            "commands": self.commands,
            "host": {},
            "artifacts": {},
            "catalog": {},
            "matrix": {},
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
        binary = REPO_ROOT / "target" / "debug" / "firestone"
        require(binary.is_file(), f"default Firestone binary is missing: {binary}")
        require(os.access(binary, os.X_OK), f"default Firestone binary is not executable: {binary}")
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

    def json_command(
        self,
        *arguments: str,
        action: str,
        timeout: float = COMMAND_TIMEOUT_SECONDS,
        expected_code: int = 0,
    ) -> tuple[list[dict[str, Any]], dict[str, Any]]:
        completed = self.run(
            [self.binary, "--json", *arguments],
            timeout=timeout,
            check=False,
        )
        require(
            completed.returncode == expected_code,
            f"{action} exited {completed.returncode}, expected {expected_code}: "
            f"{compact_bytes(completed.stdout + completed.stderr)}",
        )
        require(not completed.stderr, f"{action} JSON command wrote stderr")
        require_clean_stream(f"{action} JSON stdout", completed.stdout)
        try:
            records = [
                json.loads(line)
                for line in completed.stdout.decode("utf-8").splitlines()
                if line
            ]
        except json.JSONDecodeError as error:
            raise AcceptanceError(f"{action} emitted invalid NDJSON") from error
        require(records, f"{action} emitted no JSON records")
        terminal = records[-1]
        require(terminal.get("type") == "Result", f"{action} did not end with Result")
        require(terminal.get("action") == action, f"{action} Result action changed")
        payload = terminal.get("payload")
        require(isinstance(payload, dict), f"{action} Result payload is not an object")
        return records, payload

    def machine_state(self, name: str) -> dict[str, Any]:
        path = self.home / "data" / "machines" / name / "state.json"
        try:
            value = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            raise AcceptanceError(f"cannot read state for {name}: {error}") from error
        require(isinstance(value, dict), f"state for {name} is not an object")
        return value

    def remove_machine(self, name: str) -> None:
        completed = self.run(
            [self.binary, "rm", name, "--force"],
            timeout=180,
            check=False,
        )
        require(
            completed.returncode == 0,
            f"cannot remove {name}: {compact_bytes(completed.stdout + completed.stderr)}",
        )
        if name in self.created_machines:
            self.created_machines.remove(name)

    def cleanup(self) -> list[str]:
        if self._cleanup_started:
            return []
        self._cleanup_started = True
        errors: list[str] = []
        for name in reversed(self.created_machines.copy()):
            machine_dir = self.home / "data" / "machines" / name
            if not machine_dir.exists():
                self.created_machines.remove(name)
                continue
            stopped = self.run(
                [self.binary, "stop", name, "--force", "--timeout", "10s"],
                timeout=30,
                check=False,
                record=False,
            )
            if stopped.returncode not in {0, 3}:
                errors.append(f"stop {name} exited {stopped.returncode}")
            removed = self.run(
                [self.binary, "rm", name, "--force"],
                timeout=30,
                check=False,
                record=False,
            )
            if removed.returncode not in {0, 3}:
                errors.append(f"rm {name} exited {removed.returncode}")
        if not self.keep_home:
            try:
                shutil.rmtree(self.home)
            except FileNotFoundError:
                pass
            except OSError as error:
                errors.append(f"cannot remove FIRESTONE_HOME: {error}")
        return errors

    def write_evidence(self) -> None:
        parent = self.evidence_path.parent
        require(parent.exists() and parent.is_dir(), "evidence parent directory does not exist")
        if self.evidence_path.exists() or self.evidence_path.is_symlink():
            metadata = self.evidence_path.lstat()
            require(
                stat.S_ISREG(metadata.st_mode)
                and metadata.st_uid == os.getuid()
                and stat.S_IMODE(metadata.st_mode) == 0o600,
                "existing evidence path must be a current-user mode-0600 regular file",
            )
        payload = (json.dumps(self.evidence, indent=2, sort_keys=True) + "\n").encode()
        require(len(payload) <= MAX_EVIDENCE_BYTES, "evidence exceeds 1 MiB")
        temporary = parent / (
            f".{self.evidence_path.name}.{os.getpid()}.{uuid.uuid4().hex}.partial"
        )
        flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC
        if hasattr(os, "O_NOFOLLOW"):
            flags |= os.O_NOFOLLOW
        descriptor = os.open(temporary, flags, 0o600)
        try:
            with os.fdopen(descriptor, "wb", closefd=False) as stream:
                stream.write(payload)
                stream.flush()
                os.fsync(stream.fileno())
            os.close(descriptor)
            descriptor = -1
            os.replace(temporary, self.evidence_path)
            os.chmod(self.evidence_path, 0o600)
            directory = os.open(parent, os.O_RDONLY | os.O_CLOEXEC)
            try:
                os.fsync(directory)
            finally:
                os.close(directory)
        finally:
            if descriptor >= 0:
                os.close(descriptor)
            try:
                temporary.unlink()
            except FileNotFoundError:
                pass


def catalog_matrix() -> dict[str, dict[str, object]]:
    document = tomllib.loads(
        (REPO_ROOT / "catalog" / "images.toml").read_text(encoding="utf-8")
    )
    images = document.get("image")
    require(isinstance(images, list), "catalog image table is missing")
    result: dict[str, dict[str, object]] = {}
    for value in images:
        require(isinstance(value, dict), "catalog entry is not a table")
        reference = f"{value.get('distro')}:{value.get('version')}"
        result[reference] = value
    require(tuple(result) == MATRIX_REFERENCES, "E2E 11 catalog references changed")
    for reference, entry in result.items():
        arch = entry.get("arch")
        require(
            isinstance(arch, dict) and set(arch) == {"x86_64", "aarch64"},
            f"{reference} architecture tables changed",
        )
        source = arch["x86_64"]
        require(isinstance(source, dict), f"{reference} x86_64 source is invalid")
        firmware = source.get("firmware", entry.get("firmware"))
        require(firmware == "edk2", f"{reference} does not declare edk2")
        require(entry.get("sshd_path") == "/usr/sbin/sshd", f"{reference} sshd_path changed")
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
        require(actual == artifact["sha256"], f"installed {name} checksum differs from deps.toml")
        result[name] = {
            "version": dependency["version"],
            "install_name": artifact["install_name"],
            "sha256": actual,
            "url": artifact["url"],
        }
    require(
        result["cloud-hypervisor"]["version"] == "v53.0",
        "E2E 11 requires Cloud Hypervisor v53.0",
    )
    return result


def image_evidence(
    harness: Harness,
    name: str,
    reference: str,
    catalog_entry: dict[str, object],
) -> dict[str, Any]:
    state = harness.machine_state(name)
    image_state = state.get("image")
    require(isinstance(image_state, dict), f"{name} has no image state")
    image_id = image_state.get("id")
    require(isinstance(image_id, str), f"{name} did not pin an image id")
    sidecar_path = harness.home / "data" / "images" / f"{image_id}.json"
    sidecar = json.loads(sidecar_path.read_text(encoding="utf-8"))
    require(sidecar["source_ref"] == reference, f"{reference} source ref changed")
    require(sidecar["architecture"] == "x86_64", f"{reference} architecture changed")
    require(sidecar["firmware"] == "edk2", f"{reference} did not use edk2")
    require(
        sidecar.get("sshd_path", "/usr/sbin/sshd") == "/usr/sbin/sshd",
        f"{reference} sshd path changed",
    )
    arch = catalog_entry["arch"]
    require(isinstance(arch, dict), f"{reference} catalog architecture is invalid")
    source = arch["x86_64"]
    require(isinstance(source, dict), f"{reference} catalog source is invalid")
    require(sidecar["source_url"] == source["url"], f"{reference} source URL changed")
    algorithm = source.get("checksum_alg", "sha256")
    require(
        sidecar["verification_algorithm"] == algorithm
        and sidecar["verification_digest"] is not None,
        f"{reference} verifier changed",
    )
    stored = harness.home / "data" / "images" / f"{image_id}.qcow2"
    stored_sha256 = sha256(stored)
    require(
        stored_sha256 == sidecar["stored_sha256"],
        f"{reference} stored checksum differs from its sidecar",
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
        "sshd_path": sidecar.get("sshd_path", "/usr/sbin/sshd"),
        "verification_algorithm": sidecar["verification_algorithm"],
        "verification_digest": sidecar["verification_digest"],
    }


def run_acceptance(harness: Harness) -> None:
    require(sys.platform == "linux", "E2E 11 requires Linux")
    require(platform.machine() == "x86_64", "E2E 11 requires x86_64")
    kvm = Path("/dev/kvm")
    metadata = kvm.lstat()
    require(stat.S_ISCHR(metadata.st_mode), "E2E 11 requires a real /dev/kvm character device")
    flags = os.O_RDWR | os.O_CLOEXEC | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(kvm, flags)
    except OSError as error:
        raise AcceptanceError(f"E2E 11 cannot open /dev/kvm read/write: {error}") from error
    else:
        os.close(descriptor)

    catalog = catalog_matrix()
    harness.evidence["host"] = {
        "system": platform.system(),
        "machine": platform.machine(),
        "kernel": platform.release(),
        "kvm_character_device": True,
        "kvm_open_read_write": True,
        "initial_home_empty": True,
    }
    harness.evidence["catalog"] = {
        "references": list(MATRIX_REFERENCES),
        "architecture": "x86_64",
        "firmware": "edk2",
        "default_user": "root",
        "sshd_path": "/usr/sbin/sshd",
    }

    doctor_fix, _ = harness.json_command(
        "doctor",
        "--fix",
        action="doctor",
        timeout=DOCTOR_TIMEOUT_SECONDS,
    )
    _, doctor = harness.json_command("doctor", action="doctor")
    checks = doctor.get("checks")
    require(isinstance(checks, list) and len(checks) == 13, "doctor did not return 13 checks")
    failures = [check for check in checks if check.get("status") == "fail"]
    require(not failures, f"doctor failed before E2E 11: {failures!r}")
    harness.evidence["doctor"] = {
        "fix_check_count": len(doctor_fix.get("checks", [])),
        "check_count": len(checks),
        "failures": [],
    }
    harness.evidence["artifacts"] = installed_artifacts(harness)

    for reference in MATRIX_REFERENCES:
        started = time.monotonic()
        name = "catalog-" + re_safe_name(reference)
        harness.created_machines.append(name)
        _, created = harness.json_command(
            "create",
            name,
            reference,
            "--net",
            "none",
            action="create",
        )
        spec = created.get("spec")
        state = created.get("state")
        require(isinstance(spec, dict) and isinstance(state, dict), f"{reference} create payload changed")
        require(spec.get("user") == "root", f"{reference} default user is not root")
        network = spec.get("network")
        require(
            isinstance(network, dict) and network.get("mode") == "none",
            f"{reference} E2E 11 machine enabled networking",
        )
        require(state.get("status") == "created", f"{reference} was not created")

        records, started_payload = harness.json_command(
            "start",
            name,
            "--timeout",
            "900s",
            action="start",
            timeout=START_TIMEOUT_SECONDS,
        )
        require(started_payload.get("status") == "running", f"{reference} did not reach running")
        step_ids = [record.get("id") for record in records if record.get("type") == "StepDone"]
        require("boot" in step_ids and "ssh" in step_ids, f"{reference} omitted boot or SSH readiness")

        shell = harness.run(
            [harness.binary, "shell", name, "--", "id", "-u"],
            timeout=60,
        )
        require_clean_stream(f"{reference} shell stdout", shell.stdout)
        require_clean_stream(f"{reference} shell stderr", shell.stderr)
        require(shell.stdout.strip() == b"0", f"{reference} shell did not log in as root")
        require(not shell.stderr, f"{reference} shell wrote stderr")

        image = image_evidence(harness, name, reference, catalog[reference])
        _, stopped = harness.json_command(
            "stop",
            name,
            "--timeout",
            "90s",
            action="stop",
            timeout=150,
        )
        require(stopped.get("status") == "stopped", f"{reference} did not stop")
        harness.remove_machine(name)
        harness.evidence["matrix"][reference] = {
            "elapsed_ms": round((time.monotonic() - started) * 1000, 3),
            "machine": name,
            "create_status": "created",
            "start_status": "running",
            "readiness_steps": step_ids,
            "ssh_root_command": "id -u",
            "ssh_root_uid": 0,
            "stop_status": "stopped",
            "removed": True,
            "image": image,
        }


def re_safe_name(reference: str) -> str:
    return "".join(character if character.isalnum() else "-" for character in reference).lower()


def install_harness_signal_handlers() -> None:
    def interrupted(signum: int, _frame: Any) -> None:
        raise AcceptanceError(f"E2E 11 harness interrupted by signal {signum}")

    for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(signum, interrupted)


def main() -> int:
    if os.environ.get("FIRESTONE_E2E") != "1":
        print("skipped E2E 11 catalog matrix; set FIRESTONE_E2E=1 to run on Linux x86_64 KVM")
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
        print(f"E2E 11 catalog matrix failed: {failure}", file=sys.stderr)
        return 1
    require(harness is not None, "E2E 11 harness was not initialized")
    print(f"E2E 11 catalog matrix passed; evidence: {harness.evidence_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
