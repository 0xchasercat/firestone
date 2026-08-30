from __future__ import annotations

import importlib.util
import json
import os
import py_compile
import stat
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock


REPO_ROOT = Path(__file__).resolve().parents[2]
GATE_PATH = REPO_ROOT / "scripts" / "m5-linux-mvp-e2e.py"
MODULE_SPEC = importlib.util.spec_from_file_location("m5_linux_mvp_e2e", GATE_PATH)
if MODULE_SPEC is None or MODULE_SPEC.loader is None:
    raise RuntimeError(f"cannot load {GATE_PATH}")
GATE = importlib.util.module_from_spec(MODULE_SPEC)
sys.modules[MODULE_SPEC.name] = GATE
MODULE_SPEC.loader.exec_module(GATE)

KVM_HARNESSES = (
    "m1-kvm-e2e.py",
    "m2-kvm-e2e.py",
    "m3-kvm-e2e.py",
    "m4-kvm-e2e.py",
    "m5-catalog-kvm-e2e.py",
    "m5-linux-mvp-e2e.py",
)


class FinalLinuxMvpGateTests(unittest.TestCase):
    def test_all_python_harnesses_compile_and_kvm_gates_skip(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            python_sources = sorted((REPO_ROOT / "scripts").glob("*.py"))
            python_sources.extend(sorted((REPO_ROOT / "scripts/tests").glob("*.py")))
            for index, source in enumerate(python_sources):
                py_compile.compile(
                    os.fspath(source),
                    cfile=os.fspath(output / f"{index}.pyc"),
                    doraise=True,
                )

            environment = os.environ.copy()
            environment.pop("FIRESTONE_E2E", None)
            for harness in KVM_HARNESSES:
                completed = subprocess.run(
                    [sys.executable, REPO_ROOT / "scripts" / harness],
                    cwd=REPO_ROOT,
                    env=environment,
                    stdin=subprocess.DEVNULL,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    timeout=10,
                    check=False,
                )
                self.assertEqual(completed.returncode, 0, harness)
                self.assertIn(b"skipped", completed.stdout, harness)
                self.assertEqual(completed.stderr, b"", harness)

    def test_regular_file_is_never_accepted_as_kvm(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            fake_kvm = Path(temporary) / "kvm"
            fake_kvm.write_bytes(b"")
            os.chmod(fake_kvm, 0o666)
            with self.assertRaisesRegex(GATE.GateError, "not a real character device"):
                GATE.validate_kvm_device(fake_kvm)

    def test_release_digest_is_independently_required(self) -> None:
        with mock.patch.dict(os.environ, {}, clear=True):
            with self.assertRaisesRegex(GATE.GateError, "expected-release-sha256"):
                GATE.parse_args(
                    [
                        "--expected-commit",
                        "1" * 40,
                        "--release-artifact",
                        "/tmp/firestone-v0.1.0-x86_64-unknown-linux-musl",
                        "--evidence-dir",
                        "/tmp/firestone-evidence",
                    ]
                )

    def test_doctor_attestation_is_bound_to_digest_head_urls_and_jobs(self) -> None:
        commit = "1" * 40
        run_id = 42
        run_url = f"https://github.com/0xchasercat/firestone/actions/runs/{run_id}"
        document = {
            "schema": 1,
            "repository": "0xchasercat/firestone",
            "workflow": GATE.DOCTOR_WORKFLOW_PATH,
            "run_id": run_id,
            "run_url": run_url,
            "head_sha": commit,
            "head_branch": "main",
            "status": "completed",
            "conclusion": "success",
            "build_job": {
                "job_id": 1,
                "name": GATE.DOCTOR_BUILD_JOB,
                "status": "completed",
                "conclusion": "success",
                "url": f"{run_url}/job/1",
            },
            "rows": {
                distro: {
                    "job_id": index,
                    "name": name,
                    "status": "completed",
                    "conclusion": "success",
                    "url": f"{run_url}/job/{index}",
                }
                for index, (distro, name) in enumerate(GATE.DOCTOR_JOB_NAMES.items(), start=2)
            },
        }
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary).resolve() / "doctor-attestation.json"
            payload = (json.dumps(document, sort_keys=True) + "\n").encode()
            path.write_bytes(payload)
            os.chmod(path, 0o600)
            digest = GATE.hashlib.sha256(payload).hexdigest()
            verified = GATE.verify_doctor_workflow_attestation(path, digest, commit)
            self.assertTrue(verified["verified_from_prevalidated_manifest"])
            self.assertEqual(set(verified["rows"]), {"ubuntu", "fedora", "arch"})

            wrong_head = dict(document, head_sha="2" * 40)
            wrong_payload = (json.dumps(wrong_head, sort_keys=True) + "\n").encode()
            path.write_bytes(wrong_payload)
            wrong_digest = GATE.hashlib.sha256(wrong_payload).hexdigest()
            with self.assertRaisesRegex(GATE.GateError, "accepted main"):
                GATE.verify_doctor_workflow_attestation(path, wrong_digest, commit)

    def test_pending_signal_is_observed_while_finalization_is_blocked(self) -> None:
        with mock.patch.object(GATE, "INTERRUPTED_SIGNAL", None):
            with mock.patch.object(GATE.signal, "sigpending", return_value={GATE.signal.SIGTERM}):
                self.assertEqual(GATE.observed_interrupt_signal(), GATE.signal.SIGTERM)
                self.assertIn("signal", GATE.interrupted_message())

    def test_host_setup_lock_rejects_a_duplicate_gate(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            lock_path = Path(temporary) / "gate.lock"
            first = GATE.GateLock(lock_path)
            second = GATE.GateLock(lock_path)
            first.acquire()
            try:
                with self.assertRaisesRegex(GATE.GateError, "another final Linux MVP gate"):
                    second.acquire()
            finally:
                second.close()
                first.close()
            self.assertEqual(stat.S_IMODE(lock_path.stat().st_mode), 0o600)

    def test_repository_snapshot_rechecks_every_pinned_input(self) -> None:
        head = subprocess.run(
            ["git", "rev-parse", "HEAD"],
            cwd=REPO_ROOT,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=10,
            check=True,
        ).stdout.strip()
        with tempfile.TemporaryDirectory() as temporary:
            work = Path(temporary) / "work"
            work.mkdir(mode=0o700)
            snapshot = GATE.prepare_repository_snapshot(work, head)
            self.assertEqual(
                GATE.validate_snapshot_hashes(snapshot),
                GATE.EXPECTED_FILE_HASHES,
            )
            self.assertEqual(stat.S_IMODE(snapshot.stat().st_mode), 0o700)

    def test_checkpoint_contains_only_private_manifests_and_checksums(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            evidence = Path(temporary) / "evidence"
            evidence.mkdir(mode=0o700)
            manifest = {"schema": 1, "gate": "linux-x86_64-mvp", "result": "running"}
            GATE.write_checkpoint(evidence, manifest)
            self.assertEqual(
                {entry.name for entry in evidence.iterdir()},
                {GATE.ACCEPTANCE_MANIFEST_NAME, GATE.ACCEPTANCE_CHECKSUM_NAME},
            )
            for entry in evidence.iterdir():
                self.assertEqual(stat.S_IMODE(entry.stat().st_mode), 0o600)
            loaded, digest = GATE.verify_checkpoint(evidence)
            self.assertEqual(loaded, manifest)
            self.assertRegex(digest, r"^[0-9a-f]{64}$")

    def test_resume_accepts_only_a_verified_contiguous_prefix(self) -> None:
        commit = "1" * 40
        release_sha = "2" * 64
        repository = {
            "canonical": GATE.CANONICAL_REPOSITORY,
            "commit": commit,
            "gate_source": {"path": GATE.__file__, "sha256": "3" * 64},
        }
        release = {"name": GATE.RELEASE_ARTIFACT_NAME, "sha256": release_sha}
        pins = {"files": {"deps.toml": GATE.EXPECTED_FILE_HASHES["deps.toml"]}}
        host = {"system": "Linux", "architecture": "x86_64"}

        with tempfile.TemporaryDirectory() as temporary:
            evidence = Path(temporary) / "evidence"
            evidence.mkdir(mode=0o700)
            m1_path = evidence / GATE.HARNESSES[0].evidence_name
            m1_document = {
                "schema": 1,
                "result": "passed",
                "commit": commit,
                "host": {
                    "system": "Linux",
                    "architecture": "x86_64",
                    "firestone_sha256": release_sha,
                },
                "artifacts": {
                    name: {"version": identity["version"], "sha256": identity["sha256"]}
                    for name, identity in GATE.EXPECTED_RELEASE_DEPENDENCIES.items()
                },
                "scenarios": {
                    "e2e_1_doctor": {
                        "checks": [{"status": "ok"} for _ in range(13)],
                    },
                    "e2e_5_graceful_stop": {
                        "start_result": {"status": "running"},
                        "start_steps": ["boot", "ssh"],
                        "login_console_line": "ubuntu login:",
                        "cloud_init_status": ["status: done"],
                        "api": {
                            "vmm_ping": {"status": 200},
                            "vm_info": {"status": 200},
                        },
                        "vmconfig": {"net_present": False},
                        "image": {"id": "image"},
                        "stop_result": {"status": "stopped"},
                        "last_exit": {"reason": "guest shutdown"},
                        "shutdown_console_line": "Power down",
                    },
                    "verify_4_5_conversion_overlay_fio": {
                        "image": {"source_format": "raw", "stored_format": "qcow2"},
                        "vmconfig": {"disks": []},
                        "fio_version": "fio-3",
                        "workload": {
                            "size": "64m",
                            "rw": "randrw",
                            "block_size": "4k",
                            "runtime_seconds": 10,
                        },
                        "overlay": {
                            "read_bw_bytes": 1,
                            "read_iops": 1,
                            "write_bw_bytes": 1,
                            "write_iops": 1,
                        },
                        "raw_auxiliary_disk": {
                            "read_bw_bytes": 1,
                            "read_iops": 1,
                            "write_bw_bytes": 1,
                            "write_iops": 1,
                        },
                        "threshold_applied": False,
                    },
                    "e2e_6_vmm_sigkill_restart": {
                        "vmm_pid": 10,
                        "failed_after_ms": 1,
                        "failed_last_exit": {"reason": "vmm exited"},
                        "restart_result": {"status": "running"},
                        "restart_login_console_line": "ubuntu login:",
                    },
                    "e2e_7_shim_sigkill_stop": {
                        "shim_pid": 11,
                        "unsupervised_after_ms": 1,
                        "listed_status": "running (unsupervised)",
                        "stop_result": {"status": "stopped"},
                        "last_exit": {"reason": "guest shutdown"},
                    },
                },
            }
            incomplete = dict(m1_document)
            incomplete.pop("scenarios")
            m1_path.write_text(json.dumps(incomplete) + "\n", encoding="utf-8")
            os.chmod(m1_path, 0o600)
            with self.assertRaisesRegex(GATE.GateError, "scenario evidence is missing"):
                GATE.validate_passed_evidence(
                    GATE.HARNESSES[0],
                    m1_path,
                    expected_commit=commit,
                    release_sha256=release_sha,
                )
            m1_path.write_text(json.dumps(m1_document) + "\n", encoding="utf-8")
            os.chmod(m1_path, 0o600)
            m1_fact = GATE.evidence_fact(m1_path, GATE.HARNESSES[0].max_evidence_bytes)

            previous = GATE.build_manifest(
                repository=repository,
                host=host,
                pins=pins,
                release=release,
            )
            GATE.mark_harness_passed(
                previous,
                GATE.HARNESSES[0],
                m1_fact,
                execution="executed",
            )
            GATE.harness_record(previous, "m1")["cleanup"] = {
                "home_removed": True,
                "harness_removed_home": True,
                "host_setup_restored": True,
                "errors": [],
            }
            m2_path = evidence / GATE.HARNESSES[1].evidence_name
            m2_path.write_text(json.dumps({"schema": 1, "result": "failed"}) + "\n", encoding="utf-8")
            os.chmod(m2_path, 0o600)
            previous["result"] = "failed"
            GATE.write_checkpoint(evidence, previous)

            current = GATE.build_manifest(
                repository=repository,
                host=host,
                pins=pins,
                release=release,
            )
            completed = GATE.validate_resume(
                evidence,
                current,
                expected_commit=commit,
                release_sha256=release_sha,
            )
            self.assertEqual(completed, {"m1"})
            self.assertEqual(GATE.harness_record(current, "m1")["execution"], "resumed_verified")
            self.assertEqual(GATE.harness_record(current, "m2")["status"], "pending")
            self.assertFalse(m2_path.exists())
            self.assertEqual(current["resume"]["cleared_incomplete_evidence"], ["m2"])
            GATE.write_checkpoint(evidence, current)

            with m1_path.open("ab") as stream:
                stream.write(b"changed\n")
            with self.assertRaisesRegex(GATE.GateError, "resume checksum failed"):
                GATE.validate_resume(
                    evidence,
                    GATE.build_manifest(
                        repository=repository,
                        host=host,
                        pins=pins,
                        release=release,
                    ),
                    expected_commit=commit,
                    release_sha256=release_sha,
                )

    def test_invalid_or_unexpected_child_evidence_is_removed(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            evidence = Path(temporary) / "evidence"
            evidence.mkdir(mode=0o700)
            spec = GATE.HarnessSpec(
                "fake",
                "/tmp/fake.py",
                "fake.json",
                1,
                16,
                (),
                (),
                (),
            )
            bad = evidence / "fake.json"
            bad.write_bytes(b"x" * 100)
            os.chmod(bad, 0o644)
            rogue = evidence / "rogue"
            rogue.mkdir(mode=0o700)
            (rogue / "secret").write_text("must not survive", encoding="utf-8")
            errors = GATE.sanitize_harness_evidence(evidence, spec, set())
            self.assertTrue(errors)
            self.assertFalse(bad.exists())
            self.assertFalse(rogue.exists())
            self.assertEqual(list(evidence.iterdir()), [])

    def test_failed_harness_is_cleaned_without_kvm(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            work = root / "work"
            evidence = root / "evidence"
            work.mkdir(mode=0o700)
            evidence.mkdir(mode=0o700)
            fake_binary = root / "firestone"
            fake_binary.write_text("#!/bin/sh\nexit 3\n", encoding="utf-8")
            os.chmod(fake_binary, 0o755)
            fake_harness = root / "fake-harness.py"
            fake_harness.write_text(
                "import json, os, pathlib, sys\n"
                "home = pathlib.Path(os.environ['FIRESTONE_HOME'])\n"
                "(home / 'left-behind').write_text('owned')\n"
                "evidence = pathlib.Path(os.environ['FIRESTONE_E2E_EVIDENCE'])\n"
                "evidence.write_text(json.dumps({'schema': 1, 'result': 'failed'}) + '\\n')\n"
                "evidence.chmod(0o600)\n"
                "sys.exit(1)\n",
                encoding="utf-8",
            )
            os.chmod(fake_harness, 0o600)
            spec = GATE.HarnessSpec(
                "fake",
                os.fspath(fake_harness),
                "fake.json",
                5,
                64 * 1024,
                (),
                (),
                (),
            )
            executor = GATE.HarnessExecutor(
                work_root=work,
                evidence_dir=evidence,
                binary=fake_binary,
                expected_commit="1" * 40,
                release_sha256="2" * 64,
                deadline=time.monotonic() + 10,
            )
            record = executor.run(spec)
            self.assertEqual(record["status"], "failed")
            self.assertEqual(record["returncode"], 1)
            self.assertTrue(record["cleanup"]["home_removed"])
            self.assertFalse((work / "home-fake").exists())
            self.assertEqual(stat.S_IMODE((evidence / "fake.json").stat().st_mode), 0o600)
            self.assertEqual(executor.cleanup(), [])

    @unittest.skipUnless(sys.platform == "linux", "Linux /proc process-group regression")
    def test_process_group_cleanup_does_not_signal_reused_group_after_leader_exit(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            child_pid_path = root / "child.pid"
            leader_source = root / "leader.py"
            leader_source.write_text(
                "import os, pathlib, subprocess, sys\n"
                "environment = os.environ.copy()\n"
                "environment['FIRESTONE_HOME'] = sys.argv[1]\n"
                "child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'], "
                "cwd=sys.argv[1], env=environment)\n"
                f"pathlib.Path({str(child_pid_path)!r}).write_text(str(child.pid))\n",
                encoding="utf-8",
            )
            leader = subprocess.Popen(
                [sys.executable, leader_source, root],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                start_new_session=True,
            )
            leader_start = GATE.process_start_ticks(leader.pid)
            self.assertIsNotNone(leader_start)
            leader_identity = GATE.ProcessIdentity(leader.pid, leader_start)
            self.assertEqual(leader.wait(timeout=5), 0)
            child_pid = int(child_pid_path.read_text(encoding="utf-8"))
            child_start = GATE.process_start_ticks(child_pid)
            self.assertIsNotNone(child_start)
            try:
                with mock.patch.object(
                    GATE.os,
                    "killpg",
                    side_effect=AssertionError("simulated reused process group was signalled"),
                ) as killpg:
                    self.assertEqual(
                        GATE.terminate_process_group(leader, leader_identity, root), []
                    )
                    killpg.assert_not_called()
                self.assertNotEqual(GATE.process_start_ticks(child_pid), child_start)
            finally:
                try:
                    os.kill(child_pid, 9)
                except ProcessLookupError:
                    pass

    def test_dependency_order_stops_after_the_first_failure(self) -> None:
        specs = GATE.HARNESSES[:3]
        manifest = {
            "harnesses": [
                {
                    "id": spec.identifier,
                    "status": "pending",
                    "source": spec.source,
                    "source_sha256": GATE.EXPECTED_FILE_HASHES[spec.source],
                    "evidence_sha256": None,
                }
                for spec in specs
            ],
            "e2e": [
                {"id": identifier, "status": "pending", "evidence_sha256": None}
                for spec in specs
                for identifier in spec.e2e_ids
            ],
            "verify": [
                {
                    "id": identifier,
                    "status": "open",
                    "current_gate_status": "pending",
                    "evidence_sha256": None,
                }
                for spec in specs
                for identifier in spec.verify_ids
            ],
        }
        called: list[str] = []
        checkpoints: list[int] = []

        def run_one(spec: object) -> dict[str, object]:
            called.append(spec.identifier)
            if spec.identifier == "m2":
                return {"id": "m2", "status": "failed", "error": "expected failure"}
            return {"id": spec.identifier, "status": "passed", "evidence_sha256": "a" * 64}

        with mock.patch.object(GATE, "HARNESSES", specs):
            failure = GATE.execute_in_dependency_order(
                manifest,
                set(),
                run_one,
                lambda: checkpoints.append(1),
            )
        self.assertEqual(failure, "expected failure")
        self.assertEqual(called, ["m1", "m2"])
        self.assertEqual(len(checkpoints), 2)


if __name__ == "__main__":
    unittest.main()
