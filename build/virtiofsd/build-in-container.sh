#!/bin/sh

set -eu

fail() {
    printf 'build-in-container: %s\n' "$*" >&2
    exit 1
}

require_sha256() {
    path=$1
    expected=$2

    actual=$(sha256sum "$path" | awk '{print $1}')
    [ "$actual" = "$expected" ] || fail "$path checksum mismatch: expected $expected, got $actual"
}

require_version() {
    label=$1
    actual=$2
    expected=$3

    [ "$actual" = "$expected" ] || fail "$label is '$actual', expected '$expected'"
}

[ "$#" -eq 1 ] || fail 'expected one architecture argument'
architecture=$1
recipe_dir=/recipe
work_dir=/work
output_dir=/output
readonly architecture recipe_dir work_dir output_dir

# shellcheck disable=SC1091 # The recipe is mounted at a fixed container path.
. "$recipe_dir/versions.env"

case "$architecture" in
    x86_64)
        target=x86_64-unknown-linux-musl
        libcap_ng_sha=$LIBCAP_NG_X86_64_SHA256
        libseccomp_sha=$LIBSECCOMP_X86_64_SHA256
        strip_command='strip'
        expected_machine='Advanced Micro Devices X86-64'
        linker='gcc'
        ;;
    aarch64)
        target=aarch64-unknown-linux-musl
        libcap_ng_sha=$LIBCAP_NG_AARCH64_SHA256
        libseccomp_sha=$LIBSECCOMP_AARCH64_SHA256
        strip_command='aarch64-linux-musl-strip'
        expected_machine=AArch64
        linker='aarch64-linux-musl-gcc.sh'
        ;;
    *)
        fail "unsupported architecture '$architecture'"
        ;;
esac
readonly target libcap_ng_sha libseccomp_sha strip_command expected_machine linker

source_archive="$work_dir/inputs/virtiofsd-source.tar.gz"
libcap_ng_archive="$work_dir/inputs/libcap-ng-static.apk"
libseccomp_archive="$work_dir/inputs/libseccomp-static.apk"
source_dir="$work_dir/source"
sysroot_dir="$work_dir/sysroot"
artifact_name="virtiofsd-${VIRTIOFSD_VERSION}-${target}"
artifact_path="$output_dir/$artifact_name"
readonly source_archive libcap_ng_archive libseccomp_archive source_dir sysroot_dir artifact_name artifact_path

require_sha256 "$source_archive" "$VIRTIOFSD_SOURCE_SHA256"
require_sha256 "$libcap_ng_archive" "$libcap_ng_sha"
require_sha256 "$libseccomp_archive" "$libseccomp_sha"

require_version rustc "$(rustc --version | awk '{print $2}')" "$RUST_VERSION"
require_version rustc-commit "$(rustc --version --verbose | awk '/^commit-hash:/ {print $2}')" "$RUSTC_COMMIT"
require_version cargo-commit "$(cargo --version --verbose | awk '/^commit-hash:/ {print $2}')" "$CARGO_COMMIT"

if [ "$architecture" = x86_64 ]; then
    require_version gcc "$(gcc -dumpfullversion -dumpversion)" "$X86_64_GCC_VERSION"
    require_version binutils "$(ld --version | awk 'NR == 1 {print $NF}')" "$X86_64_BINUTILS_VERSION"
    require_version musl "$(/lib/ld-musl-x86_64.so.1 2>&1 | awk '/Version/ {print $2}')" "$X86_64_MUSL_VERSION"
else
    require_version cross-gcc "$(aarch64-linux-musl-gcc -dumpfullversion -dumpversion)" "$AARCH64_GCC_VERSION"
    require_version cross-binutils "$(aarch64-linux-musl-ld --version | awk 'NR == 1 {print $NF}')" "$AARCH64_BINUTILS_VERSION"
    require_sha256 /usr/local/bin/aarch64-linux-musl-gcc "$AARCH64_GCC_SHA256"
    require_sha256 /usr/bin/aarch64-linux-musl-gcc.sh "$AARCH64_GCC_WRAPPER_SHA256"
    require_sha256 /usr/local/aarch64-linux-musl/lib/libc.a "$AARCH64_LIBC_A_SHA256"
    require_version cross-musl "$(/usr/local/bin/qemu-aarch64 /usr/local/aarch64-linux-musl/lib/libc.so 2>&1 | awk '/Version/ {print $2}')" "$AARCH64_MUSL_VERSION"
fi

mkdir -p "$source_dir" "$sysroot_dir" "$work_dir/cargo-home" "$work_dir/home" "$work_dir/target"
tar -xzf "$source_archive" --strip-components=1 -C "$source_dir"
tar -xzf "$libcap_ng_archive" -C "$sysroot_dir"
tar -xzf "$libseccomp_archive" -C "$sysroot_dir"

require_sha256 "$source_dir/Cargo.lock" "$VIRTIOFSD_CARGO_LOCK_SHA256"
libcap_ng_package_version=$(tar -xOzf "$libcap_ng_archive" .PKGINFO | awk -F' = ' '/^pkgver = / {print $2}')
libseccomp_package_version=$(tar -xOzf "$libseccomp_archive" .PKGINFO | awk -F' = ' '/^pkgver = / {print $2}')
require_version libcap-ng-package "$libcap_ng_package_version" "$LIBCAP_NG_VERSION"
require_version libseccomp-package "$libseccomp_package_version" "$LIBSECCOMP_VERSION"

