#!/usr/bin/env python3
"""Validate the Linux x86_64 built-in catalog against vendor release metadata."""

from __future__ import annotations

import argparse
import json
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
IMAGE_KEYS = {
    "distro",
    "version",
    "aliases",
    "default",
    "firmware",
    "format",
    "sshd_path",
    "arch",
}
ARCH_KEYS = {"url", "checksum_url", "checksum_alg", "firmware"}
MAX_MANIFEST_BYTES = 2 * 1024 * 1024
MAX_METADATA_BYTES = 32 * 1024 * 1024
MAX_REDIRECTS = 5
USER_AGENT = "firestone-catalog-validator/0.2"
FEDORA_DOWNLOAD_HOST = "download.fedoraproject.org"
FEDORA_RELEASES_URL = "https://getfedora.org/releases.json"
UBUNTU_RELEASES_URL = (
    "https://cloud-images.ubuntu.com/releases/streams/v1/"
    "com.ubuntu.cloud:released:download.json"
)
DEFAULT_GUEST_USER = "root"
DEFAULT_SSHD_PATH = "/usr/sbin/sshd"
AUTHORIZED_ENTRIES = {
    "ubuntu:24.04": {
        "aliases": ["noble"],
        "default": True,
        "checksum_alg": "sha256",
    },
    "ubuntu:22.04": {
        "aliases": ["jammy"],
        "default": False,
        "checksum_alg": "sha256",
    },
    "debian:12": {
        "aliases": ["bookworm"],
        "default": True,
        "checksum_alg": "sha512",
    },
    "debian:13": {
        "aliases": ["trixie"],
        "default": False,
        "checksum_alg": "sha512",
    },
    "fedora:44": {
        "aliases": [],
        "default": True,
        "checksum_alg": "sha256",
    },
}


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


def require_mapping(value: object, context: str) -> dict[str, object]:
    if not isinstance(value, dict):
        raise ValidationError(f"{context}: expected an object")
    return value


def require_list(value: object, context: str) -> list[object]:
    if not isinstance(value, list):
        raise ValidationError(f"{context}: expected an array")
    return value


def require_https(url: str, context: str) -> None:
    parsed = urlsplit(url)
    if (
        parsed.scheme != "https"
        or not parsed.hostname
        or parsed.username is not None
        or parsed.password is not None
        or parsed.query
        or parsed.fragment
    ):
        raise ValidationError(f"{context}: expected a strict https URL, got {url!r}")


def require_immutable_build_url(url: str, context: str) -> None:
    require_https(url, context)
    components = {component.lower() for component in urlsplit(url).path.split("/")}
    moving = components & {"current", "latest"}
    if moving:
        raise ValidationError(
            f"{context}: source uses moving path component {sorted(moving)[0]!r}"
        )


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


def fetch_bounded_text(
    url: str,
    timeout: float,
    maximum: int,
    label: str,
) -> tuple[str, int, str, tuple[str, ...]]:
    response, redirects = request(url, method="GET", timeout=timeout)
    with response:
        status = response.status
        final_url = response.geturl()
        content_type = response.headers.get("Content-Type", "")
        body = response.read(maximum + 1)

    if not 200 <= status < 300:
        raise ValidationError(f"{label} request returned HTTP {status}: {url}")
    if len(body) > maximum:
        raise ValidationError(f"{label} response exceeds {maximum} bytes: {url}")
    if urlsplit(final_url).scheme != "https":
        raise ValidationError(f"{label} request redirected outside https: {final_url}")
    text = body.decode("utf-8", errors="strict")
    if "text/html" in content_type.lower() or "<html" in text[:512].lower():
        raise ValidationError(f"{label} URL returned HTML: {url}")
    return text, status, final_url, redirects


def fetch_manifest(url: str, timeout: float) -> tuple[str, int, str, tuple[str, ...]]:
    return fetch_bounded_text(
        url,
        timeout,
        MAX_MANIFEST_BYTES,
        "checksum",
    )


def fetch_json_document(url: str, timeout: float, label: str) -> object:
    text, _, _, _ = fetch_bounded_text(url, timeout, MAX_METADATA_BYTES, label)
    try:
        return json.loads(text)
    except json.JSONDecodeError as error:
        raise ValidationError(f"{label} response is not valid JSON: {error}") from error


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


