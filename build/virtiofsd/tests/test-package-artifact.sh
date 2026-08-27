#!/usr/bin/env bash

set -euo pipefail

fail() {
    printf 'test-package-artifact: %s\n' "$*" >&2
    exit 1
}

test_dir=$(mktemp -d /tmp/firestone-package-artifact-test.XXXXXXXX)
readonly test_dir
cleanup() {
    case "$test_dir" in
        /tmp/firestone-package-artifact-test.*)
            rm -rf -- "$test_dir"
            ;;
        *)
            printf 'test-package-artifact: refusing to remove unexpected test path %s\n' "$test_dir" >&2
            ;;
    esac
}
trap cleanup EXIT

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
readonly script_dir
recipe_dir=$(cd -- "$script_dir/.." && pwd -P)
readonly recipe_dir
readonly package_script="$recipe_dir/package-artifact.sh"
readonly artifact_name=virtiofsd-test-x86_64-unknown-linux-musl

mkdir -p "$test_dir/input" "$test_dir/first" "$test_dir/second" "$test_dir/extracted"
printf 'binary\n' >"$test_dir/input/$artifact_name"
printf 'build-info\n' >"$test_dir/input/$artifact_name.build-info"
chmod 0755 "$test_dir/input/$artifact_name"
chmod 0644 "$test_dir/input/$artifact_name.build-info"
(cd "$test_dir/input" && sha256sum "$artifact_name" >SHA256SUMS)
chmod 0644 "$test_dir/input/SHA256SUMS"

"$package_script" "$test_dir/input" "$artifact_name" "$test_dir/first/candidate.tar"
"$package_script" "$test_dir/input" "$artifact_name" "$test_dir/second/candidate.tar"
cmp "$test_dir/first/candidate.tar" "$test_dir/second/candidate.tar"

printf '%s\n' SHA256SUMS "$artifact_name" "$artifact_name.build-info" | LC_ALL=C sort >"$test_dir/expected-names"
tar -tf "$test_dir/first/candidate.tar" >"$test_dir/actual-names"
cmp "$test_dir/expected-names" "$test_dir/actual-names"

tar -xf "$test_dir/first/candidate.tar" -C "$test_dir/extracted"
[[ $(stat -c '%a' "$test_dir/extracted/$artifact_name") == 755 ]] || fail 'extracted binary mode is not 755'
[[ $(stat -c '%a' "$test_dir/extracted/$artifact_name.build-info") == 644 ]] ||
    fail 'extracted build-info mode is not 644'
[[ $(stat -c '%a' "$test_dir/extracted/SHA256SUMS") == 644 ]] || fail 'extracted checksum mode is not 644'
(cd "$test_dir/extracted" && sha256sum -c SHA256SUMS)
cmp "$test_dir/input/$artifact_name" "$test_dir/extracted/$artifact_name"
cmp "$test_dir/input/$artifact_name.build-info" "$test_dir/extracted/$artifact_name.build-info"

if tar --numeric-owner --full-time -tvf "$test_dir/first/candidate.tar" | awk '$2 != "0/0" { exit 1 }'; then
    :
else
    fail 'tar owner or group is not numeric 0/0'
fi
member_timestamps=$(tar --numeric-owner --full-time -tvf "$test_dir/first/candidate.tar" |
    awk '{print $4 " " $5}' | sort -u)
if [[ $member_timestamps != '2026-07-06 09:53:22' ]]; then
    fail 'tar member timestamps do not match SOURCE_DATE_EPOCH'
fi

printf 'test-package-artifact: pass\n'
