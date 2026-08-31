#!/usr/bin/env bash

set -euo pipefail

readonly CLOUD_HYPERVISOR_VERSION="v53.0"
readonly CLOUD_HYPERVISOR_RELEASE_URL="https://github.com/cloud-hypervisor/cloud-hypervisor/releases/tag/v53.0"
readonly CLOUD_HYPERVISOR_X86_64_URL="https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/v53.0/cloud-hypervisor-static"
readonly CLOUD_HYPERVISOR_AARCH64_URL="https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/v53.0/cloud-hypervisor-static-aarch64"

readonly RHF_VERSION="0.5.0"
readonly RHF_RELEASE_URL="https://github.com/cloud-hypervisor/rust-hypervisor-firmware/releases/tag/0.5.0"
readonly RHF_X86_64_URL="https://github.com/cloud-hypervisor/rust-hypervisor-firmware/releases/download/0.5.0/hypervisor-fw"
readonly RHF_AARCH64_URL="https://github.com/cloud-hypervisor/rust-hypervisor-firmware/releases/download/0.5.0/hypervisor-fw-aarch64"

# Cloud Hypervisor v53.0 pins this edk2 release in scripts/test_assets.yaml.
readonly EDK2_VERSION="ch-1e1b96f126"
readonly EDK2_RELEASE_URL="https://github.com/cloud-hypervisor/edk2/releases/tag/ch-1e1b96f126"
readonly EDK2_X86_64_URL="https://github.com/cloud-hypervisor/edk2/releases/download/ch-1e1b96f126/CLOUDHV.fd"
readonly EDK2_AARCH64_URL="https://github.com/cloud-hypervisor/edk2/releases/download/ch-1e1b96f126/CLOUDHV_EFI.fd"

# Firestone publishes reproducible static binaries because upstream v1.14.0 has
# no versioned binary release assets. Keep upstream source provenance explicit.
readonly VIRTIOFSD_VERSION="v1.14.0"
readonly VIRTIOFSD_COMMIT="c2540f8db14caba81c1e37fba23fc7bf2cd7f0dd"
readonly VIRTIOFSD_RELEASE_URL="https://github.com/0xchasercat/firestone/releases/tag/virtiofsd-v1.14.0-firestone.1"
readonly VIRTIOFSD_X86_64_URL="https://github.com/0xchasercat/firestone/releases/download/virtiofsd-v1.14.0-firestone.1/virtiofsd-v1.14.0-x86_64-unknown-linux-musl"
readonly VIRTIOFSD_AARCH64_URL="https://github.com/0xchasercat/firestone/releases/download/virtiofsd-v1.14.0-firestone.1/virtiofsd-v1.14.0-aarch64-unknown-linux-musl"
readonly VIRTIOFSD_SOURCE_URL="https://gitlab.com/virtio-fs/virtiofsd/-/archive/v1.14.0/virtiofsd-v1.14.0.tar.gz"

# Firestone-owned x86_64 helper release and exact upstream source identities.
readonly HELPERS_RELEASE_TAG="helpers-v0.1.0-firestone.1"
readonly HELPERS_RELEASE_URL="https://github.com/0xchasercat/firestone/releases/tag/$HELPERS_RELEASE_TAG"
readonly HELPERS_ASSET_BASE="https://github.com/0xchasercat/firestone/releases/download/$HELPERS_RELEASE_TAG"
readonly PASST_VERSION="2025_02_17.a1e48a0"
readonly PASST_COMMIT="a1e48a02ff3550eb7875a7df6726086e9b3a1213"
readonly PASST_X86_64_URL="$HELPERS_ASSET_BASE/passt-2025_02_17.a1e48a0-x86_64-unknown-linux-musl"
readonly PASST_SOURCE_URL="https://passt.top/passt/snapshot/passt-a1e48a02ff3550eb7875a7df6726086e9b3a1213.tar.xz"
readonly QEMU_IMG_VERSION="8.2.2"
readonly QEMU_COMMIT="11aa0b1ff115b86160c4d37e7c37e6a6b13b77ea"
readonly QEMU_IMG_X86_64_URL="$HELPERS_ASSET_BASE/qemu-img-8.2.2-x86_64-unknown-linux-musl"
readonly QEMU_SOURCE_URL="https://download.qemu.org/qemu-8.2.2.tar.xz"
readonly QEMU_SIGNATURE_URL="https://download.qemu.org/qemu-8.2.2.tar.xz.sig"
readonly QEMU_SIGNING_FINGERPRINT="CEACC9E15534EBABB82D3FA03353C9CEF108B584"
readonly HELPERS_SOURCE_URL="$HELPERS_ASSET_BASE/firestone-static-helpers-v0.1.0-corresponding-source.tar"
readonly HELPERS_BUILD_INFO_URL="$HELPERS_ASSET_BASE/firestone-static-helpers-v0.1.0-build-info.txt"