def validate_authorized_catalog(images: list[dict[str, object]]) -> None:
    references = [
        f"{require_string(image, 'distro', 'image')}:"
        f"{require_string(image, 'version', 'image')}"
        for image in images
    ]
    expected_references = list(AUTHORIZED_ENTRIES)
    if references != expected_references:
        raise ValidationError(
            "catalog references must be exactly "
            f"{', '.join(expected_references)} in that order; got {', '.join(references)}"
        )

    for image, image_ref in zip(images, references, strict=True):
        expected = AUTHORIZED_ENTRIES[image_ref]
        unknown_keys = set(image) - IMAGE_KEYS
        if unknown_keys:
            names = ", ".join(sorted(unknown_keys))
            raise ValidationError(f"{image_ref}: unknown keys: {names}")
        if image.get("aliases") != expected["aliases"]:
            raise ValidationError(
                f"{image_ref}: aliases must be exactly {expected['aliases']!r}"
            )
        if image.get("default") is not expected["default"]:
            raise ValidationError(
                f"{image_ref}: default must be {expected['default']!r}"
            )
        if image.get("firmware") != "edk2":
            raise ValidationError(f"{image_ref}: Linux x86_64 firmware must be edk2")
        if image.get("format") != "qcow2":
            raise ValidationError(f"{image_ref}: format must be 'qcow2'")
        if image.get("sshd_path") != DEFAULT_SSHD_PATH:
            raise ValidationError(
                f"{image_ref}: sshd_path must be exactly {DEFAULT_SSHD_PATH!r}"
            )

        arch_tables = image.get("arch")
        if not isinstance(arch_tables, dict) or set(arch_tables) != ARCHITECTURES:
            raise ValidationError(
                f"{image_ref}: compile/catalog arch tables must be exactly "
                f"{sorted(ARCHITECTURES)!r}"
            )
        for architecture in sorted(ARCHITECTURES):
            context = f"{image_ref}/{architecture}"
            entry = require_mapping(arch_tables[architecture], context)
            unknown_arch_keys = set(entry) - ARCH_KEYS
            if unknown_arch_keys:
                names = ", ".join(sorted(unknown_arch_keys))
                raise ValidationError(f"{context}: unknown keys: {names}")
            if entry.get("firmware", image["firmware"]) != "edk2":
                raise ValidationError(f"{context}: effective firmware must be edk2")
            if entry.get("checksum_alg", "sha256") != expected["checksum_alg"]:
                raise ValidationError(
                    f"{context}: checksum_alg must be {expected['checksum_alg']!r}"
                )
            require_immutable_build_url(
                require_string(entry, "url", context),
                f"{context} image",
            )
            require_immutable_build_url(
                require_string(entry, "checksum_url", context),
                f"{context} checksum",
            )


def ubuntu_release_metadata(
    image_ref: str,
    image_url: str,
    checksum_url: str,
    document: object,
) -> tuple[str, str]:
    _, version = image_ref.split(":", 1)
    codename = str(AUTHORIZED_ENTRIES[image_ref]["aliases"][0])
    root = require_mapping(document, "Ubuntu released-stream metadata")
    products = require_mapping(root.get("products"), "Ubuntu products")
    matches = []
    for value in products.values():
        product = require_mapping(value, "Ubuntu product")
        if (
            product.get("os") == "ubuntu"
            and product.get("release") == codename
            and product.get("arch") == "amd64"
        ):
            matches.append(product)
    if len(matches) != 1:
        raise ValidationError(
            f"{image_ref}: Ubuntu metadata has {len(matches)} amd64 {codename} products"
        )
    product = matches[0]
    if not str(product.get("release_title", "")).startswith(version):
        raise ValidationError(f"{image_ref}: Ubuntu release title does not match {version}")
    versions = require_mapping(product.get("versions"), f"{image_ref} Ubuntu versions")
    if not versions:
        raise ValidationError(f"{image_ref}: Ubuntu metadata has no released builds")
    build = max(versions)
    release = require_mapping(versions[build], f"{image_ref} Ubuntu build {build}")
    items = require_mapping(release.get("items"), f"{image_ref} Ubuntu build items")
    disk = require_mapping(items.get("disk1.img"), f"{image_ref} Ubuntu disk1.img")
    path = require_string(disk, "path", f"{image_ref} Ubuntu disk1.img")
    normalized_path = path.removeprefix("server/")
    expected_url = f"https://cloud-images.ubuntu.com/{normalized_path}"
    if image_url != expected_url:
        raise ValidationError(
            f"{image_ref}: image URL is not current Ubuntu released build {build}; "
            f"expected {expected_url}"
        )
    expected_checksum = image_url.rsplit("/", 1)[0] + "/SHA256SUMS"
    if checksum_url != expected_checksum:
        raise ValidationError(
            f"{image_ref}: checksum URL must be {expected_checksum}"
        )
    digest = require_string(disk, "sha256", f"{image_ref} Ubuntu disk1.img")
    if re.fullmatch(r"[0-9a-fA-F]{64}", digest) is None:
        raise ValidationError(f"{image_ref}: Ubuntu metadata has an invalid sha256")
    return f"Ubuntu released-stream build {build}", digest.lower()


