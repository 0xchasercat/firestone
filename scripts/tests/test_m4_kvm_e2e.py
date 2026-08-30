from __future__ import annotations

import importlib.util
import os
import socket
import sys
import tempfile
import threading
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
HARNESS_PATH = REPO_ROOT / "scripts" / "m4-kvm-e2e.py"
MODULE_SPEC = importlib.util.spec_from_file_location("m4_kvm_e2e", HARNESS_PATH)
if MODULE_SPEC is None or MODULE_SPEC.loader is None:
    raise RuntimeError(f"cannot load {HARNESS_PATH}")
HARNESS = importlib.util.module_from_spec(MODULE_SPEC)
sys.modules[MODULE_SPEC.name] = HARNESS
MODULE_SPEC.loader.exec_module(HARNESS)


@unittest.skipUnless(hasattr(socket, "AF_UNIX"), "Unix socket regression")
class M4HttpClientTests(unittest.TestCase):
    def test_http_request_accepts_path_socket_address(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            socket_path = Path(temporary) / "serve.sock"
            requests: list[bytes] = []
            failures: list[BaseException] = []
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as server:
                server.bind(os.fspath(socket_path))
                server.listen(1)
                server.settimeout(2)

                def serve() -> None:
                    try:
                        connection, _ = server.accept()
                        with connection:
                            request = bytearray()
                            while b"\r\n\r\n" not in request:
                                block = connection.recv(4096)
                                if not block:
                                    break
                                request.extend(block)
                            requests.append(bytes(request))
                            connection.sendall(
                                b"HTTP/1.1 200 OK\r\n"
                                b"Content-Length: 2\r\n"
                                b"Connection: close\r\n\r\n{}"
                            )
                    except BaseException as error:
                        failures.append(error)

                thread = threading.Thread(target=serve, daemon=True)
                thread.start()
                harness = object.__new__(HARNESS.Harness)
                harness.socket_path = socket_path
                response = harness.http_request("GET", "/v1/version", timeout=2)
                thread.join(timeout=3)

            self.assertFalse(thread.is_alive())
            self.assertEqual(failures, [])
            self.assertEqual(response.status, 200)
            self.assertEqual(response.body, b"{}")
            self.assertEqual(len(requests), 1)
            self.assertTrue(requests[0].startswith(b"GET /v1/version HTTP/1.1\r\n"))


if __name__ == "__main__":
    unittest.main()
