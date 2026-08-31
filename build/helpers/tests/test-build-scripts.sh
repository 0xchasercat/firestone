#!/usr/bin/env bash

set -euo pipefail

fail() {
    printf 'test-build-scripts: %s\n' "$*" >&2
    exit 1
}

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
repository_root=$(cd -- "$script_dir/../../.." && pwd -P)
recipe_root="$repository_root/build/helpers"
temporary_dir=$(mktemp -d "${TMPDIR:-/tmp}/firestone-helper-tests.XXXXXX")
cleanup() {
    rm -rf "$temporary_dir"
}
trap cleanup EXIT HUP INT TERM

bash -n "$repository_root/scripts/build-helpers.sh" \
    "$repository_root/scripts/fetch-helper-inputs.sh" \
    "$repository_root/scripts/pin-deps.sh"
sh -n "$recipe_root/build-in-container.sh"

# shellcheck disable=SC1091 # This test checks the committed pin file.
source "$recipe_root/versions.env"
[[ $(sha256_file "$recipe_root/packages.lock") == "$PACKAGES_LOCK_SHA256" ]] ||
    fail 'production packages.lock checksum is stale'
[[ $(sha256_file "$recipe_root/sources.lock") == "$SOURCES_LOCK_SHA256" ]] ||
    fail 'production sources.lock checksum is stale'
[[ $(sha256_file "$recipe_root/$QEMU_SIGNING_KEY_ASSET") == "$QEMU_SIGNING_KEY_SHA256" ]] ||
    fail 'production QEMU key checksum is stale'