export AR_aarch64_unknown_linux_musl=aarch64-linux-musl-ar
export CARGO_HOME="$work_dir/cargo-home"
export CARGO_INCREMENTAL=0
export CARGO_NET_GIT_FETCH_WITH_CLI=false
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1
export CARGO_PROFILE_RELEASE_DEBUG=0
export CARGO_PROFILE_RELEASE_LTO=true
export CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-musl-gcc.sh
export CARGO_TARGET_DIR="$work_dir/target"
export CC_aarch64_unknown_linux_musl=aarch64-linux-musl-gcc
export HOME="$work_dir/home"
export LANG=C.UTF-8
export LC_ALL=C.UTF-8
export LIBCAPNG_LIB_PATH="$sysroot_dir/usr/lib"
export LIBCAPNG_LINK_TYPE=static
export LIBSECCOMP_LIB_PATH="$sysroot_dir/usr/lib"
export LIBSECCOMP_LINK_TYPE=static
export RUSTUP_TOOLCHAIN="$RUST_VERSION"
export RUSTUP_UPDATE_ROOT=https://static.rust-lang.org/rustup
export SOURCE_DATE_EPOCH
export TZ=UTC
export RUSTFLAGS="-C target-feature=+crt-static -C link-self-contained=yes -C debuginfo=0 -C codegen-units=1 -C metadata=firestone-virtiofsd-v1.14.0 -C link-arg=-Wl,--build-id=none --remap-path-prefix=$work_dir=/firestone-build --remap-path-prefix=/usr/local/cargo=/rust-cargo --remap-path-prefix=/usr/local/rustup=/rust-toolchain"

printf 'building %s for %s with Rust %s\n' "$VIRTIOFSD_VERSION" "$target" "$RUST_VERSION"
cargo build --locked --release --target "$target" --manifest-path "$source_dir/Cargo.toml"

readonly built_binary="$work_dir/target/$target/release/virtiofsd"
[ -f "$built_binary" ] || fail "cargo did not produce $built_binary"
"$strip_command" --strip-all "$built_binary"
chmod 0755 "$built_binary"

elf_machine=$(readelf -h "$built_binary" | awk -F: '/Machine:/ {sub(/^[[:space:]]+/, "", $2); print $2}')
readonly elf_machine
[ "$elf_machine" = "$expected_machine" ] ||
    fail "ELF machine is '$elf_machine', expected '$expected_machine'"
if readelf -l "$built_binary" | grep -q 'INTERP'; then
    fail 'ELF contains a PT_INTERP program header'
fi
if readelf -d "$built_binary" 2>/dev/null | grep -q '(NEEDED)'; then
    fail 'ELF contains a dynamic NEEDED entry'
fi

mode=$(stat -c '%a' "$built_binary")
readonly mode
[ "$mode" = 755 ] || fail "binary mode is $mode, expected 755"

version_output=not-run-cross-architecture
if [ "$architecture" = x86_64 ]; then
    version_output=$($built_binary --version)
    [ "$version_output" = 'virtiofsd 1.14.0' ] ||
        fail "virtiofsd --version returned '$version_output'"
fi

readonly temporary_artifact="$output_dir/.${artifact_name}.tmp.$$"
cp "$built_binary" "$temporary_artifact"
chmod 0755 "$temporary_artifact"
mv "$temporary_artifact" "$artifact_path"

artifact_sha=$(sha256sum "$artifact_path" | awk '{print $1}')
readonly artifact_sha
printf '%s  %s\n' "$artifact_sha" "$artifact_name" >"$artifact_path.sha256"
cat >"$artifact_path.build-info" <<EOF
virtiofsd_version=$VIRTIOFSD_VERSION
virtiofsd_commit=$VIRTIOFSD_COMMIT
source_url=$VIRTIOFSD_SOURCE_URL
source_sha256=$VIRTIOFSD_SOURCE_SHA256
cargo_lock_sha256=$VIRTIOFSD_CARGO_LOCK_SHA256
source_date_epoch=$SOURCE_DATE_EPOCH
target=$target
rust_version=$RUST_VERSION
rustc_commit=$RUSTC_COMMIT
cargo_commit=$CARGO_COMMIT
rust_builder_image=$RUST_BUILDER_IMAGE
rust_cross_host_image=$RUST_CROSS_HOST_IMAGE
cross_builder_image=$CROSS_BUILDER_IMAGE
alpine_package_release=$ALPINE_PACKAGE_RELEASE
libcap_ng_version=$LIBCAP_NG_VERSION
libseccomp_version=$LIBSECCOMP_VERSION
linker=$linker
elf_machine=$elf_machine
pt_interp=absent
dynamic_needed=absent
mode=$mode
version_output=$version_output
sha256=$artifact_sha
EOF

printf 'artifact %s\n' "$artifact_path"
printf 'sha256 %s\n' "$artifact_sha"