usage() {
    cat <<'EOF'
Usage: scripts/pin-deps.sh [verify|refresh] [--arch ARCH] [--manifest PATH]

Commands:
  verify   Download exact pinned artifacts and compare them with deps.toml.
           ARCH defaults to the host architecture; use --arch all for release checks.
  refresh  Download every exact pinned artifact and rewrite deps.toml checksums.
           Refresh never queries a "latest" endpoint and only supports --arch all.

Architectures: x86_64, aarch64, all
EOF
}

fail() {
    printf 'pin-deps: %s\n' "$*" >&2
    exit 1
}

normalize_arch() {
    case "$1" in
        x86_64 | amd64)
            printf '%s\n' x86_64
            ;;
        aarch64 | arm64)
            printf '%s\n' aarch64
            ;;
        all)
            printf '%s\n' all
            ;;
        *)
            fail "unsupported architecture '$1' (expected x86_64, aarch64, or all)"
            ;;
    esac
}

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        fail "sha256sum or shasum is required"
    fi
}

download() {
    local url=$1
    local output=$2

    printf 'download %s\n' "$url" >&2
    if ! curl \
        --fail \
        --location \
        --proto '=https' \
        --proto-redir '=https' \
        --silent \
        --show-error \
        --retry 3 \
        --retry-all-errors \
        --connect-timeout 20 \
        --output "$output" \
        "$url"
    then
        rm -f -- "$output"
        fail "download failed: $url"
    fi
}

download_hash() {
    local label=$1
    local url=$2
    local output="$temporary_dir/$label"

    download "$url" "$output"
    sha256_file "$output"
}

