#!/bin/sh

set -eu

fail() {
    printf 'build-firestone-release: %s\n' "$*" >&2
    exit 1
}

[ "$#" -eq 1 ] || fail 'expected one target argument'
target=$1
source_root=/source
work_root=/work
output_root=/output
recipe_root="$source_root/build/firestone"
readonly target source_root work_root output_root recipe_root

# shellcheck disable=SC1091 # The recipe is mounted at a fixed path.
. "$recipe_root/versions.env"

case "$target" in
    x86_64-unknown-linux-musl)
        expected_machine='Advanced Micro Devices X86-64'
        ;;
    aarch64-unknown-linux-musl)
        expected_machine='AArch64'
        ;;
    *)
        fail "unsupported release target '$target'"
        ;;
esac
readonly expected_machine

export RUSTUP_TOOLCHAIN="$RUST_VERSION"
"$recipe_root/verify-inputs.sh" "$source_root" "$target"

[ -n "${FIRESTONE_GIT_COMMIT:-}" ] || fail 'FIRESTONE_GIT_COMMIT is required'
[ "${#FIRESTONE_GIT_COMMIT}" -eq 40 ] || fail 'FIRESTONE_GIT_COMMIT must be full 40-hex'
case "$FIRESTONE_GIT_COMMIT" in
    *[!0-9a-f]*) fail 'FIRESTONE_GIT_COMMIT must be lowercase hexadecimal' ;;
esac
case "${SOURCE_DATE_EPOCH:-}" in
    '' | *[!0-9]*) fail 'SOURCE_DATE_EPOCH must be an unsigned integer' ;;
esac

mkdir -p "$work_root/cargo-home" "$work_root/home" "$work_root/target"
export CARGO_HOME="$work_root/cargo-home"
export CARGO_INCREMENTAL=0
export CARGO_NET_GIT_FETCH_WITH_CLI=false
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1
export CARGO_PROFILE_RELEASE_DEBUG=0
export CARGO_PROFILE_RELEASE_LTO=fat
export CARGO_PROFILE_RELEASE_STRIP=symbols
export CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse
export CARGO_TARGET_DIR="$work_root/target"
export HOME="$work_root/home"
export LANG=C.UTF-8
export LC_ALL=C.UTF-8
export TZ=UTC
export RUSTFLAGS="--remap-path-prefix=$source_root=/firestone-source --remap-path-prefix=$work_root=/firestone-build --remap-path-prefix=/usr/local/cargo=/rust-cargo --remap-path-prefix=/usr/local/rustup=/rust-toolchain -C link-arg=-Wl,--build-id=none"

printf 'building firestone v%s for %s\n' "$FIRESTONE_VERSION" "$target"
cargo build \
    --locked \
    --release \
    --target "$target" \
    --manifest-path "$source_root/Cargo.toml" \
    --package firestone

built_binary="$work_root/target/$target/release/firestone"
[ -f "$built_binary" ] || fail "cargo did not produce $built_binary"
chmod 0755 "$built_binary"

elf_machine=$(readelf -h "$built_binary" | awk -F: '/Machine:/ {sub(/^[[:space:]]+/, "", $2); print $2}')
[ "$elf_machine" = "$expected_machine" ] ||
    fail "ELF machine is '$elf_machine', expected '$expected_machine'"
if readelf -l "$built_binary" | grep -q 'INTERP'; then
    fail 'ELF contains a PT_INTERP program header'
fi
if readelf -d "$built_binary" 2>/dev/null | grep -q '(NEEDED)'; then
    fail 'ELF contains a dynamic NEEDED entry'
fi
if readelf -S "$built_binary" | grep -q '\.symtab'; then
    fail 'ELF symbol table was not stripped'
fi

artifact="firestone-v${FIRESTONE_VERSION}-${target}"
artifact_sha=$(sha256sum "$built_binary" | awk '{print $1}')
readonly artifact artifact_sha
install -m 0755 "$built_binary" "$output_root/$artifact"
printf '%s  %s\n' "$artifact_sha" "$artifact" >"$output_root/SHA256SUMS"
chmod 0644 "$output_root/SHA256SUMS"
printf 'complete %s sha256 %s\n' "$output_root/$artifact" "$artifact_sha"