def debian_release_metadata(
    image_ref: str,
    image_url: str,
    checksum_url: str,
    timeout: float,
) -> tuple[str, None]:
    _, version = image_ref.split(":", 1)
    codename = str(AUTHORIZED_ENTRIES[image_ref]["aliases"][0])
    metadata_url = (
        f"https://cloud.debian.org/images/cloud/{codename}/latest/"
        f"debian-{version}-genericcloud-amd64.json"
    )
    document = fetch_json_document(metadata_url, timeout, f"{image_ref} Debian metadata")
    root = require_mapping(document, f"{image_ref} Debian metadata")
    items = require_list(root.get("items"), f"{image_ref} Debian items")
    matches: list[dict[str, object]] = []
    for value in items:
        item = require_mapping(value, f"{image_ref} Debian item")
        if item.get("kind") != "Build":
            continue
        data = require_mapping(item.get("data"), f"{image_ref} Debian item data")
        info = require_mapping(data.get("info"), f"{image_ref} Debian info")
        if (
            info.get("arch") == "amd64"
            and info.get("release") == codename
            and str(info.get("release_id")) == version
            and info.get("type") == "official"
            and info.get("vendor") == "genericcloud"
        ):
            matches.append(info)
    if len(matches) != 1:
        raise ValidationError(
            f"{image_ref}: Debian metadata has {len(matches)} official genericcloud amd64 records"
        )
    build = require_string(matches[0], "version", f"{image_ref} Debian info")
    directory = f"https://cloud.debian.org/images/cloud/{codename}/{build}"
    expected_url = (
        f"{directory}/debian-{version}-genericcloud-amd64-{build}.qcow2"
    )
    if image_url != expected_url:
        raise ValidationError(
            f"{image_ref}: image URL is not current Debian build {build}; "
            f"expected {expected_url}"
        )
    expected_checksum = f"{directory}/SHA512SUMS"
    if checksum_url != expected_checksum:
        raise ValidationError(
            f"{image_ref}: checksum URL must be {expected_checksum}"
        )
    return f"Debian official genericcloud build {build}", None


