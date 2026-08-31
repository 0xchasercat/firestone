#!/usr/bin/env bash

set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/fetch-helper-inputs.sh --output-dir DIR

Downloads the exact helper sources and Alpine package closure, verifies every
SHA-256 and the QEMU release signature, and pulls the pinned amd64 builder
image. The output directory must be empty.
EOF
}

fail() {
    printf 'fetch-helper-inputs: %s\n' "$*" >&2
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
    local output=$2

    "$curl_command" \
        --fail \
        --location \
        --proto '=https' \
        --proto-redir '=https' \
        --silent \
        --show-error \
        --retry 5 \
        --retry-all-errors \
        --connect-timeout 20 \
        --output "$output" \
        "$url"
}

fetch_lock() {
    local lock=$1
    local destination=$2
    local expected filename url extra actual line_number=0

    while read -r expected filename url extra; do
        line_number=$((line_number + 1))
        [[ -n ${expected:-} ]] || continue
        [[ $expected != \#* ]] || continue
        [[ -z ${extra:-} ]] || fail "$lock:$line_number has extra fields"
        [[ $expected =~ ^[0-9a-f]{64}$ ]] || fail "$lock:$line_number has an invalid SHA-256"
        [[ $filename =~ ^[A-Za-z0-9][A-Za-z0-9._+-]*$ ]] ||
            fail "$lock:$line_number has an unsafe file name '$filename'"
        [[ $url == https://* ]] || fail "$lock:$line_number URL is not HTTPS"
        [[ ! -e $destination/$filename ]] || fail "$lock repeats '$filename'"

        printf 'download %s\n' "$url"
        download "$url" "$destination/$filename"
        actual=$(sha256_file "$destination/$filename")
        [[ $actual == "$expected" ]] ||
            fail "$filename checksum mismatch: expected $expected, got $actual"
        chmod 0644 "$destination/$filename"
    done <"$lock"
}

requested_output=
while (($# > 0)); do
    case "$1" in
        --output-dir)
            (($# >= 2)) || fail '--output-dir requires a value'
            requested_output=$2
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
[[ -n $requested_output ]] || fail '--output-dir is required'

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
repository_root=$(cd -- "$script_dir/.." && pwd -P)
recipe_root=${FIRESTONE_HELPER_RECIPE_ROOT:-"$repository_root/build/helpers"}
curl_command=${FIRESTONE_HELPER_CURL:-curl}
docker_command=${FIRESTONE_HELPER_DOCKER:-docker}
gpg_command=${FIRESTONE_HELPER_GPG:-gpg}
readonly script_dir repository_root recipe_root curl_command docker_command gpg_command

for command_name in "$curl_command" "$docker_command" "$gpg_command"; do
    command -v "$command_name" >/dev/null 2>&1 || fail "$command_name is required"
done
[[ -f $recipe_root/versions.env ]] || fail "missing $recipe_root/versions.env"
[[ -f $recipe_root/packages.lock ]] || fail "missing $recipe_root/packages.lock"
[[ -f $recipe_root/sources.lock ]] || fail "missing $recipe_root/sources.lock"

# shellcheck disable=SC1091 # The recipe root is selected above.
source "$recipe_root/versions.env"
[[ $BUILDER_IMAGE =~ @sha256:[0-9a-f]{64}$ ]] || fail 'builder image is not pinned by digest'
[[ $HELPER_ARCH == x86_64 ]] || fail "unsupported helper architecture '$HELPER_ARCH'"
[[ $QEMU_SIGNING_FINGERPRINT =~ ^[0-9A-F]{40}$ ]] || fail 'invalid QEMU signing fingerprint'

packages_lock_sha=$(sha256_file "$recipe_root/packages.lock")
sources_lock_sha=$(sha256_file "$recipe_root/sources.lock")
key_sha=$(sha256_file "$recipe_root/$QEMU_SIGNING_KEY_ASSET")
[[ $packages_lock_sha == "$PACKAGES_LOCK_SHA256" ]] || fail 'packages.lock checksum does not match versions.env'
[[ $sources_lock_sha == "$SOURCES_LOCK_SHA256" ]] || fail 'sources.lock checksum does not match versions.env'
[[ $key_sha == "$QEMU_SIGNING_KEY_SHA256" ]] || fail 'QEMU signing key checksum does not match versions.env'

mkdir -p -- "$requested_output"
output_dir=$(cd -- "$requested_output" && pwd -P)
readonly output_dir
shopt -s nullglob dotglob
output_entries=("$output_dir"/*)
shopt -u nullglob dotglob
[[ ${#output_entries[@]} -eq 0 ]] || fail 'output directory must be empty'

temporary_dir=$(mktemp -d "${TMPDIR:-/tmp}/firestone-helper-inputs.XXXXXX")
cleanup() {
    rm -rf "$temporary_dir"
}
trap cleanup EXIT HUP INT TERM

staging_dir="$temporary_dir/input"
mkdir -m 0755 "$staging_dir" "$staging_dir/packages" "$staging_dir/sources"
fetch_lock "$recipe_root/packages.lock" "$staging_dir/packages"
fetch_lock "$recipe_root/sources.lock" "$staging_dir/sources"
install -m 0644 "$recipe_root/$QEMU_SIGNING_KEY_ASSET" "$staging_dir/$QEMU_SIGNING_KEY_ASSET"

[[ $(sha256_file "$staging_dir/sources/$PASST_SOURCE_ASSET") == "$PASST_SOURCE_SHA256" ]] ||
    fail 'passt source lock and versions.env disagree'
[[ $(sha256_file "$staging_dir/sources/$QEMU_SOURCE_ASSET") == "$QEMU_SOURCE_SHA256" ]] ||
    fail 'QEMU source lock and versions.env disagree'
[[ $(sha256_file "$staging_dir/sources/$QEMU_SIGNATURE_ASSET") == "$QEMU_SIGNATURE_SHA256" ]] ||
    fail 'QEMU signature lock and versions.env disagree'

gnupg_home="$temporary_dir/gnupg"
mkdir -m 0700 "$gnupg_home"
"$gpg_command" --batch --no-options --homedir "$gnupg_home" \
    --import "$staging_dir/$QEMU_SIGNING_KEY_ASSET" >/dev/null 2>&1
actual_fingerprint=$("$gpg_command" --batch --no-options --homedir "$gnupg_home" \
    --with-colons --fingerprint | awk -F: '$1 == "fpr" { print $10; exit }')
[[ $actual_fingerprint == "$QEMU_SIGNING_FINGERPRINT" ]] ||
    fail "QEMU signing key fingerprint is '$actual_fingerprint'"
"$gpg_command" --batch --no-options --no-auto-key-retrieve --homedir "$gnupg_home" \
    --verify "$staging_dir/sources/$QEMU_SIGNATURE_ASSET" \
    "$staging_dir/sources/$QEMU_SOURCE_ASSET"

"$docker_command" pull --platform linux/amd64 "$BUILDER_IMAGE"
cp -Rp "$staging_dir/." "$output_dir/"
printf 'verified helper inputs in %s\n' "$output_dir"
