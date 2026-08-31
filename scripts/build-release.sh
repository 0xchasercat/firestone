#!/usr/bin/env bash

set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/build-release.sh --target TARGET --output-dir DIR

Build one Firestone Linux musl binary with the pinned native container.

Targets:
  x86_64-unknown-linux-musl
  aarch64-unknown-linux-musl
EOF
}

fail() {
    printf 'build-release: %s\n' "$*" >&2
    exit 1
}
sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        fail 'sha256sum or shasum is required'
    fi
}

download() {
    local url=$1
    local expected_sha=$2
    local output=$3
    local actual_sha

    [[ $url == https://* ]] || fail "download URL is not HTTPS: $url"
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
    actual_sha=$(sha256_file "$output")
    [[ $actual_sha == "$expected_sha" ]] ||
        fail "$url checksum mismatch: expected $expected_sha, got $actual_sha"
}
manifest_value() {
    local section=$1
    local key=$2
    local manifest=$3

    awk -v wanted_section="[$section]" -v wanted_key="$key" '
        $0 == wanted_section { in_section = 1; next }
        in_section && /^\[/ { exit }
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


cleanup() {
    if [[ -n ${work_dir:-} && -d $work_dir && $work_dir != / ]]; then
        case "$(basename -- "$work_dir")" in
            firestone-release-build.*) rm -rf -- "$work_dir" ;;
            *) printf 'build-release: refusing to remove unexpected work directory %s\n' "$work_dir" >&2 ;;
        esac
    fi
}

target=
requested_output=
while [[ $# -gt 0 ]]; do
    case "$1" in
        --target)
            [[ $# -ge 2 ]] || fail '--target requires a value'
            target=$2
            shift 2
            ;;
        --output-dir)
            [[ $# -ge 2 ]] || fail '--output-dir requires a value'
            requested_output=$2
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

case "$target" in
    x86_64-unknown-linux-musl) target_arch=x86_64 ;;
    aarch64-unknown-linux-musl) target_arch=aarch64 ;;
    *) fail '--target must be x86_64-unknown-linux-musl or aarch64-unknown-linux-musl' ;;
esac
[[ -n $requested_output ]] || fail '--output-dir is required'

for command_name in curl docker git; do
    command -v "$command_name" >/dev/null 2>&1 || fail "$command_name is required"
done

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
repository_root=$(cd -- "$script_dir/.." && pwd -P)
recipe_root="$repository_root/build/firestone"
readonly script_dir repository_root recipe_root

# shellcheck disable=SC1091 # The file is resolved from the repository root above.
source "$recipe_root/versions.env"
[[ $RUST_IMAGE =~ @sha256:[0-9a-f]{64}$ ]] || fail 'Rust image is not pinned by digest'
case "$target_arch" in
    x86_64)
        musl_headers_url=$MUSL_HEADERS_X86_64_URL
        musl_headers_sha=$MUSL_HEADERS_X86_64_SHA256
        ;;
    aarch64)
        musl_headers_url=$MUSL_HEADERS_AARCH64_URL
        musl_headers_sha=$MUSL_HEADERS_AARCH64_SHA256
        ;;
esac
[[ $musl_headers_url == https://* ]] || fail 'musl headers URL must be HTTPS'
[[ $musl_headers_sha =~ ^[0-9a-f]{64}$ ]] || fail 'musl headers checksum must be lowercase SHA-256'
readonly musl_headers_url musl_headers_sha
deps_manifest="$repository_root/deps.toml"
helper_docker_env=()
if [[ $target_arch == x86_64 ]]; then
    passt_asset=$(manifest_value dependency.passt.x86_64 asset "$deps_manifest")
    passt_url=$(manifest_value dependency.passt.x86_64 url "$deps_manifest")
    passt_sha=$(manifest_value dependency.passt.x86_64 sha256 "$deps_manifest")
    qemu_img_asset=$(manifest_value dependency.qemu-img.x86_64 asset "$deps_manifest")
    qemu_img_url=$(manifest_value dependency.qemu-img.x86_64 url "$deps_manifest")
    qemu_img_sha=$(manifest_value dependency.qemu-img.x86_64 sha256 "$deps_manifest")
    for value in "$passt_asset" "$qemu_img_asset"; do
        [[ -n $value && $value != */* ]] || fail "invalid embedded helper asset name '$value'"
    done
    for value in "$passt_sha" "$qemu_img_sha"; do
        [[ $value =~ ^[0-9a-f]{64}$ ]] || fail "invalid embedded helper checksum '$value'"
    done
    helper_docker_env=(
        --env FIRESTONE_EMBEDDED_HELPERS_DIR=/work/inputs
        --env FIRESTONE_REQUIRE_EMBEDDED_HELPERS=1
    )
fi

host_arch=$(uname -m)
case "$host_arch" in
    x86_64 | amd64) host_arch=x86_64 ;;
    aarch64 | arm64) host_arch=aarch64 ;;
    *) fail "unsupported build host architecture '$host_arch'" ;;
esac
[[ $host_arch == "$target_arch" ]] ||
    fail "$target requires a native $target_arch host; refusing emulated release output on $host_arch"

[[ -z $(git -C "$repository_root" status --porcelain --untracked-files=all) ]] ||
    fail 'git worktree must be clean so the embedded revision identifies every source byte'
git_commit=$(git -C "$repository_root" rev-parse --verify HEAD)
source_date_epoch=$(git -C "$repository_root" show -s --format=%ct HEAD)
[[ $git_commit =~ ^[0-9a-f]{40}$ ]] || fail "git returned invalid revision '$git_commit'"
[[ $source_date_epoch =~ ^[0-9]+$ ]] || fail "git returned invalid commit timestamp '$source_date_epoch'"
readonly git_commit source_date_epoch

mkdir -p -- "$requested_output"
output_dir=$(cd -- "$requested_output" && pwd -P)
readonly output_dir
[[ $output_dir != / ]] || fail 'output directory cannot be the filesystem root'
case "$output_dir/" in
    "$repository_root/"*) fail 'output directory must be outside the git worktree' ;;
esac
shopt -s nullglob dotglob
output_entries=("$output_dir"/*)
shopt -u nullglob dotglob
[[ ${#output_entries[@]} -eq 0 ]] || fail 'output directory must be empty'

work_dir=$(mktemp -d "${TMPDIR:-/tmp}/firestone-release-build.XXXXXX")
readonly work_dir
trap cleanup EXIT
mkdir -p "$work_dir/cargo-home" "$work_dir/home" "$work_dir/inputs" "$work_dir/target"
download "$musl_headers_url" "$musl_headers_sha" "$work_dir/inputs/musl-dev.apk"
if [[ $target_arch == x86_64 ]]; then
    download "$passt_url" "$passt_sha" "$work_dir/inputs/$passt_asset"
    download "$qemu_img_url" "$qemu_img_sha" "$work_dir/inputs/$qemu_img_asset"
fi

docker pull "$RUST_IMAGE"
docker run \
    --rm \
    --read-only \
    --cap-drop ALL \
    --security-opt no-new-privileges \
    --tmpfs /tmp:rw,nosuid,nodev,noexec,mode=1777 \
    --user "$(id -u):$(id -g)" \
    --env CARGO_HOME=/work/cargo-home \
    --env CARGO_INCREMENTAL=0 \
    --env FIRESTONE_GIT_COMMIT="$git_commit" \
    --env HOME=/work/home \
    --env SOURCE_DATE_EPOCH="$source_date_epoch" \
    "${helper_docker_env[@]}" \
    --mount "type=bind,src=$repository_root,dst=/source,readonly" \
    --mount "type=bind,src=$work_dir,dst=/work" \
    --mount "type=bind,src=$output_dir,dst=/output" \
    --workdir /source \
    "$RUST_IMAGE" \
    /source/build/firestone/build-in-container.sh "$target"
