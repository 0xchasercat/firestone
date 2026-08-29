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
        dynamic_libc=/lib/ld-musl-x86_64.so.1
        musl_headers_arch=x86_64
        musl_headers_sha=$MUSL_HEADERS_X86_64_SHA256
        ;;
    aarch64-unknown-linux-musl)
        expected_machine='AArch64'
        dynamic_libc=/lib/ld-musl-aarch64.so.1
        musl_headers_arch=aarch64
        musl_headers_sha=$MUSL_HEADERS_AARCH64_SHA256
        ;;
    *)
        fail "unsupported release target '$target'"
        ;;
esac
readonly expected_machine dynamic_libc musl_headers_arch musl_headers_sha

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
musl_headers_apk="$work_root/inputs/musl-dev.apk"
[ -f "$musl_headers_apk" ] || fail "pinned musl headers are missing: $musl_headers_apk"
actual_headers_sha=$(sha256sum "$musl_headers_apk" | awk '{print $1}')
[ "$actual_headers_sha" = "$musl_headers_sha" ] ||
    fail "musl headers checksum mismatch: expected $musl_headers_sha, got $actual_headers_sha"
musl_headers_root="$work_root/musl-headers"
mkdir -p "$musl_headers_root"
tar -xzf "$musl_headers_apk" -C "$musl_headers_root"
musl_package_name=$(awk -F' = ' '/^pkgname = / {print $2}' "$musl_headers_root/.PKGINFO")
musl_package_version=$(awk -F' = ' '/^pkgver = / {print $2}' "$musl_headers_root/.PKGINFO")
musl_package_arch=$(awk -F' = ' '/^arch = / {print $2}' "$musl_headers_root/.PKGINFO")
[ "$musl_package_name" = musl-dev ] || fail "headers package is '$musl_package_name', expected 'musl-dev'"
[ "$musl_package_version" = "$MUSL_HEADERS_VERSION" ] ||
    fail "musl headers version is '$musl_package_version', expected '$MUSL_HEADERS_VERSION'"
[ "$musl_package_arch" = "$musl_headers_arch" ] ||
    fail "musl headers architecture is '$musl_package_arch', expected '$musl_headers_arch'"
for header in assert.h stdint.h; do
    [ -f "$musl_headers_root/usr/include/$header" ] || fail "musl headers package is missing $header"
done
readonly musl_headers_apk musl_headers_root actual_headers_sha

rust_sysroot=$(rustc --print sysroot)
self_contained_lib="$rust_sysroot/lib/rustlib/$target/lib/self-contained"
for crt_file in crti.o crtn.o libc.a; do
    [ -f "$self_contained_lib/$crt_file" ] ||
        fail "pinned Rust target is missing self-contained $crt_file"
done
[ -f "$dynamic_libc" ] || fail "pinned image is missing $dynamic_libc"
host_link_dir="$work_root/host-link"
mkdir -p "$host_link_dir" "$work_root/cargo-home" "$work_root/home" "$work_root/target"
ln -s "$self_contained_lib/crti.o" "$host_link_dir/crti.o"
ln -s "$self_contained_lib/crtn.o" "$host_link_dir/crtn.o"
ln -s "$dynamic_libc" "$host_link_dir/libc.so"
readonly rust_sysroot self_contained_lib host_link_dir
export CARGO_HOME="$work_root/cargo-home"
export C_INCLUDE_PATH="$musl_headers_root/usr/include"
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
export LIBRARY_PATH="$host_link_dir"
export LC_ALL=C.UTF-8
export TZ=UTC
export RUSTFLAGS="--remap-path-prefix=$source_root=/firestone-source --remap-path-prefix=$work_root=/firestone-build --remap-path-prefix=/usr/local/cargo=/rust-cargo --remap-path-prefix=/usr/local/rustup=/rust-toolchain -C target-feature=+crt-static -C link-self-contained=yes -C link-arg=-Wl,--build-id=none"

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
