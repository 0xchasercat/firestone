#!/usr/bin/env python3
"""Run the complete Linux x86_64 MVP acceptance sequence on real KVM."""

from __future__ import annotations

import argparse
import atexit
import datetime as dt
import fcntl
import hashlib
import json
import os
import platform
import re
import shutil
import signal
import stat
import struct
import subprocess
import sys
import tarfile
import tempfile
import time
import tomllib
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable


class GateError(RuntimeError):
    """The final acceptance preflight, one harness, or cleanup failed."""


REPO_ROOT = Path(__file__).resolve().parents[1]
CANONICAL_REPOSITORY = "github.com/0xchasercat/firestone"
ACCEPTED_ORIGIN_URLS = {
    "git@github.com:0xchasercat/firestone.git",
    "https://github.com/0xchasercat/firestone.git",
}
AGGREGATE_TIMEOUT_SECONDS = 21_600
PROCESS_CLEANUP_GRACE_SECONDS = 90
HOME_PROCESS_TERM_SECONDS = 15
HOME_PROCESS_KILL_SECONDS = 5
MAX_SMALL_OUTPUT_BYTES = 1024 * 1024
MAX_ACCEPTANCE_MANIFEST_BYTES = 512 * 1024
MAX_RELEASE_ARTIFACT_BYTES = 256 * 1024 * 1024
MAX_REPOSITORY_ARCHIVE_BYTES = 128 * 1024 * 1024
KVM_GET_API_VERSION = 0xAE00
KVM_API_VERSION = 12
RELEASE_ARTIFACT_NAME = "firestone-v0.1.0-x86_64-unknown-linux-musl"
RELEASE_CHECKSUM_NAME = "SHA256SUMS"
ACCEPTANCE_MANIFEST_NAME = "acceptance.json"
ACCEPTANCE_CHECKSUM_NAME = "SHA256SUMS"
SHA256_PATTERN = re.compile(r"[0-9a-f]{64}\Z")
COMMIT_PATTERN = re.compile(r"[0-9a-f]{40}\Z")
INTERRUPTED_SIGNAL: int | None = None
HANDLED_SIGNALS = frozenset({signal.SIGINT, signal.SIGTERM, signal.SIGHUP})
FINAL_SIGNAL_MASK: set[signal.Signals] | None = None

EXPECTED_FILE_HASHES = {
    "Cargo.lock": "b2ebf4acaa7900c67a0bf54316aae3a227b6a45b6485940f66e06b6258f4b49b",
    "build/firestone/versions.env": "65b2beb19b2cea5e93ece587f60f05171f49111221e3220aac65867095fca6df",
    "catalog/images.toml": "89f3e1827ed143e02da6d90e8c18bc28f273fcaba9cc301db663dac9cc4d3acf",
    "deps.toml": "57019e51437d3b5129f26803acf3d186e4c4c4ea79d869aa386c9b7e69156c74",
    "docs/verification/doctor-matrix.md": "c3254636863e741ae237f61b86b1fca0bbc8d097f1f40f799d090325e4eeb844",
    "scripts/m1-kvm-e2e.py": "719660526334393f2cb5df6b0b7d2eaf5a106f6e41674fdf3feb9476da11d124",
    "scripts/m2-kvm-e2e.py": "152a8f0ae19a0f46b2415186f545e4109c1d4ca47dbd57e335e0757e72f6d66f",
    "scripts/m3-kvm-e2e.py": "61c6b2d74b3ee609b63409aaaafbf59851340c47d27eb84ba0e629796b7b473f",
    "scripts/m4-kvm-e2e.py": "16a14771c0c0e6916d49554f0ed1af24b8fa8ef7e9c28ad9351e587e86c3a99e",
    "scripts/m5-catalog-kvm-e2e.py": "500e414ab16e08b25e592d049491e6c51e3240ce1569dc1207f8efdbc27d833b",
    "scripts/m5-doctor-matrix.py": "dc6e0b44e9c37194089ad84953065349edf65d7525bfb1a81bf9762873286aa6",
}

EXPECTED_RELEASE_DEPENDENCIES = {
    "cloud-hypervisor": {
        "version": "v53.0",
        "sha256": "448af3d4e59b22c2987f7df94c213ad40fb53a10d437e42b5ee6c4fce7c29ecc",
    },
    "cloud-hypervisor-edk2": {
        "version": "ch-1e1b96f126",
        "sha256": "9fb511fc0dd423d90a79615a90a8ace9b9e078b4a115ea2c459e0ac2f4e60218",
    },
    "passt": {
        "version": "2025_02_17.a1e48a0",
        "sha256": "40e59201765c60a0a5bbd0f2caae1aae3fd8f9a9a0628a835159fb2f17ff7025",
    },
    "qemu-img": {
        "version": "8.2.2",
        "sha256": "30bff329fe1001635cafcfebddc68a1c824d25110c66f968b428c4cf4785d75d",
    },
    "rust-hypervisor-firmware": {
        "version": "0.5.0",
        "sha256": "4a0a1e977368f6b15d2198a216bdedf9a350bf5e5ae07e29e695373ec16ad958",
    },
    "virtiofsd": {
        "version": "v1.14.0",
        "sha256": "9ad3e33c45dd816b24ad483b60ca469974ba54c3b37ef93be3da2a623986646f",
    },
}

EXPECTED_BUILD_VALUES = {
    "FIRESTONE_VERSION": "0.1.0",
    "RUST_VERSION": "1.85.0",
    "RUSTC_COMMIT": "4d91de4e48198da2e33413efdcd9cd2cc0c46688",
    "CARGO_COMMIT": "d73d2caf9e41a39daf2a8d6ce60ec80bf354d2a7",
    "RUST_IMAGE": "rust@sha256:bea885d2711087e67a9f7a7cd1a164976f4c35389478512af170730014d2452a",
    "GCC_VERSION": "14.2.0",
    "BINUTILS_VERSION": "2.43.1",
    "CARGO_LOCK_SHA256": EXPECTED_FILE_HASHES["Cargo.lock"],
    "DEPS_TOML_SHA256": EXPECTED_FILE_HASHES["deps.toml"],
    "MUSL_HEADERS_VERSION": "1.2.5-r11",
    "MUSL_HEADERS_X86_64_SHA256": "d3b5ab01046a92b9a168b790f516606e320f015cbd4deeb584c5e115a02124ba",
}

BASELINE_OPEN_VERIFY_ITEMS = {3, 7, 8, 14, 16}
USER_APPROVED_DEFERRALS = (
    "aarch64 runtime validation; aarch64 catalog and release paths are compile-only",
    "non-Linux supervisor authority",
    "hostile VMM or sidecar wrapper containment",
)

DOCTOR_MATRIX_RUNS = (
    {
        "distro": "ubuntu",
        "name": "Ubuntu 24.04.4",
        "image_manifest": "sha256:33ceb71981b602c1a7443a53469e4dba065f7503eab3078a2d7a57a2ab987517",
        "elapsed_seconds": 28,
    },
    {
        "distro": "fedora",
        "name": "Fedora 44",
        "image_manifest": "sha256:43b29f65a41eb9c35e1cd5323e3bdf3b655c2357a9f4f1ff2f9c2798e5045d80",
        "elapsed_seconds": 33,
    },
    {
        "distro": "arch",
        "name": "Arch Linux 20260823.0.578598",
        "image_manifest": "sha256:b860afd5823683f7ea389ba5f00d812f4fe55f6f286dea329d2abeefa535e309",
        "elapsed_seconds": 17,
    },
)
DOCTOR_WORKFLOW_PATH = ".github/workflows/m5-doctor-matrix.yml"
DOCTOR_BUILD_JOB = "Build and validate Linux x86_64 inputs"
DOCTOR_JOB_NAMES = {
    "ubuntu": "Doctor on Ubuntu 24.04",
    "fedora": "Doctor on Fedora 44",
    "arch": "Doctor on Arch Linux",
}


@dataclass(frozen=True)
class HarnessSpec:
    identifier: str
    source: str
    evidence_name: str
    timeout_seconds: int
    max_evidence_bytes: int
    e2e_ids: tuple[int, ...]
    verify_ids: tuple[int, ...]
    machine_names: tuple[str, ...]


HARNESSES = (
    HarnessSpec(
        "m1",
        "scripts/m1-kvm-e2e.py",
        "m1.json",
        3_600,
        4 * 1024 * 1024,
        (1, 5, 6, 7),
        (1, 2, 4, 5, 6, 9, 12),
        ("m1-graceful", "m1-convert", "m1-vmm-crash", "m1-shim-crash"),
    ),
    HarnessSpec(
        "m2",
        "scripts/m2-kvm-e2e.py",
        "m2.json",
        3_600,
        8 * 1024 * 1024,
        (2, 10),
        (11, 13, 17),
        ("ubuntu", "m2-readiness"),
    ),
    HarnessSpec(
        "m3",
        "scripts/m3-kvm-e2e.py",
        "m3.json",
        5_400,
        16 * 1024 * 1024,
        (3, 4, 8),
        (7, 8, 10, 14, 15, 16),
        ("m3-main", "m3-tap"),
    ),
    HarnessSpec(
        "m4",
        "scripts/m4-kvm-e2e.py",
        "m4.json",
        3_600,
        8 * 1024 * 1024,
        (9,),
        (),
        ("ubuntu",),
    ),
    HarnessSpec(
        "m5-catalog",
        "scripts/m5-catalog-kvm-e2e.py",
        "m5-catalog.json",
        9_000,
        1024 * 1024,
        (11,),
        (3,),
        (
            "catalog-ubuntu-24-04",
            "catalog-ubuntu-22-04",
            "catalog-debian-12",
            "catalog-debian-13",
            "catalog-fedora-44",
        ),
    ),
)

EXPECTED_SCENARIOS = {
    "m1": {
        "e2e_1_doctor",
        "e2e_5_graceful_stop",
        "verify_4_5_conversion_overlay_fio",
        "e2e_6_vmm_sigkill_restart",
        "e2e_7_shim_sigkill_stop",
    },
    "m2": {
        "doctor",
        "e2e_2",
        "empty_command_run_pty",
        "shell",
        "verify_11_17_regression",
        "verify_13_console",
        "e2e_10",
        "start_boundaries",
    },
    "m3": {
        "doctor",
        "create",
        "initial_start",
        "e2e_3",
        "e2e_4",
        "verify_10_initial_merge",
        "configured_key_v1",
        "verify_7_normal_stop",
        "unchanged_stop_start",
        "e2e_8_changed_restart",
        "e2e_8_unchanged_restart",
        "verify_7_vmm_crash",
        "verify_8_tap",
    },
    "m4": {"doctor", "e2e_9"},
}
CATALOG_REFERENCES = (
    "ubuntu:24.04",
    "ubuntu:22.04",
    "debian:12",
    "debian:13",
    "fedora:44",
)


def require(condition: bool, message: str) -> None:
    if not condition:
        raise GateError(message)


def utc_now() -> str:
    return dt.datetime.now(dt.UTC).isoformat()

def observed_interrupt_signal() -> int | None:
    if INTERRUPTED_SIGNAL is not None:
        return INTERRUPTED_SIGNAL
    pending = signal.sigpending().intersection(HANDLED_SIGNALS)
    if pending:
        return int(min(pending, key=int))
    return None


def interrupted_message() -> str | None:
    signum = observed_interrupt_signal()
    if signum is None:
        return None
    return f"final Linux MVP gate interrupted by signal {signum}"


def require_not_interrupted() -> None:
    message = interrupted_message()
    if message is not None:
        raise GateError(message)


def block_final_signals() -> None:
    global FINAL_SIGNAL_MASK
    if FINAL_SIGNAL_MASK is None:
        FINAL_SIGNAL_MASK = signal.pthread_sigmask(signal.SIG_BLOCK, HANDLED_SIGNALS)


def restore_final_signal_mask() -> None:
    global FINAL_SIGNAL_MASK
    if FINAL_SIGNAL_MASK is None:
        return
    previous = FINAL_SIGNAL_MASK
    FINAL_SIGNAL_MASK = None
    for signum in HANDLED_SIGNALS:
        signal.signal(signum, signal.SIG_DFL)
    signal.pthread_sigmask(signal.SIG_SETMASK, previous)


def compact_bytes(value: bytes, limit: int = 4_096) -> str:
    if len(value) > limit:
        value = value[-limit:]
        prefix = f"[... output truncated to {limit} bytes ...]\n"
    else:
        prefix = ""
    rendered: list[str] = []
    for byte in value:
        if byte == 0x0A:
            rendered.append("\n")
        elif 0x20 <= byte <= 0x7E:
            rendered.append(chr(byte))
        else:
            rendered.append(f"\\x{byte:02x}")
    return prefix + "".join(rendered)


def run_small(
    argv: list[str | os.PathLike[str]],
    *,
    timeout: float,
    cwd: Path = REPO_ROOT,
    environment: dict[str, str] | None = None,
    expected_codes: set[int] | None = None,
) -> subprocess.CompletedProcess[bytes]:
    command = [os.fspath(value) for value in argv]
    try:
        completed = subprocess.run(
            command,
            cwd=cwd,
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=timeout,
            check=False,
        )
    except subprocess.TimeoutExpired as error:
        raise GateError(f"command timed out after {timeout:.1f}s: {command!r}") from error
    require(
        len(completed.stdout) <= MAX_SMALL_OUTPUT_BYTES
        and len(completed.stderr) <= MAX_SMALL_OUTPUT_BYTES,
        f"command output exceeded 1 MiB: {command!r}",
    )
    if expected_codes is None:
        expected_codes = {0}
    require(
        completed.returncode in expected_codes,
        f"command exited {completed.returncode}: {command!r}; "
        f"stdout={compact_bytes(completed.stdout)!r}; "
        f"stderr={compact_bytes(completed.stderr)!r}",
    )
    return completed


