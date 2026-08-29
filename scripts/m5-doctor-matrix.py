#!/usr/bin/env python3
"""Validate fresh-host doctor behavior inside an unprivileged distro container."""

from __future__ import annotations

import argparse
import grp
import json
import os
import platform
import shutil
import stat
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any


class MatrixError(RuntimeError):
    """One fresh-host doctor contract failed."""


MINIMUM_SPACE_BYTES = 5 * 1024 * 1024 * 1024
MAX_OUTPUT_BYTES = 8 * 1024 * 1024
DOCTOR_TIMEOUT_SECONDS = 900
CHECK_IDS = (
    "host_arch",
    "kvm",
    "nested_virtualization",
    "runtime_dir",
    "vendored_binaries",
    "virtiofsd",
    "passt",
    "qemu_img",
    "ssh",
    "user_namespaces",
    "ssh_key",
    "data_space",
    "stale_state",
)
DISTROS = {
    "ubuntu": {
        "os_id": "ubuntu",
        "package_query": [
            "dpkg-query",
            "-W",
            "-f=${Package}\t${Version}\t${Status}\\n",
            "passt",
            "qemu-utils",
            "openssh-client",
            "util-linux",
        ],
        "packages": ("passt", "qemu-utils", "openssh-client", "util-linux"),
        "owner_query": ["dpkg-query", "-S"],
        "passt_fix": "sudo apt-get install -y build-essential ca-certificates git",
        "qemu_fix": "sudo apt-get install qemu-utils",
        "ssh_fix": "sudo apt-get install openssh-client",
        "passt_status": "fail",
    },
    "fedora": {
        "os_id": "fedora",
        "package_query": [
            "rpm",
            "-q",
            "--qf",
            "%{NAME}\\t%{VERSION}-%{RELEASE}\\n",
            "passt",
            "qemu-img",
            "openssh-clients",
            "util-linux",
        ],
        "packages": ("passt", "qemu-img", "openssh-clients", "util-linux"),
        "owner_query": ["rpm", "-qf"],
        "passt_fix": "sudo dnf install passt",
        "qemu_fix": "sudo dnf install qemu-img",
        "ssh_fix": "sudo dnf install openssh-clients",
        "passt_status": "ok",
    },
    "arch": {
        "os_id": "arch",
        "package_query": [
            "pacman",
            "-Q",
            "passt",
            "qemu-img",
            "openssh",
            "util-linux",
        ],
        "packages": ("passt", "qemu-img", "openssh", "util-linux"),
        "owner_query": ["pacman", "-Qo"],
        "passt_fix": "sudo pacman -S passt",
        "qemu_fix": "sudo pacman -S qemu-img",
        "ssh_fix": "sudo pacman -S openssh",
        "passt_status": "ok",
    },
}


def require(condition: bool, message: str) -> None:
    if not condition:
        raise MatrixError(message)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--distro", required=True, choices=tuple(DISTROS))
    parser.add_argument("--firestone", required=True, type=Path)
    return parser.parse_args()


def run(
    argv: list[str | os.PathLike[str]],
    *,
    environment: dict[str, str] | None = None,
    timeout: float = 60,
    expected_codes: set[int] | None = None,
) -> subprocess.CompletedProcess[bytes]:
    command = [os.fspath(value) for value in argv]
    try:
        completed = subprocess.run(
            command,
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=timeout,
            check=False,
        )
    except subprocess.TimeoutExpired as error:
        raise MatrixError(f"command timed out after {timeout:.1f}s: {command!r}") from error
    require(
        len(completed.stdout) <= MAX_OUTPUT_BYTES
        and len(completed.stderr) <= MAX_OUTPUT_BYTES,
        f"command output exceeded 8 MiB: {command!r}",
    )
    if expected_codes is None:
        expected_codes = {0}
    require(
        completed.returncode in expected_codes,
        f"command exited {completed.returncode}: {command!r}; "
        f"stdout={completed.stdout[-4096:]!r}; stderr={completed.stderr[-4096:]!r}",
    )
    return completed


def parse_os_release(path: Path = Path("/etc/os-release")) -> dict[str, str]:
    fields: dict[str, str] = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        value = value.strip()
        if len(value) >= 2 and value[0] == value[-1] and value[0] in {'"', "'"}:
            value = value[1:-1]
        fields[key] = value
    return fields


def parse_package_versions(distro: str, output: bytes) -> dict[str, str]:
    text = output.decode("utf-8", errors="strict")
    versions: dict[str, str] = {}
    if distro == "ubuntu":
        for line in text.splitlines():
            name, version, status = line.split("\t", 2)
            require(status == "install ok installed", f"{name} is not installed: {status}")
            versions[name] = version
    elif distro == "fedora":
        for line in text.splitlines():
            name, version = line.split("\t", 1)
            versions[name] = version
    else:
        for line in text.splitlines():
            name, version = line.split(maxsplit=1)
            versions[name] = version
    return versions


