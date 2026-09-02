from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
HARNESS_PATH = REPO_ROOT / "scripts" / "m6-kvm-e2e.py"
MODULE_SPEC = importlib.util.spec_from_file_location("m6_kvm_e2e", HARNESS_PATH)
if MODULE_SPEC is None or MODULE_SPEC.loader is None:
    raise RuntimeError(f"cannot load {HARNESS_PATH}")
HARNESS = importlib.util.module_from_spec(MODULE_SPEC)
sys.modules[MODULE_SPEC.name] = HARNESS
MODULE_SPEC.loader.exec_module(HARNESS)


class SaturatingCounterTests(unittest.TestCase):
    def test_saturating_numbers_reports_every_sentinel_and_ignores_booleans(self) -> None:
        sample = {
            "cpu": {"cpu_time_ns": 5, "vcpus": 2},
            "block": [
                {"device": "_disk0", "read_bytes": None, "write_ops": 2**63},
                {"device": "_disk1", "read_bytes": 2**64 - 1},
            ],
            "flag": True,
        }

        self.assertEqual(
            HARNESS.saturating_numbers(sample),
            ["$.block[0].write_ops", "$.block[1].read_bytes"],
        )

    def test_saturating_numbers_accepts_a_clean_sample(self) -> None:
        self.assertEqual(HARNESS.saturating_numbers({"a": [1, 2, {"b": 0}]}), [])


class WebSocketFramingTests(unittest.TestCase):
    def test_websocket_accept_matches_the_rfc_6455_example(self) -> None:
        self.assertEqual(
            HARNESS.websocket_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=",
        )

    def test_encode_websocket_frame_masks_the_payload_at_every_length_class(self) -> None:
        mask = b"\x01\x02\x03\x04"
        short = HARNESS.encode_websocket_frame(0x2, b"ping", mask)
        self.assertEqual(short[:2], bytes([0x82, 0x84]))
        self.assertEqual(short[2:6], mask)
        self.assertEqual(
            bytes(byte ^ mask[index % 4] for index, byte in enumerate(short[6:])),
            b"ping",
        )
        medium = HARNESS.encode_websocket_frame(0x1, b"x" * 200, mask)
        self.assertEqual(medium[:2], bytes([0x81, 0xFE]))
        self.assertEqual(medium[2:4], (200).to_bytes(2, "big"))
        large = HARNESS.encode_websocket_frame(0x2, b"y" * 70_000, mask)
        self.assertEqual(large[:2], bytes([0x82, 0xFF]))
        self.assertEqual(large[2:10], (70_000).to_bytes(8, "big"))

    def test_decode_websocket_frame_waits_for_whole_frames(self) -> None:
        frame = bytes([0x82, 0x05]) + b"hello"
        self.assertIsNone(HARNESS.decode_websocket_frame(frame[:1]))
        self.assertIsNone(HARNESS.decode_websocket_frame(frame[:4]))
        self.assertEqual(
            HARNESS.decode_websocket_frame(frame + b"trailing"),
            (True, 0x2, b"hello", 7),
        )

    def test_decode_websocket_frame_reads_the_extended_length_forms(self) -> None:
        medium = bytes([0x81, 126]) + (300).to_bytes(2, "big") + b"z" * 300
        self.assertEqual(HARNESS.decode_websocket_frame(medium), (True, 0x1, b"z" * 300, 304))
        self.assertIsNone(HARNESS.decode_websocket_frame(medium[:3]))

    def test_decode_websocket_frame_refuses_a_masked_server_frame(self) -> None:
        masked = HARNESS.encode_websocket_frame(0x2, b"no", b"\x00\x00\x00\x00")
        with self.assertRaises(HARNESS.AcceptanceError):
            HARNESS.decode_websocket_frame(masked)