check_lock() {
    local lock=$1 expected_count=$2
    local hash name url extra count=0 names_file
    names_file=$(mktemp "$temporary_dir/lock-names.XXXXXX")

    while read -r hash name url extra; do
        [[ -n ${hash:-} ]] || continue
        [[ $hash != \#* ]] || continue
        [[ $hash =~ ^[0-9a-f]{64}$ ]] || fail "$lock contains an invalid hash"
        [[ $name =~ ^[A-Za-z0-9][A-Za-z0-9._+-]*$ ]] || fail "$lock contains an unsafe name"
        [[ $url == https://* ]] || fail "$lock contains a non-HTTPS URL"
        [[ -z ${extra:-} ]] || fail "$lock contains extra fields"
        ! grep -Fx "$name" "$names_file" >/dev/null || fail "$lock repeats $name"
        echo "$name" >>"$names_file"
        count=$((count + 1))
    done <"$lock"
    [[ $count -eq $expected_count ]] || fail "$lock has $count entries, expected $expected_count"
}

check_lock "$recipe_root/packages.lock" 91
check_lock "$recipe_root/sources.lock" 16
grep -F "$PASST_SOURCE_SHA256  $PASST_SOURCE_ASSET  https://github.com/0xchasercat/firestone/releases/download/helpers-v0.1.0-firestone.1/" "$recipe_root/sources.lock" >/dev/null
grep -F "$QEMU_SOURCE_SHA256  $QEMU_SOURCE_ASSET  https://download.qemu.org/" "$recipe_root/sources.lock" >/dev/null
if grep -R -E '(^|[[:space:]])git (clone|checkout|fetch)' \
    "$repository_root/scripts/build-helpers.sh" \
    "$repository_root/scripts/fetch-helper-inputs.sh" \
    "$recipe_root/build-in-container.sh"; then
    fail 'helper build must not fetch source through git'
fi

test_root="$temporary_dir/fixture"
fixture_recipe="$test_root/recipe"
downloads="$test_root/downloads"
mock_bin="$test_root/bin"
mkdir -p "$fixture_recipe" "$downloads" "$mock_bin"
printf 'package bytes\n' >"$downloads/package.apk"
printf 'passt source\n' >"$downloads/passt.tar.xz"
printf 'qemu source\n' >"$downloads/qemu.tar.xz"
printf 'qemu signature\n' >"$downloads/qemu.tar.xz.sig"
printf 'test key\n' >"$fixture_recipe/qemu-key.asc"

package_sha=$(sha256_file "$downloads/package.apk")
passt_sha=$(sha256_file "$downloads/passt.tar.xz")
qemu_sha=$(sha256_file "$downloads/qemu.tar.xz")
signature_sha=$(sha256_file "$downloads/qemu.tar.xz.sig")
key_sha=$(sha256_file "$fixture_recipe/qemu-key.asc")
printf '%s  package.apk  https://example.invalid/package.apk\n' "$package_sha" >"$fixture_recipe/packages.lock"
cat >"$fixture_recipe/sources.lock" <<EOF
$passt_sha  passt.tar.xz  https://example.invalid/passt.tar.xz
$qemu_sha  qemu.tar.xz  https://example.invalid/qemu.tar.xz
$signature_sha  qemu.tar.xz.sig  https://example.invalid/qemu.tar.xz.sig
EOF
packages_lock_sha=$(sha256_file "$fixture_recipe/packages.lock")
sources_lock_sha=$(sha256_file "$fixture_recipe/sources.lock")
cat >"$fixture_recipe/versions.env" <<EOF
HELPER_ARCH=x86_64
BUILDER_IMAGE=example.invalid/builder@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
SOURCE_DATE_EPOCH=1
PASST_SOURCE_ASSET=passt.tar.xz
PASST_SOURCE_SHA256=$passt_sha
QEMU_SOURCE_ASSET=qemu.tar.xz
QEMU_SOURCE_SHA256=$qemu_sha
QEMU_SIGNATURE_ASSET=qemu.tar.xz.sig
QEMU_SIGNATURE_SHA256=$signature_sha
QEMU_SIGNING_FINGERPRINT=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
QEMU_SIGNING_KEY_ASSET=qemu-key.asc
QEMU_SIGNING_KEY_SHA256=$key_sha
PACKAGES_LOCK_SHA256=$packages_lock_sha
SOURCES_LOCK_SHA256=$sources_lock_sha
EOF

cat >"$mock_bin/curl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
output=
url=
while (($# > 0)); do
    case "$1" in
        --output) output=$2; shift 2 ;;
        https://*) url=$1; shift ;;
        *) shift ;;
    esac
done
[[ -n $output && -n $url ]]
cp "$FIXTURE_DOWNLOADS/${url##*/}" "$output"
EOF

cat >"$mock_bin/gpg" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
for argument in "$@"; do
    if [[ $argument == --with-colons ]]; then
        printf 'fpr:::::::::AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA:\n'
        exit 0
    fi
done
exit 0
EOF

cat >"$mock_bin/docker" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$FAKE_DOCKER_LOG"
case "${1:-}" in
    pull) exit 0 ;;
    info) printf '%s\n' x86_64; exit 0 ;;
    image)
        if [[ ${3:-} == --format ]]; then printf '%s\n' amd64; fi
        exit 0
        ;;
    run)
        count=0
        [[ ! -f $FAKE_DOCKER_STATE ]] || count=$(cat "$FAKE_DOCKER_STATE")
        count=$((count + 1))
        printf '%s\n' "$count" >"$FAKE_DOCKER_STATE"
        output=
        while (($# > 0)); do
            if [[ $1 == --mount ]]; then
                mount=$2
                case "$mount" in
                    *,dst=/output) output=$(printf '%s\n' "$mount" | sed -n 's/.*src=\([^,]*\),dst=\/output.*/\1/p') ;;
                esac
                shift 2
            else
                shift
            fi
        done
        [[ -n $output ]]
        mkdir -p "$output/LICENSES"
        printf 'passt\n' >"$output/passt"
        printf 'qemu\n' >"$output/qemu-img"
        if [[ ${FAKE_DOCKER_MISMATCH:-0} == 1 && $count -eq 2 ]]; then
            printf 'different\n' >>"$output/qemu-img"
        fi
        printf 'source\n' >"$output/firestone-static-helpers-corresponding-source.tar"
        printf 'info\n' >"$output/helpers.build-info"
        printf 'license\n' >"$output/LICENSES/COPYING"
        : >"$output/SHA256SUMS"
        chmod 0755 "$output/passt" "$output/qemu-img"
        chmod 0644 "$output/firestone-static-helpers-corresponding-source.tar" \
            "$output/helpers.build-info" "$output/SHA256SUMS" "$output/LICENSES/COPYING"
        exit 0
        ;;
    *) exit 0 ;;
