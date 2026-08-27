"""Unit tests for the built-in image catalog validator."""

from __future__ import annotations

import importlib.util
import sys
import unittest
import urllib.request
from pathlib import Path
from types import ModuleType


def load_validator() -> ModuleType:
    path = Path(__file__).parents[1] / "validate-catalog-images.py"
    spec = importlib.util.spec_from_file_location("validate_catalog_images", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load validator from {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


validator = load_validator()


class FedoraOriginTests(unittest.TestCase):
    def test_fedora_redirector_expected_url_accepted(self) -> None:
        validator.require_fedora_redirector_url(
            "https://download.fedoraproject.org/pub/fedora/linux/releases/44/image",
            "fedora:44/x86_64",
        )

    def test_fedora_redirector_untrusted_origin_rejected(self) -> None:
        urls = (
            "http://download.fedoraproject.org/pub/fedora/image",
            "https://dl.fedoraproject.org/pub/fedora/image",
            "https://download.fedoraproject.org.example/pub/fedora/image",
            "https://download.fedoraproject.org:443/pub/fedora/image",
            "https://download.fedoraproject.org/not-fedora/image",
        )
        for url in urls:
            with self.subTest(url=url):
                with self.assertRaises(validator.ValidationError):
                    validator.require_fedora_redirector_url(url, "fedora")


class RedirectPolicyTests(unittest.TestCase):
    def test_redirect_https_target_recorded(self) -> None:
        handler = validator.SafeRedirectHandler()
        request = urllib.request.Request("https://download.fedoraproject.org/start")

        redirected = handler.redirect_request(
            request,
            None,
            302,
            "Found",
            {},
            "https://mirror.example/fedora/image",
        )

        self.assertIsNotNone(redirected)
        self.assertEqual(
            handler.targets,
            ["https://mirror.example/fedora/image"],
        )

    def test_redirect_http_downgrade_rejected(self) -> None:
        handler = validator.SafeRedirectHandler()
        request = urllib.request.Request("https://download.fedoraproject.org/start")

        with self.assertRaises(validator.ValidationError):
            handler.redirect_request(
                request,
                None,
                302,
                "Found",
                {},
                "http://mirror.example/fedora/image",
            )

    def test_redirect_over_limit_rejected(self) -> None:
        handler = validator.SafeRedirectHandler()
        request = urllib.request.Request("https://download.fedoraproject.org/start")
        handler.targets.extend(
            f"https://mirror.example/redirect-{index}"
            for index in range(validator.MAX_REDIRECTS)
        )

        with self.assertRaises(validator.ValidationError):
            handler.redirect_request(
                request,
                None,
                302,
                "Found",
                {},
                "https://mirror.example/fedora/image",
            )


if __name__ == "__main__":
    unittest.main()
