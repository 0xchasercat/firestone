#!/usr/bin/env bash

set -euo pipefail

fail() {
    printf 'test-publish-output: %s\n' "$*" >&2
    exit 1
}

test_dir=$(mktemp -d /tmp/firestone-publish-output-test.XXXXXXXX)
readonly test_dir
cleanup() {
    case "$test_dir" in
        /tmp/firestone-publish-output-test.*)
            rm -rf -- "$test_dir"
            ;;
        *)
            printf 'test-publish-output: refusing to remove unexpected test path %s\n' "$test_dir" >&2
            ;;
    esac
}
trap cleanup EXIT

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
readonly script_dir
publish_script=$(cd -- "$script_dir/.." && pwd -P)/publish-output.sh
readonly publish_script
readonly artifact_name=virtiofsd-test-x86_64-unknown-linux-musl

mkdir -p "$test_dir/concurrent/output" "$test_dir/concurrent/a" "$test_dir/concurrent/b"
printf 'binary-a\n' >"$test_dir/concurrent/a/binary"
printf 'variant=a\n' >"$test_dir/concurrent/a/build-info"
printf 'binary-b\n' >"$test_dir/concurrent/b/binary"
printf 'variant=b\n' >"$test_dir/concurrent/b/build-info"

set +e
"$publish_script" \
    "$test_dir/concurrent/a/binary" \
    "$test_dir/concurrent/a/build-info" \
    "$test_dir/concurrent/output" \
    "$artifact_name" >"$test_dir/concurrent/a.log" 2>&1 &
first_pid=$!
"$publish_script" \
    "$test_dir/concurrent/b/binary" \
    "$test_dir/concurrent/b/build-info" \
    "$test_dir/concurrent/output" \
    "$artifact_name" >"$test_dir/concurrent/b.log" 2>&1 &
second_pid=$!
wait "$first_pid"
first_status=$?
wait "$second_pid"
second_status=$?
set -e

successes=0
[[ $first_status -eq 0 ]] && successes=$((successes + 1))
[[ $second_status -eq 0 ]] && successes=$((successes + 1))
[[ $successes -eq 1 ]] || fail "expected one successful publisher, got statuses $first_status and $second_status"

published_binary="$test_dir/concurrent/output/$artifact_name"
published_info="$published_binary.build-info"
published_checksum="$published_binary.sha256"
(cd "$test_dir/concurrent/output" && sha256sum -c "$artifact_name.sha256")
[[ $(stat -c '%a' "$published_binary") == 755 ]] || fail 'concurrent binary mode is not 755'
[[ $(stat -c '%a' "$published_info") == 644 ]] || fail 'concurrent build-info mode is not 644'
[[ $(stat -c '%a' "$published_checksum") == 644 ]] || fail 'concurrent checksum mode is not 644'

if cmp -s "$published_binary" "$test_dir/concurrent/a/binary"; then
    cmp -s "$published_info" "$test_dir/concurrent/a/build-info" || fail 'binary A was paired with different build-info'
elif cmp -s "$published_binary" "$test_dir/concurrent/b/binary"; then
    cmp -s "$published_info" "$test_dir/concurrent/b/build-info" || fail 'binary B was paired with different build-info'
else
    fail 'published binary does not match either contender'
fi

if find "$test_dir/concurrent/output" -maxdepth 1 \
    \( -name ".${artifact_name}.lock" -o -name ".${artifact_name}.stage.*" \) \
    -print -quit | grep -q .; then
    fail 'concurrent publication left a lock or stage path'
fi

mkdir -p "$test_dir/symlink/output" "$test_dir/symlink/source"
printf 'binary\n' >"$test_dir/symlink/source/binary"
printf 'build-info\n' >"$test_dir/symlink/source/build-info"
printf 'do-not-touch\n' >"$test_dir/symlink/victim"
ln -s "$test_dir/symlink/victim" "$test_dir/symlink/output/$artifact_name.build-info"

if "$publish_script" \
    "$test_dir/symlink/source/binary" \
    "$test_dir/symlink/source/build-info" \
    "$test_dir/symlink/output" \
    "$artifact_name" >"$test_dir/symlink/publish.log" 2>&1; then
    fail 'publisher accepted a preexisting build-info symlink'
fi
grep -F "refusing to replace existing output" "$test_dir/symlink/publish.log" >/dev/null
[[ $(<"$test_dir/symlink/victim") == 'do-not-touch' ]] || fail 'symlink target was modified'
[[ -L $test_dir/symlink/output/$artifact_name.build-info ]] || fail 'preexisting sidecar symlink was replaced'
[[ ! -e $test_dir/symlink/output/$artifact_name ]] || fail 'binary was published beside a rejected symlink'
[[ ! -e $test_dir/symlink/output/$artifact_name.sha256 ]] || fail 'checksum was published beside a rejected symlink'

if find "$test_dir/symlink/output" -maxdepth 1 \
    \( -name ".${artifact_name}.lock" -o -name ".${artifact_name}.stage.*" \) \
    -print -quit | grep -q .; then
    fail 'symlink rejection left a lock or stage path'
fi

printf 'test-publish-output: pass\n'
