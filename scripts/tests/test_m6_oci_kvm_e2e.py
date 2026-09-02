from __future__ import annotations

import copy
import importlib.util
import sys
import unittest
from pathlib import Path
from typing import Any


REPO_ROOT = Path(__file__).resolve().parents[2]
HARNESS_PATH = REPO_ROOT / "scripts" / "m6-oci-kvm-e2e.py"
MODULE_SPEC = importlib.util.spec_from_file_location("m6_oci_kvm_e2e", HARNESS_PATH)
if MODULE_SPEC is None or MODULE_SPEC.loader is None:
    raise RuntimeError(f"cannot load {HARNESS_PATH}")
HARNESS = importlib.util.module_from_spec(MODULE_SPEC)
sys.modules[MODULE_SPEC.name] = HARNESS
MODULE_SPEC.loader.exec_module(HARNESS)

REFERENCE = "docker.io/library/alpine:3.20"
DIGEST = "sha256:" + "c" * 64


def good_sidecar() -> dict[str, Any]:
    return {
        "version": 1,
        "id": "image-" + "a" * 64,
        "kind": "oci",
        "source_ref": REFERENCE,
        "source_url": None,
        "source_sha256": "c" * 64,
        "stored_sha256": "d" * 64,
        "firmware": None,
        "source_format": "raw",
        "stored_format": "qcow2",
        "verification_algorithm": "sha256",
        "verification_digest": "c" * 64,
        "oci": {
            "registry_ref": REFERENCE,
            "manifest_digest": DIGEST,
            "config_digest": "sha256:" + "e" * 64,
            "entrypoint": [],
            "cmd": ["/bin/sh"],
            "env": ["PATH=/usr/bin"],
            "workdir": "/",
            "user": None,
            "boot": "firestone-init",
        },
    }


class SidecarContractTests(unittest.TestCase):
    def test_a_conforming_oci_sidecar_reports_no_problem(self) -> None:
        self.assertEqual(HARNESS.oci_sidecar_problems(good_sidecar(), REFERENCE), [])

    def test_every_fixed_field_is_checked(self) -> None:
        cases = {
            "kind": ("kind", "disk"),
            "source_url": ("source_url", "https://example.invalid/x.qcow2"),
            "firmware": ("firmware", "edk2"),
            "source_format": ("source_format", "qcow2"),
            "verification_algorithm": ("verification_algorithm", "sha512"),
        }
        for label, (key, value) in cases.items():
            sidecar = good_sidecar()
            sidecar[key] = value
            problems = HARNESS.oci_sidecar_problems(sidecar, REFERENCE)
            self.assertTrue(problems, f"{label} went unnoticed")
            self.assertTrue(
                any(key in problem for problem in problems),
                f"{label} was not named: {problems}",
            )

    def test_a_missing_oci_object_is_one_problem(self) -> None:
        sidecar = good_sidecar()
        del sidecar["oci"]
        self.assertEqual(
            HARNESS.oci_sidecar_problems(sidecar, REFERENCE),
            ["the sidecar carries no oci object"],
        )

    def test_an_incomplete_oci_object_names_the_expected_key_set(self) -> None:
        sidecar = good_sidecar()
        del sidecar["oci"]["workdir"]
        problems = HARNESS.oci_sidecar_problems(sidecar, REFERENCE)
        self.assertEqual(len(problems), 1)
        self.assertIn("workdir", problems[0])

    def test_source_sha256_must_be_the_manifest_digest_hex(self) -> None:
        sidecar = good_sidecar()
        sidecar["source_sha256"] = "f" * 64
        sidecar["verification_digest"] = "f" * 64
        problems = HARNESS.oci_sidecar_problems(sidecar, REFERENCE)
        self.assertEqual(problems, ["source_sha256 is not the manifest digest's hex"])

    def test_a_wrong_reference_is_reported_on_both_sides(self) -> None:
        sidecar = good_sidecar()
        problems = HARNESS.oci_sidecar_problems(sidecar, "docker.io/library/nginx:latest")
        self.assertEqual(len(problems), 2)

    def test_a_non_string_runtime_list_is_reported(self) -> None:
        sidecar = copy.deepcopy(good_sidecar())
        sidecar["oci"]["cmd"] = ["/bin/sh", 3]
        self.assertIn(
            "oci.cmd is not an array of strings",
            HARNESS.oci_sidecar_problems(sidecar, REFERENCE),
        )


