#!/usr/bin/env python3
"""Validate built-in catalog metadata against upstream checksum manifests."""

from __future__ import annotations

import argparse
import re
import ssl
import sys
import tomllib
import urllib.error
import urllib.request
from pathlib import Path
from urllib.parse import unquote, urlsplit

ALGORITHMS = {"sha256": 64, "sha512": 128}
ARCHITECTURES = {"x86_64", "aarch64"}
FIRMWARE = {"rhf", "edk2"}
IMAGE_KEYS = {"distro", "version", "aliases", "default", "firmware", "format", "arch"}
ARCH_KEYS = {"url", "checksum_url", "checksum_alg"}
MAX_MANIFEST_BYTES = 2 * 1024 * 1024
MAX_REDIRECTS = 5
USER_AGENT = "firestone-catalog-validator/0.1"
FEDORA_DOWNLOAD_HOST = "download.fedoraproject.org"


class ValidationError(Exception):
    """A catalog entry failed structural or upstream validation."""


class SafeRedirectHandler(urllib.request.HTTPRedirectHandler):
    """Follow a bounded number of HTTPS redirects and record each target."""

    def __init__(self) -> None:
        super().__init__()
        self.targets: list[str] = []

    def redirect_request(
        self,
        req: urllib.request.Request,
        fp: object,
        code: int,
        msg: str,
        headers: object,
        newurl: str,
    ) -> urllib.request.Request | None:
        if len(self.targets) >= MAX_REDIRECTS:
            raise ValidationError(
                f"redirect limit exceeded ({MAX_REDIRECTS}): {req.full_url}"
            )
        require_https(newurl, "redirect target")
        self.targets.append(newurl)
        return super().redirect_request(req, fp, code, msg, headers, newurl)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "catalog",
        nargs="?",
        type=Path,
        default=Path("catalog/images.toml"),
        help="catalog TOML path (default: catalog/images.toml)",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=30.0,
        help="per-request timeout in seconds (default: 30)",
    )
    return parser.parse_args()


def require_string(table: dict[str, object], key: str, context: str) -> str:
    value = table.get(key)
    if not isinstance(value, str) or not value:
        raise ValidationError(f"{context}: {key} must be a non-empty string")
    return value


def require_https(url: str, context: str) -> None:
    parsed = urlsplit(url)
    if (
        parsed.scheme != "https"
        or not parsed.hostname
        or parsed.username is not None
        or parsed.password is not None
    ):
        raise ValidationError(f"{context}: expected an https URL, got {url!r}")


def require_fedora_redirector_url(url: str, context: str) -> None:
    parsed = urlsplit(url)
    if (
        parsed.scheme != "https"
        or parsed.netloc != FEDORA_DOWNLOAD_HOST
        or not parsed.path.startswith("/pub/fedora/")
    ):
        raise ValidationError(
            f"{context}: Fedora source must start at "
            f"https://{FEDORA_DOWNLOAD_HOST}/pub/fedora/"
        )


def request(
    url: str,
    *,
    method: str,
    timeout: float,
    headers: dict[str, str] | None = None,
) -> tuple[urllib.response.addinfourl, tuple[str, ...]]:
    request_headers = {"User-Agent": USER_AGENT, "Accept-Encoding": "identity"}
    if headers:
        request_headers.update(headers)
    req = urllib.request.Request(url, method=method, headers=request_headers)
    context = ssl.create_default_context()
    redirect_handler = SafeRedirectHandler()
    opener = urllib.request.build_opener(
        redirect_handler,
        urllib.request.HTTPSHandler(context=context),
    )
    response = opener.open(req, timeout=timeout)
    return response, tuple(redirect_handler.targets)