def path_metadata(path: Path, *, exact_mode: int | None = None) -> os.stat_result:
    try:
        metadata = path.lstat()
    except OSError as error:
        raise GateError(f"cannot inspect {path}: {error}") from error
    require(stat.S_ISREG(metadata.st_mode), f"expected a regular file without symlinks: {path}")
    require(metadata.st_uid in {0, os.getuid()}, f"file has an untrusted owner: {path}")
    require(metadata.st_mode & 0o022 == 0, f"file is group/world writable: {path}")
    if exact_mode is not None:
        require(
            stat.S_IMODE(metadata.st_mode) == exact_mode,
            f"{path} must have mode {exact_mode:04o}",
        )
    return metadata


def open_regular(path: Path, *, exact_mode: int | None = None) -> tuple[int, os.stat_result]:
    before = path_metadata(path, exact_mode=exact_mode)
    flags = os.O_RDONLY | os.O_CLOEXEC | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(path, flags)
    except OSError as error:
        raise GateError(f"cannot open {path}: {error}") from error
    after = os.fstat(descriptor)
    try:
        require(stat.S_ISREG(after.st_mode), f"opened object is not a regular file: {path}")
        require(
            (after.st_dev, after.st_ino) == (before.st_dev, before.st_ino),
            f"file changed while opening: {path}",
        )
    except BaseException:
        os.close(descriptor)
        raise
    return descriptor, after


def sha256_descriptor(descriptor: int) -> str:
    digest = hashlib.sha256()
    os.lseek(descriptor, 0, os.SEEK_SET)
    while block := os.read(descriptor, 1024 * 1024):
        digest.update(block)
    os.lseek(descriptor, 0, os.SEEK_SET)
    return digest.hexdigest()


def sha256_regular(path: Path, *, exact_mode: int | None = None) -> str:
    descriptor, _ = open_regular(path, exact_mode=exact_mode)
    try:
        return sha256_descriptor(descriptor)
    finally:
        os.close(descriptor)


def read_regular_bytes(
    path: Path,
    *,
    limit: int,
    exact_mode: int | None = None,
) -> bytes:
    descriptor, metadata = open_regular(path, exact_mode=exact_mode)
    try:
        require(metadata.st_size <= limit, f"{path} exceeds {limit} bytes")
        result = bytearray()
        while block := os.read(descriptor, min(65_536, limit + 1 - len(result))):
            result.extend(block)
            require(len(result) <= limit, f"{path} exceeds {limit} bytes")
        return bytes(result)
    finally:
        os.close(descriptor)


def validate_directory(path: Path, *, mode: int, empty: bool = False) -> None:
    try:
        metadata = path.lstat()
    except OSError as error:
        raise GateError(f"cannot inspect directory {path}: {error}") from error
    require(stat.S_ISDIR(metadata.st_mode), f"expected a directory without symlinks: {path}")
    require(metadata.st_uid == os.getuid(), f"directory has the wrong owner: {path}")
    require(stat.S_IMODE(metadata.st_mode) == mode, f"{path} must have mode {mode:04o}")
    if empty:
        require(not any(path.iterdir()), f"directory must be empty: {path}")


def validate_creation_parent(path: Path) -> None:
    require(path.is_absolute(), f"path must be absolute: {path}")
    require(".." not in path.parts, f"path must not contain '..': {path}")
    parent = path.parent
    try:
        resolved = parent.resolve(strict=True)
    except OSError as error:
        raise GateError(f"cannot resolve parent of {path}: {error}") from error
    require(resolved == parent, f"path parent must not contain symlinks: {parent}")
    metadata = parent.stat()
    require(stat.S_ISDIR(metadata.st_mode), f"path parent is not a directory: {parent}")
    require(metadata.st_uid in {0, os.getuid()}, f"path parent has an untrusted owner: {parent}")
    writable = stat.S_IMODE(metadata.st_mode) & 0o022
    sticky_root = metadata.st_uid == 0 and bool(metadata.st_mode & stat.S_ISVTX)
    require(not writable or sticky_root, f"path parent is writable without root sticky protection: {parent}")


def atomic_write_private(path: Path, payload: bytes, *, limit: int) -> None:
    require(len(payload) <= limit, f"private manifest exceeds {limit} bytes: {path.name}")
    temporary = path.with_name(f".{path.name}.{os.getpid()}.{uuid.uuid4().hex}.tmp")
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC | getattr(os, "O_NOFOLLOW", 0)
    descriptor = os.open(temporary, flags, 0o600)
    try:
        with os.fdopen(descriptor, "wb", closefd=False) as stream:
            stream.write(payload)
            stream.flush()
            os.fsync(stream.fileno())
        os.close(descriptor)
        descriptor = -1
        os.replace(temporary, path)
        os.chmod(path, 0o600)
        directory = os.open(path.parent, os.O_RDONLY | os.O_CLOEXEC)
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


def validate_kvm_device(path: Path = Path("/dev/kvm")) -> dict[str, Any]:
    try:
        metadata = path.lstat()
    except OSError as error:
        raise GateError(f"cannot inspect {path}: {error}") from error
    require(stat.S_ISCHR(metadata.st_mode), f"{path} is not a real character device")
    flags = os.O_RDWR | os.O_CLOEXEC | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(path, flags)
    except OSError as error:
        raise GateError(f"cannot open {path} read/write: {error}") from error
    try:
        try:
            api_version = fcntl.ioctl(descriptor, KVM_GET_API_VERSION)
        except OSError as error:
            raise GateError(f"{path} did not answer KVM_GET_API_VERSION: {error}") from error
    finally:
        os.close(descriptor)
    require(api_version == KVM_API_VERSION, f"KVM API version is {api_version}, expected 12")
    return {
        "path": os.fspath(path),
        "character_device": True,
        "opened_read_write": True,
        "api_version": api_version,
        "major": os.major(metadata.st_rdev),
        "minor": os.minor(metadata.st_rdev),
        "mode": f"{stat.S_IMODE(metadata.st_mode):04o}",
        "uid": metadata.st_uid,
        "gid": metadata.st_gid,
    }


class GateLock:
    def __init__(self, path: Path) -> None:
        self.path = path
        self.descriptor = -1

    def acquire(self) -> None:
        flags = os.O_RDWR | os.O_CREAT | os.O_CLOEXEC | getattr(os, "O_NOFOLLOW", 0)
        try:
            descriptor = os.open(self.path, flags, 0o600)
        except OSError as error:
            raise GateError(f"cannot open final-gate lock {self.path}: {error}") from error
        metadata = os.fstat(descriptor)
        try:
            require(stat.S_ISREG(metadata.st_mode), "final-gate lock is not a regular file")
            require(metadata.st_uid == os.getuid(), "final-gate lock has the wrong owner")
            require(stat.S_IMODE(metadata.st_mode) == 0o600, "final-gate lock is not mode 0600")
            try:
                fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError as error:
                raise GateError("another final Linux MVP gate owns the host setup lock") from error
            os.ftruncate(descriptor, 0)
            os.write(descriptor, f"pid={os.getpid()}\n".encode("ascii"))
            os.fsync(descriptor)
        except BaseException:
            os.close(descriptor)
            raise
        self.descriptor = descriptor

    def close(self) -> None:
        if self.descriptor >= 0:
            try:
                fcntl.flock(self.descriptor, fcntl.LOCK_UN)
            finally:
                os.close(self.descriptor)
                self.descriptor = -1

    def __enter__(self) -> GateLock:
        self.acquire()
        return self

    def __exit__(self, _kind: object, _value: object, _traceback: object) -> None:
        self.close()


def gate_lock_path() -> Path:
    runtime = Path(f"/run/user/{os.getuid()}")
    try:
        metadata = runtime.lstat()
    except OSError:
        runtime = Path("/tmp")
    else:
        if not (
            stat.S_ISDIR(metadata.st_mode)
            and metadata.st_uid == os.getuid()
            and stat.S_IMODE(metadata.st_mode) == 0o700
        ):
            runtime = Path("/tmp")
    return runtime / f"firestone-m5-linux-mvp-e2e-{os.getuid()}.lock"


def git_output(*arguments: str) -> str:
    completed = run_small(["git", "-C", REPO_ROOT, *arguments], timeout=15)
    try:
        return completed.stdout.decode("utf-8", errors="strict").strip()
    except UnicodeDecodeError as error:
        raise GateError(f"git output was not UTF-8: {arguments!r}") from error


def validate_repository(expected_commit: str) -> dict[str, Any]:
    require(COMMIT_PATTERN.fullmatch(expected_commit) is not None, "expected commit must be full lowercase 40-hex")
    top = Path(git_output("rev-parse", "--show-toplevel"))
    require(top == REPO_ROOT, f"git top level is {top}, expected {REPO_ROOT}")
    origin = git_output("remote", "get-url", "origin")
    require(origin in ACCEPTED_ORIGIN_URLS, f"origin is not {CANONICAL_REPOSITORY}: {origin!r}")
    branch = git_output("branch", "--show-current")
    require(branch == "main", f"final acceptance must run on main, not {branch!r}")
    head = git_output("rev-parse", "--verify", "HEAD")
    require(head == expected_commit, f"HEAD is {head}, expected {expected_commit}")
    origin_main = git_output("rev-parse", "--verify", "refs/remotes/origin/main")
    require(origin_main == expected_commit, f"origin/main is {origin_main}, expected {expected_commit}")
    status_output = run_small(
        ["git", "-C", REPO_ROOT, "status", "--porcelain=v1", "--untracked-files=all"],
        timeout=30,
    ).stdout
    require(status_output == b"", "git worktree must be completely clean, including untracked files")
    return {
        "canonical": CANONICAL_REPOSITORY,
        "origin": origin,
        "branch": branch,
        "commit": head,
        "origin_main": origin_main,
        "clean": True,
        "gate_source": {
            "path": "scripts/m5-linux-mvp-e2e.py",
            "sha256": sha256_regular(Path(__file__)),
        },
    }


def verify_doctor_workflow_attestation(
    path: Path,
    expected_sha256: str,
    expected_commit: str,
) -> dict[str, Any]:
    require(path.is_absolute(), "doctor attestation path must be absolute")
    require(path.parent.resolve(strict=True) == path.parent, "doctor attestation parent must not contain symlinks")
    require(SHA256_PATTERN.fullmatch(expected_sha256) is not None, "doctor attestation SHA-256 must be lowercase 64-hex")
    payload = read_regular_bytes(path, limit=64 * 1024, exact_mode=0o600)
    actual_sha256 = hashlib.sha256(payload).hexdigest()
    require(
        actual_sha256 == expected_sha256,
        f"doctor attestation SHA-256 is {actual_sha256}, expected {expected_sha256}",
    )
    try:
        document = json.loads(payload)
    except json.JSONDecodeError as error:
        raise GateError("doctor attestation is invalid JSON") from error
    require(isinstance(document, dict), "doctor attestation is not an object")
    require(document.get("schema") == 1, "doctor attestation schema changed")
    require(document.get("repository") == "0xchasercat/firestone", "doctor attestation repository changed")
    require(document.get("workflow") == DOCTOR_WORKFLOW_PATH, "doctor attestation workflow changed")
    require(document.get("head_sha") == expected_commit, "doctor attestation did not verify accepted main")
    require(document.get("head_branch") == "main", "doctor attestation did not verify main")
    require(document.get("status") == "completed", "doctor attestation run is incomplete")
    require(document.get("conclusion") == "success", "doctor attestation run failed")
    run_id = document.get("run_id")
    require(isinstance(run_id, int) and run_id > 0, "doctor attestation run ID is invalid")
    run_url = f"https://github.com/0xchasercat/firestone/actions/runs/{run_id}"
    require(document.get("run_url") == run_url, "doctor attestation run URL changed")

    build = document.get("build_job")
    require(isinstance(build, dict), "doctor attestation build job is missing")
    require(build.get("name") == DOCTOR_BUILD_JOB, "doctor attestation build job changed")
    require(build.get("status") == "completed" and build.get("conclusion") == "success", "doctor attestation build job failed")
    build_id = build.get("job_id")
    require(isinstance(build_id, int) and build_id > 0, "doctor attestation build job ID is invalid")
    require(build.get("url") == f"{run_url}/job/{build_id}", "doctor attestation build job URL changed")

    rows = document.get("rows")
    require(isinstance(rows, dict) and set(rows) == set(DOCTOR_JOB_NAMES), "doctor attestation row set changed")
    for distro, name in DOCTOR_JOB_NAMES.items():
        job = rows[distro]
        require(isinstance(job, dict), f"doctor attestation row is invalid: {distro}")
        require(job.get("name") == name, f"doctor attestation job name changed: {distro}")
        require(job.get("status") == "completed" and job.get("conclusion") == "success", f"doctor attestation row failed: {distro}")
        job_id = job.get("job_id")
        require(isinstance(job_id, int) and job_id > 0, f"doctor attestation job ID is invalid: {distro}")
        require(job.get("url") == f"{run_url}/job/{job_id}", f"doctor attestation job URL changed: {distro}")

    result = dict(document)
    result["verified_from_prevalidated_manifest"] = True
    result["attestation"] = {
        "file": path.name,
        "sha256": actual_sha256,
        "bytes": len(payload),
        "mode": "0600",
    }
    return result



