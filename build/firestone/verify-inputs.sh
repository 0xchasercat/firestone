#!/bin/sh

set -eu

fail() {
    printf 'verify-release-inputs: %s\n' "$*" >&2
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

require_value() {
    label=$1
    actual=$2
    expected=$3
    [ "$actual" = "$expected" ] || fail "$label is '$actual', expected '$expected'"
}

require_sha256() {
    label=$1
    path=$2
    expected=$3
    [ -f "$path" ] || fail "$label is missing: $path"
    actual=$(sha256_file "$path")
    [ "$actual" = "$expected" ] || fail "$label checksum mismatch: expected $expected, got $actual"
}

[ "$#" -eq 2 ] || fail 'usage: verify-inputs.sh SOURCE_ROOT TARGET'
source_root=$1
target=$2
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
readonly source_root target script_dir

# shellcheck disable=SC1091 # The file is resolved next to this script.
. "$script_dir/versions.env"

case "$target" in
    x86_64-unknown-linux-musl | aarch64-unknown-linux-musl) ;;
    *) fail "unsupported release target '$target'" ;;
esac

for command_name in awk cargo gcc ld rustc; do
    command -v "$command_name" >/dev/null 2>&1 || fail "$command_name is required"
done

require_sha256 Cargo.lock "$source_root/Cargo.lock" "$CARGO_LOCK_SHA256"
require_sha256 deps.toml "$source_root/deps.toml" "$DEPS_TOML_SHA256"
workspace_version=$(awk '
    $0 == "[workspace.package]" { in_package = 1; next }
    in_package && /^\[/ { exit }
    in_package && /^[[:space:]]*version[[:space:]]*=/ {
        value = $0
        sub("^[[:space:]]*version[[:space:]]*=[[:space:]]*\"", "", value)
        sub("\"[[:space:]]*$", "", value)
        print value
        exit
    }
' "$source_root/Cargo.toml")
require_value 'Firestone workspace version' "$workspace_version" "$FIRESTONE_VERSION"

rustc_verbose=$(rustc --version --verbose)
rustc_release=$(printf '%s\n' "$rustc_verbose" | awk '/^release:/ {print $2}')
rustc_commit=$(printf '%s\n' "$rustc_verbose" | awk '/^commit-hash:/ {print $2}')
rustc_host=$(printf '%s\n' "$rustc_verbose" | awk '/^host:/ {print $2}')
require_value 'rustc version' "$rustc_release" "$RUST_VERSION"
require_value 'rustc commit' "$rustc_commit" "$RUSTC_COMMIT"
require_value 'rustc host' "$rustc_host" "$target"

cargo_verbose=$(cargo --version --verbose)
cargo_release=$(printf '%s\n' "$cargo_verbose" | awk '/^release:/ {print $2}')
cargo_commit=$(printf '%s\n' "$cargo_verbose" | awk '/^commit-hash:/ {print $2}')
require_value 'cargo version' "$cargo_release" "$RUST_VERSION"
require_value 'cargo commit' "$cargo_commit" "$CARGO_COMMIT"
require_value gcc "$(gcc -dumpfullversion -dumpversion)" "$GCC_VERSION"
require_value binutils "$(ld --version | awk 'NR == 1 {print $NF}')" "$BINUTILS_VERSION"

printf 'verified release inputs for %s\n' "$target"