def fedora_release_metadata(
    image_ref: str,
    image_url: str,
    checksum_url: str,
    document: object,
) -> tuple[str, str]:
    releases = require_list(document, "Fedora releases metadata")
    cloud_versions: list[int] = []
    generic: list[dict[str, object]] = []
    for value in releases:
        release = require_mapping(value, "Fedora release")
        if release.get("variant") != "Cloud" or release.get("arch") != "x86_64":
            continue
        raw_version = str(release.get("version", ""))
        if raw_version.isdigit():
            cloud_versions.append(int(raw_version))
        if (
            release.get("subvariant") == "Cloud_Base"
            and str(release.get("link", "")).endswith(".qcow2")
        ):
            generic.append(release)
    if not cloud_versions:
        raise ValidationError("Fedora metadata has no stable x86_64 Cloud release")
    current = str(max(cloud_versions))
    _, configured = image_ref.split(":", 1)
    if configured != current:
        raise ValidationError(
            f"{image_ref}: Fedora current stable Cloud release is {current}"
        )
    candidates = [release for release in generic if str(release.get("version")) == current]
    if len(candidates) != 1:
        raise ValidationError(
            f"{image_ref}: Fedora metadata has {len(candidates)} current Generic qcow2 records"
        )
    release = candidates[0]
    expected_url = require_string(release, "link", f"{image_ref} Fedora release")
    if image_url != expected_url:
        raise ValidationError(
            f"{image_ref}: image URL does not match Fedora releases.json; "
            f"expected {expected_url}"
        )
    filename = Path(urlsplit(image_url).path).name
    match = re.fullmatch(
        rf"Fedora-Cloud-Base-Generic-{re.escape(current)}-([0-9.]+)\.x86_64\.qcow2",
        filename,
    )
    if match is None:
        raise ValidationError(f"{image_ref}: unexpected Fedora Generic filename {filename!r}")
    build = match.group(1)
    expected_checksum = (
        image_url.rsplit("/", 1)[0]
        + f"/Fedora-Cloud-{current}-{build}-x86_64-CHECKSUM"
    )
    if checksum_url != expected_checksum:
        raise ValidationError(
            f"{image_ref}: checksum URL must be {expected_checksum}"
        )
    digest = require_string(release, "sha256", f"{image_ref} Fedora release")
    return f"Fedora releases.json stable build {current}-{build}", digest.lower()


def validate_catalog(path: Path, timeout: float) -> int:
    images = load_catalog(path)
    validate_authorized_catalog(images)
    ubuntu_document: object | None = None
    fedora_document: object | None = None
    validated = 0

    for image in images:
        distro = require_string(image, "distro", "image")
        version = require_string(image, "version", distro)
        image_ref = f"{distro}:{version}"
        arch_tables = require_mapping(image.get("arch"), f"{image_ref} arch")
        entry = require_mapping(arch_tables["x86_64"], f"{image_ref}/x86_64")
        context = f"{image_ref}/x86_64"
        url = require_string(entry, "url", context)
        checksum_url = require_string(entry, "checksum_url", context)
        algorithm = str(entry.get("checksum_alg", "sha256"))
        require_https(url, context)
        require_https(checksum_url, context)
        if distro == "fedora":
            require_fedora_redirector_url(url, context)
            require_fedora_redirector_url(checksum_url, context)

        if distro == "ubuntu":
            if ubuntu_document is None:
                ubuntu_document = fetch_json_document(
                    UBUNTU_RELEASES_URL,
                    timeout,
                    "Ubuntu released-stream metadata",
                )
            metadata_fact, metadata_digest = ubuntu_release_metadata(
                image_ref,
                url,
                checksum_url,
                ubuntu_document,
            )
        elif distro == "debian":
            metadata_fact, metadata_digest = debian_release_metadata(
                image_ref,
                url,
                checksum_url,
                timeout,
            )
        elif distro == "fedora":
            if fedora_document is None:
                fedora_document = fetch_json_document(
                    FEDORA_RELEASES_URL,
                    timeout,
                    "Fedora releases metadata",
                )
            metadata_fact, metadata_digest = fedora_release_metadata(
                image_ref,
                url,
                checksum_url,
                fedora_document,
            )
        else:
            raise ValidationError(f"{image_ref}: unauthorized distribution")

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
        if metadata_digest is not None and digest != metadata_digest:
            raise ValidationError(
                f"{context}: checksum manifest digest {digest} differs from "
                f"release metadata digest {metadata_digest}"
            )
        final_image_host = urlsplit(final_image_url).hostname
        final_manifest_host = urlsplit(final_manifest_url).hostname
        print(f"metadata {context}: {metadata_fact}")
        print(
            f"ok {context}: {filename} "
            f"({image_method} {image_status}; checksum GET {manifest_status}; "
            f"{algorithm} {digest}; hosts "
            f"image={final_image_host} checksum={final_manifest_host}; redirects "
            f"image={len(image_redirects)} checksum={len(manifest_redirects)})"
        )
        validated += 1

    print(
        f"policy: user={DEFAULT_GUEST_USER} (MachineSpec default), "
        f"sshd_path={DEFAULT_SSHD_PATH}, firmware=edk2, architecture=x86_64"
    )
    return validated


def main() -> int:
    args = parse_args()
    try:
        validated = validate_catalog(args.catalog, args.timeout)
    except ValidationError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(f"validated {validated} Linux x86_64 catalog entries")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
