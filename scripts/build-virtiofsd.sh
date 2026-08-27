#!/usr/bin/env bash

set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/build-virtiofsd.sh --arch ARCH --output-dir DIR

Build the pinned virtiofsd v1.14.0 static executable in a pinned container.

Required arguments:
  --arch ARCH       x86_64 or aarch64
  --output-dir DIR  Existing or new directory outside the repository
EOF
}

fail() {
    printf 'build-virtiofsd: %s\n' "$*" >&2
    exit 1
}

download() {
    local url=$1
    local expected_sha=$2
    local output=$3
    local actual_sha

    [[ "$url" == https://* ]] || fail "download URL is not HTTPS: $url"
    printf 'download %s\n' "$url"
    curl \
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
    actual_sha=$(sha256sum "$output" | awk '{print $1}')
    [[ "$actual_sha" == "$expected_sha" ]] ||
        fail "$url checksum mismatch: expected $expected_sha, got $actual_sha"
}

cleanup() {
    if [[ -n "${build_work_dir:-}" && -d "$build_work_dir" && "$build_work_dir" != / ]]; then
        case "$(basename "$build_work_dir")" in
            firestone-virtiofsd-build.*)
                rm -rf -- "$build_work_dir"
                ;;
            *)
                printf 'build-virtiofsd: refusing to remove unexpected work directory %s\n' "$build_work_dir" >&2
                ;;
        esac
    fi
}

architecture=
requested_output_dir=
while [[ $# -gt 0 ]]; do
    case "$1" in
        --arch)
            [[ $# -ge 2 ]] || fail '--arch requires a value'
            architecture=$2
            shift 2
            ;;
        --output-dir)
            [[ $# -ge 2 ]] || fail '--output-dir requires a value'
            requested_output_dir=$2
            shift 2
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            fail "unknown argument '$1'"
            ;;
    esac
done

[[ "$architecture" == x86_64 || "$architecture" == aarch64 ]] ||
    fail '--arch must be x86_64 or aarch64'
[[ -n "$requested_output_dir" ]] || fail '--output-dir is required'

for command in curl docker sha256sum; do
    command -v "$command" >/dev/null 2>&1 || fail "$command is required"
done

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
readonly script_dir
repository_root=$(cd -- "$script_dir/.." && pwd -P)
readonly repository_root
readonly recipe_dir="$repository_root/build/virtiofsd"
readonly versions_file="$recipe_dir/versions.env"

# shellcheck disable=SC1090,SC1091 # Resolved from the repository root above.
source "$versions_file"

[[ "$RUST_BUILDER_IMAGE" =~ @sha256:[0-9a-f]{64}$ ]] || fail 'Rust builder image is not pinned by digest'
[[ "$RUST_CROSS_HOST_IMAGE" =~ @sha256:[0-9a-f]{64}$ ]] || fail 'cross-build Rust image is not pinned by digest'
[[ "$CROSS_BUILDER_IMAGE" =~ @sha256:[0-9a-f]{64}$ ]] || fail 'cross builder image is not pinned by digest'

mkdir -p -- "$requested_output_dir"
output_dir=$(cd -- "$requested_output_dir" && pwd -P)
readonly output_dir
[[ "$output_dir" != / ]] || fail 'output directory cannot be the filesystem root'
case "$output_dir/" in
    "$repository_root/"*)
        fail 'output directory must be outside the git worktree'
        ;;
esac

readonly temporary_base=${TMPDIR:-/tmp}
[[ -d "$temporary_base" ]] || fail "temporary directory base does not exist: $temporary_base"
build_work_dir=$(mktemp -d "$temporary_base/firestone-virtiofsd-build.XXXXXX")
readonly build_work_dir
[[ "$build_work_dir" != / ]] || fail 'temporary build directory resolved to the filesystem root'
trap cleanup EXIT

mkdir -p "$build_work_dir/inputs"
download "$VIRTIOFSD_SOURCE_URL" "$VIRTIOFSD_SOURCE_SHA256" "$build_work_dir/inputs/virtiofsd-source.tar.gz"

case "$architecture" in
    x86_64)
        target=x86_64-unknown-linux-musl
        libcap_ng_url=$LIBCAP_NG_X86_64_URL
        libcap_ng_sha=$LIBCAP_NG_X86_64_SHA256
        libseccomp_url=$LIBSECCOMP_X86_64_URL
        libseccomp_sha=$LIBSECCOMP_X86_64_SHA256
        ;;
    aarch64)
        target=aarch64-unknown-linux-musl
        libcap_ng_url=$LIBCAP_NG_AARCH64_URL
        libcap_ng_sha=$LIBCAP_NG_AARCH64_SHA256
        libseccomp_url=$LIBSECCOMP_AARCH64_URL
        libseccomp_sha=$LIBSECCOMP_AARCH64_SHA256
        ;;
esac
readonly target libcap_ng_url libcap_ng_sha libseccomp_url libseccomp_sha

download "$libcap_ng_url" "$libcap_ng_sha" "$build_work_dir/inputs/libcap-ng-static.apk"
download "$libseccomp_url" "$libseccomp_sha" "$build_work_dir/inputs/libseccomp-static.apk"

readonly docker_target="builder-$architecture"
readonly image_tag="firestone-virtiofsd-builder:${VIRTIOFSD_VERSION#v}-$architecture"
docker build \
    --pull \
    --build-arg "CROSS_BUILDER_IMAGE=$CROSS_BUILDER_IMAGE" \
    --build-arg "RUST_BUILDER_IMAGE=$RUST_BUILDER_IMAGE" \
    --build-arg "RUST_CROSS_HOST_IMAGE=$RUST_CROSS_HOST_IMAGE" \
    --build-arg "RUST_VERSION=$RUST_VERSION" \
    --file "$recipe_dir/Dockerfile" \
    --target "$docker_target" \
    --tag "$image_tag" \
    "$recipe_dir"

printf 'builder %s\n' "$image_tag"
docker run \
    --rm \
    --read-only \
    --cap-drop ALL \
    --security-opt no-new-privileges \
    --tmpfs /tmp:rw,nosuid,nodev,noexec,mode=1777 \
    --user "$(id -u):$(id -g)" \
    --env CARGO_HOME=/work/cargo-home \
    --env HOME=/work/home \
    --env RUSTUP_HOME=/usr/local/rustup \
    --mount "type=bind,src=$recipe_dir,dst=/recipe,readonly" \
    --mount "type=bind,src=$build_work_dir,dst=/work" \
    --mount "type=bind,src=$output_dir,dst=/output" \
    --entrypoint /bin/sh \
    "$image_tag" \
    /recipe/build-in-container.sh "$architecture"

readonly artifact="$output_dir/virtiofsd-${VIRTIOFSD_VERSION}-${target}"
[[ -x "$artifact" ]] || fail "expected executable was not produced: $artifact"
printf 'complete %s\n' "$artifact"
