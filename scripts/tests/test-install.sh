#!/usr/bin/env bash
# Exercises install.sh without network: the platform gate, release-tag parsing,
# checksum verification, the install directory rules, and PATH detection.

set -euo pipefail

fail() {
    printf 'test-install: %s\n' "$*" >&2
    exit 1
}

cleanup() {
    [[ -z ${temporary_dir:-} || ! -d $temporary_dir || $temporary_dir == / ]] || rm -rf -- "$temporary_dir"
}

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
repository_root=$(cd -- "$script_dir/../.." && pwd -P)
installer="$repository_root/install.sh"
[[ -f $installer ]] || fail "installer not found at $installer"

temporary_dir=$(mktemp -d "${TMPDIR:-/tmp}/firestone-install-test.XXXXXX")
trap cleanup EXIT

fake_bin="$temporary_dir/bin"
fixture="$temporary_dir/fixture"
home="$temporary_dir/home"
mkdir -p "$fake_bin" "$fixture" "$home"

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

# install.sh requires sha256sum; supply a delegating shim where it is absent.
if command -v sha256sum >/dev/null 2>&1; then
    sha256sum_path=$(command -v sha256sum)
else
    shasum_path=$(command -v shasum) || fail 'neither sha256sum nor shasum is available'
    cat >"$fake_bin/sha256sum" <<EOF
#!/bin/sh
exec "$shasum_path" -a 256 "\$@"
EOF
    chmod 0755 "$fake_bin/sha256sum"
    sha256sum_path="$fake_bin/sha256sum"
fi

cat >"$fake_bin/uname" <<'EOF'
#!/bin/sh
case "${1:-}" in
    -s) printf '%s\n' "${FAKE_UNAME_S:-Linux}" ;;
    -m) printf '%s\n' "${FAKE_UNAME_M:-x86_64}" ;;
    *) printf '%s\n' "${FAKE_UNAME_S:-Linux}" ;;
esac
EOF

# Serves the release fixture and records every requested URL. Any URL without a
# fixture file fails the way a real download of a missing asset fails.
cat >"$fake_bin/curl" <<'EOF'
#!/bin/sh
destination=
url=
while [ $# -gt 0 ]; do
    case $1 in
        -o) destination=$2; shift 2 ;;
        -* | =*) shift ;;
        *) url=$1; shift ;;
    esac
done
printf '%s\n' "$url" >>"$FIXTURE_DIR/requests.log"
case $url in
    *"/releases/latest") source_file="$FIXTURE_DIR/release.json" ;;
    *"/SHA256SUMS") source_file="$FIXTURE_DIR/SHA256SUMS" ;;
    *"-x86_64-unknown-linux-musl") source_file="$FIXTURE_DIR/artifact" ;;
    *) exit 22 ;;
esac
[ -f "$source_file" ] || exit 22
cp -- "$source_file" "$destination"
EOF

cat >"$fake_bin/wget" <<'EOF'
#!/bin/sh
destination=
url=
while [ $# -gt 0 ]; do
    case $1 in
        -O) destination=$2; shift 2 ;;
        -* | =*) shift ;;
        *) url=$1; shift ;;
    esac
done
printf '%s\n' "$url" >>"$FIXTURE_DIR/requests.log"
case $url in
    *"/releases/latest") source_file="$FIXTURE_DIR/release.json" ;;
    *"/SHA256SUMS") source_file="$FIXTURE_DIR/SHA256SUMS" ;;
    *"-x86_64-unknown-linux-musl") source_file="$FIXTURE_DIR/artifact" ;;
    *) exit 8 ;;
esac
[ -f "$source_file" ] || exit 8
cp -- "$source_file" "$destination"
EOF

chmod 0755 "$fake_bin/uname" "$fake_bin/curl" "$fake_bin/wget"

cat >"$fixture/artifact" <<'EOF'
#!/bin/sh
echo 'firestone 9.9.9'
EOF

# The real endpoint answers with a pretty-printed document whose tag_name sits
# among many other fields; a proxy may collapse it onto one line.
write_release_json() {
    cat >"$fixture/release.json" <<EOF
{
  "url": "https://api.github.com/repos/0xchasercat/firestone/releases/380656971",
  "html_url": "https://github.com/0xchasercat/firestone/releases/tag/v9.9.9",
  "id": 380656971,
  "tag_name": "$1",
  "target_commitish": "main",
  "name": "$1",
  "draft": false,
  "prerelease": false
}
EOF
}

write_release_json_one_line() {
    printf '{"url":"https://api.github.com/x","id":1,"tag_name":"%s","name":"%s","draft":false}\n' \
        "$1" "$1" >"$fixture/release.json"
}

write_checksums() {
    printf '%s  firestone-%s-x86_64-unknown-linux-musl\n' \
        "$(sha256_of "$fixture/artifact")" "$1" >"$fixture/SHA256SUMS"
}

reset_fixture() {
    rm -f "$fixture/requests.log" "$fixture/release.json" "$fixture/SHA256SUMS"
    : >"$fixture/requests.log"
}