def package_facts(distro: str, config: dict[str, object]) -> dict[str, Any]:
    package_query = config["package_query"]
    require(isinstance(package_query, list), "package query is invalid")
    queried = run([str(value) for value in package_query])
    versions = parse_package_versions(distro, queried.stdout)
    packages = config["packages"]
    require(isinstance(packages, tuple), "package list is invalid")
    require(set(versions) == set(packages), f"observed packages differ: {versions!r}")

    tools: dict[str, dict[str, str]] = {}
    for tool in ("passt", "qemu-img", "ssh", "ssh-keygen", "unshare"):
        path = shutil.which(tool)
        require(path is not None, f"installed tool is missing from PATH: {tool}")
        owner_query = config["owner_query"]
        require(isinstance(owner_query, list), "owner query is invalid")
        owner = run([str(value) for value in owner_query] + [path])
        owner_text = (owner.stdout + owner.stderr).decode("utf-8", errors="strict").strip()
        require(owner_text, f"package manager did not identify the owner of {path}")
        tools[tool] = {"path": path, "owner": owner_text}
    return {"versions": versions, "tools": tools}


def doctor_environment(path: str) -> dict[str, str]:
    environment = os.environ.copy()
    environment["PATH"] = path
    for name in (
        "FIRESTONE_HOME",
        "FIRESTONE_CONFIG_DIR",
        "FIRESTONE_DATA_DIR",
        "FIRESTONE_RUNTIME_DIR",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_RUNTIME_DIR",
    ):
        environment.pop(name, None)
    return environment


def doctor_report(
    binary: Path,
    environment: dict[str, str],
    *,
    fix: bool,
) -> dict[str, dict[str, Any]]:
    arguments = [binary, "--json", "doctor"]
    if fix:
        arguments.append("--fix")
    completed = run(
        arguments,
        environment=environment,
        timeout=DOCTOR_TIMEOUT_SECONDS if fix else 120,
        expected_codes={0, 5},
    )
    require(not completed.stderr, "JSON doctor wrote stderr")
    try:
        records = [
            json.loads(line)
            for line in completed.stdout.decode("utf-8", errors="strict").splitlines()
            if line
        ]
    except json.JSONDecodeError as error:
        raise MatrixError("doctor emitted invalid NDJSON") from error
    require(len(records) == 1, f"doctor emitted {len(records)} records instead of one")
    terminal = records[0]
    require(terminal.get("type") == "Result", "doctor did not emit a Result")
    require(terminal.get("action") == "doctor", "doctor Result action changed")
    payload = terminal.get("payload")
    require(isinstance(payload, dict), "doctor payload is not an object")
    checks = payload.get("checks")
    require(isinstance(checks, list) and len(checks) == 13, "doctor did not emit 13 checks")
    result: dict[str, dict[str, Any]] = {}
    for check in checks:
        require(isinstance(check, dict), "doctor check is not an object")
        identifier = check.get("id")
        require(isinstance(identifier, str), "doctor check id is invalid")
        result[identifier] = check
    require(tuple(result) == CHECK_IDS, f"doctor check order changed: {tuple(result)!r}")
    has_failures = any(check.get("status") == "fail" for check in checks)
    require(
        completed.returncode == (5 if has_failures else 0),
        "doctor exit code does not match its report",
    )
    return result