manifest_value() {
    local section=$1
    local key=$2

    awk -v wanted_section="[$section]" -v wanted_key="$key" '
        $0 == wanted_section {
            in_section = 1
            next
        }
        in_section && /^\[/ {
            exit
        }
        in_section && $0 ~ "^[[:space:]]*" wanted_key "[[:space:]]*=" {
            value = $0
            sub("^[[:space:]]*" wanted_key "[[:space:]]*=[[:space:]]*", "", value)
            sub(/[[:space:]]*#.*/, "", value)
            gsub(/^"|"$/, "", value)
            print value
            exit
        }
    ' "$manifest"
}

require_manifest_value() {
    local section=$1
    local key=$2
    local expected=$3
    local actual

    actual=$(manifest_value "$section" "$key")
    if [[ "$actual" != "$expected" ]]; then
        fail "$manifest: [$section] $key is '$actual', expected '$expected'"
    fi
}

require_manifest_sha() {
    local section=$1
    local actual

    actual=$(manifest_value "$section" sha256)
    [[ "$actual" =~ ^[0-9a-f]{64}$ ]] || fail "$manifest: [$section] has an invalid sha256"
}

verify_artifact() {
    local section=$1
    local label=$2
    local expected_url=$3
    local expected_sha actual_sha

    require_manifest_value "$section" url "$expected_url"
    expected_sha=$(manifest_value "$section" sha256)
    [[ "$expected_sha" =~ ^[0-9a-f]{64}$ ]] || fail "$manifest: [$section] has an invalid sha256"
    actual_sha=$(download_hash "$label" "$expected_url")
    if [[ "$actual_sha" != "$expected_sha" ]]; then
        fail "$label checksum mismatch: expected $expected_sha, got $actual_sha"
    fi
    printf 'verified %s %s\n' "$label" "$actual_sha"
}

verify_manifest_metadata() {
    local manifest_version

    manifest_version=$(awk '
        /^\[/ { exit }
        /^[[:space:]]*manifest_version[[:space:]]*=/ {
            value = $0
            sub("^[[:space:]]*manifest_version[[:space:]]*=[[:space:]]*", "", value)
            sub(/[[:space:]]*#.*/, "", value)
            print value
            exit
        }
    ' "$manifest")
    [[ "$manifest_version" == 1 ]] || fail "$manifest: manifest_version is '$manifest_version', expected '1'"

    require_manifest_value dependency.cloud-hypervisor version "$CLOUD_HYPERVISOR_VERSION"
    require_manifest_value dependency.cloud-hypervisor release_url "$CLOUD_HYPERVISOR_RELEASE_URL"
    require_manifest_value dependency.cloud-hypervisor availability binary

    require_manifest_value dependency.rust-hypervisor-firmware version "$RHF_VERSION"
    require_manifest_value dependency.rust-hypervisor-firmware release_url "$RHF_RELEASE_URL"
    require_manifest_value dependency.rust-hypervisor-firmware availability binary

    require_manifest_value dependency.cloud-hypervisor-edk2 version "$EDK2_VERSION"
    require_manifest_value dependency.cloud-hypervisor-edk2 release_url "$EDK2_RELEASE_URL"
    require_manifest_value dependency.cloud-hypervisor-edk2 availability binary

    require_manifest_value dependency.virtiofsd version "$VIRTIOFSD_VERSION"
    require_manifest_value dependency.virtiofsd commit "$VIRTIOFSD_COMMIT"
    require_manifest_value dependency.virtiofsd release_url "$VIRTIOFSD_RELEASE_URL"
    require_manifest_value dependency.virtiofsd availability binary
    require_manifest_value dependency.virtiofsd.source url "$VIRTIOFSD_SOURCE_URL"
    require_manifest_value dependency.passt version "$PASST_VERSION"
    require_manifest_value dependency.passt commit "$PASST_COMMIT"
    require_manifest_value dependency.passt release_url "$HELPERS_RELEASE_URL"
    require_manifest_value dependency.passt availability binary
    require_manifest_value dependency.passt.x86_64 url "$PASST_X86_64_URL"
    require_manifest_value dependency.qemu-img version "$QEMU_IMG_VERSION"
    require_manifest_value dependency.qemu-img commit "$QEMU_COMMIT"
    require_manifest_value dependency.qemu-img release_url "$HELPERS_RELEASE_URL"
    require_manifest_value dependency.qemu-img availability binary
    require_manifest_value dependency.qemu-img.x86_64 url "$QEMU_IMG_X86_64_URL"
    require_manifest_value helper.release tag "$HELPERS_RELEASE_TAG"
    require_manifest_value helper.release.corresponding-source url "$HELPERS_SOURCE_URL"
    require_manifest_value helper.release.build-info url "$HELPERS_BUILD_INFO_URL"

    require_manifest_value helper.passt version "$PASST_VERSION"
    require_manifest_value helper.passt commit "$PASST_COMMIT"
    require_manifest_value helper.passt architecture x86_64
    require_manifest_value helper.passt.source url "$PASST_SOURCE_URL"
    require_manifest_value helper.qemu-img version "$QEMU_IMG_VERSION"
    require_manifest_value helper.qemu-img commit "$QEMU_COMMIT"
    require_manifest_value helper.qemu-img architecture x86_64
    require_manifest_value helper.qemu-img signing_fingerprint "$QEMU_SIGNING_FINGERPRINT"
    require_manifest_value helper.qemu-img.source url "$QEMU_SOURCE_URL"
    require_manifest_value helper.qemu-img.signature url "$QEMU_SIGNATURE_URL"

    require_manifest_sha dependency.cloud-hypervisor.x86_64
    require_manifest_sha dependency.cloud-hypervisor.aarch64
    require_manifest_sha dependency.rust-hypervisor-firmware.x86_64
    require_manifest_sha dependency.rust-hypervisor-firmware.aarch64
    require_manifest_sha dependency.cloud-hypervisor-edk2.x86_64
    require_manifest_sha dependency.cloud-hypervisor-edk2.aarch64
    require_manifest_sha dependency.virtiofsd.x86_64
    require_manifest_sha dependency.virtiofsd.aarch64
    require_manifest_sha dependency.virtiofsd.source
    require_manifest_sha dependency.passt.x86_64
    require_manifest_sha dependency.qemu-img.x86_64
    require_manifest_sha helper.release.corresponding-source
    require_manifest_sha helper.release.build-info
    require_manifest_sha helper.passt.source
    require_manifest_sha helper.qemu-img.source
    require_manifest_sha helper.qemu-img.signature
}

verify_arch() {
    local selected_arch=$1

    case "$selected_arch" in
        x86_64)
            verify_artifact dependency.cloud-hypervisor.x86_64 cloud-hypervisor-x86_64 "$CLOUD_HYPERVISOR_X86_64_URL"
            verify_artifact dependency.rust-hypervisor-firmware.x86_64 rust-hypervisor-firmware-x86_64 "$RHF_X86_64_URL"
            verify_artifact dependency.cloud-hypervisor-edk2.x86_64 cloud-hypervisor-edk2-x86_64 "$EDK2_X86_64_URL"
            verify_artifact dependency.virtiofsd.x86_64 virtiofsd-x86_64 "$VIRTIOFSD_X86_64_URL"
            verify_artifact dependency.passt.x86_64 passt-x86_64 "$PASST_X86_64_URL"
            verify_artifact dependency.qemu-img.x86_64 qemu-img-x86_64 "$QEMU_IMG_X86_64_URL"
            ;;
        aarch64)
            verify_artifact dependency.cloud-hypervisor.aarch64 cloud-hypervisor-aarch64 "$CLOUD_HYPERVISOR_AARCH64_URL"
            verify_artifact dependency.rust-hypervisor-firmware.aarch64 rust-hypervisor-firmware-aarch64 "$RHF_AARCH64_URL"
            verify_artifact dependency.cloud-hypervisor-edk2.aarch64 cloud-hypervisor-edk2-aarch64 "$EDK2_AARCH64_URL"
            verify_artifact dependency.virtiofsd.aarch64 virtiofsd-aarch64 "$VIRTIOFSD_AARCH64_URL"
            ;;
        *)
            fail "internal error: verify_arch received '$selected_arch'"
            ;;
    esac
}

write_manifest() {
    local output=$1
    local ch_x86_sha=$2
    local ch_arm_sha=$3
    local rhf_x86_sha=$4
    local rhf_arm_sha=$5
    local edk2_x86_sha=$6
    local edk2_arm_sha=$7
    local virtiofsd_x86_sha=$8
    local virtiofsd_arm_sha=$9
    shift 9
    local virtiofsd_source_sha=$1
    local passt_source_sha=$2
    local qemu_source_sha=$3
    local qemu_signature_sha=$4
    local passt_x86_sha=$5
    local qemu_img_x86_sha=$6
    local helpers_source_sha=$7
    local helpers_build_info_sha=$8

    cat >"$output" <<EOF
# Generated by scripts/pin-deps.sh from exact release URLs.
# Do not edit checksums by hand. Run scripts/pin-deps.sh refresh.
manifest_version = 1

[dependency.cloud-hypervisor]
version = "$CLOUD_HYPERVISOR_VERSION"
release_url = "$CLOUD_HYPERVISOR_RELEASE_URL"
availability = "binary"

[dependency.cloud-hypervisor.x86_64]
asset = "cloud-hypervisor-static"
install_name = "cloud-hypervisor-$CLOUD_HYPERVISOR_VERSION"
url = "$CLOUD_HYPERVISOR_X86_64_URL"
sha256 = "$ch_x86_sha"

[dependency.cloud-hypervisor.aarch64]
asset = "cloud-hypervisor-static-aarch64"
install_name = "cloud-hypervisor-$CLOUD_HYPERVISOR_VERSION"
url = "$CLOUD_HYPERVISOR_AARCH64_URL"
sha256 = "$ch_arm_sha"

[dependency.rust-hypervisor-firmware]
version = "$RHF_VERSION"
release_url = "$RHF_RELEASE_URL"
availability = "binary"

[dependency.rust-hypervisor-firmware.x86_64]
asset = "hypervisor-fw"
install_name = "hypervisor-fw-$RHF_VERSION"
url = "$RHF_X86_64_URL"
sha256 = "$rhf_x86_sha"

[dependency.rust-hypervisor-firmware.aarch64]
asset = "hypervisor-fw-aarch64"
install_name = "hypervisor-fw-$RHF_VERSION"
url = "$RHF_AARCH64_URL"
sha256 = "$rhf_arm_sha"

[dependency.cloud-hypervisor-edk2]
version = "$EDK2_VERSION"
release_url = "$EDK2_RELEASE_URL"
availability = "binary"

[dependency.cloud-hypervisor-edk2.x86_64]
asset = "CLOUDHV.fd"
install_name = "CLOUDHV-$EDK2_VERSION.fd"
url = "$EDK2_X86_64_URL"
sha256 = "$edk2_x86_sha"

[dependency.cloud-hypervisor-edk2.aarch64]
asset = "CLOUDHV_EFI.fd"
install_name = "CLOUDHV_EFI-$EDK2_VERSION.fd"
url = "$EDK2_AARCH64_URL"
sha256 = "$edk2_arm_sha"

# Firestone-owned binaries reproduce upstream v1.14.0 for both release targets.
[dependency.virtiofsd]
version = "$VIRTIOFSD_VERSION"
commit = "$VIRTIOFSD_COMMIT"
release_url = "$VIRTIOFSD_RELEASE_URL"
availability = "binary"

[dependency.virtiofsd.x86_64]
asset = "virtiofsd-v1.14.0-x86_64-unknown-linux-musl"
install_name = "virtiofsd-$VIRTIOFSD_VERSION"
url = "$VIRTIOFSD_X86_64_URL"
sha256 = "$virtiofsd_x86_sha"

[dependency.virtiofsd.aarch64]
asset = "virtiofsd-v1.14.0-aarch64-unknown-linux-musl"
install_name = "virtiofsd-$VIRTIOFSD_VERSION"
url = "$VIRTIOFSD_AARCH64_URL"
sha256 = "$virtiofsd_arm_sha"

[dependency.virtiofsd.source]
asset = "virtiofsd-v1.14.0.tar.gz"
url = "$VIRTIOFSD_SOURCE_URL"
sha256 = "$virtiofsd_source_sha"

# Firestone-owned helpers are embedded only in the accepted x86_64 standalone release.
[dependency.passt]
version = "$PASST_VERSION"
commit = "$PASST_COMMIT"
release_url = "$HELPERS_RELEASE_URL"
availability = "binary"
architectures = ["x86_64"]
license = "GPL-2.0-or-later AND BSD-3-Clause"

[dependency.passt.x86_64]
asset = "passt-2025_02_17.a1e48a0-x86_64-unknown-linux-musl"
install_name = "passt-2025_02_17.a1e48a0"
url = "$PASST_X86_64_URL"
sha256 = "$passt_x86_sha"

[dependency.qemu-img]
version = "$QEMU_IMG_VERSION"
commit = "$QEMU_COMMIT"
release_url = "$HELPERS_RELEASE_URL"
availability = "binary"
architectures = ["x86_64"]
license = "GPL-2.0-or-later"

[dependency.qemu-img.x86_64]
asset = "qemu-img-8.2.2-x86_64-unknown-linux-musl"
install_name = "qemu-img-8.2.2"
url = "$QEMU_IMG_X86_64_URL"
sha256 = "$qemu_img_x86_sha"

[helper.release]
tag = "$HELPERS_RELEASE_TAG"

[helper.release.corresponding-source]
asset = "firestone-static-helpers-v0.1.0-corresponding-source.tar"
url = "$HELPERS_SOURCE_URL"
sha256 = "$helpers_source_sha"

[helper.release.build-info]
asset = "firestone-static-helpers-v0.1.0-build-info.txt"
url = "$HELPERS_BUILD_INFO_URL"
sha256 = "$helpers_build_info_sha"

# Exact upstream source pins consumed by the x86_64 static helper build.
[helper.passt]
version = "$PASST_VERSION"
commit = "$PASST_COMMIT"
architecture = "x86_64"
license = "GPL-2.0-or-later AND BSD-3-Clause"

[helper.passt.source]
asset = "passt-$PASST_COMMIT.tar.xz"
url = "$PASST_SOURCE_URL"
sha256 = "$passt_source_sha"

[helper.qemu-img]
version = "$QEMU_IMG_VERSION"
commit = "$QEMU_COMMIT"
architecture = "x86_64"
license = "GPL-2.0-or-later"
signing_fingerprint = "$QEMU_SIGNING_FINGERPRINT"

[helper.qemu-img.source]
asset = "qemu-$QEMU_IMG_VERSION.tar.xz"
url = "$QEMU_SOURCE_URL"
sha256 = "$qemu_source_sha"

[helper.qemu-img.signature]
asset = "qemu-$QEMU_IMG_VERSION.tar.xz.sig"
url = "$QEMU_SIGNATURE_URL"
sha256 = "$qemu_signature_sha"
EOF
}

verify_manifest_shape() {
    local expected_manifest="$temporary_dir/expected-deps.toml"

    write_manifest \
        "$expected_manifest" \
        "$(manifest_value dependency.cloud-hypervisor.x86_64 sha256)" \
        "$(manifest_value dependency.cloud-hypervisor.aarch64 sha256)" \
        "$(manifest_value dependency.rust-hypervisor-firmware.x86_64 sha256)" \
        "$(manifest_value dependency.rust-hypervisor-firmware.aarch64 sha256)" \
        "$(manifest_value dependency.cloud-hypervisor-edk2.x86_64 sha256)" \
        "$(manifest_value dependency.cloud-hypervisor-edk2.aarch64 sha256)" \
        "$(manifest_value dependency.virtiofsd.x86_64 sha256)" \
        "$(manifest_value dependency.virtiofsd.aarch64 sha256)" \
        "$(manifest_value dependency.virtiofsd.source sha256)" \
        "$(manifest_value helper.passt.source sha256)" \
        "$(manifest_value helper.qemu-img.source sha256)" \
        "$(manifest_value helper.qemu-img.signature sha256)" \
        "$(manifest_value dependency.passt.x86_64 sha256)" \
        "$(manifest_value dependency.qemu-img.x86_64 sha256)" \
        "$(manifest_value helper.release.corresponding-source sha256)" \
        "$(manifest_value helper.release.build-info sha256)"

    cmp -s "$manifest" "$expected_manifest" ||
        fail "$manifest is not in canonical generated form; run scripts/pin-deps.sh refresh --arch all --manifest '$manifest'"
}

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
command_name=verify
manifest="$repo_root/deps.toml"
requested_arch=""

if (($# > 0)) && [[ $1 != --* ]]; then
    command_name=$1
    shift
fi

while (($# > 0)); do
    case "$1" in
        --arch)
            (($# >= 2)) || fail "--arch requires a value"
            requested_arch=$2
            shift 2
            ;;
        --manifest)
            (($# >= 2)) || fail "--manifest requires a path"
            manifest=$2
            shift 2
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            usage >&2
            fail "unknown argument '$1'"
            ;;
    esac
done

case "$command_name" in
    verify | refresh) ;;
    -h | --help)
        usage
        exit 0
        ;;
    *)
        usage >&2
        fail "unknown command '$command_name'"
        ;;
esac

command -v curl >/dev/null 2>&1 || fail "curl is required"
temporary_dir=$(mktemp -d "${TMPDIR:-/tmp}/firestone-pin-deps.XXXXXX")
generated_manifest=""
cleanup() {
    rm -rf "$temporary_dir"
    if [[ -n "$generated_manifest" ]]; then
        rm -f "$generated_manifest"
    fi
}
trap cleanup EXIT

if [[ "$command_name" == refresh ]]; then
    if [[ -n "$requested_arch" ]] && [[ $(normalize_arch "$requested_arch") != all ]]; then
        fail "refresh requires --arch all because deps.toml contains both architectures"
    fi

    ch_x86_sha=$(download_hash cloud-hypervisor-x86_64 "$CLOUD_HYPERVISOR_X86_64_URL")
    ch_arm_sha=$(download_hash cloud-hypervisor-aarch64 "$CLOUD_HYPERVISOR_AARCH64_URL")
    rhf_x86_sha=$(download_hash rust-hypervisor-firmware-x86_64 "$RHF_X86_64_URL")
    rhf_arm_sha=$(download_hash rust-hypervisor-firmware-aarch64 "$RHF_AARCH64_URL")
    edk2_x86_sha=$(download_hash cloud-hypervisor-edk2-x86_64 "$EDK2_X86_64_URL")
    edk2_arm_sha=$(download_hash cloud-hypervisor-edk2-aarch64 "$EDK2_AARCH64_URL")
    virtiofsd_x86_sha=$(download_hash virtiofsd-x86_64 "$VIRTIOFSD_X86_64_URL")
    virtiofsd_arm_sha=$(download_hash virtiofsd-aarch64 "$VIRTIOFSD_AARCH64_URL")
    virtiofsd_source_sha=$(download_hash virtiofsd-source "$VIRTIOFSD_SOURCE_URL")
    passt_source_sha=$(download_hash passt-source "$PASST_SOURCE_URL")
    qemu_source_sha=$(download_hash qemu-source "$QEMU_SOURCE_URL")
    qemu_signature_sha=$(download_hash qemu-signature "$QEMU_SIGNATURE_URL")
    passt_x86_sha=$(download_hash passt-x86_64 "$PASST_X86_64_URL")
    qemu_img_x86_sha=$(download_hash qemu-img-x86_64 "$QEMU_IMG_X86_64_URL")
    helpers_source_sha=$(download_hash helpers-corresponding-source "$HELPERS_SOURCE_URL")
    helpers_build_info_sha=$(download_hash helpers-build-info "$HELPERS_BUILD_INFO_URL")

    mkdir -p "$(dirname "$manifest")"
    generated_manifest=$(mktemp "$(dirname "$manifest")/.deps.toml.XXXXXX")
    write_manifest \
        "$generated_manifest" \
        "$ch_x86_sha" \
        "$ch_arm_sha" \
        "$rhf_x86_sha" \
        "$rhf_arm_sha" \
        "$edk2_x86_sha" \
        "$edk2_arm_sha" \
        "$virtiofsd_x86_sha" \
        "$virtiofsd_arm_sha" \
        "$virtiofsd_source_sha" \
        "$passt_source_sha" \
        "$qemu_source_sha" \
        "$qemu_signature_sha" \
        "$passt_x86_sha" \
        "$qemu_img_x86_sha" \
        "$helpers_source_sha" \
        "$helpers_build_info_sha"
    mv "$generated_manifest" "$manifest"
    generated_manifest=""
    printf 'refreshed %s from exact pinned release URLs\n' "$manifest"
    exit 0
fi

[[ -f "$manifest" ]] || fail "manifest not found: $manifest"
verify_manifest_metadata
verify_manifest_shape

if [[ -z "$requested_arch" ]]; then
    requested_arch=$(uname -m)
fi
selected_arch=$(normalize_arch "$requested_arch")

if [[ "$selected_arch" == all ]]; then
    verify_arch x86_64
    verify_arch aarch64
else
    verify_arch "$selected_arch"
fi
verify_artifact dependency.virtiofsd.source virtiofsd-source "$VIRTIOFSD_SOURCE_URL"
verify_artifact helper.passt.source passt-source "$PASST_SOURCE_URL"
verify_artifact helper.qemu-img.source qemu-source "$QEMU_SOURCE_URL"
verify_artifact helper.qemu-img.signature qemu-signature "$QEMU_SIGNATURE_URL"
verify_artifact helper.release.corresponding-source helpers-corresponding-source "$HELPERS_SOURCE_URL"
verify_artifact helper.release.build-info helpers-build-info "$HELPERS_BUILD_INFO_URL"
printf 'verified %s for %s\n' "$manifest" "$selected_arch"