status=0
output=
# run_installer <install-dir> [NAME=VALUE ...] — runs install.sh with the fake
# tools first on PATH and captures its combined output and exit status.
run_installer() {
    local install_dir=$1
    shift
    set +e
    output=$(
        env \
            PATH="$fake_bin:$PATH" \
            HOME="$home" \
            FIXTURE_DIR="$fixture" \
            FIRESTONE_INSTALL_DIR="$install_dir" \
            "$@" \
            sh "$installer" 2>&1
    )
    status=$?
    set -e
}

expect_output() {
    case "$output" in
        *"$1"*) ;;
        *) fail "expected output to contain '$1', got: $output" ;;
    esac
}

refuse_output() {
    case "$output" in
        *"$1"*) fail "output unexpectedly contained '$1': $output" ;;
        *) ;;
    esac
}

# 1. macOS is refused before anything is downloaded.
reset_fixture
run_installer "$temporary_dir/never" FAKE_UNAME_S=Darwin
[[ $status -ne 0 ]] || fail 'macOS host unexpectedly passed the platform gate'
expect_output 'this host runs macOS'
[[ ! -s $fixture/requests.log ]] || fail 'macOS host made a request'

# 2. A non-Linux, non-macOS host is refused by name.
reset_fixture
run_installer "$temporary_dir/never" FAKE_UNAME_S=FreeBSD
[[ $status -ne 0 ]] || fail 'FreeBSD host unexpectedly passed the platform gate'
expect_output 'this host runs FreeBSD'

# 3. aarch64 is refused with the reason it is refused.
reset_fixture
run_installer "$temporary_dir/never" FAKE_UNAME_M=aarch64
[[ $status -ne 0 ]] || fail 'aarch64 host unexpectedly passed the platform gate'
expect_output 'no aarch64 runtime release yet'
[[ ! -s $fixture/requests.log ]] || fail 'aarch64 host made a request'

# 4. An unknown architecture is refused by name.
reset_fixture
run_installer "$temporary_dir/never" FAKE_UNAME_M=riscv64
[[ $status -ne 0 ]] || fail 'riscv64 host unexpectedly passed the platform gate'
expect_output 'this host is riscv64'

# 5. The latest release resolves through the API, verifies, and installs.
reset_fixture
write_release_json v9.9.9
write_checksums v9.9.9
install_dir="$temporary_dir/case5/bin"
run_installer "$install_dir"
[[ $status -eq 0 ]] || fail "latest-release install failed: $output"
expect_output 'Installed firestone 9.9.9 at '
expect_output 'Checksum verified.'
grep -q 'releases/latest$' "$fixture/requests.log" || fail 'the API endpoint was not requested'
grep -q 'download/v9.9.9/firestone-v9.9.9-x86_64-unknown-linux-musl$' "$fixture/requests.log" ||
    fail 'the release asset for the resolved tag was not requested'
[[ -f $install_dir/firestone ]] || fail 'the binary was not installed'
mode=$(ls -l "$install_dir/firestone" | cut -c1-10)
[[ $mode == '-rwxr-xr-x' ]] || fail "installed mode is $mode, expected -rwxr-xr-x"
[[ -z $(find "$install_dir" -name '.firestone.install.*' -print -quit) ]] ||
    fail 'a staging file was left behind'

# 6. An install directory outside PATH prints the exact export line.
expect_output "$install_dir is not on PATH"
expect_output "export PATH=\"$install_dir:\$PATH\""
expect_output "Until then, run it as $install_dir/firestone."

# 7. An install directory on PATH prints the version and no export line.
reset_fixture
write_release_json v9.9.9
write_checksums v9.9.9
install_dir="$temporary_dir/case7/bin"
mkdir -p "$install_dir"
set +e
output=$(
    env \
        PATH="$fake_bin:$install_dir:$PATH" \
        HOME="$home" \
        FIXTURE_DIR="$fixture" \
        FIRESTONE_INSTALL_DIR="$install_dir" \
        sh "$installer" 2>&1
)
status=$?
set -e
[[ $status -eq 0 ]] || fail "on-PATH install failed: $output"
expect_output 'Installed firestone 9.9.9 at '
refuse_output 'is not on PATH'
refuse_output 'export PATH='

# 8. A one-line API document parses to the same tag.
reset_fixture
write_release_json_one_line v9.9.9
write_checksums v9.9.9
install_dir="$temporary_dir/case8/bin"
run_installer "$install_dir"
[[ $status -eq 0 ]] || fail "one-line API document failed to parse: $output"
grep -q 'download/v9.9.9/firestone-v9.9.9-x86_64-unknown-linux-musl$' "$fixture/requests.log" ||
    fail 'the one-line document resolved the wrong tag'

# 9. An API document without a tag stops with the pin hint.
reset_fixture
printf '{"message":"Not Found"}\n' >"$fixture/release.json"
run_installer "$temporary_dir/case9/bin"
[[ $status -ne 0 ]] || fail 'a tagless API document unexpectedly installed something'
expect_output 'without a release tag'
expect_output 'FIRESTONE_VERSION=vX.Y.Z'