class ProgressTests(unittest.TestCase):
    def test_image_progress_selects_only_the_image_step(self) -> None:
        records = [
            {"type": "StepStart", "id": "image"},
            {"type": "Progress", "id": "image", "done": 1, "total": 10},
            {"type": "Progress", "id": "disk", "done": 5, "total": None},
            {"type": "Progress", "id": "image", "done": 10, "total": 10},
        ]
        self.assertEqual(HARNESS.image_progress(records), [(1, 10), (10, 10)])

    def test_image_progress_refuses_a_malformed_event(self) -> None:
        with self.assertRaises(HARNESS.AcceptanceError):
            HARNESS.image_progress([{"type": "Progress", "id": "image", "done": "1"}])

    def test_require_monotonic_progress_accepts_a_rising_series(self) -> None:
        HARNESS.require_monotonic_progress([(0, 10), (5, 10), (10, 10)], "pull")

    def test_require_monotonic_progress_rejects_regress_overshoot_and_silence(self) -> None:
        for pairs in ([(5, 10), (4, 10)], [(11, 10)], []):
            with self.assertRaises(HARNESS.AcceptanceError):
                HARNESS.require_monotonic_progress(pairs, "pull")

    def test_step_reasons_collects_every_skip_for_one_step(self) -> None:
        records = [
            {"type": "StepSkip", "id": "image", "reason": "cached"},
            {"type": "StepSkip", "id": "disk", "reason": "exists"},
        ]
        self.assertEqual(HARNESS.step_reasons(records, "image"), ["cached"])


class ConsoleTests(unittest.TestCase):
    CONSOLE = (
        "[    0.36] Run /sbin/firestone-init as init process\r\n"
        "firestone-init: root filesystem grown to 1572864 blocks of 4096 bytes\r\n"
        "firestone-init: eth0 configured with 149.50.108.10\r\n"
        "firestone-init: started `/bin/sh` as pid 607\r\n"
    )

    def test_grown_blocks_reads_the_reported_block_count(self) -> None:
        self.assertEqual(HARNESS.grown_blocks(self.CONSOLE), 1_572_864)

    def test_grown_blocks_is_none_when_the_guest_did_not_grow(self) -> None:
        self.assertIsNone(HARNESS.grown_blocks("firestone-init: started `/bin/sh` as pid 1\n"))
        self.assertIsNone(
            HARNESS.grown_blocks("firestone-init: root filesystem grown to many blocks\n")
        )

    def test_init_lines_keeps_order_and_strips_the_console_prefix(self) -> None:
        self.assertEqual(
            HARNESS.init_lines(self.CONSOLE),
            [
                "firestone-init: root filesystem grown to 1572864 blocks of 4096 bytes",
                "firestone-init: eth0 configured with 149.50.108.10",
                "firestone-init: started `/bin/sh` as pid 607",
            ],
        )


class HttpTests(unittest.TestCase):
    def test_parse_http_response_returns_the_status_headers_and_sized_body(self) -> None:
        status, headers, body = HARNESS.parse_http_response(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 4\r\n\r\nhi!!extra"
        )
        self.assertEqual(status, 200)
        self.assertEqual(headers["content-type"], "text/html")
        self.assertEqual(body, b"hi!!")

    def test_parse_http_response_refuses_a_truncated_body(self) -> None:
        with self.assertRaises(HARNESS.AcceptanceError):
            HARNESS.parse_http_response(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nshort")


if __name__ == "__main__":
    unittest.main()