class HttpFramingTests(unittest.TestCase):
    def test_http_response_complete_uses_framing_not_end_of_stream(self) -> None:
        head = b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n"
        self.assertFalse(HARNESS.http_response_complete(head[:10]))
        self.assertFalse(HARNESS.http_response_complete(head + b"ab"))
        self.assertTrue(HARNESS.http_response_complete(head + b"abcd"))

    def test_http_response_complete_recognizes_the_chunked_terminator(self) -> None:
        head = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n"
        self.assertFalse(HARNESS.http_response_complete(head + b"2\r\nhi\r\n"))
        self.assertTrue(HARNESS.http_response_complete(head + b"2\r\nhi\r\n0\r\n\r\n"))

    def test_http_response_complete_is_false_without_a_declared_length(self) -> None:
        self.assertFalse(HARNESS.http_response_complete(b"HTTP/1.1 101 Switching\r\n\r\n"))

    def test_parse_http_response_decodes_both_body_framings(self) -> None:
        sized = HARNESS.parse_http_response(
            b"HTTP/1.1 409 Conflict\r\nContent-Length: 5\r\n\r\nbusy!"
        )
        self.assertEqual((sized.status, sized.body), (409, b"busy!"))
        chunked = HARNESS.parse_http_response(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n"
        )
        self.assertEqual(chunked.body, b"abc")

    def test_http_status_code_reads_the_status_line(self) -> None:
        self.assertEqual(HARNESS.http_status_code(b"HTTP/1.1 101 Switching Protocols"), 101)
        with self.assertRaises(HARNESS.AcceptanceError):
            HARNESS.http_status_code(b"nonsense")


class ForwardComparisonTests(unittest.TestCase):
    def test_canonical_forwards_accepts_both_spellings_and_compares_as_a_multiset(self) -> None:
        self.assertEqual(
            HARNESS.canonical_forwards(["18080→8080", "9090:90"]),
            HARNESS.canonical_forwards(["tcp:9090:90", "18080:8080"]),
        )

    def test_canonical_forwards_keeps_protocol_and_bind_distinct(self) -> None:
        self.assertNotEqual(
            HARNESS.canonical_forwards(["udp:5353:53"]),
            HARNESS.canonical_forwards(["5353:53"]),
        )
        self.assertNotEqual(
            HARNESS.canonical_forwards(["127.0.0.1:2222:22"]),
            HARNESS.canonical_forwards(["2222:22"]),
        )

    def test_canonical_forwards_refuses_a_value_with_no_guest_port(self) -> None:
        with self.assertRaises(HARNESS.AcceptanceError):
            HARNESS.canonical_forwards(["8080"])


class PruneAndListTests(unittest.TestCase):
    def test_prune_rows_sorts_and_projects_every_row(self) -> None:
        payload = {
            "removed": [
                {"kind": "partial", "id": "machines/a/x.partial", "bytes": 8},
                {"kind": "log", "id": "a/console.log.previous", "bytes": 4},
            ]
        }
        self.assertEqual(
            HARNESS.prune_rows(payload),
            [("log", "a/console.log.previous", 4), ("partial", "machines/a/x.partial", 8)],
        )

    def test_prune_rows_refuses_a_malformed_row(self) -> None:
        with self.assertRaises(HARNESS.AcceptanceError):
            HARNESS.prune_rows({"removed": [{"kind": "log", "id": "a", "bytes": "4"}]})

    def test_machine_row_finds_the_named_machine_and_refuses_a_miss(self) -> None:
        rows = [{"name": "other"}, {"name": "m6", "status": "running"}]
        self.assertEqual(HARNESS.machine_row(rows, "m6")["status"], "running")
        with self.assertRaises(HARNESS.AcceptanceError):
            HARNESS.machine_row(rows, "absent")


class GuestReadingTests(unittest.TestCase):
    def test_parse_free_total_bytes_reads_the_mem_row(self) -> None:
        output = (
            "               total        used        free\n"
            "Mem:      2077192192   304996352   485060608\n"
            "Swap:              0           0           0\n"
        )
        self.assertEqual(HARNESS.parse_free_total_bytes(output), 2_077_192_192)

    def test_parse_free_total_bytes_refuses_output_without_a_mem_row(self) -> None:
        with self.assertRaises(HARNESS.AcceptanceError):
            HARNESS.parse_free_total_bytes("total used free\n")

    def test_parse_single_number_takes_the_last_numeric_line(self) -> None:
        self.assertEqual(HARNESS.parse_single_number("1B-blocks\n23843045376\n"), 23_843_045_376)
        with self.assertRaises(HARNESS.AcceptanceError):
            HARNESS.parse_single_number("   \n")
        with self.assertRaises(HARNESS.AcceptanceError):
            HARNESS.parse_single_number("not-a-number\n")


if __name__ == "__main__":
    unittest.main()