esac
EOF

cat >"$mock_bin/uname" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "${FAKE_UNAME_ARCH:-x86_64}"
EOF

cat >"$mock_bin/stat" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ ${1:-} == -c && ${2:-} == %a ]]; then
    shift 2
    if [[ -x $1 ]]; then printf '%s\n' 755; else printf '%s\n' 644; fi
else
    exec /usr/bin/stat "$@"
fi
EOF
chmod 0755 "$mock_bin"/*

export FIXTURE_DOWNLOADS="$downloads"
export FAKE_DOCKER_LOG="$test_root/docker.log"
export FAKE_DOCKER_STATE="$test_root/docker.state"
PATH="$mock_bin:$PATH" \
FIRESTONE_HELPER_RECIPE_ROOT="$fixture_recipe" \
    "$repository_root/scripts/fetch-helper-inputs.sh" --output-dir "$test_root/input"
[[ -f $test_root/input/packages/package.apk ]]
[[ -f $test_root/input/sources/qemu.tar.xz ]]
grep -F 'pull --platform linux/amd64 example.invalid/builder@sha256:' "$FAKE_DOCKER_LOG" >/dev/null

: >"$FAKE_DOCKER_LOG"
rm -f "$FAKE_DOCKER_STATE"
PATH="$mock_bin:$PATH" \
FIRESTONE_HELPER_RECIPE_ROOT="$fixture_recipe" \
    "$repository_root/scripts/build-helpers.sh" \
        --input-dir "$test_root/input" \
        --output-dir "$test_root/output"
[[ $(grep -c '^run ' "$FAKE_DOCKER_LOG") -eq 2 ]]
[[ $(grep -c -- '--network none' "$FAKE_DOCKER_LOG") -eq 2 ]]
[[ $(grep -c -- '--pull never' "$FAKE_DOCKER_LOG") -eq 2 ]]
[[ -x $test_root/output/passt && -x $test_root/output/qemu-img ]]

: >"$FAKE_DOCKER_LOG"
rm -f "$FAKE_DOCKER_STATE"
if PATH="$mock_bin:$PATH" \
    FIRESTONE_HELPER_RECIPE_ROOT="$fixture_recipe" \
    FAKE_DOCKER_MISMATCH=1 \
    "$repository_root/scripts/build-helpers.sh" \
        --input-dir "$test_root/input" \
        --output-dir "$test_root/mismatch-output" >"$test_root/mismatch.stdout" 2>"$test_root/mismatch.stderr"; then
    fail 'mismatched double builds were accepted'
fi
grep -F 'double builds differ at qemu-img' "$test_root/mismatch.stderr" >/dev/null

if PATH="$mock_bin:$PATH" \
    FIRESTONE_HELPER_RECIPE_ROOT="$fixture_recipe" \
    FAKE_UNAME_ARCH=arm64 \
    "$repository_root/scripts/build-helpers.sh" \
        --input-dir "$test_root/input" \
        --output-dir "$test_root/arm-output" >"$test_root/arm.stdout" 2>"$test_root/arm.stderr"; then
    fail 'arm64 host was accepted'
fi
grep -F 'native x86_64 host required' "$test_root/arm.stderr" >/dev/null

printf 'helper build script tests passed\n'