def assert_initial_report(
    report: dict[str, dict[str, Any]],
    config: dict[str, object],
) -> None:
    require(report["host_arch"]["status"] == "ok", "x86_64 Linux was not accepted")
    kvm = report["kvm"]
    require(kvm["status"] == "fail", "inaccessible fake KVM did not fail")
    require(kvm.get("fix") == "sudo usermod -aG kvm $USER", "KVM group fix changed")
    require("device group is kvm" in kvm["reason"], "KVM reason omitted the observed group")
    require(
        report["nested_virtualization"]["status"] == "warn",
        "nested virtualization did not warn without KVM access",
    )
    runtime = report["runtime_dir"]
    require(runtime["status"] == "fail", "missing runtime fallback was not reported")
    require(runtime.get("fix") == "firestone doctor --fix", "runtime fix changed")
    for identifier in ("vendored_binaries", "virtiofsd", "ssh_key"):
        check = report[identifier]
        require(check["status"] == "fail", f"fresh {identifier} did not fail")
        require(check.get("fix") == "firestone doctor --fix", f"{identifier} fix changed")

    passt = report["passt"]
    require(passt["status"] == "fail", "missing passt did not fail")
    require(passt.get("fix") is None, "doctor exposed an unverified passt package as a fix")
    require(
        f"`{config['passt_fix']}`" in passt.get("hint", ""),
        "passt hint omitted the detected package-manager command",
    )
    require("2025_02_17.a1e48a0" in passt.get("hint", ""), "passt hint omitted minimum")

    qemu = report["qemu_img"]
    require(qemu["status"] == "fail", "missing qemu-img did not fail")
    require(qemu.get("fix") == config["qemu_fix"], "qemu-img package fix changed")
    ssh = report["ssh"]
    require(ssh["status"] == "fail", "missing OpenSSH did not fail")
    require(ssh.get("fix") == config["ssh_fix"], "OpenSSH package fix changed")
    namespaces = report["user_namespaces"]
    require(namespaces["status"] == "warn", "missing unshare did not warn")
    require("--sandbox none" in namespaces.get("hint", ""), "namespace fallback hint changed")
    space = report["data_space"]
    require(space["status"] == "warn", "small data filesystem did not warn")
    require("free space" in space.get("hint", ""), "space warning omitted its fix hint")
    require(report["stale_state"]["status"] == "ok", "empty host reported stale state")


def unshare_works(environment: dict[str, str]) -> bool:
    completed = run(
        ["unshare", "-U", "true"],
        environment=environment,
        expected_codes=set(range(0, 256)),
    )
    return completed.returncode == 0


def assert_fixed_report(
    report: dict[str, dict[str, Any]],
    config: dict[str, object],
    namespaces_work: bool,
) -> None:
    require(report["kvm"]["status"] == "fail", "doctor --fix changed KVM access")
    require(
        report["kvm"].get("fix") == "sudo usermod -aG kvm $USER",
        "doctor --fix changed the KVM group fix",
    )
    require(report["runtime_dir"]["status"] == "warn", "runtime fallback was not created")
    require(
        "using secure fallback" in report["runtime_dir"]["reason"],
        "runtime fallback reason changed",
    )
    require(report["vendored_binaries"]["status"] == "ok", "vendored dependencies were not fixed")
    require(report["virtiofsd"]["status"] == "ok", "virtiofsd was not fixed")
    require(report["qemu_img"]["status"] == "ok", "installed qemu-img was not observed")
    require(report["ssh"]["status"] == "ok", "installed OpenSSH was not observed")
    require(report["ssh_key"]["status"] == "ok", "SSH key was not generated")
    require(report["data_space"]["status"] == "warn", "small data filesystem warning disappeared")
    require(
        report["passt"]["status"] == config["passt_status"],
        f"installed passt status differs: {report['passt']!r}",
    )
    if config["passt_status"] == "fail":
        passt_text = report["passt"].get("reason", "") + report["passt"].get("hint", "")
        require("2025_02_17.a1e48a0" in passt_text, "old Ubuntu passt failure omitted minimum")
    expected_namespace_status = "ok" if namespaces_work else "warn"
    require(
        report["user_namespaces"]["status"] == expected_namespace_status,
        "doctor user-namespace status differs from the observed unshare result",
    )
    if not namespaces_work:
        require(
            "--sandbox none" in report["user_namespaces"].get("hint", ""),
            "failed unshare omitted the virtiofsd fallback",
        )


def create_command_traps(directory: Path, marker: Path) -> None:
    for command in ("sudo", "apt-get", "dnf", "pacman", "usermod", "sysctl"):
        path = directory / command
        path.write_text(
            "#!/bin/sh\nprintf '%s\\n' \"$0 $*\" >> "
            + json.dumps(os.fspath(marker))
            + "\nexit 99\n",
            encoding="utf-8",
        )
        path.chmod(0o700)