def prepare_repository_snapshot(work_root: Path, expected_commit: str) -> Path:
    archive = work_root / "repository.tar"
    descriptor = os.open(
        archive,
        os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC | getattr(os, "O_NOFOLLOW", 0),
        0o600,
    )
    try:
        with os.fdopen(descriptor, "wb", closefd=False) as stream:
            try:
                completed = subprocess.run(
                    ["git", "-C", REPO_ROOT, "archive", "--format=tar", expected_commit],
                    cwd=REPO_ROOT,
                    stdin=subprocess.DEVNULL,
                    stdout=stream,
                    stderr=subprocess.PIPE,
                    timeout=120,
                    check=False,
                )
            except subprocess.TimeoutExpired as error:
                raise GateError("git archive timed out after 120s") from error
            require(
                len(completed.stderr) <= MAX_SMALL_OUTPUT_BYTES,
                "git archive stderr exceeded 1 MiB",
            )
            require(
                completed.returncode == 0,
                f"git archive exited {completed.returncode}: {compact_bytes(completed.stderr)}",
            )
            stream.flush()
            os.fsync(stream.fileno())
        os.close(descriptor)
        descriptor = -1
    finally:
        if descriptor >= 0:
            os.close(descriptor)

    archive_size = path_metadata(archive, exact_mode=0o600).st_size
    require(0 < archive_size <= MAX_REPOSITORY_ARCHIVE_BYTES, "repository archive size is invalid")
    snapshot = work_root / "repository"
    snapshot.mkdir(mode=0o700)
    total_bytes = 0
    try:
        with tarfile.open(archive, mode="r:") as repository:
            for member in repository.getmembers():
                relative = Path(member.name)
                require(
                    member.name != ""
                    and not relative.is_absolute()
                    and ".." not in relative.parts,
                    f"unsafe repository archive member: {member.name!r}",
                )
                destination = snapshot / relative
                require(
                    destination == snapshot or snapshot in destination.parents,
                    f"repository member escapes snapshot: {member.name!r}",
                )
                if member.isdir():
                    destination.mkdir(mode=0o700, parents=True, exist_ok=True)
                    os.chmod(destination, 0o700)
                    continue
                require(member.isfile(), f"unsupported repository archive member: {member.name!r}")
                require(member.size <= MAX_REPOSITORY_ARCHIVE_BYTES, "repository member is too large")
                total_bytes += member.size
                require(total_bytes <= MAX_REPOSITORY_ARCHIVE_BYTES, "repository snapshot is too large")
                destination.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
                source = repository.extractfile(member)
                require(source is not None, f"cannot read repository member: {member.name!r}")
                mode = 0o700 if member.mode & 0o111 else 0o600
                output = os.open(
                    destination,
                    os.O_WRONLY
                    | os.O_CREAT
                    | os.O_EXCL
                    | os.O_CLOEXEC
                    | getattr(os, "O_NOFOLLOW", 0),
                    mode,
                )
                try:
                    remaining = member.size
                    while remaining:
                        block = source.read(min(1024 * 1024, remaining))
                        require(block != b"", f"repository member was truncated: {member.name!r}")
                        view = memoryview(block)
                        while view:
                            written = os.write(output, view)
                            require(written > 0, "repository snapshot accepted no bytes")
                            view = view[written:]
                        remaining -= len(block)
                    require(source.read(1) == b"", f"repository member exceeded its declared size: {member.name!r}")
                    os.fsync(output)
                finally:
                    os.close(output)
                    source.close()
                os.chmod(destination, mode)
    except (tarfile.TarError, OSError) as error:
        raise GateError(f"cannot extract exact repository snapshot: {error}") from error
    finally:
        archive.unlink(missing_ok=True)
    validate_directory(snapshot, mode=0o700)
    return snapshot


def validate_snapshot_hashes(root: Path) -> dict[str, str]:
    observed: dict[str, str] = {}
    for relative, expected in EXPECTED_FILE_HASHES.items():
        actual = sha256_regular(root / relative)
        require(actual == expected, f"{relative} SHA-256 is {actual}, expected {expected}")
        observed[relative] = actual
    return observed



def parse_env_file(path: Path) -> dict[str, str]:
    result: dict[str, str] = {}
    for line in read_regular_bytes(path, limit=64 * 1024).decode("utf-8").splitlines():
        if not line or line.startswith("#"):
            continue
        key, separator, value = line.partition("=")
        require(separator == "=" and key, f"invalid assignment in {path}: {line!r}")
        require(key not in result, f"duplicate assignment {key} in {path}")
        result[key] = value
    return result


def validate_pins(root: Path = REPO_ROOT) -> dict[str, Any]:
    observed_files = validate_snapshot_hashes(root)

    build_values = parse_env_file(root / "build/firestone/versions.env")
    for key, expected in EXPECTED_BUILD_VALUES.items():
        require(build_values.get(key) == expected, f"release pin {key} changed")

    dependencies_document = tomllib.loads(
        read_regular_bytes(root / "deps.toml", limit=1024 * 1024).decode("utf-8")
    )
    require(dependencies_document.get("manifest_version") == 1, "deps.toml manifest version changed")
    dependencies = dependencies_document.get("dependency")
    require(isinstance(dependencies, dict), "deps.toml dependency table is missing")
    require(
        set(dependencies) == set(EXPECTED_RELEASE_DEPENDENCIES),
        f"deps.toml dependency set changed: {sorted(dependencies)}",
    )

    recorded_dependencies: dict[str, Any] = {}
    for name in sorted(dependencies):
        dependency = dependencies[name]
        require(isinstance(dependency, dict), f"dependency {name} is not a table")
        record: dict[str, Any] = {"version": dependency.get("version")}
        if dependency.get("commit") is not None:
            record["commit"] = dependency["commit"]
        artifacts: dict[str, str] = {}
        for architecture in ("x86_64", "aarch64", "source"):
            artifact = dependency.get(architecture)
            if not isinstance(artifact, dict) or "sha256" not in artifact:
                continue
            digest = artifact["sha256"]
            require(
                isinstance(digest, str) and SHA256_PATTERN.fullmatch(digest) is not None,
                f"dependency {name}.{architecture} has an invalid SHA-256",
            )
            artifacts[architecture] = digest
        record["artifacts"] = artifacts
        recorded_dependencies[name] = record

        release_expected = EXPECTED_RELEASE_DEPENDENCIES[name]
        require(dependency.get("version") == release_expected["version"], f"{name} version changed")
        x86 = dependency.get("x86_64")
        require(isinstance(x86, dict), f"{name}.x86_64 is missing")
        require(x86.get("sha256") == release_expected["sha256"], f"{name}.x86_64 SHA-256 changed")

    return {
        "files": observed_files,
        "release_inputs": {key: build_values[key] for key in EXPECTED_BUILD_VALUES},
        "dependencies": recorded_dependencies,
        "passt": {
            "version": "2025_02_17.a1e48a0",
            "commit": "a1e48a02ff3550eb7875a7df6726086e9b3a1213",
        },
    }


def validate_elf_x86_64_static_pie(descriptor: int, size: int) -> None:
    require(size >= 64, "release artifact is too short for an ELF header")
    header = os.pread(descriptor, 64, 0)
    require(header[:4] == b"\x7fELF", "release artifact is not ELF")
    require(header[4] == 2 and header[5] == 1, "release artifact must be 64-bit little-endian ELF")
    values = struct.unpack_from("<HHIQQQIHHHHHH", header, 16)
    elf_type, machine = values[0], values[1]
    program_offset, program_entry_size, program_count = values[4], values[8], values[9]
    require(elf_type == 3, f"release ELF type is {elf_type}, expected static PIE ET_DYN")
    require(machine == 62, f"release ELF machine is {machine}, expected x86_64")
    require(0 < program_count <= 256, "release ELF program-header count is invalid")
    require(program_entry_size >= 56, "release ELF program-header entry is too short")
    require(
        program_offset + program_entry_size * program_count <= size,
        "release ELF program-header table exceeds the file",
    )
    for index in range(program_count):
        offset = program_offset + index * program_entry_size
        program = os.pread(descriptor, 56, offset)
        require(len(program) == 56, "release ELF program header was truncated")
        program_type, _, file_offset, _, _, file_size, _, _ = struct.unpack("<IIQQQQQQ", program)
        require(program_type != 3, "release ELF contains PT_INTERP")
        if program_type != 2:
            continue
        require(file_size <= 1024 * 1024, "release ELF dynamic table is unreasonably large")
        require(file_offset + file_size <= size, "release ELF dynamic table exceeds the file")
        dynamic = os.pread(descriptor, file_size, file_offset)
        for entry_offset in range(0, len(dynamic) - 15, 16):
            tag, _ = struct.unpack_from("<qQ", dynamic, entry_offset)
            require(tag != 1, "release ELF contains a dynamic NEEDED entry")
            if tag == 0:
                break


def stage_release_artifact(
    source: Path,
    work_root: Path,
    expected_sha256: str,
) -> tuple[Path, dict[str, Any]]:
    require(source.is_absolute(), "release artifact path must be absolute")
    require(source.parent.resolve(strict=True) == source.parent, "release artifact parent must not contain symlinks")
    require(source.name == RELEASE_ARTIFACT_NAME, f"release artifact must be named {RELEASE_ARTIFACT_NAME}")
    require(SHA256_PATTERN.fullmatch(expected_sha256) is not None, "expected release SHA-256 must be lowercase 64-hex")
    descriptor, metadata = open_regular(source, exact_mode=0o755)
    try:
        require(0 < metadata.st_size <= MAX_RELEASE_ARTIFACT_BYTES, "release artifact size is invalid")
        digest = sha256_descriptor(descriptor)
        require(
            digest == expected_sha256,
            f"release artifact SHA-256 is {digest}, expected independently supplied {expected_sha256}",
        )
        validate_elf_x86_64_static_pie(descriptor, metadata.st_size)

        checksum_path = source.parent / RELEASE_CHECKSUM_NAME
        checksum_bytes = read_regular_bytes(checksum_path, limit=64 * 1024)
        expected_line = f"{digest}  {RELEASE_ARTIFACT_NAME}\n".encode("ascii")
        require(checksum_bytes == expected_line, "release SHA256SUMS does not exactly match the artifact")

        release_root = work_root / "release"
        release_root.mkdir(mode=0o700)
        staged = release_root / "firestone"
        output = os.open(
            staged,
            os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC | getattr(os, "O_NOFOLLOW", 0),
            0o700,
        )
        try:
            os.lseek(descriptor, 0, os.SEEK_SET)
            while block := os.read(descriptor, 1024 * 1024):
                view = memoryview(block)
                while view:
                    written = os.write(output, view)
                    require(written > 0, "staged release artifact accepted no bytes")
                    view = view[written:]
            os.fsync(output)
        finally:
            os.close(output)
        os.chmod(staged, 0o755)
    finally:
        os.close(descriptor)

    require(sha256_regular(staged) == digest, "staged release artifact changed bytes")
    checksum_sha = sha256_regular(source.parent / RELEASE_CHECKSUM_NAME)
    return staged, {
        "name": RELEASE_ARTIFACT_NAME,
        "sha256": digest,
        "bytes": metadata.st_size,
        "mode": "0755",
        "checksum_file": RELEASE_CHECKSUM_NAME,
        "checksum_file_sha256": checksum_sha,
        "target": "x86_64-unknown-linux-musl",
        "static_pie": True,
        "pt_interp_absent": True,
        "dynamic_needed_absent": True,
        "independently_supplied_sha256_match": True,
    }


def validate_release_identity(binary: Path, work_root: Path, expected_commit: str) -> None:
    version = run_small([binary, "--version"], timeout=10)
    require(version.stdout == b"firestone 0.1.0\n", "release --version output changed")
    require(version.stderr == b"", "release --version wrote stderr")

    home = work_root / "release-version-home"
    home.mkdir(mode=0o700)
    environment = os.environ.copy()
    environment["FIRESTONE_HOME"] = os.fspath(home)
    completed = run_small([binary, "--json", "version"], timeout=10, environment=environment)
    require(completed.stderr == b"", "release JSON version wrote stderr")
    try:
        records = [json.loads(line) for line in completed.stdout.splitlines() if line]
    except json.JSONDecodeError as error:
        raise GateError("release JSON version was not NDJSON") from error
    require(len(records) == 1, "release JSON version did not emit exactly one record")
    record = records[0]
    require(record.get("type") == "Result" and record.get("action") == "version", "release version Result changed")
    payload = record.get("payload")
    require(isinstance(payload, dict), "release version payload is not an object")
    require(payload.get("version") == "0.1.0", "release version payload changed")
    require(payload.get("architecture") == "x86_64", "release architecture is not x86_64")
    identity = payload.get("identity")
    require(isinstance(identity, dict), "release identity is missing")
    require(identity.get("release") == "v0.1.0", "release name changed")
    require(identity.get("git_commit") == expected_commit, "release commit does not match accepted main")
    require(payload.get("dependencies") == EXPECTED_RELEASE_DEPENDENCIES, "release dependency identities changed")
    shutil.rmtree(home)


def prepare_work_root() -> Path:
    root = Path(tempfile.mkdtemp(prefix="firestone-m5-linux-mvp-e2e.", dir="/tmp"))
    os.chmod(root, 0o700)
    validate_directory(root, mode=0o700, empty=True)
    return root


def prepare_evidence_directory(path: Path, *, resume: bool) -> None:
    validate_creation_parent(path)
    if resume:
        require(path.resolve(strict=True) == path, "resume evidence path must not contain symlinks")
        validate_directory(path, mode=0o700)
        return
    try:
        path.mkdir(mode=0o700)
    except OSError as error:
        raise GateError(f"cannot create evidence directory {path}: {error}") from error
    validate_directory(path, mode=0o700, empty=True)


def allowed_evidence_names() -> set[str]:
    return {
        ACCEPTANCE_MANIFEST_NAME,
        ACCEPTANCE_CHECKSUM_NAME,
        *(spec.evidence_name for spec in HARNESSES),
    }


def validate_evidence_directory_entries(path: Path) -> None:
    allowed = allowed_evidence_names()
    for entry in path.iterdir():
        require(entry.name in allowed, f"unexpected evidence entry: {entry.name}")
        path_metadata(entry, exact_mode=0o600)


def write_checkpoint(evidence_dir: Path, manifest: dict[str, Any]) -> None:
    payload = (json.dumps(manifest, indent=2, sort_keys=True) + "\n").encode("utf-8")
    acceptance_path = evidence_dir / ACCEPTANCE_MANIFEST_NAME
    atomic_write_private(acceptance_path, payload, limit=MAX_ACCEPTANCE_MANIFEST_BYTES)

    names = [ACCEPTANCE_MANIFEST_NAME]
    for spec in HARNESSES:
        child_path = evidence_dir / spec.evidence_name
        if child_path.exists():
            read_child_evidence_document(spec, child_path)
            names.append(spec.evidence_name)
    lines = [f"{sha256_regular(evidence_dir / name, exact_mode=0o600)}  {name}\n" for name in sorted(names)]
    atomic_write_private(
        evidence_dir / ACCEPTANCE_CHECKSUM_NAME,
        "".join(lines).encode("ascii"),
        limit=64 * 1024,
    )
    validate_evidence_directory_entries(evidence_dir)


