from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path
from typing import Any


REPO_ROOT = Path(__file__).resolve().parents[2]
HARNESS_PATH = REPO_ROOT / "scripts" / "m5-catalog-kvm-e2e.py"
MODULE_SPEC = importlib.util.spec_from_file_location("m5_catalog_kvm_e2e", HARNESS_PATH)
if MODULE_SPEC is None or MODULE_SPEC.loader is None:
    raise RuntimeError(f"cannot load {HARNESS_PATH}")
HARNESS = importlib.util.module_from_spec(MODULE_SPEC)
sys.modules[MODULE_SPEC.name] = HARNESS
MODULE_SPEC.loader.exec_module(HARNESS)


class CatalogDoctorEvidenceTests(unittest.TestCase):
    def test_doctor_evidence_uses_terminal_payload_not_record_list(self) -> None:
        checks = [{"id": f"check-{index}", "status": "ok"} for index in range(13)]

        class FakeHarness:
            def __init__(self) -> None:
                self.calls: list[tuple[tuple[str, ...], dict[str, Any]]] = []

            def json_command(
                self, *arguments: str, **keywords: Any
            ) -> tuple[list[dict[str, Any]], dict[str, Any]]:
                self.calls.append((arguments, keywords))
                records = [
                    {"type": "Progress", "id": "doctor"},
                    {"type": "Result", "action": "doctor"},
                ]
                return records, {"checks": checks}

        harness = FakeHarness()
        evidence = HARNESS.doctor_evidence(harness)

        self.assertEqual(
            evidence,
            {"fix_check_count": 13, "check_count": 13, "failures": []},
        )
        self.assertEqual(harness.calls[0][0], ("doctor", "--fix"))
        self.assertEqual(harness.calls[1][0], ("doctor",))


if __name__ == "__main__":
    unittest.main()