def main() -> int:
    args = parse_args()
    config = DISTROS[args.distro]
    binary = args.firestone.expanduser().resolve(strict=True)
    require(binary.is_file() and os.access(binary, os.X_OK), "Firestone binary is not executable")
    require(os.geteuid() != 0, "doctor matrix must run as an unprivileged user")
    require(platform.system() == "Linux", "doctor matrix requires Linux")
    require(platform.machine() == "x86_64", "doctor matrix requires x86_64")

    os_release = parse_os_release()
    require(os_release.get("ID") == config["os_id"], "container distro does not match matrix row")
    home = Path.home().resolve(strict=True)
    home_metadata = home.stat()
    require(home_metadata.st_uid == os.getuid(), "HOME has the wrong owner")
    require(stat.S_IMODE(home_metadata.st_mode) == 0o700, "HOME must be mode 0700")
    for path in (
        home / ".config" / "firestone",
        home / ".local" / "share" / "firestone",
        Path(f"/tmp/firestone-{os.getuid()}"),
    ):
        require(not path.exists(), f"fresh host already has Firestone state at {path}")
    require("XDG_RUNTIME_DIR" not in os.environ, "matrix requires the runtime fallback path")

    kvm = Path("/dev/kvm")
    kvm_metadata = kvm.lstat()
    require(stat.S_ISREG(kvm_metadata.st_mode), "container KVM fixture must be a regular file")
    require(not os.access(kvm, os.R_OK | os.W_OK), "container KVM fixture is unexpectedly usable")
    require(grp.getgrgid(kvm_metadata.st_gid).gr_name == "kvm", "KVM fixture group is not kvm")
    kvm_fingerprint = (
        kvm_metadata.st_dev,
        kvm_metadata.st_ino,
        kvm_metadata.st_uid,
        kvm_metadata.st_gid,
        stat.S_IMODE(kvm_metadata.st_mode),
    )

    free_bytes = shutil.disk_usage(home).free
    require(free_bytes < MINIMUM_SPACE_BYTES, "matrix HOME filesystem must exercise the space warning")
    before_packages = package_facts(args.distro, config)

    with tempfile.TemporaryDirectory(prefix="firestone-empty-path-") as empty_path_raw:
        empty_path = Path(empty_path_raw)
        empty_path.chmod(0o700)
        initial = doctor_report(
            binary,
            doctor_environment(os.fspath(empty_path)),
            fix=False,
        )
    assert_initial_report(initial, config)

    with tempfile.TemporaryDirectory(prefix="firestone-command-traps-") as trap_raw:
        trap = Path(trap_raw)
        trap.chmod(0o700)
        marker = trap / "invoked"
        create_command_traps(trap, marker)
        full_path = os.pathsep.join((os.fspath(trap), os.defpath, "/usr/local/bin"))
        environment = doctor_environment(full_path)
        namespaces_work = unshare_works(environment)
        fixed = doctor_report(binary, environment, fix=True)
        require(not marker.exists(), "doctor --fix invoked a privileged or package-manager command")
    assert_fixed_report(fixed, config, namespaces_work)

    after_packages = package_facts(args.distro, config)
    require(before_packages == after_packages, "doctor --fix changed distro packages or tool ownership")
    after_kvm = kvm.lstat()
    require(
        kvm_fingerprint
        == (
            after_kvm.st_dev,
            after_kvm.st_ino,
            after_kvm.st_uid,
            after_kvm.st_gid,
            stat.S_IMODE(after_kvm.st_mode),
        ),
        "doctor --fix changed the KVM fixture",
    )

    data = home / ".local" / "share" / "firestone"
    runtime = Path(f"/tmp/firestone-{os.getuid()}")
    private_key = data / "ssh" / "id_ed25519"
    public_key = data / "ssh" / "id_ed25519.pub"
    require(data.is_dir(), "doctor --fix did not create the data directory")
    require(runtime.is_dir(), "doctor --fix did not create the runtime fallback")
    require(stat.S_IMODE(data.stat().st_mode) == 0o700, "data directory mode changed")
    require(stat.S_IMODE(runtime.stat().st_mode) == 0o700, "runtime fallback mode changed")
    require(stat.S_IMODE(private_key.stat().st_mode) == 0o600, "private key mode changed")
    require(stat.S_IMODE(public_key.stat().st_mode) == 0o644, "public key mode changed")

    summary = {
        "distro": args.distro,
        "os_release": {
            "id": os_release.get("ID"),
            "version_id": os_release.get("VERSION_ID"),
            "pretty_name": os_release.get("PRETTY_NAME"),
        },
        "architecture": platform.machine(),
        "container_has_kvm": False,
        "kvm_fixture": "root-owned regular file with group kvm; never a KVM device",
        "free_bytes": free_bytes,
        "package_versions": before_packages["versions"],
        "package_owners": before_packages["tools"],
        "initial_statuses": {key: value["status"] for key, value in initial.items()},
        "fixed_statuses": {key: value["status"] for key, value in fixed.items()},
        "unshare_succeeded": namespaces_work,
        "doctor_fix_euid": os.geteuid(),
        "privileged_commands_invoked": False,
        "package_state_unchanged": True,
        "kvm_fixture_unchanged": True,
    }
    print(json.dumps(summary, sort_keys=True))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (MatrixError, OSError, ValueError, KeyError, TypeError) as error:
        print(f"fresh-host doctor matrix failed: {error}", file=sys.stderr)
        raise SystemExit(1) from error