def probe_image(url: str, timeout: float) -> tuple[str, int, str, tuple[str, ...]]:
    try:
        response, redirects = request(url, method="HEAD", timeout=timeout)
        method = "HEAD"
    except urllib.error.HTTPError as error:
        if error.code not in {403, 405, 501}:
            raise
        error.close()
        response, redirects = request(
            url,
            method="GET",
            timeout=timeout,
            headers={"Range": "bytes=0-0"},
        )
        method = "GET range"

    with response:
        status = response.status
        final_url = response.geturl()
        content_type = response.headers.get("Content-Type")
        if method == "GET range":
            response.read(1)

    if not 200 <= status < 300:
        raise ValidationError(f"image request returned HTTP {status}: {url}")
    if urlsplit(final_url).scheme != "https":
        raise ValidationError(f"image request redirected outside https: {final_url}")
    if content_type and "text/html" in content_type.lower():
        raise ValidationError(f"image URL returned HTML instead of an image: {url}")
    return method, status, final_url, redirects


def fetch_manifest(url: str, timeout: float) -> tuple[str, int, str, tuple[str, ...]]:
    response, redirects = request(url, method="GET", timeout=timeout)
    with response:
        status = response.status
        final_url = response.geturl()
        content_type = response.headers.get("Content-Type", "")
        body = response.read(MAX_MANIFEST_BYTES + 1)

    if not 200 <= status < 300:
        raise ValidationError(f"checksum request returned HTTP {status}: {url}")
    if len(body) > MAX_MANIFEST_BYTES:
        raise ValidationError(f"checksum response exceeds {MAX_MANIFEST_BYTES} bytes: {url}")
    if urlsplit(final_url).scheme != "https":
        raise ValidationError(f"checksum request redirected outside https: {final_url}")
    text = body.decode("utf-8", errors="strict")
    if "text/html" in content_type.lower() or "<html" in text[:512].lower():
        raise ValidationError(f"checksum URL returned HTML instead of a manifest: {url}")
    return text, status, final_url, redirects


def checksum_for(manifest: str, filename: str, algorithm: str) -> str:
    digest_length = ALGORITHMS[algorithm]
    digest_pattern = rf"[0-9a-fA-F]{{{digest_length}}}"
    gnu = re.compile(rf"^\s*({digest_pattern})\s+[ *]?(.+?)\s*$")
    bsd = re.compile(
        rf"^\s*{re.escape(algorithm)}\s*\((.+)\)\s*=\s*({digest_pattern})\s*$",
        re.IGNORECASE,
    )

    matches: list[str] = []
    for line in manifest.splitlines():
        gnu_match = gnu.match(line)
        if gnu_match:
            name = gnu_match.group(2).removeprefix("./")
            if name == filename:
                matches.append(gnu_match.group(1).lower())
            continue

        bsd_match = bsd.match(line)
        if bsd_match and bsd_match.group(1).removeprefix("./") == filename:
            matches.append(bsd_match.group(2).lower())

    if not matches:
        raise ValidationError(
            f"checksum manifest has no {algorithm} record for {filename}"
        )
    if len(set(matches)) != 1:
        raise ValidationError(
            f"checksum manifest has conflicting {algorithm} records for {filename}"
        )
    return matches[0]


def load_catalog(path: Path) -> list[dict[str, object]]:
    try:
        with path.open("rb") as handle:
            document = tomllib.load(handle)
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise ValidationError(f"cannot load {path}: {error}") from error

    unknown_keys = set(document) - {"image"}
    if unknown_keys:
        names = ", ".join(sorted(unknown_keys))
        raise ValidationError(f"{path}: unknown top-level keys: {names}")
    images = document.get("image")
    if not isinstance(images, list) or not images:
        raise ValidationError(f"{path}: expected at least one [[image]] entry")
    if not all(isinstance(image, dict) for image in images):
        raise ValidationError(f"{path}: each [[image]] entry must be a table")
    return images