def verify_checkpoint(evidence_dir: Path) -> tuple[dict[str, Any], str]:
    validate_evidence_directory_entries(evidence_dir)
    checksum_path = evidence_dir / ACCEPTANCE_CHECKSUM_NAME
    checksum = read_regular_bytes(checksum_path, limit=64 * 1024, exact_mode=0o600)
    expected_names = {entry.name for entry in evidence_dir.iterdir()} - {ACCEPTANCE_CHECKSUM_NAME}
    observed_names: set[str] = set()
    for raw_line in checksum.splitlines():
        match = re.fullmatch(rb"([0-9a-f]{64})  ([A-Za-z0-9.-]+)", raw_line)
        require(match is not None, "resume SHA256SUMS contains a malformed line")
        digest = match.group(1).decode("ascii")
        name = match.group(2).decode("ascii")
        require(name not in observed_names, f"resume SHA256SUMS repeats {name}")
        require(name in expected_names, f"resume SHA256SUMS names an unexpected file: {name}")
        require(
            sha256_regular(evidence_dir / name, exact_mode=0o600) == digest,
            f"resume checksum failed for {name}",
        )
        observed_names.add(name)
    require(observed_names == expected_names, "resume SHA256SUMS does not cover every evidence manifest")
    acceptance = read_regular_bytes(
        evidence_dir / ACCEPTANCE_MANIFEST_NAME,
        limit=MAX_ACCEPTANCE_MANIFEST_BYTES,
        exact_mode=0o600,
    )
    try:
        manifest = json.loads(acceptance)
    except json.JSONDecodeError as error:
        raise GateError("resume acceptance manifest is invalid JSON") from error
    require(isinstance(manifest, dict), "resume acceptance manifest is not an object")
    for spec in HARNESSES:
        child_path = evidence_dir / spec.evidence_name
        if child_path.exists():
            read_child_evidence_document(spec, child_path)
    return manifest, hashlib.sha256(acceptance).hexdigest()


def evidence_fact(path: Path, limit: int) -> dict[str, Any]:
    metadata = path_metadata(path, exact_mode=0o600)
    require(metadata.st_size <= limit, f"evidence {path.name} exceeds {limit} bytes")
    return {
        "file": path.name,
        "sha256": sha256_regular(path, exact_mode=0o600),
        "bytes": metadata.st_size,
        "mode": "0600",
    }

def read_child_evidence_document(spec: HarnessSpec, path: Path) -> dict[str, Any]:
    payload = read_regular_bytes(path, limit=spec.max_evidence_bytes, exact_mode=0o600)
    try:
        document = json.loads(payload)
    except json.JSONDecodeError as error:
        raise GateError(f"{spec.identifier} evidence is invalid JSON") from error
    require(isinstance(document, dict), f"{spec.identifier} evidence is not an object")
    require(document.get("schema") == 1, f"{spec.identifier} evidence schema changed")
    require(document.get("result") in {"running", "failed", "passed"}, f"{spec.identifier} evidence result is invalid")
    return document


def nested_value(document: dict[str, Any], *keys: str) -> Any:
    value: Any = document
    for key in keys:
        require(isinstance(value, dict) and key in value, f"evidence is missing {'.'.join(keys)}")
        value = value[key]
    return value


def validate_installed_artifacts(document: dict[str, Any], identifier: str) -> None:
    artifacts = document.get("artifacts")
    require(isinstance(artifacts, dict), f"{identifier} artifact evidence is missing")
    require(
        set(artifacts) == set(EXPECTED_RELEASE_DEPENDENCIES),
        f"{identifier} artifact evidence has the wrong dependency set",
    )
    for name, expected in EXPECTED_RELEASE_DEPENDENCIES.items():
        artifact = artifacts.get(name)
        require(isinstance(artifact, dict), f"{identifier} artifact {name} is invalid")
        require(artifact.get("version") == expected["version"], f"{identifier} artifact {name} version changed")
        require(artifact.get("sha256") == expected["sha256"], f"{identifier} artifact {name} hash changed")


def evidence_mapping(value: Any, label: str) -> dict[str, Any]:
    require(isinstance(value, dict) and value, f"{label} evidence is empty or invalid")
    return value


def evidence_number(value: Any, label: str) -> float:
    require(isinstance(value, (int, float)) and not isinstance(value, bool) and value >= 0, f"{label} evidence is not a nonnegative number")
    return float(value)


def validate_stream_evidence(value: Any, label: str) -> None:
    stream = evidence_mapping(value, label)
    require(isinstance(stream.get("bytes"), int) and stream["bytes"] >= 0, f"{label} byte count is invalid")
    require(isinstance(stream.get("lines"), int) and stream["lines"] >= 0, f"{label} line count is invalid")
    require(isinstance(stream.get("sha256"), str) and SHA256_PATTERN.fullmatch(stream["sha256"]) is not None, f"{label} hash is invalid")


def validate_fio_evidence(value: Any, label: str) -> None:
    fio = evidence_mapping(value, label)
    for key in ("read_bw_bytes", "read_iops", "write_bw_bytes", "write_iops"):
        evidence_number(fio.get(key), f"{label}.{key}")


def validate_teardown_evidence(value: Any, label: str, expected_state: str) -> None:
    teardown = evidence_mapping(value, label)
    require(teardown.get("state") == expected_state, f"{label} final state changed")
    for key in ("runtime_removed", "sockets_removed", "state_process_ids_cleared"):
        require(teardown.get(key) is True, f"{label}.{key} is not true")
    processes = evidence_mapping(teardown.get("processes"), f"{label}.processes")
    require(processes.get("all_gone") is True, f"{label} retained processes")


def validate_m1_scenarios(scenarios: dict[str, Any]) -> None:
    graceful = evidence_mapping(scenarios["e2e_5_graceful_stop"], "m1 graceful stop")
    require(nested_value(graceful, "start_result", "status") == "running", "m1 graceful start did not run")
    require(isinstance(graceful.get("start_steps"), list) and graceful["start_steps"], "m1 start-step evidence is empty")
    require(isinstance(graceful.get("login_console_line"), str) and graceful["login_console_line"], "m1 login evidence is empty")
    cloud_status = graceful.get("cloud_init_status")
    require(isinstance(cloud_status, list) and "status: done" in cloud_status, "m1 cloud-init did not finish")
    require(nested_value(graceful, "api", "vmm_ping", "status") == 200, "m1 VMM ping evidence failed")
    require(nested_value(graceful, "api", "vm_info", "status") == 200, "m1 VMM info evidence failed")
    evidence_mapping(graceful.get("vmconfig"), "m1 VmConfig")
    evidence_mapping(graceful.get("image"), "m1 image")
    require(nested_value(graceful, "stop_result", "status") == "stopped", "m1 graceful stop result changed")
    require(nested_value(graceful, "last_exit", "reason") == "guest shutdown", "m1 graceful exit reason changed")
    require(isinstance(graceful.get("shutdown_console_line"), str) and graceful["shutdown_console_line"], "m1 shutdown evidence is empty")

    fio = evidence_mapping(scenarios["verify_4_5_conversion_overlay_fio"], "m1 conversion/fio")
    require(nested_value(fio, "image", "source_format") == "raw", "m1 raw source evidence changed")
    require(nested_value(fio, "image", "stored_format") == "qcow2", "m1 converted image evidence changed")
    evidence_mapping(fio.get("vmconfig"), "m1 converted VmConfig")
    require(isinstance(fio.get("fio_version"), str) and fio["fio_version"], "m1 fio version is missing")
    workload = evidence_mapping(fio.get("workload"), "m1 fio workload")
    require(
        workload.get("size") == "64m"
        and workload.get("rw") == "randrw"
        and workload.get("block_size") == "4k"
        and workload.get("runtime_seconds") == 10,
        "m1 fio workload changed",
    )
    validate_fio_evidence(fio.get("overlay"), "m1 overlay fio")
    validate_fio_evidence(fio.get("raw_auxiliary_disk"), "m1 raw fio")
    require(fio.get("threshold_applied") is False, "m1 fio evidence introduced a threshold")

    vmm = evidence_mapping(scenarios["e2e_6_vmm_sigkill_restart"], "m1 VMM crash")
    require(isinstance(vmm.get("vmm_pid"), int) and vmm["vmm_pid"] > 0, "m1 VMM pid is invalid")
    evidence_number(vmm.get("failed_after_ms"), "m1 VMM failure latency")
    evidence_mapping(vmm.get("failed_last_exit"), "m1 VMM failure exit")
    require(nested_value(vmm, "restart_result", "status") == "running", "m1 VMM restart did not run")
    require(isinstance(vmm.get("restart_login_console_line"), str) and vmm["restart_login_console_line"], "m1 VMM restart login is missing")

    shim = evidence_mapping(scenarios["e2e_7_shim_sigkill_stop"], "m1 shim crash")
    require(isinstance(shim.get("shim_pid"), int) and shim["shim_pid"] > 0, "m1 shim pid is invalid")
    evidence_number(shim.get("unsupervised_after_ms"), "m1 unsupervised latency")
    require(shim.get("listed_status") == "running (unsupervised)", "m1 shim recovery status changed")
    require(nested_value(shim, "stop_result", "status") == "stopped", "m1 unsupervised stop failed")
    require(nested_value(shim, "last_exit", "reason") == "guest shutdown", "m1 unsupervised exit reason changed")