# 10. FIRESTONE_VERSION pins the tag and skips the API entirely.
reset_fixture
write_checksums v0.1.4
install_dir="$temporary_dir/case10/bin"
run_installer "$install_dir" FIRESTONE_VERSION=v0.1.4
[[ $status -eq 0 ]] || fail "pinned install failed: $output"
expect_output 'pinned by FIRESTONE_VERSION'
if grep -q 'api.github.com' "$fixture/requests.log"; then
    fail 'a pinned install still called the releases API'
fi
grep -q 'download/v0.1.4/firestone-v0.1.4-x86_64-unknown-linux-musl$' "$fixture/requests.log" ||
    fail 'the pinned tag was not the tag downloaded'
[[ -f $install_dir/firestone ]] || fail 'the pinned binary was not installed'

# 11. A malformed FIRESTONE_VERSION is refused before any download.
reset_fixture
run_installer "$temporary_dir/case11/bin" FIRESTONE_VERSION='0.1.4; rm -rf /'
[[ $status -ne 0 ]] || fail 'a malformed pinned tag unexpectedly passed validation'
expect_output 'does not look like v0.1.4'
[[ ! -s $fixture/requests.log ]] || fail 'a malformed pinned tag still downloaded something'

# 12. A checksum mismatch stops the install and leaves nothing behind.
reset_fixture
write_release_json v9.9.9
printf '%s  firestone-v9.9.9-x86_64-unknown-linux-musl\n' \
    "0000000000000000000000000000000000000000000000000000000000000000" >"$fixture/SHA256SUMS"
install_dir="$temporary_dir/case12/bin"
run_installer "$install_dir"
[[ $status -ne 0 ]] || fail 'a checksum mismatch unexpectedly installed the binary'
expect_output 'does not match its published SHA-256 checksum'
[[ ! -e $install_dir/firestone ]] || fail 'a mismatched download was installed anyway'

# 13. SHA256SUMS without a line for this artifact stops the install.
reset_fixture
write_release_json v9.9.9
printf '%s  some-other-file\n' "$(sha256_of "$fixture/artifact")" >"$fixture/SHA256SUMS"
install_dir="$temporary_dir/case13/bin"
run_installer "$install_dir"
[[ $status -ne 0 ]] || fail 'an unlisted artifact unexpectedly installed'
expect_output 'does not list firestone-v9.9.9-x86_64-unknown-linux-musl'
[[ ! -e $install_dir/firestone ]] || fail 'an unlisted artifact was installed anyway'

# 14. A missing release asset stops the install with the releases page.
reset_fixture
write_release_json v9.9.9
rm -f "$fixture/artifact"
run_installer "$temporary_dir/case14/bin"
[[ $status -ne 0 ]] || fail 'a missing release asset unexpectedly installed'
expect_output 'could not download firestone-v9.9.9-x86_64-unknown-linux-musl'
cat >"$fixture/artifact" <<'EOF'
#!/bin/sh
echo 'firestone 9.9.9'
EOF

# 15. An unwritable install directory stops without sudo.
if [[ $(id -u) -ne 0 ]]; then
    reset_fixture
    write_release_json v9.9.9
    write_checksums v9.9.9
    install_dir="$temporary_dir/case15/bin"
    mkdir -p "$install_dir"
    chmod 0500 "$install_dir"
    run_installer "$install_dir"
    chmod 0700 "$install_dir"
    [[ $status -ne 0 ]] || fail 'an unwritable install directory unexpectedly installed'
    expect_output 'is not writable by this user'
    expect_output 'never uses sudo'
    [[ ! -e $install_dir/firestone ]] || fail 'an unwritable directory received a binary'
fi

# 16. Without curl, the wget branch installs the same bytes. The sandbox PATH
# holds exactly the external commands install.sh is allowed to need.
reset_fixture
write_release_json v9.9.9
write_checksums v9.9.9
sandbox_bin="$temporary_dir/sandbox-bin"
mkdir -p "$sandbox_bin"
for tool in mktemp sed tr head grep mkdir cp mv chmod rm; do
    tool_path=$(command -v "$tool") || fail "$tool is not installed"
    ln -sf "$tool_path" "$sandbox_bin/$tool"
done
ln -sf "$sha256sum_path" "$sandbox_bin/sha256sum"
cp "$fake_bin/uname" "$sandbox_bin/uname"
cp "$fake_bin/wget" "$sandbox_bin/wget"
chmod 0755 "$sandbox_bin/uname" "$sandbox_bin/wget"
install_dir="$temporary_dir/case16/bin"
set +e
output=$(
    env -i \
        PATH="$sandbox_bin" \
        HOME="$home" \
        FIXTURE_DIR="$fixture" \
        FIRESTONE_INSTALL_DIR="$install_dir" \
        /bin/sh "$installer" 2>&1
)
status=$?
set -e
[[ $status -eq 0 ]] || fail "wget fallback install failed: $output"
expect_output 'Installed firestone 9.9.9 at '
[[ -f $install_dir/firestone ]] || fail 'the wget fallback installed nothing'
cmp -s "$fixture/artifact" "$install_dir/firestone" ||
    fail 'the wget fallback installed different bytes'

printf 'install.sh cases passed\n'
