#!/bin/sh

set -eu

fail() {
    printf 'build-in-container: %s\n' "$*" >&2
    exit 1
}

sha256_file() {
    sha256sum "$1" | awk '{print $1}'
}

require_version() {
    label=$1
    actual=$2
    expected=$3
    [ "$actual" = "$expected" ] || fail "$label is '$actual', expected '$expected'"
}

verify_lock() {
    lock=$1
    directory=$2
    line_number=0

    while read -r expected filename url extra; do
        line_number=$((line_number + 1))
        [ -n "${expected:-}" ] || continue
        [ "${expected#\#}" = "$expected" ] || continue
        [ -z "${extra:-}" ] || fail "$lock:$line_number has extra fields"
        [ "${#expected}" -eq 64 ] || fail "$lock:$line_number has an invalid SHA-256 length"
        case "$expected" in
            *[!0-9a-f]*) fail "$lock:$line_number has an invalid SHA-256" ;;
        esac
        case "$filename" in
            '' | */* | .* | *[!A-Za-z0-9._+-]*) fail "$lock:$line_number has an unsafe file name" ;;
        esac
        case "$url" in https://*) ;; *) fail "$lock:$line_number URL is not HTTPS" ;; esac
        [ -f "$directory/$filename" ] || fail "missing $directory/$filename"
        actual=$(sha256_file "$directory/$filename")
        [ "$actual" = "$expected" ] ||
            fail "$filename checksum mismatch: expected $expected, got $actual"
    done <"$lock"
}

verify_elf() {
    binary=$1
    expected_type=$2
    label=$3
    header=/work/"$label".elf-header
    program_headers=/work/"$label".program-headers
    dynamic=/work/"$label".dynamic
    notes=/work/"$label".notes

    readelf -h "$binary" >"$header"
    readelf -l "$binary" >"$program_headers"
    readelf -d "$binary" >"$dynamic" 2>&1 || true
    readelf -n "$binary" >"$notes" 2>&1 || true
    grep -Eq 'Class:[[:space:]]+ELF64' "$header" || fail "$label is not ELF64"
    grep -Eq 'Data:[[:space:]]+2.s complement, little endian' "$header" || fail "$label is not little-endian"
    grep -Eq 'Machine:[[:space:]]+Advanced Micro Devices X86-64' "$header" || fail "$label is not x86_64"
    actual_type=$(awk -F: '/Type:/ { sub(/^[[:space:]]+/, "", $2); print $2; exit }' "$header")
    [ "$actual_type" = "$expected_type" ] || fail "$label has ELF type '$actual_type'"
    if grep -q 'INTERP' "$program_headers"; then
        fail "$label contains a PT_INTERP program header"
    fi
    if grep -q '(NEEDED)' "$dynamic"; then
        fail "$label contains a dynamic NEEDED entry"
    fi
    if grep -q 'Build ID:' "$notes"; then
        fail "$label contains a linker build ID"
    fi
}

extract_license() {
    archive=$1
    member=$2
    output=$3
    tar -xOf "$archive" "$member" >"$output" || fail "missing license $member"
    [ -s "$output" ] || fail "license $member is empty"
    chmod 0644 "$output"
}

[ "$(id -u)" -eq 0 ] || fail 'container build must run as root for offline APK installation'
[ -d /source/build/helpers ] || fail 'recipe is not mounted at /source'
[ -d /inputs/packages ] || fail 'package inputs are not mounted at /inputs/packages'
[ -d /inputs/sources ] || fail 'source inputs are not mounted at /inputs/sources'
[ -d /work ] || fail 'work directory is not mounted'
[ -d /output ] || fail 'output directory is not mounted'

recipe_dir=/source/build/helpers
provided_source_date_epoch=${SOURCE_DATE_EPOCH:-}
# shellcheck disable=SC1091 # The recipe is mounted at a fixed container path.
. "$recipe_dir/versions.env"
[ "$provided_source_date_epoch" = "$SOURCE_DATE_EPOCH" ] || fail 'SOURCE_DATE_EPOCH does not match versions.env'
[ "$HELPER_ARCH" = x86_64 ] || fail "unsupported helper architecture '$HELPER_ARCH'"
[ "$(uname -m)" = x86_64 ] || fail 'builder container is not native x86_64'
[ "$(sha256_file "$recipe_dir/packages.lock")" = "$PACKAGES_LOCK_SHA256" ] || fail 'packages.lock checksum mismatch'
[ "$(sha256_file "$recipe_dir/sources.lock")" = "$SOURCES_LOCK_SHA256" ] || fail 'sources.lock checksum mismatch'
[ "$(sha256_file "/inputs/$QEMU_SIGNING_KEY_ASSET")" = "$QEMU_SIGNING_KEY_SHA256" ] || fail 'QEMU signing key checksum mismatch'
verify_lock "$recipe_dir/packages.lock" /inputs/packages
verify_lock "$recipe_dir/sources.lock" /inputs/sources

apk add --no-network --repositories-file /dev/null /inputs/packages/*.apk >/dev/null
apk info -e "gcc=$GCC_PACKAGE_VERSION" || fail 'wrong GCC package version'
apk info -e "binutils=$BINUTILS_PACKAGE_VERSION" || fail 'wrong binutils package version'
apk info -e "musl=$MUSL_PACKAGE_VERSION" || fail 'wrong musl package version'
apk info -e "python3=$PYTHON_PACKAGE_VERSION" || fail 'wrong Python package version'
apk info -e "samurai=$SAMURAI_PACKAGE_VERSION" || fail 'wrong samurai package version'
apk info -e "glib-static=$GLIB_PACKAGE_VERSION" || fail 'wrong glib-static package version'
apk info -e "gettext-static=$GETTEXT_STATIC_PACKAGE_VERSION" || fail 'wrong gettext-static package version'
apk info -e "pcre2-static=$PCRE2_STATIC_PACKAGE_VERSION" || fail 'wrong pcre2-static package version'
apk info -e "zlib-static=$ZLIB_STATIC_PACKAGE_VERSION" || fail 'wrong zlib-static package version'
require_version gcc "$(gcc -dumpfullversion)" "$GCC_VERSION"
require_version binutils "$(ld --version | awk 'NR == 1 { print $NF }')" "$BINUTILS_VERSION"
require_version python "$(python3 -c 'import platform; print(platform.python_version())')" "$PYTHON_VERSION"
require_version samurai "$(samu --version)" "$SAMURAI_VERSION"
require_version glib "$(pkg-config --modversion glib-2.0)" "$GLIB_VERSION"

export ARFLAGS=crD
export LANG=C
export LC_ALL=C
export SOURCE_DATE_EPOCH
export TZ=UTC
export ZERO_AR_DATE=1
common_cflags="-O2 -g0 -fno-ident -ffile-prefix-map=/work=/usr/src/firestone-helpers -fdebug-prefix-map=/work=/usr/src/firestone-helpers"
common_ldflags='-Wl,--build-id=none -Wl,-z,relro -Wl,-z,now'

passt_source=/work/passt-source
qemu_source=/work/qemu-source
mkdir "$passt_source" "$qemu_source"
tar -xJf "/inputs/sources/$PASST_SOURCE_ASSET" --strip-components=1 -C "$passt_source"
tar -xJf "/inputs/sources/$QEMU_SOURCE_ASSET" --strip-components=1 -C "$qemu_source"
[ "$(cat "$qemu_source/VERSION")" = "$QEMU_IMG_VERSION" ] || fail 'QEMU archive VERSION mismatch'

printf '%s\n' '-D__builtin_cpu_supports(x)=0' >"$passt_source/firestone-baseline.rsp"
(
    cd "$passt_source"
    make -j1 passt \
        VERSION="$PASST_VERSION" \
        CFLAGS="$common_cflags" \
        CPPFLAGS=@firestone-baseline.rsp \
        LDFLAGS="-static $common_ldflags"
)
passt_binary="$passt_source/passt"

(
    cd "$qemu_source"
    CFLAGS="$common_cflags" LDFLAGS="$common_ldflags" \
        ./configure \
            --static \
            --without-default-features \
            --enable-tools \
            --disable-system \
            --disable-user \
            --disable-docs \
            --enable-stack-protector \
            --ninja=samu \
            --prefix=/usr > /work/qemu-configure.log
    samu -C build -v qemu-img > /work/qemu-build.log
)
qemu_binary="$qemu_source/build/qemu-img"
[ -f "$passt_binary" ] || fail 'passt build produced no binary'
[ -f "$qemu_binary" ] || fail 'QEMU build produced no qemu-img binary'
for linked_archive in libglib-2.0.a libintl.a libatomic.a pcre2-8 libz.a; do
    grep -F "$linked_archive" /work/qemu-build.log >/dev/null ||
        fail "qemu-img static link closure lacks $linked_archive"
done

strip --strip-all --remove-section=.comment "$passt_binary" "$qemu_binary"
chmod 0755 "$passt_binary" "$qemu_binary"
verify_elf "$passt_binary" 'EXEC (Executable file)' passt
verify_elf "$qemu_binary" 'DYN (Position-Independent Executable file)' qemu-img

passt_stderr=/work/passt-version.stderr
passt_version_output=$("$passt_binary" --version 2>"$passt_stderr")
[ ! -s "$passt_stderr" ] || fail 'passt --version wrote unexpected stderr'
require_version passt "$(printf '%s\n' "$passt_version_output" | sed -n '1p')" "passt $PASST_VERSION"
"$passt_binary" --help >/work/passt-help 2>/work/passt-help.stderr
[ ! -s /work/passt-help.stderr ] || fail 'passt --help wrote unexpected stderr'
for option in --foreground --one-off --vhost-user --socket --repair-path --log-file --tcp-ports --udp-ports; do
    grep -F -- "$option" /work/passt-help >/dev/null || fail "passt help lacks $option"
done

qemu_stderr=/work/qemu-version.stderr
qemu_version_output=$("$qemu_binary" --version 2>"$qemu_stderr")
[ ! -s "$qemu_stderr" ] || fail 'qemu-img --version wrote unexpected stderr'
require_version qemu-img "$(printf '%s\n' "$qemu_version_output" | sed -n '1p')" "qemu-img version $QEMU_IMG_VERSION"

smoke=/work/qemu-smoke
mkdir "$smoke"
dd if=/dev/zero of="$smoke/source.raw" bs=1 count=0 seek=1048576 2>/dev/null
"$qemu_binary" convert -f raw -O qcow2 "$smoke/source.raw" "$smoke/base.qcow2"
"$qemu_binary" info --output=json -f qcow2 "$smoke/base.qcow2" >"$smoke/base.json"
grep -Eq '"format"[[:space:]]*:[[:space:]]*"qcow2"' "$smoke/base.json" || fail 'qemu-img info did not report qcow2'
"$qemu_binary" create -f qcow2 -F qcow2 -b "$smoke/base.qcow2" "$smoke/overlay.qcow2" 2097152 >/dev/null
"$qemu_binary" info --output=json -f qcow2 "$smoke/overlay.qcow2" >"$smoke/overlay.json"
grep -F "\"backing-filename\": \"$smoke/base.qcow2\"" "$smoke/overlay.json" >/dev/null ||
    fail 'qemu-img overlay did not retain the exact backing path'

publish=/work/publish
licenses="$publish/LICENSES"
mkdir -p "$licenses/passt" "$licenses/qemu" "$licenses/glib" "$licenses/gettext" \
    "$licenses/gcc" "$licenses/pcre2" "$licenses/zlib" "$licenses/musl"
install -m 0755 "$passt_binary" "$publish/passt"
install -m 0755 "$qemu_binary" "$publish/qemu-img"
extract_license "/inputs/sources/$PASST_SOURCE_ASSET" \
    "passt-$PASST_COMMIT/LICENSES/GPL-2.0-or-later.txt" "$licenses/passt/GPL-2.0-or-later.txt"
extract_license "/inputs/sources/$PASST_SOURCE_ASSET" \
    "passt-$PASST_COMMIT/LICENSES/BSD-3-Clause.txt" "$licenses/passt/BSD-3-Clause.txt"
extract_license "/inputs/sources/$QEMU_SOURCE_ASSET" "qemu-$QEMU_IMG_VERSION/COPYING" "$licenses/qemu/COPYING"
extract_license "/inputs/sources/$QEMU_SOURCE_ASSET" "qemu-$QEMU_IMG_VERSION/COPYING.LIB" "$licenses/qemu/COPYING.LIB"
extract_license "/inputs/sources/$QEMU_SOURCE_ASSET" "qemu-$QEMU_IMG_VERSION/LICENSE" "$licenses/qemu/LICENSE"
extract_license /inputs/sources/glib-2.84.4.tar.xz glib-2.84.4/LICENSES/LGPL-2.1-or-later.txt "$licenses/glib/LGPL-2.1-or-later.txt"
extract_license /inputs/sources/gettext-0.24.1.tar.xz gettext-0.24.1/COPYING "$licenses/gettext/COPYING"
extract_license /inputs/sources/gettext-0.24.1.tar.xz gettext-0.24.1/gettext-runtime/intl/COPYING.LIB "$licenses/gettext/COPYING.LIB"
extract_license /inputs/sources/gcc-14.2.0.tar.xz gcc-14.2.0/COPYING "$licenses/gcc/COPYING"
extract_license /inputs/sources/gcc-14.2.0.tar.xz gcc-14.2.0/COPYING.LIB "$licenses/gcc/COPYING.LIB"
extract_license /inputs/sources/gcc-14.2.0.tar.xz gcc-14.2.0/COPYING.RUNTIME "$licenses/gcc/COPYING.RUNTIME"
extract_license /inputs/sources/pcre2-10.46.tar.bz2 pcre2-10.46/LICENCE.md "$licenses/pcre2/LICENCE.md"
extract_license /inputs/sources/zlib-1.3.2.tar.gz zlib-1.3.2/LICENSE "$licenses/zlib/LICENSE"
extract_license /inputs/sources/musl-1.2.5.tar.gz musl-1.2.5/COPYRIGHT "$licenses/musl/COPYRIGHT"

bundle_stage=/work/source-bundle/firestone-static-helpers-corresponding-source
mkdir -p "$bundle_stage/sources" "$bundle_stage/packages" "$bundle_stage/recipe/build/helpers" \
    "$bundle_stage/recipe/scripts" "$bundle_stage/recipe/.github/workflows"
cp /inputs/sources/* "$bundle_stage/sources/"
cp /inputs/packages/* "$bundle_stage/packages/"
cp "/inputs/$QEMU_SIGNING_KEY_ASSET" "$bundle_stage/"
cp "$recipe_dir/versions.env" "$recipe_dir/packages.lock" "$recipe_dir/sources.lock" \
    "$recipe_dir/build-in-container.sh" "$bundle_stage/recipe/build/helpers/"
cp /source/scripts/fetch-helper-inputs.sh /source/scripts/build-helpers.sh /source/scripts/pin-deps.sh \
    "$bundle_stage/recipe/scripts/"
cp /source/deps.toml "$bundle_stage/recipe/"
cp /source/.github/workflows/helpers.yml "$bundle_stage/recipe/.github/workflows/"
find "$bundle_stage" -type d -exec chmod 0755 {} +
find "$bundle_stage" -type f -exec chmod 0644 {} +
chmod 0755 "$bundle_stage/recipe/build/helpers/build-in-container.sh" \
    "$bundle_stage/recipe/scripts/fetch-helper-inputs.sh" \
    "$bundle_stage/recipe/scripts/build-helpers.sh" \
    "$bundle_stage/recipe/scripts/pin-deps.sh"
(
    cd "$bundle_stage"
    find . -type f ! -name SOURCE-MANIFEST.sha256 -print | LC_ALL=C sort > /work/source-manifest-files
    while IFS= read -r path; do sha256sum "$path"; done < /work/source-manifest-files >SOURCE-MANIFEST.sha256
)
chmod 0644 "$bundle_stage/SOURCE-MANIFEST.sha256"
source_bundle="$publish/firestone-static-helpers-corresponding-source.tar"
tar --sort=name --format=gnu --mtime="@$SOURCE_DATE_EPOCH" --owner=0 --group=0 --numeric-owner \
    -cf "$source_bundle" -C /work/source-bundle firestone-static-helpers-corresponding-source
chmod 0644 "$source_bundle"

passt_sha=$(sha256_file "$publish/passt")
qemu_sha=$(sha256_file "$publish/qemu-img")
source_bundle_sha=$(sha256_file "$source_bundle")
cat >"$publish/helpers.build-info" <<EOF
schema_version=1
architecture=x86_64
source_date_epoch=$SOURCE_DATE_EPOCH
builder_image=$BUILDER_IMAGE
builder_alpine_version=$BUILDER_ALPINE_VERSION
packages_lock_sha256=$PACKAGES_LOCK_SHA256
sources_lock_sha256=$SOURCES_LOCK_SHA256
passt_version=$PASST_VERSION
passt_commit=$PASST_COMMIT
passt_source_sha256=$PASST_SOURCE_SHA256
passt_baseline_dispatch=__builtin_cpu_supports(x)=0
passt_elf_type=EXEC
passt_pt_interp=absent
passt_dt_needed=absent
passt_build_id=absent
passt_sha256=$passt_sha
qemu_img_version=$QEMU_IMG_VERSION
qemu_commit=$QEMU_COMMIT
qemu_source_sha256=$QEMU_SOURCE_SHA256
qemu_signature_sha256=$QEMU_SIGNATURE_SHA256
qemu_signing_fingerprint=$QEMU_SIGNING_FINGERPRINT
qemu_configure=--static --without-default-features --enable-tools --disable-system --disable-user --disable-docs --enable-stack-protector --ninja=samu --prefix=/usr
qemu_static_archives=libglib-2.0.a,libintl.a,libatomic.a,libpcre2-8.a,libz.a,musl-libc,libgcc
qemu_img_elf_type=DYN-static-pie
qemu_img_pt_interp=absent
qemu_img_dt_needed=absent
qemu_img_build_id=absent
qemu_img_sha256=$qemu_sha
corresponding_source_sha256=$source_bundle_sha
gcc_version=$GCC_VERSION
binutils_version=$BINUTILS_VERSION
musl_version=$MUSL_VERSION
python_version=$PYTHON_VERSION
samurai_version=$SAMURAI_VERSION
glib_version=$GLIB_VERSION
EOF
chmod 0644 "$publish/helpers.build-info"

(
    cd "$publish"
    find . -type f ! -name SHA256SUMS -print | LC_ALL=C sort > /work/publish-files
    while IFS= read -r path; do sha256sum "${path#./}"; done < /work/publish-files >SHA256SUMS
)
chmod 0644 "$publish/SHA256SUMS"
cp -Rp "$publish/." /output/
printf 'built passt %s and qemu-img %s\n' "$passt_sha" "$qemu_sha"