def validate_m2_scenarios(scenarios: dict[str, Any]) -> None:
    e2e2 = evidence_mapping(scenarios["e2e_2"], "m2 E2E 2")
    for key in ("home_empty_before_preflight", "image_cache_empty_before_run", "warm_under_5_seconds", "ssh_command_completed"):
        require(e2e2.get(key) is True, f"m2 E2E 2 {key} is not true")
    evidence_number(e2e2.get("cold_run_ms"), "m2 cold run")
    require(0 <= evidence_number(e2e2.get("warm_run_ms"), "m2 warm run") < 5_000, "m2 warm run exceeded five seconds")
    evidence_mapping(e2e2.get("image"), "m2 image")
    for key in ("cold_stdout", "cold_stderr", "warm_stdout", "warm_stderr"):
        validate_stream_evidence(e2e2.get(key), f"m2 {key}")

    prompt = evidence_mapping(scenarios["empty_command_run_pty"], "m2 run PTY")
    require(prompt.get("root_prompt") is True and prompt.get("exit_code") == 0, "m2 run PTY failed")
    require(isinstance(prompt.get("guest_command_marker"), str) and prompt["guest_command_marker"], "m2 run marker is missing")

    shell = evidence_mapping(scenarios["shell"], "m2 shell")
    require(nested_value(shell, "argv", "stdout") == "M2_ARGV:alpha:beta", "m2 shell argv changed")
    require(nested_value(shell, "argv", "exit_code") == 0, "m2 shell argv failed")
    require(shell.get("users") == {"default": "root", "override": "ubuntu"}, "m2 shell users changed")
    require(shell.get("exit_code") == 37 and shell.get("guest_signal") == signal.SIGTERM and shell.get("signal_exit_code") == 255, "m2 shell exit propagation changed")

    units = evidence_mapping(scenarios["verify_11_17_regression"], "m2 guest units")
    facts = evidence_mapping(units.get("facts"), "m2 guest unit facts")
    expected_facts = {
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
    require(all(facts.get(key) == value for key, value in expected_facts.items()), "m2 guest unit facts changed")
    require(units.get("verify_11_status_unchanged") is True and units.get("verify_17_status_unchanged") is True, "m2 guest unit regression evidence failed")

    console = evidence_mapping(scenarios["verify_13_console"], "m2 console")
    for key in ("first_attach", "second_attach"):
        attach = evidence_mapping(console.get(key), f"m2 {key}")
        require(attach.get("exit_code") == 0 and attach.get("connected") is True and attach.get("interacted") is True and attach.get("terminal_restored") is True, f"m2 {key} failed")
    interrupted = evidence_mapping(console.get("interrupted_attach"), "m2 interrupted console")
    require(interrupted.get("exit_code") == 130 and interrupted.get("interrupted") is True and interrupted.get("terminal_restored") is True, "m2 interrupted console failed")
    require(console.get("staging_mode") == "0600" and console.get("console_log_mode") == "0600", "m2 console modes changed")
    require(console.get("marker_counts") == [1, 1], "m2 console markers changed")
    require(console.get("serial_history_complete") is True and console.get("concurrent_corruption") is False, "m2 console history evidence failed")

    structured = evidence_mapping(scenarios["e2e_10"], "m2 structured output")
    frames = evidence_mapping(structured.get("json"), "m2 JSON frames")
    require(set(frames) == {"ssh_config", "run", "shell", "console"}, "m2 JSON frame set changed")
    require(all(isinstance(frame, dict) and frame for frame in frames.values()), "m2 JSON frame evidence is empty")
    redirected = evidence_mapping(structured.get("redirected_non_tty"), "m2 redirected output")
    require(nested_value(redirected, "ssh_config", "exit_code") == 0, "m2 redirected ssh-config failed")
    require(nested_value(redirected, "console_error", "exit_code") == 2, "m2 redirected console error changed")

    boundaries = evidence_mapping(scenarios["start_boundaries"], "m2 start boundaries")
    require(set(boundaries) == {"default", "no_wait", "readiness_timeout", "interrupted_launch_rollback", "interrupted_readiness"}, "m2 start-boundary set changed")
    default = evidence_mapping(boundaries["default"], "m2 default readiness")
    require(nested_value(default, "result", "status") == "running", "m2 default readiness failed")
    ordered = default.get("ordered_steps")
    require(isinstance(ordered, list) and ["StepDone", "boot"] in ordered and ["StepDone", "ssh"] in ordered, "m2 readiness ordering evidence changed")
    no_wait = evidence_mapping(boundaries["no_wait"], "m2 no-wait")
    require(no_wait.get("status_at_return") == "running" and no_wait.get("boot_step_present") is False and no_wait.get("ssh_step_present") is False, "m2 no-wait evidence failed")
    timeout = evidence_mapping(boundaries["readiness_timeout"], "m2 readiness timeout")
    require(timeout.get("timeout") == "20s" and timeout.get("state_at_return") == "running" and timeout.get("vm_left_running") is True, "m2 readiness timeout evidence failed")
    rollback = evidence_mapping(boundaries["interrupted_launch_rollback"], "m2 interrupted launch")
    require(rollback.get("runtime_removed") is True and rollback.get("final_state") in {"created", "stopped"}, "m2 interrupted launch rollback failed")
    background = evidence_mapping(boundaries["interrupted_readiness"], "m2 interrupted readiness")
    require(background.get("vm_left_running") is True and background.get("final_state") == "running", "m2 interrupted readiness failed")


def validate_m3_scenarios(scenarios: dict[str, Any]) -> None:
    doctor = evidence_mapping(scenarios["doctor"], "m3 doctor")
    require(doctor.get("check_count") == 13 and nested_value(doctor, "user_namespaces", "status") == "ok", "m3 doctor evidence failed")
    require(nested_value(scenarios, "create", "result", "state", "status") == "created", "m3 create evidence failed")
    initial = evidence_mapping(scenarios["initial_start"], "m3 initial start")
    require(nested_value(initial, "result", "status") == "running", "m3 initial start failed")
    processes = nested_value(initial, "runtime", "inventory", "processes")
    require(isinstance(processes, dict) and set(processes) == {"shim", "vmm", "passt", "virtiofsd-0", "virtiofsd-1"}, "m3 initial process inventory changed")
    evidence_mapping(initial.get("seed"), "m3 initial seed")
    evidence_mapping(initial.get("known_hosts"), "m3 initial trust")

    forwarding = evidence_mapping(scenarios["e2e_3"], "m3 forwarding")
    require(nested_value(forwarding, "tcp", "guest_port") == 80, "m3 TCP forwarding changed")
    require(nested_value(forwarding, "udp", "guest_port") == 5353, "m3 UDP forwarding changed")
    validate_stream_evidence(nested_value(forwarding, "tcp", "curl"), "m3 TCP forwarding")
    validate_stream_evidence(nested_value(forwarding, "ssh_over_vsock", "stdout"), "m3 vsock SSH")
    require(nested_value(forwarding, "udp", "request_sha256") != nested_value(forwarding, "udp", "response_sha256"), "m3 UDP response did not transform the request")

    mounts = evidence_mapping(scenarios["e2e_4"], "m3 mounts")
    require(mounts.get("multiple_tags") == ["share0", "share1"] and mounts.get("readonly_write_denied") is True, "m3 mount evidence failed")
    require(nested_value(mounts, "facts", "rw_source") == "share0" and nested_value(mounts, "facts", "ro_source") == "share1", "m3 mount tags changed")

    merge = evidence_mapping(scenarios["verify_10_initial_merge"], "m3 initial cloud merge")
    require(merge.get("verify_10_merged_user_and_firestone_parts") is True, "m3 cloud merge evidence failed")
    require(nested_value(merge, "facts", "network_metric") == "111", "m3 initial network config changed")
    configured_key = evidence_mapping(scenarios["configured_key_v1"], "m3 configured key")
    require(configured_key.get("exit_code") == 0 and nested_value(configured_key, "known_hosts", "mode") == "0600", "m3 configured key evidence failed")

    normal = evidence_mapping(scenarios["verify_7_normal_stop"], "m3 normal stop")
    require(nested_value(normal, "result", "status") == "stopped", "m3 normal stop failed")
    validate_teardown_evidence(normal.get("teardown"), "m3 normal teardown", "stopped")
    unchanged_start = evidence_mapping(scenarios["unchanged_stop_start"], "m3 unchanged stop/start")
    require(unchanged_start.get("seed_preserved") is True and unchanged_start.get("trust_preserved") is True and nested_value(unchanged_start, "result", "status") == "running", "m3 unchanged stop/start failed")

    changed = evidence_mapping(scenarios["e2e_8_changed_restart"], "m3 changed restart")
    require(nested_value(changed, "result", "status") == "running", "m3 changed restart failed")
    require(nested_value(changed, "old_processes_gone", "all_gone") is True, "m3 changed restart retained processes")
    require(changed.get("stale_known_hosts_entries_absent") is True, "m3 changed restart retained trust")
    require(nested_value(changed, "merged_cloud_init", "verify_10_merged_user_and_firestone_parts") is True, "m3 changed cloud merge failed")
    require(nested_value(changed, "merged_cloud_init", "facts", "network_metric") == "222", "m3 changed network config failed")
    require(nested_value(changed, "new_key", "exit_code") == 0, "m3 changed SSH key failed")

    unchanged = evidence_mapping(scenarios["e2e_8_unchanged_restart"], "m3 unchanged restart")
    require(nested_value(unchanged, "result", "status") == "running", "m3 unchanged restart failed")
    require(nested_value(unchanged, "old_processes_gone", "all_gone") is True, "m3 unchanged restart retained processes")
    require(unchanged.get("instance_id_preserved") is True and unchanged.get("known_hosts_preserved") is True, "m3 unchanged restart identity changed")
    validate_teardown_evidence(scenarios["verify_7_vmm_crash"], "m3 VMM crash teardown", "failed")

    tap = evidence_mapping(scenarios["verify_8_tap"], "m3 TAP")
    require(tap.get("ip_and_mask_absent") is True and tap.get("tap_removed") is True, "m3 TAP evidence failed")
    require(tap.get("firestone_user_cap_eff") == "0000000000000000" and tap.get("vmm_cap_eff") == "0000000000000000", "m3 TAP capabilities changed")
    require(nested_value(tap, "launch_plan_network", "mode") == "tap", "m3 TAP launch plan changed")
    inventory = nested_value(tap, "inventory", "processes")
    require(isinstance(inventory, dict) and set(inventory) == {"shim", "vmm"}, "m3 TAP launched sidecars")
    require(nested_value(tap, "stop", "result", "status") == "stopped", "m3 TAP stop failed")
    validate_teardown_evidence(nested_value(tap, "stop", "teardown"), "m3 TAP teardown", "stopped")


def validate_m4_scenarios(scenarios: dict[str, Any]) -> None:
    require(nested_value(scenarios, "doctor", "check_count") == 13, "m4 doctor evidence count changed")
    e2e = evidence_mapping(scenarios["e2e_9"], "m4 E2E 9")
    socket_evidence = evidence_mapping(e2e.get("socket"), "m4 socket")
    require(socket_evidence.get("runtime_mode") == "0700" and socket_evidence.get("lock_mode") == "0600" and socket_evidence.get("atomic_mode_0600") is True, "m4 socket evidence failed")
    require(nested_value(e2e, "start", "status") == 200 and nested_value(e2e, "start", "terminal_result_last") is True and nested_value(e2e, "start", "reported_running") is True, "m4 REST start evidence failed")
    require(nested_value(e2e, "cli_rest_show", "same_parsed_payload") is True and nested_value(e2e, "cli_rest_show", "byte_equal") is True, "m4 CLI/REST evidence failed")
    restart = evidence_mapping(e2e.get("serve_restart"), "m4 serve restart")
    require(restart.get("killed_returncode") < 0 and restart.get("shim_survived") is True and restart.get("vmm_survived") is True and restart.get("reported_running_after_restart") is True, "m4 serve restart evidence failed")
    require(nested_value(e2e, "stop", "status") == 200 and nested_value(e2e, "stop", "reported_stopped") is True, "m4 REST stop evidence failed")
    require(nested_value(e2e, "remove", "status") == 204 and nested_value(e2e, "remove", "empty_body") is True and nested_value(e2e, "remove", "machine_directory_absent") is True, "m4 REST removal evidence failed")


def validate_scenario_evidence(spec: HarnessSpec, document: dict[str, Any]) -> None:
    validate_installed_artifacts(document, spec.identifier)
    if spec.identifier == "m5-catalog":
        require(document.get("scenario") == "e2e11-catalog-matrix", "catalog evidence scenario changed")
        catalog = document.get("catalog")
        require(isinstance(catalog, dict), "catalog policy evidence is missing")
        require(catalog.get("references") == list(CATALOG_REFERENCES), "catalog evidence references changed")
        require(catalog.get("architecture") == "x86_64", "catalog evidence architecture changed")
        require(catalog.get("firmware") == "edk2", "catalog evidence firmware changed")
        require(catalog.get("default_user") == "root", "catalog evidence user changed")
        doctor = document.get("doctor")
        require(
            isinstance(doctor, dict)
            and doctor.get("check_count") == 13
            and doctor.get("failures") == [],
            "catalog doctor evidence did not pass all checks",
        )
        matrix = document.get("matrix")
        require(isinstance(matrix, dict), "catalog matrix evidence is missing")
        require(set(matrix) == set(CATALOG_REFERENCES), "catalog evidence rows changed")
        for reference in CATALOG_REFERENCES:
            row = evidence_mapping(matrix[reference], f"catalog row {reference}")
            require(row.get("machine") == "catalog-" + "".join(character if character.isalnum() else "-" for character in reference).lower(), f"{reference} machine name changed")
            require(row.get("create_status") == "created", f"{reference} create evidence failed")
            require(row.get("start_status") == "running", f"{reference} start evidence failed")
            readiness = row.get("readiness_steps")
            require(isinstance(readiness, list) and "boot" in readiness and "ssh" in readiness, f"{reference} readiness evidence failed")
            require(row.get("ssh_root_command") == "id -u" and row.get("ssh_root_uid") == 0, f"{reference} root SSH evidence failed")
            require(row.get("stop_status") == "stopped", f"{reference} stop evidence failed")
            require(row.get("removed") is True, f"{reference} removal evidence failed")
            image = evidence_mapping(row.get("image"), f"{reference} image")
            require(image.get("source_ref") == reference, f"{reference} source identity changed")
            require(image.get("architecture") == "x86_64" and image.get("firmware") == "edk2" and image.get("sshd_path") == "/usr/sbin/sshd", f"{reference} image policy changed")
            require(image.get("stored_sha256") == image.get("stored_sha256_recomputed"), f"{reference} stored image hash changed")
            for key in ("source_sha256", "stored_sha256", "stored_sha256_recomputed"):
                require(isinstance(image.get(key), str) and SHA256_PATTERN.fullmatch(image[key]) is not None, f"{reference} {key} is invalid")
            require(isinstance(image.get("verification_digest"), str) and image["verification_digest"], f"{reference} verification digest is missing")
            require(isinstance(image.get("size"), int) and image["size"] > 0, f"{reference} image size is invalid")
        return

    scenarios = document.get("scenarios")
    require(isinstance(scenarios, dict), f"{spec.identifier} scenario evidence is missing")
    require(
        set(scenarios) == EXPECTED_SCENARIOS[spec.identifier],
        f"{spec.identifier} scenario evidence keys changed",
    )
    doctor = scenarios.get("e2e_1_doctor" if spec.identifier == "m1" else "doctor")
    require(isinstance(doctor, dict), f"{spec.identifier} doctor evidence is missing")
    if spec.identifier == "m1":
        checks = doctor.get("checks")
        require(
            isinstance(checks, list)
            and len(checks) == 13
            and not any(isinstance(check, dict) and check.get("status") == "fail" for check in checks),
            "m1 doctor evidence did not pass all checks",
        )
        require(nested_value(scenarios, "e2e_5_graceful_stop", "api", "vmm_ping", "status") == 200, "m1 VMM ping evidence failed")
        require(nested_value(scenarios, "e2e_5_graceful_stop", "api", "vm_info", "status") == 200, "m1 VMM info evidence failed")
        require(nested_value(scenarios, "e2e_7_shim_sigkill_stop", "listed_status") == "running (unsupervised)", "m1 shim recovery evidence failed")
        validate_m1_scenarios(scenarios)
    elif spec.identifier == "m2":
        ssh_modes = nested_value(document, "ssh_file_modes")
        require(isinstance(ssh_modes, dict), "m2 SSH mode evidence is invalid")
        try:
            known_hosts_mode = int(str(ssh_modes.get("known_hosts")), 8)
        except ValueError as error:
            raise GateError("m2 known_hosts mode evidence is invalid") from error
        require(
            ssh_modes.get("private_key") == "0600"
            and ssh_modes.get("public_key") == "0644"
            and known_hosts_mode & 0o022 == 0
            and known_hosts_mode & 0o600 == 0o600,
            "m2 SSH mode evidence changed",
        )
        require(nested_value(scenarios, "doctor", "check_count") == 13, "m2 doctor evidence count changed")
        validate_m2_scenarios(scenarios)
    elif spec.identifier == "m3":
        passt = nested_value(document, "pins", "passt")
        require(
            isinstance(passt, dict)
            and passt.get("version") == "2025_02_17.a1e48a0"
            and passt.get("commit") == "a1e48a02ff3550eb7875a7df6726086e9b3a1213",
            "m3 passt evidence changed",
        )
        require(nested_value(scenarios, "verify_8_tap", "tap_removed") is True, "m3 TAP evidence did not clean up")
        require(nested_value(scenarios, "e2e_8_changed_restart", "stale_known_hosts_entries_absent") is True, "m3 changed restart evidence failed")
        require(nested_value(scenarios, "e2e_8_unchanged_restart", "instance_id_preserved") is True, "m3 unchanged restart evidence failed")
        require(nested_value(scenarios, "e2e_8_unchanged_restart", "known_hosts_preserved") is True, "m3 known-host evidence failed")
        userns_cleanup = nested_value(document, "host_setup", "user_namespaces", "cleanup")
        require(
            isinstance(userns_cleanup, dict)
            and (userns_cleanup.get("restored") is True or userns_cleanup.get("not_required") is True),
            "m3 user-namespace evidence did not restore host setup",
        )
        require(nested_value(document, "host_setup", "tap", "cleanup", "removed") is True, "m3 TAP host setup was not removed")
        validate_m3_scenarios(scenarios)
    elif spec.identifier == "m4":
        require(nested_value(scenarios, "doctor", "check_count") == 13, "m4 doctor evidence count changed")
        require(nested_value(scenarios, "e2e_9", "socket", "atomic_mode_0600") is True, "m4 socket evidence failed")
        require(nested_value(scenarios, "e2e_9", "cli_rest_show", "byte_equal") is True, "m4 CLI/REST evidence failed")
        require(nested_value(scenarios, "e2e_9", "remove", "machine_directory_absent") is True, "m4 removal evidence failed")
        validate_m4_scenarios(scenarios)


def validate_passed_evidence(
    spec: HarnessSpec,
    path: Path,
    *,
    expected_commit: str,
    release_sha256: str,
) -> dict[str, Any]:
    fact = evidence_fact(path, spec.max_evidence_bytes)
    payload = read_regular_bytes(path, limit=spec.max_evidence_bytes, exact_mode=0o600)
    try:
        document = json.loads(payload)
    except json.JSONDecodeError as error:
        raise GateError(f"{spec.identifier} evidence is invalid JSON") from error
    require(isinstance(document, dict), f"{spec.identifier} evidence is not an object")
    require(document.get("schema") == 1, f"{spec.identifier} evidence schema changed")
    require(document.get("result") == "passed", f"{spec.identifier} evidence did not pass")
    if spec.identifier != "m5-catalog":
        require(document.get("commit") == expected_commit, f"{spec.identifier} evidence commit changed")
    host = document.get("host")
    require(isinstance(host, dict), f"{spec.identifier} evidence host is missing")
    architecture = host.get("architecture", host.get("machine"))
    require(architecture == "x86_64", f"{spec.identifier} evidence is not x86_64")
    require(host.get("system") == "Linux", f"{spec.identifier} evidence is not Linux")
    if spec.identifier != "m5-catalog":
        require(
            host.get("firestone_sha256") == release_sha256,
            f"{spec.identifier} evidence used a different Firestone binary",
        )
    if spec.identifier in {"m2", "m3", "m4"}:
        require(
            host.get("harness_sha256") == EXPECTED_FILE_HASHES[spec.source],
            f"{spec.identifier} evidence used a different harness",
        )
    validate_scenario_evidence(spec, document)
    if spec.identifier != "m1":
        cleanup = document.get("cleanup")
        require(isinstance(cleanup, dict), f"{spec.identifier} evidence cleanup is missing")
        require(cleanup.get("completed") is True, f"{spec.identifier} cleanup did not complete")
        require(cleanup.get("home_removed") is True, f"{spec.identifier} home was not removed")
        require(cleanup.get("home_kept_by_request") is False, f"{spec.identifier} evidence kept its home")
        require(cleanup.get("errors") == [], f"{spec.identifier} evidence cleanup recorded errors")
        if spec.identifier == "m3":
            require(cleanup.get("tap_removed") is True, "m3 evidence left its TAP setup")
            require(cleanup.get("userns_policy_restored") is True, "m3 evidence left user-namespace setup")
    return fact


def build_manifest(
    *,
    repository: dict[str, Any],
    host: dict[str, Any],
    pins: dict[str, Any],
    release: dict[str, Any],
    doctor_run: dict[str, Any] | None = None,
) -> dict[str, Any]:
    if doctor_run is None:
        doctor_run = {
            "verified_from_prevalidated_manifest": False,
            "run_id": 33276918451,
            "repository": "0xchasercat/firestone",
            "workflow": DOCTOR_WORKFLOW_PATH,
            "head_sha": repository.get("commit"),
            "head_branch": "main",
            "status": "completed",
            "conclusion": "success",
            "rows": {
                distro: {
                    "job_id": index,
                    "name": name,
                    "status": "completed",
                    "conclusion": "success",
                }
                for index, (distro, name) in enumerate(DOCTOR_JOB_NAMES.items(), start=1)
            },
        }
    require(doctor_run.get("conclusion") == "success", "doctor workflow identity is not successful")
    harness_records = []
    for spec in HARNESSES:
        harness_records.append(
            {
                "id": spec.identifier,
                "status": "pending",
                "source": spec.source,
                "source_sha256": EXPECTED_FILE_HASHES[spec.source],
                "evidence": spec.evidence_name,
                "evidence_sha256": None,
                "timeout_seconds": spec.timeout_seconds,
                "max_evidence_bytes": spec.max_evidence_bytes,
                "e2e": list(spec.e2e_ids),
                "verify": list(spec.verify_ids),
                "execution": None,
                "cleanup": None,
            }
        )

    e2e = []
    for identifier in range(1, 12):
        source = next(spec for spec in HARNESSES if identifier in spec.e2e_ids)
        e2e.append(
            {
                "id": identifier,
                "status": "pending",
                "source_harness": source.identifier,
                "source_sha256": EXPECTED_FILE_HASHES[source.source],
                "evidence_sha256": None,
            }
        )

    verify = []
    for identifier in range(1, 18):
        source = next(spec for spec in HARNESSES if identifier in spec.verify_ids)
        verify.append(
            {
                "id": identifier,
                "status": "open" if identifier in BASELINE_OPEN_VERIFY_ITEMS else "resolved",
                "current_gate_status": "pending",
                "source_harness": source.identifier,
                "source_sha256": EXPECTED_FILE_HASHES[source.source],
                "evidence_sha256": None,
            }
        )

    rows = []
    doctor_rows = doctor_run.get("rows")
    require(isinstance(doctor_rows, dict), "verified doctor workflow rows are missing")
    for row in DOCTOR_MATRIX_RUNS:
        job = doctor_rows.get(row["distro"])
        require(isinstance(job, dict) and job.get("conclusion") == "success", f"doctor row failed: {row['distro']}")
        rows.append(
            {
                **row,
                "status": "passed",
                "github_run_id": doctor_run["run_id"],
                "github_job_id": job.get("job_id"),
                "github_job_name": job.get("name"),
                "kvm_evidence": False,
            }
        )

    return {
        "schema": 1,
        "gate": "linux-x86_64-mvp",
        "result": "running",
        "started_at": utc_now(),
        "finished_at": None,
        "repository": repository,
        "host": host,
        "release_artifact": release,
        "pins": pins,
        "timeouts": {
            "aggregate_seconds": AGGREGATE_TIMEOUT_SECONDS,
            "process_cleanup_grace_seconds": PROCESS_CLEANUP_GRACE_SECONDS,
            "catalog_rows_max": 5,
            "catalog_evidence_bytes_max": 1024 * 1024,
        },
        "doctor_matrix": {
            "workflow": DOCTOR_WORKFLOW_PATH,
            "source": {
                "path": "scripts/m5-doctor-matrix.py",
                "sha256": EXPECTED_FILE_HASHES["scripts/m5-doctor-matrix.py"],
            },
            "committed_evidence": {
                "path": "docs/verification/doctor-matrix.md",
                "sha256": EXPECTED_FILE_HASHES["docs/verification/doctor-matrix.md"],
            },
            "verified_run": doctor_run,
            "rows": rows,
        },
        "harnesses": harness_records,
        "e2e": e2e,
        "verify": verify,
        "deferrals": [
            {"scope": value, "user_approved": True} for value in USER_APPROVED_DEFERRALS
        ],
        "resume": None,
        "error": None,
    }


def harness_record(manifest: dict[str, Any], identifier: str) -> dict[str, Any]:
    records = manifest["harnesses"]
    return next(record for record in records if record["id"] == identifier)


def mark_harness_passed(
    manifest: dict[str, Any],
    spec: HarnessSpec,
    fact: dict[str, Any],
    *,
    execution: str,
) -> None:
    record = harness_record(manifest, spec.identifier)
    record["status"] = "passed"
    record["execution"] = execution
    record["evidence_sha256"] = fact["sha256"]
    for item in manifest["e2e"]:
        if item["id"] in spec.e2e_ids:
            item["status"] = "passed"
            item["evidence_sha256"] = fact["sha256"]
    for item in manifest["verify"]:
        if item["id"] in spec.verify_ids:
            item["status"] = "resolved"
            item["current_gate_status"] = "passed"
            item["evidence_sha256"] = fact["sha256"]


def validate_resume(
    evidence_dir: Path,
    current: dict[str, Any],
    *,
    expected_commit: str,
    release_sha256: str,
) -> set[str]:
    previous, acceptance_sha = verify_checkpoint(evidence_dir)
    require(previous.get("schema") == 1 and previous.get("gate") == current["gate"], "resume gate identity changed")
    require(previous.get("repository") == current["repository"], "resume repository identity changed")
    require(previous.get("release_artifact") == current["release_artifact"], "resume release artifact changed")
    require(previous.get("pins") == current["pins"], "resume dependency pins changed")
    require(previous.get("deferrals") == current["deferrals"], "resume deferral set changed")

    completed: set[str] = set()
    found_gap = False
    for spec in HARNESSES:
        previous_record = harness_record(previous, spec.identifier)
        if previous_record.get("status") != "passed":
            found_gap = True
            continue
        require(not found_gap, "resume evidence has a passed harness after an incomplete dependency")
        require(previous_record.get("source") == spec.source, f"resume source changed for {spec.identifier}")
        require(
            previous_record.get("source_sha256") == EXPECTED_FILE_HASHES[spec.source],
            f"resume source hash changed for {spec.identifier}",
        )
        fact = validate_passed_evidence(
            spec,
            evidence_dir / spec.evidence_name,
            expected_commit=expected_commit,
            release_sha256=release_sha256,
        )
        require(
            previous_record.get("evidence_sha256") == fact["sha256"],
            f"resume evidence hash changed for {spec.identifier}",
        )
        cleanup = previous_record.get("cleanup")
        require(isinstance(cleanup, dict), f"resume cleanup record is missing for {spec.identifier}")
        require(cleanup.get("home_removed") is True, f"resume home cleanup is incomplete for {spec.identifier}")
        require(cleanup.get("harness_removed_home") is True, f"resume harness cleanup is incomplete for {spec.identifier}")
        require(cleanup.get("host_setup_restored") is True, f"resume host cleanup is incomplete for {spec.identifier}")
        require(cleanup.get("errors") == [], f"resume cleanup recorded errors for {spec.identifier}")
        mark_harness_passed(current, spec, fact, execution="resumed_verified")
        harness_record(current, spec.identifier)["cleanup"] = cleanup
        completed.add(spec.identifier)
        print(
            f"resume: independently verified {spec.identifier} evidence "
            f"sha256={fact['sha256']}",
            flush=True,
        )

    cleared_suffix: list[str] = []
    for spec in HARNESSES:
        if spec.identifier in completed:
            continue
        stale = evidence_dir / spec.evidence_name
        if not stale.exists() and not stale.is_symlink():
            continue
        remove_owned_evidence_entry(stale)
        cleared_suffix.append(spec.identifier)
        print(f"resume: removed stale {spec.identifier} evidence before rerun", flush=True)

    current["resume"] = {
        "accepted": True,
        "prior_acceptance_sha256": acceptance_sha,
        "completed_harnesses": sorted(completed),
        "cleared_incomplete_evidence": cleared_suffix,
    }
    return completed


@dataclass(frozen=True)
class ProcessIdentity:
    pid: int
    start_ticks: int


def process_start_ticks(pid: int) -> int | None:
    try:
        value = (Path("/proc") / str(pid) / "stat").read_text(encoding="utf-8")
    except (FileNotFoundError, PermissionError, ProcessLookupError, OSError):
        return None
    close = value.rfind(")")
    if close < 0:
        return None
    fields = value[close + 2 :].split()
    if len(fields) <= 19:
        return None
    try:
        return int(fields[19])
    except ValueError:
        return None


def process_references_home(pid: int, home: Path) -> bool:
    entry = Path("/proc") / str(pid)
    try:
        if entry.stat().st_uid != os.getuid():
            return False
    except (FileNotFoundError, PermissionError, ProcessLookupError, OSError):
        return False
    home_bytes = os.fsencode(home)

    try:
        arguments = (entry / "cmdline").read_bytes()[:MAX_SMALL_OUTPUT_BYTES].split(b"\0")
    except (FileNotFoundError, PermissionError, ProcessLookupError, OSError):
        arguments = []
    if any(value == home_bytes or value.startswith(home_bytes + b"/") for value in arguments):
        return True

    try:
        variables = (entry / "environ").read_bytes()[:MAX_SMALL_OUTPUT_BYTES].split(b"\0")
    except (FileNotFoundError, PermissionError, ProcessLookupError, OSError):
        variables = []
    if b"FIRESTONE_HOME=" + home_bytes in variables:
        return True

    links = [entry / "cwd", entry / "exe"]
    try:
        links.extend((entry / "fd").iterdir())
    except (FileNotFoundError, PermissionError, ProcessLookupError, OSError):
        pass
    for link in links:
        try:
            target = os.fsencode(os.path.realpath(link))
        except OSError:
            continue
        if target == home_bytes or target.startswith(home_bytes + b"/"):
            return True
    return False


def home_processes(home: Path) -> list[ProcessIdentity]:
    proc = Path("/proc")
    if not proc.is_dir():
        return []
    excluded = {os.getpid()}
    parent = os.getppid()
    while parent > 1 and parent not in excluded:
        excluded.add(parent)
        status = proc / str(parent) / "status"
        try:
            parent_line = next(
                line for line in status.read_text(encoding="utf-8").splitlines()
                if line.startswith("PPid:")
            )
            parent = int(parent_line.split()[1])
        except (FileNotFoundError, PermissionError, StopIteration, ValueError, OSError):
            break
    result: list[ProcessIdentity] = []
    for entry in proc.iterdir():
        if not entry.name.isdigit():
            continue
        pid = int(entry.name)
        if pid in excluded or not process_references_home(pid, home):
            continue
        start_ticks = process_start_ticks(pid)
        if start_ticks is not None:
            result.append(ProcessIdentity(pid, start_ticks))
    return result


def identity_alive(identity: ProcessIdentity) -> bool:
    return process_start_ticks(identity.pid) == identity.start_ticks


def terminate_home_processes(home: Path) -> list[str]:
    errors: list[str] = []
    identities = home_processes(home)
    for identity in identities:
        if not identity_alive(identity):
            continue
        try:
            os.kill(identity.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        except OSError as error:
            errors.append(f"cannot terminate owned pid {identity.pid}: {error}")
    deadline = time.monotonic() + HOME_PROCESS_TERM_SECONDS
    while any(identity_alive(identity) for identity in identities) and time.monotonic() < deadline:
        time.sleep(0.05)
    for identity in identities:
        if not identity_alive(identity):
            continue
        try:
            os.kill(identity.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        except OSError as error:
            errors.append(f"cannot kill owned pid {identity.pid}: {error}")
    deadline = time.monotonic() + HOME_PROCESS_KILL_SECONDS
    while any(identity_alive(identity) for identity in identities) and time.monotonic() < deadline:
        time.sleep(0.05)
    survivors = [identity.pid for identity in identities if identity_alive(identity)]
    if survivors:
        errors.append(f"owned processes survived cleanup: {survivors}")
    return errors


def harness_leader_owns_group(
    process: subprocess.Popen[bytes], identity: ProcessIdentity | None
) -> bool:
    if identity is None or identity.pid != process.pid or process.poll() is not None:
        return False
    if not identity_alive(identity):
        return False
    try:
        return os.getpgid(identity.pid) == identity.pid
    except (ProcessLookupError, PermissionError, OSError):
        return False


def terminate_process_group(
    process: subprocess.Popen[bytes], identity: ProcessIdentity | None, home: Path
) -> list[str]:
    errors: list[str] = []
    process_group = process.pid
    if not harness_leader_owns_group(process, identity):
        process.poll()
        return terminate_home_processes(home)
    try:
        os.killpg(process_group, signal.SIGTERM)
    except ProcessLookupError:
        pass
    except OSError as error:
        errors.append(f"cannot terminate harness process group {process_group}: {error}")
    deadline = time.monotonic() + PROCESS_CLEANUP_GRACE_SECONDS
    while harness_leader_owns_group(process, identity) and time.monotonic() < deadline:
        time.sleep(0.05)
    if harness_leader_owns_group(process, identity):
        try:
            os.killpg(process_group, signal.SIGKILL)
        except ProcessLookupError:
            pass
        except OSError as error:
            errors.append(f"cannot kill harness process group {process_group}: {error}")
    deadline = time.monotonic() + HOME_PROCESS_KILL_SECONDS
    while harness_leader_owns_group(process, identity) and time.monotonic() < deadline:
        time.sleep(0.05)
    if harness_leader_owns_group(process, identity):
        errors.append(f"harness process group {process_group} survived SIGKILL")
    process.poll()
    errors.extend(terminate_home_processes(home))
    return errors


@dataclass(frozen=True)
class M3HostSnapshot:
    userns_value: str | None
    network_names: frozenset[str]


def m3_host_snapshot() -> M3HostSnapshot:
    policy = Path("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
    try:
        value = policy.read_text(encoding="utf-8").strip()
    except FileNotFoundError:
        value = None
    networks = Path("/sys/class/net")
    names = frozenset(entry.name for entry in networks.iterdir()) if networks.is_dir() else frozenset()
    return M3HostSnapshot(value, names)


def cleanup_m3_host_setup(snapshot: M3HostSnapshot, harness_pid: int) -> list[str]:
    errors: list[str] = []
    networks = Path("/sys/class/net")
    current = {entry.name for entry in networks.iterdir()} if networks.is_dir() else set()
    prefix = f"fst{harness_pid:x}"
    for name in sorted(current - set(snapshot.network_names)):
        if not name.startswith(prefix) or re.fullmatch(r"[A-Za-z0-9_.-]{1,15}", name) is None:
            continue
        try:
            run_small(
                ["sudo", "-n", "ip", "tuntap", "del", "dev", name, "mode", "tap"],
                timeout=15,
            )
        except GateError as error:
            errors.append(f"cannot remove owned TAP {name}: {error}")
        if (networks / name).exists():
            errors.append(f"owned TAP survived cleanup: {name}")

    policy = Path("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
    if snapshot.userns_value is not None and policy.exists():
        try:
            current_value = policy.read_text(encoding="utf-8").strip()
        except OSError as error:
            errors.append(f"cannot read user-namespace policy during cleanup: {error}")
        else:
            if current_value != snapshot.userns_value:
                try:
                    run_small(
                        [
                            "sudo",
                            "-n",
                            "sysctl",
                            "-w",
                            f"kernel.apparmor_restrict_unprivileged_userns={snapshot.userns_value}",
                        ],
                        timeout=15,
                    )
                    restored = policy.read_text(encoding="utf-8").strip()
                    require(restored == snapshot.userns_value, "user-namespace policy restore did not stick")
                except (GateError, OSError) as error:
                    errors.append(f"cannot restore user-namespace policy: {error}")
    return errors


def remove_owned_evidence_entry(entry: Path) -> None:
    metadata = entry.lstat()
    require(metadata.st_uid == os.getuid(), f"evidence entry has the wrong owner: {entry.name}")
    if stat.S_ISDIR(metadata.st_mode) and not stat.S_ISLNK(metadata.st_mode):
        shutil.rmtree(entry)
    else:
        entry.unlink()


def sanitize_harness_evidence(
    evidence_dir: Path,
    spec: HarnessSpec,
    preexisting_names: set[str],
) -> list[str]:
    errors: list[str] = []
    expected_name = spec.evidence_name
    for entry in list(evidence_dir.iterdir()):
        if entry.name in preexisting_names:
            continue
        if entry.name == expected_name:
            try:
                read_child_evidence_document(spec, entry)
            except (GateError, OSError) as error:
                try:
                    remove_owned_evidence_entry(entry)
                except (GateError, OSError) as cleanup_error:
                    errors.append(f"cannot remove invalid {expected_name}: {cleanup_error}")
                errors.append(f"invalid {expected_name} removed: {error}")
            continue
        try:
            remove_owned_evidence_entry(entry)
        except (GateError, OSError) as error:
            errors.append(f"cannot remove unexpected evidence {entry.name}: {error}")
        else:
            errors.append(f"unexpected child evidence removed: {entry.name}")
    expected = evidence_dir / expected_name
    if expected.exists():
        try:
            read_child_evidence_document(spec, expected)
        except (GateError, OSError) as error:
            try:
                remove_owned_evidence_entry(expected)
            except (GateError, OSError) as cleanup_error:
                errors.append(f"cannot remove invalid {expected_name}: {cleanup_error}")
            errors.append(f"invalid {expected_name} removed: {error}")
    return errors


class HarnessExecutor:
    def __init__(
        self,
        *,
        work_root: Path,
        evidence_dir: Path,
        binary: Path,
        expected_commit: str,
        release_sha256: str,
        deadline: float,
        repository_root: Path = REPO_ROOT,
        git_dir: Path | None = None,
    ) -> None:
        self.work_root = work_root
        self.evidence_dir = evidence_dir
        self.binary = binary
        self.expected_commit = expected_commit
        self.release_sha256 = release_sha256
        self.deadline = deadline
        self.repository_root = repository_root
        self.git_dir = git_dir
        self.current_process: subprocess.Popen[bytes] | None = None
        self.current_process_identity: ProcessIdentity | None = None
        self.current_home: Path | None = None
        self.homes: dict[str, Path] = {}
        self.m3_snapshot: M3HostSnapshot | None = None
        self.m3_pid: int | None = None
        self._cleanup_started = False

    def child_environment(self, home: Path, evidence_path: Path) -> dict[str, str]:
        environment = os.environ.copy()
        environment["FIRESTONE_E2E"] = "1"
        environment["FIRESTONE_HOME"] = os.fspath(home)
        environment["FIRESTONE_BIN"] = os.fspath(self.binary)
        environment["FIRESTONE_E2E_EVIDENCE"] = os.fspath(evidence_path)
        environment["FIRESTONE_E2E_COMMIT"] = self.expected_commit
        environment["PYTHONUNBUFFERED"] = "1"
        environment.pop("FIRESTONE_E2E_KEEP", None)
        environment.pop("GIT_DIR", None)
        environment.pop("GIT_WORK_TREE", None)
        if self.git_dir is not None:
            environment["GIT_DIR"] = os.fspath(self.git_dir)
            environment["GIT_WORK_TREE"] = os.fspath(self.repository_root)
        return environment

    def force_cleanup_home(self, spec: HarnessSpec, home: Path) -> list[str]:
        errors: list[str] = []
        if not home.exists():
            return errors
        environment = self.child_environment(home, self.evidence_dir / spec.evidence_name)
        for name in reversed(spec.machine_names):
            for arguments in (
                [self.binary, "--json", "stop", name, "--force", "--timeout", "5s"],
                [self.binary, "--json", "rm", name, "--force"],
            ):
                try:
                    subprocess.run(
                        [os.fspath(value) for value in arguments],
                        cwd=self.repository_root,
                        env=environment,
                        stdin=subprocess.DEVNULL,
                        stdout=subprocess.DEVNULL,
                        stderr=subprocess.DEVNULL,
                        timeout=25,
                        check=False,
                    )
                except (OSError, subprocess.TimeoutExpired) as error:
                    errors.append(f"fallback cleanup command failed for {name}: {error}")
        errors.extend(terminate_home_processes(home))
        if not home_processes(home):
            try:
                shutil.rmtree(home)
            except FileNotFoundError:
                pass
            except OSError as error:
                errors.append(f"cannot remove owned home {home}: {error}")
        if home.exists():
            errors.append(f"owned home survived cleanup: {home.name}")
        return errors

    def run(self, spec: HarnessSpec) -> dict[str, Any]:
        home = self.work_root / f"home-{spec.identifier}"
        home.mkdir(mode=0o700)
        validate_directory(home, mode=0o700, empty=True)
        self.homes[spec.identifier] = home
        evidence_path = self.evidence_dir / spec.evidence_name
        require(
            not evidence_path.exists() and not evidence_path.is_symlink(),
            f"stale evidence must be removed before running {spec.identifier}",
        )
        preexisting_evidence_names = {entry.name for entry in self.evidence_dir.iterdir()}
        source = self.repository_root / spec.source
        started_at = utc_now()
        started = time.monotonic()
        remaining = self.deadline - started
        timeout = min(float(spec.timeout_seconds), remaining)
        require(timeout > 0, "aggregate acceptance timeout expired before the next harness")

        snapshot = m3_host_snapshot() if spec.identifier == "m3" else None
        if snapshot is not None:
            self.m3_snapshot = snapshot
        process: subprocess.Popen[bytes] | None = None
        process_identity: ProcessIdentity | None = None
        failure: str | None = None
        timed_out = False
        cleanup_errors: list[str] = []
        harness_removed_home = False
        returncode: int | None = None
        source_sha256 = EXPECTED_FILE_HASHES.get(spec.source, "")
        try:
            require_not_interrupted()
            source_sha256 = sha256_regular(source)
            expected_source_sha = EXPECTED_FILE_HASHES.get(spec.source, source_sha256)
            require(
                source_sha256 == expected_source_sha,
                f"{spec.identifier} source changed before execution",
            )
            print(
                f"==> {spec.identifier}: {spec.source} "
                f"(timeout {timeout:.0f}s, aggregate remaining {remaining:.0f}s)",
                flush=True,
            )
            self.current_home = home
            process = subprocess.Popen(
                [sys.executable, source],
                cwd=self.repository_root,
                env=self.child_environment(home, evidence_path),
                stdin=subprocess.DEVNULL,
                start_new_session=True,
            )
            self.current_process = process
            start_ticks = process_start_ticks(process.pid)
            if start_ticks is not None:
                process_identity = ProcessIdentity(process.pid, start_ticks)
                self.current_process_identity = process_identity
            else:
                require(
                    sys.platform != "linux" or process.poll() is not None,
                    "cannot capture live harness leader process identity",
                )
            if snapshot is not None:
                self.m3_pid = process.pid
            deadline = started + timeout
            while process.poll() is None:
                cancellation = interrupted_message()
                if cancellation is not None:
                    failure = cancellation
                    break
                remaining_wait = deadline - time.monotonic()
                if remaining_wait <= 0:
                    timed_out = True
                    failure = f"{spec.identifier} timed out after {timeout:.1f}s"
                    break
                try:
                    process.wait(timeout=min(1.0, remaining_wait))
                except subprocess.TimeoutExpired:
                    pass
            returncode = process.poll()
            if returncode not in {None, 0} and failure is None:
                failure = f"{spec.identifier} exited {returncode}"
        except (OSError, GateError) as error:
            failure = f"cannot run {spec.identifier}: {error}"
        finally:
            if process is not None:
                cleanup_errors.extend(terminate_process_group(process, process_identity, home))
                returncode = process.poll()
            self.current_process = None
            self.current_process_identity = None
            self.current_home = None
            harness_removed_home = not home.exists()
            cleanup_errors.extend(self.force_cleanup_home(spec, home))
            if snapshot is not None and process is not None:
                host_cleanup = cleanup_m3_host_setup(snapshot, process.pid)
                cleanup_errors.extend(host_cleanup)
                if not host_cleanup:
                    self.m3_snapshot = None
                    self.m3_pid = None
            cleanup_errors.extend(
                sanitize_harness_evidence(
                    self.evidence_dir,
                    spec,
                    preexisting_evidence_names,
                )
            )
            try:
                validate_snapshot_hashes(self.repository_root)
            except GateError as error:
                cleanup_errors.append(f"repository snapshot changed during {spec.identifier}: {error}")

        if failure is None and interrupted_message() is not None:
            failure = interrupted_message()
        elapsed_ms = round((time.monotonic() - started) * 1000, 3)
        record: dict[str, Any] = {
            "id": spec.identifier,
            "status": "failed",
            "source": spec.source,
            "source_sha256": source_sha256,
            "evidence": spec.evidence_name,
            "evidence_sha256": None,
            "timeout_seconds": spec.timeout_seconds,
            "max_evidence_bytes": spec.max_evidence_bytes,
            "e2e": list(spec.e2e_ids),
            "verify": list(spec.verify_ids),
            "execution": "executed",
            "started_at": started_at,
            "finished_at": utc_now(),
            "elapsed_ms": elapsed_ms,
            "returncode": returncode,
            "timed_out": timed_out,
            "cleanup": {
                "harness_removed_home": harness_removed_home,
                "home_removed": not home.exists(),
                "host_setup_restored": not cleanup_errors,
                "errors": cleanup_errors,
            },
        }

        if evidence_path.exists():
            try:
                fact = evidence_fact(evidence_path, spec.max_evidence_bytes)
                record["evidence_sha256"] = fact["sha256"]
            except GateError as error:
                cleanup_errors.append(str(error))

        if failure is None and not harness_removed_home:
            failure = f"{spec.identifier} returned success without removing its isolated home"
        if failure is None and cleanup_errors:
            failure = "; ".join(cleanup_errors)
        if failure is None:
            try:
                fact = validate_passed_evidence(
                    spec,
                    evidence_path,
                    expected_commit=self.expected_commit,
                    release_sha256=self.release_sha256,
                )
            except (GateError, OSError, ValueError, KeyError, TypeError) as error:
                failure = f"{spec.identifier} evidence validation failed: {error}"
            else:
                record["status"] = "passed"
                record["evidence_sha256"] = fact["sha256"]
                print(
                    f"<== {spec.identifier} passed; evidence sha256={fact['sha256']}",
                    flush=True,
                )
        if failure is not None:
            record["error"] = failure
        return record

    def cleanup(self) -> list[str]:
        if self._cleanup_started:
            return []
        self._cleanup_started = True
        errors: list[str] = []
        if self.current_process is not None:
            if self.current_home is not None:
                errors.extend(
                    terminate_process_group(
                        self.current_process,
                        self.current_process_identity,
                        self.current_home,
                    )
                )
            else:
                for home in self.homes.values():
                    errors.extend(terminate_home_processes(home))
            self.current_process = None
            self.current_process_identity = None
            self.current_home = None
        for spec in HARNESSES:
            home = self.homes.get(spec.identifier)
            if home is not None:
                errors.extend(self.force_cleanup_home(spec, home))
        if self.m3_snapshot is not None and self.m3_pid is not None:
            host_errors = cleanup_m3_host_setup(self.m3_snapshot, self.m3_pid)
            errors.extend(host_errors)
            if not host_errors:
                self.m3_snapshot = None
                self.m3_pid = None
        elif self.m3_snapshot is not None:
            self.m3_snapshot = None
        return errors


def replace_harness_record(manifest: dict[str, Any], replacement: dict[str, Any]) -> None:
    for index, record in enumerate(manifest["harnesses"]):
        if record["id"] == replacement["id"]:
            manifest["harnesses"][index] = replacement
            return
    raise GateError(f"manifest has no harness record for {replacement['id']}")


def execute_in_dependency_order(
    manifest: dict[str, Any],
    completed: set[str],
    run_one: Callable[[HarnessSpec], dict[str, Any]],
    checkpoint: Callable[[], None],
) -> str | None:
    for spec in HARNESSES:
        if spec.identifier in completed:
            continue
        record = run_one(spec)
        replace_harness_record(manifest, record)
        if record["status"] == "passed":
            fact = {
                "sha256": record["evidence_sha256"],
            }
            mark_harness_passed(manifest, spec, fact, execution="executed")
        checkpoint()
        if record["status"] != "passed":
            return str(record.get("error", f"{spec.identifier} failed"))
    return None


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--expected-commit",
        default=os.environ.get("FIRESTONE_E2E_COMMIT"),
        help="full origin/main commit; defaults to FIRESTONE_E2E_COMMIT",
    )
    parser.add_argument(
        "--release-artifact",
        type=Path,
        default=(
            Path(os.environ["FIRESTONE_E2E_RELEASE_ARTIFACT"])
            if os.environ.get("FIRESTONE_E2E_RELEASE_ARTIFACT")
            else None
        ),
        help="exact x86_64 release artifact; defaults to FIRESTONE_E2E_RELEASE_ARTIFACT",
    )
    parser.add_argument(
        "--expected-release-sha256",
        default=os.environ.get("FIRESTONE_E2E_RELEASE_SHA256"),
        help="independently obtained release digest; defaults to FIRESTONE_E2E_RELEASE_SHA256",
    )
    parser.add_argument(
        "--doctor-attestation",
        type=Path,
        default=(
            Path(os.environ["FIRESTONE_E2E_DOCTOR_ATTESTATION"])
            if os.environ.get("FIRESTONE_E2E_DOCTOR_ATTESTATION")
            else None
        ),
        help="prevalidated final-head workflow manifest; defaults to FIRESTONE_E2E_DOCTOR_ATTESTATION",
    )
    parser.add_argument(
        "--doctor-attestation-sha256",
        default=os.environ.get("FIRESTONE_E2E_DOCTOR_ATTESTATION_SHA256"),
        help="independently supplied manifest digest; defaults to FIRESTONE_E2E_DOCTOR_ATTESTATION_SHA256",
    )
    parser.add_argument(
        "--evidence-dir",
        type=Path,
        default=(
            Path(os.environ["FIRESTONE_E2E_EVIDENCE_DIR"])
            if os.environ.get("FIRESTONE_E2E_EVIDENCE_DIR")
            else None
        ),
        help="new mode-0700 evidence directory; defaults to FIRESTONE_E2E_EVIDENCE_DIR",
    )
    parser.add_argument(
        "--resume",
        action="store_true",
        help="resume only a checksum-verified contiguous completed prefix",
    )
    args = parser.parse_args(argv)
    require(args.expected_commit is not None, "--expected-commit or FIRESTONE_E2E_COMMIT is required")
    require(args.release_artifact is not None, "--release-artifact or FIRESTONE_E2E_RELEASE_ARTIFACT is required")
    require(
        args.expected_release_sha256 is not None,
        "--expected-release-sha256 or FIRESTONE_E2E_RELEASE_SHA256 is required",
    )
    require(
        args.doctor_attestation is not None,
        "--doctor-attestation or FIRESTONE_E2E_DOCTOR_ATTESTATION is required",
    )
    require(
        args.doctor_attestation_sha256 is not None,
        "--doctor-attestation-sha256 or FIRESTONE_E2E_DOCTOR_ATTESTATION_SHA256 is required",
    )
    require(args.evidence_dir is not None, "--evidence-dir or FIRESTONE_E2E_EVIDENCE_DIR is required")
    args.expected_commit = str(args.expected_commit)
    args.expected_release_sha256 = str(args.expected_release_sha256)
    args.doctor_attestation_sha256 = str(args.doctor_attestation_sha256)
    require(
        SHA256_PATTERN.fullmatch(args.doctor_attestation_sha256) is not None,
        "doctor attestation SHA-256 must be lowercase 64-hex",
    )
    require(
        SHA256_PATTERN.fullmatch(args.expected_release_sha256) is not None,
        "expected release SHA-256 must be lowercase 64-hex",
    )
    args.release_artifact = args.release_artifact.expanduser()
    args.doctor_attestation = args.doctor_attestation.expanduser()
    args.evidence_dir = args.evidence_dir.expanduser()
    require(args.release_artifact.is_absolute(), "release artifact path must be absolute")
    require(args.doctor_attestation.is_absolute(), "doctor attestation path must be absolute")
    require(args.evidence_dir.is_absolute(), "evidence directory path must be absolute")
    return args


def run_gate(args: argparse.Namespace) -> tuple[bool, Path | None, str | None]:
    require(sys.platform == "linux", "final Linux MVP acceptance requires Linux")
    require(platform.machine() == "x86_64", "final Linux MVP acceptance requires x86_64")
    require(os.geteuid() != 0, "final Linux MVP acceptance must run as an unprivileged user")
    kvm = validate_kvm_device()
    host = {
        "system": platform.system(),
        "kernel": platform.release(),
        "architecture": platform.machine(),
        "uid": os.getuid(),
        "euid": os.geteuid(),
        "kvm": kvm,
    }

    manifest: dict[str, Any] | None = None
    evidence_dir: Path | None = None
    work_root: Path | None = None
    executor: HarnessExecutor | None = None
    failure: str | None = None
    lock = GateLock(gate_lock_path())
    lock.acquire()
    try:
        repository = validate_repository(args.expected_commit)
        doctor_run = verify_doctor_workflow_attestation(
            args.doctor_attestation,
            args.doctor_attestation_sha256,
            args.expected_commit,
        )
        require_not_interrupted()
        work_root = prepare_work_root()
        repository_snapshot = prepare_repository_snapshot(work_root, args.expected_commit)
        pins = validate_pins(repository_snapshot)
        repository["execution_snapshot"] = {
            "source": "git archive",
            "commit": args.expected_commit,
            "private_root_mode": "0700",
        }
        git_dir = Path(git_output("rev-parse", "--absolute-git-dir"))
        staged_binary, release = stage_release_artifact(
            args.release_artifact,
            work_root,
            args.expected_release_sha256,
        )
        validate_release_identity(staged_binary, work_root, args.expected_commit)
        require_not_interrupted()

        evidence_dir = args.evidence_dir
        prepare_evidence_directory(evidence_dir, resume=args.resume)
        manifest = build_manifest(
            repository=repository,
            host=host,
            pins=pins,
            release=release,
            doctor_run=doctor_run,
        )
        completed: set[str] = set()
        if args.resume:
            completed = validate_resume(
                evidence_dir,
                manifest,
                expected_commit=args.expected_commit,
                release_sha256=release["sha256"],
            )
        write_checkpoint(evidence_dir, manifest)

        executor = HarnessExecutor(
            work_root=work_root,
            evidence_dir=evidence_dir,
            binary=staged_binary,
            expected_commit=args.expected_commit,
            release_sha256=release["sha256"],
            deadline=time.monotonic() + AGGREGATE_TIMEOUT_SECONDS,
            repository_root=repository_snapshot,
            git_dir=git_dir,
        )
        atexit.register(executor.cleanup)
        failure = execute_in_dependency_order(
            manifest,
            completed,
            executor.run,
            lambda: write_checkpoint(evidence_dir, manifest),
        )
        if failure is None and interrupted_message() is not None:
            failure = interrupted_message()
    except (GateError, OSError, ValueError, KeyError, TypeError) as error:
        failure = str(error)
    finally:
        block_final_signals()
        cleanup_errors: list[str] = []
        if executor is not None:
            cleanup_errors.extend(executor.cleanup())
        if work_root is not None:
            cleanup_errors.extend(terminate_home_processes(work_root))
            if not home_processes(work_root):
                try:
                    shutil.rmtree(work_root)
                except FileNotFoundError:
                    pass
                except OSError as error:
                    cleanup_errors.append(f"cannot remove aggregate work root: {error}")
            if work_root.exists():
                cleanup_errors.append("aggregate work root survived cleanup")
        if cleanup_errors:
            cleanup_message = "; ".join(cleanup_errors)
            failure = cleanup_message if failure is None else f"{failure}; {cleanup_message}"
        if failure is None and interrupted_message() is not None:
            failure = interrupted_message()
        if manifest is not None and evidence_dir is not None:
            while True:
                manifest["finished_at"] = utc_now()
                manifest["result"] = "passed" if failure is None else "failed"
                manifest["error"] = failure
                try:
                    write_checkpoint(evidence_dir, manifest)
                except (GateError, OSError) as error:
                    failure = f"cannot write final acceptance manifest: {error}"
                    break
                late_interrupt = interrupted_message()
                if failure is None and late_interrupt is not None:
                    failure = late_interrupt
                    continue
                break
        lock.close()

    return failure is None, evidence_dir, failure


def install_signal_handlers() -> None:
    global INTERRUPTED_SIGNAL
    INTERRUPTED_SIGNAL = None

    def interrupted(signum: int, _frame: object) -> None:
        global INTERRUPTED_SIGNAL
        if INTERRUPTED_SIGNAL is None:
            INTERRUPTED_SIGNAL = signum

    for signum in HANDLED_SIGNALS:
        signal.signal(signum, interrupted)


def main(argv: list[str] | None = None) -> int:
    arguments = sys.argv[1:] if argv is None else argv
    if os.environ.get("FIRESTONE_E2E") != "1":
        if any(argument in {"-h", "--help"} for argument in arguments):
            parse_args(arguments)
        print("skipped final Linux x86_64 MVP acceptance; set FIRESTONE_E2E=1 to run")
        return 0

    install_signal_handlers()
    try:
        args = parse_args(arguments)
        passed, evidence_dir, failure = run_gate(args)
    except (GateError, OSError, ValueError, KeyError, TypeError) as error:
        print(f"final Linux x86_64 MVP acceptance failed: {error}", file=sys.stderr)
        restore_final_signal_mask()
        return 1
    if not passed:
        print(f"final Linux x86_64 MVP acceptance failed: {failure}", file=sys.stderr)
        if evidence_dir is not None:
            print(f"failed evidence: {evidence_dir}", file=sys.stderr)
        restore_final_signal_mask()
        return 1
    require(evidence_dir is not None, "final evidence directory was not initialized")
    acceptance_path = evidence_dir / ACCEPTANCE_MANIFEST_NAME
    acceptance_sha = sha256_regular(acceptance_path, exact_mode=0o600)
    late_interrupt = interrupted_message()
    if late_interrupt is not None:
        document = json.loads(
            read_regular_bytes(
                acceptance_path,
                limit=MAX_ACCEPTANCE_MANIFEST_BYTES,
                exact_mode=0o600,
            )
        )
        require(isinstance(document, dict), "final acceptance manifest is not an object")
        document["result"] = "failed"
        document["error"] = late_interrupt
        document["finished_at"] = utc_now()
        write_checkpoint(evidence_dir, document)
        print(f"final Linux x86_64 MVP acceptance failed: {late_interrupt}", file=sys.stderr)
        restore_final_signal_mask()
        return 1
    restore_final_signal_mask()
    print(
        f"final Linux x86_64 MVP acceptance passed; evidence: {evidence_dir} "
        f"acceptance_sha256={acceptance_sha}",
        flush=True,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
