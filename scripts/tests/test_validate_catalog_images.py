"""Unit tests for the built-in image catalog validator."""

from __future__ import annotations

import copy
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


class ChecksumParserTests(unittest.TestCase):
    def test_checksum_for_gnu_sha512_exact_filename_selected(self) -> None:
        filename = "debian-13-genericcloud-arm64.qcow2"
        digest = "a1" * 64
        manifest = (
            f"{'b2' * 64}  {filename}.sig\n"
            f"{digest} *{filename}\n"
        )

        self.assertEqual(
            validator.checksum_for(manifest, filename, "sha512"),
            digest,
        )

    def test_checksum_for_bsd_sha256_clearsigned_text_selected(self) -> None:
        filename = "Fedora-Cloud-Base-Generic-44-1.7.x86_64.qcow2"
        uppercase_digest = "AB" * 32
        manifest = f"""-----BEGIN PGP SIGNED MESSAGE-----
Hash: SHA256

# Fedora-Cloud-44-1.7-x86_64-CHECKSUM
SHA256 ({filename}) = {uppercase_digest}
-----BEGIN PGP SIGNATURE-----
representative-signature-data
-----END PGP SIGNATURE-----
"""

        self.assertEqual(
            validator.checksum_for(manifest, filename, "sha256"),
            uppercase_digest.lower(),
        )

    def test_checksum_for_missing_filename_rejected(self) -> None:
        manifest = f"{'a3' * 32}  another-image.qcow2\n"

        with self.assertRaisesRegex(
            validator.ValidationError,
            "no sha256 record for missing-image.qcow2",
        ):
            validator.checksum_for(manifest, "missing-image.qcow2", "sha256")

    def test_checksum_for_conflicting_duplicate_digest_rejected(self) -> None:
        filename = "image.qcow2"
        manifest = (
            f"{'a4' * 32}  {filename}\n"
            f"SHA256 ({filename}) = {'b5' * 32}\n"
        )

        with self.assertRaisesRegex(
            validator.ValidationError,
            "conflicting sha256 records for image.qcow2",
        ):
            validator.checksum_for(manifest, filename, "sha256")


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


class CatalogPolicyTests(unittest.TestCase):
    def setUp(self) -> None:
        catalog = Path(__file__).parents[2] / "catalog" / "images.toml"
        self.images = validator.load_catalog(catalog)

    def test_linux_mvp_catalog_policy_accepts_built_in_entries(self) -> None:
        validator.validate_authorized_catalog(self.images)

    def test_catalog_policy_rejects_missing_compile_only_aarch64_source(self) -> None:
        images = copy.deepcopy(self.images)
        images[0]["arch"].pop("aarch64")

        with self.assertRaisesRegex(
            validator.ValidationError,
            "arch tables must be exactly",
        ):
            validator.validate_authorized_catalog(images)

    def test_linux_mvp_catalog_policy_rejects_moving_source(self) -> None:
        images = copy.deepcopy(self.images)
        images[0]["arch"]["x86_64"]["url"] = (
            "https://cloud-images.ubuntu.com/noble/current/image.img"
        )

        with self.assertRaisesRegex(
            validator.ValidationError,
            "moving path component 'current'",
        ):
            validator.validate_authorized_catalog(images)

    def test_linux_mvp_catalog_policy_rejects_extra_release(self) -> None:
        images = copy.deepcopy(self.images)
        extra = copy.deepcopy(images[-1])
        extra["version"] = "45"
        images.append(extra)

        with self.assertRaisesRegex(
            validator.ValidationError,
            "catalog references must be exactly",
        ):
            validator.validate_authorized_catalog(images)


class VendorMetadataTests(unittest.TestCase):
    def test_fedora_current_generic_release_matches_catalog_source(self) -> None:
        image_url = (
            "https://download.fedoraproject.org/pub/fedora/linux/releases/44/"
            "Cloud/x86_64/images/Fedora-Cloud-Base-Generic-44-1.7.x86_64.qcow2"
        )
        checksum_url = (
            "https://download.fedoraproject.org/pub/fedora/linux/releases/44/"
            "Cloud/x86_64/images/Fedora-Cloud-44-1.7-x86_64-CHECKSUM"
        )
        digest = "ab" * 32
        document = [
            {
                "version": "43",
                "arch": "x86_64",
                "variant": "Cloud",
                "subvariant": "Cloud_Base",
                "link": "https://example.invalid/43.qcow2",
                "sha256": "cd" * 32,
            },
            {
                "version": "44",
                "arch": "x86_64",
                "variant": "Cloud",
                "subvariant": "Cloud_Base",
                "link": image_url,
                "sha256": digest,
            },
        ]

        fact, observed = validator.fedora_release_metadata(
            "fedora:44", image_url, checksum_url, document
        )

        self.assertEqual(fact, "Fedora releases.json stable build 44-1.7")
        self.assertEqual(observed, digest)

    def test_strict_https_rejects_query_and_fragment(self) -> None:
        for url in (
            "https://example.invalid/image?mutable=1",
            "https://example.invalid/image#fragment",
        ):
            with self.subTest(url=url):
                with self.assertRaises(validator.ValidationError):
                    validator.require_https(url, "source")


if __name__ == "__main__":
    unittest.main()