def validate_catalog(path: Path, timeout: float) -> int:
    images = load_catalog(path)
    seen_refs: set[str] = set()
    default_distros: set[str] = set()
    validated = 0

    for image in images:
        unknown_keys = set(image) - IMAGE_KEYS
        if unknown_keys:
            names = ", ".join(sorted(unknown_keys))
            raise ValidationError(f"image: unknown keys: {names}")
        distro = require_string(image, "distro", "image")
        version = require_string(image, "version", distro)
        image_ref = f"{distro}:{version}"
        if image_ref in seen_refs:
            raise ValidationError(f"duplicate catalog entry: {image_ref}")
        seen_refs.add(image_ref)

        aliases = image.get("aliases")
        if not isinstance(aliases, list) or not all(
            isinstance(alias, str) and alias for alias in aliases
        ):
            raise ValidationError(f"{image_ref}: aliases must be a string array")
        if len(set(aliases)) != len(aliases):
            raise ValidationError(f"{image_ref}: aliases must be unique")

        default = image.get("default")
        if not isinstance(default, bool):
            raise ValidationError(f"{image_ref}: default must be a boolean")
        if default:
            if distro in default_distros:
                raise ValidationError(f"{distro}: multiple default entries")
            default_distros.add(distro)

        firmware = require_string(image, "firmware", image_ref)
        if firmware not in FIRMWARE:
            raise ValidationError(f"{image_ref}: unsupported firmware {firmware!r}")
        if image.get("format") != "qcow2":
            raise ValidationError(f"{image_ref}: format must be 'qcow2'")

        arch_tables = image.get("arch")
        if not isinstance(arch_tables, dict) or not arch_tables:
            raise ValidationError(f"{image_ref}: arch must contain at least one table")
        unknown_arches = set(arch_tables) - ARCHITECTURES
        if unknown_arches:
            names = ", ".join(sorted(unknown_arches))
            raise ValidationError(f"{image_ref}: unsupported architectures: {names}")

        for arch, entry in arch_tables.items():
            context = f"{image_ref}/{arch}"
            if not isinstance(entry, dict):
                raise ValidationError(f"{context}: architecture entry must be a table")
            unknown_keys = set(entry) - ARCH_KEYS
            if unknown_keys:
                names = ", ".join(sorted(unknown_keys))
                raise ValidationError(f"{context}: unknown keys: {names}")
            url = require_string(entry, "url", context)
            checksum_url = require_string(entry, "checksum_url", context)
            algorithm = entry.get("checksum_alg", "sha256")
            if algorithm not in ALGORITHMS:
                raise ValidationError(f"{context}: unsupported checksum_alg {algorithm!r}")
            require_https(url, context)
            require_https(checksum_url, context)
            if distro == "fedora":
                require_fedora_redirector_url(url, context)
                require_fedora_redirector_url(checksum_url, context)

            filename = Path(unquote(urlsplit(url).path)).name
            if not filename:
                raise ValidationError(f"{context}: image URL has no filename")

            try:
                image_method, image_status, final_image_url, image_redirects = probe_image(
                    url, timeout
                )
                (
                    manifest,
                    manifest_status,
                    final_manifest_url,
                    manifest_redirects,
                ) = fetch_manifest(checksum_url, timeout)
            except (OSError, UnicodeError) as error:
                raise ValidationError(f"{context}: upstream request failed: {error}") from error
            final_filename = Path(unquote(urlsplit(final_image_url).path)).name
            if final_filename != filename:
                raise ValidationError(
                    f"{context}: image redirect changed filename to {final_filename!r}"
                )
            digest = checksum_for(manifest, filename, algorithm)
            final_image_host = urlsplit(final_image_url).hostname
            final_manifest_host = urlsplit(final_manifest_url).hostname
            print(
                f"ok {context}: {filename} "
                f"({image_method} {image_status}; checksum GET {manifest_status}; "
                f"{algorithm} {digest}; hosts "
                f"image={final_image_host} checksum={final_manifest_host}; redirects "
                f"image={len(image_redirects)} checksum={len(manifest_redirects)})"
            )
            validated += 1

    missing_defaults = {require_string(image, "distro", "image") for image in images}
    missing_defaults -= default_distros
    if missing_defaults:
        names = ", ".join(sorted(missing_defaults))
        raise ValidationError(f"missing default entry for: {names}")
    return validated


def main() -> int:
    args = parse_args()
    try:
        validated = validate_catalog(args.catalog, args.timeout)
    except ValidationError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(f"validated {validated} catalog architecture entries")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
