#!/usr/bin/env bash

set -euo pipefail

fail() {
    printf 'test-build-release: %s\n' "$*" >&2
    exit 1
}

cleanup() {
    [[ -z ${temporary_dir:-} || ! -d $temporary_dir || $temporary_dir == / ]] || rm -rf -- "$temporary_dir"
}

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
repository_root=$(cd -- "$script_dir/../.." && pwd -P)
verifier="$repository_root/build/firestone/verify-inputs.sh"
temporary_dir=$(mktemp -d "${TMPDIR:-/tmp}/firestone-release-test.XXXXXX")
trap cleanup EXIT
source_root="$temporary_dir/source"
fake_bin="$temporary_dir/bin"
mkdir -p "$source_root" "$fake_bin"
cp "$repository_root/Cargo.toml" "$source_root/Cargo.toml"
cp "$repository_root/Cargo.lock" "$source_root/Cargo.lock"
cp "$repository_root/deps.toml" "$source_root/deps.toml"

cat >"$fake_bin/rustc" <<'EOF'
#!/bin/sh
cat <<'OUTPUT'
rustc 1.85.0 (4d91de4e4 2025-02-17)
binary: rustc
commit-hash: 4d91de4e48198da2e33413efdcd9cd2cc0c46688
commit-date: 2025-02-17
host: x86_64-unknown-linux-musl
release: 1.85.0
LLVM version: 19.1.7
OUTPUT
EOF
cat >"$fake_bin/cargo" <<'EOF'
#!/bin/sh
cat <<'OUTPUT'
cargo 1.85.0 (d73d2caf9 2024-12-31)
release: 1.85.0
commit-hash: d73d2caf9e41a39daf2a8d6ce60ec80bf354d2a7
commit-date: 2024-12-31
host: x86_64-unknown-linux-musl
OUTPUT
EOF
cat >"$fake_bin/gcc" <<'EOF'
#!/bin/sh
printf '14.2.0\n'
EOF
cat >"$fake_bin/ld" <<'EOF'
#!/bin/sh
printf 'GNU ld (GNU Binutils) 2.43.1\n'
EOF
chmod 0755 "$fake_bin/rustc" "$fake_bin/cargo" "$fake_bin/gcc" "$fake_bin/ld"

PATH="$fake_bin:$PATH" "$verifier" "$source_root" x86_64-unknown-linux-musl >/dev/null

printf '\nchanged\n' >>"$source_root/Cargo.lock"
if output=$(PATH="$fake_bin:$PATH" "$verifier" "$source_root" x86_64-unknown-linux-musl 2>&1); then
    fail 'changed Cargo.lock unexpectedly passed verification'
fi
case "$output" in
    *'Cargo.lock checksum mismatch'*) ;;
    *) fail "checksum failure lacked context: $output" ;;
esac
cp "$repository_root/Cargo.lock" "$source_root/Cargo.lock"

cat >"$fake_bin/rustc" <<'EOF'
#!/bin/sh
cat <<'OUTPUT'
rustc 1.84.0 (unknown)
binary: rustc
commit-hash: 0000000000000000000000000000000000000000
host: x86_64-unknown-linux-musl
release: 1.84.0
OUTPUT
EOF
chmod 0755 "$fake_bin/rustc"
if output=$(PATH="$fake_bin:$PATH" "$verifier" "$source_root" x86_64-unknown-linux-musl 2>&1); then
    fail 'wrong Rust toolchain unexpectedly passed verification'
fi
case "$output" in
    *"rustc version is '1.84.0', expected '1.85.0'"*) ;;
    *) fail "toolchain failure lacked context: $output" ;;
esac

printf 'release input negative cases passed\n'
