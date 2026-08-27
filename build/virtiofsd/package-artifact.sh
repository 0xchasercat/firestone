#!/usr/bin/env bash

set -euo pipefail

fail() {
    printf 'package-artifact: %s\n' "$*" >&2
    exit 1
}

path_exists() {
    [[ -e $1 || -L $1 ]]
}

cleanup() {
    if [[ ${stage_owned:-0} -eq 1 ]]; then
        case "$stage_dir" in
            "$output_parent/.${output_basename}.stage."*)
                if [[ -d $stage_dir && ! -L $stage_dir ]]; then
                    rm -rf -- "$stage_dir"
                else
                    printf 'package-artifact: refusing to remove invalid stage path %s\n' "$stage_dir" >&2
                fi
                ;;
            *)
                printf 'package-artifact: refusing to remove unexpected stage path %s\n' "$stage_dir" >&2
                ;;
        esac
    fi
}

[[ $# -eq 3 ]] || fail 'usage: package-artifact.sh INPUT_DIR ARTIFACT_NAME OUTPUT_TAR'
input_dir=$1
artifact_name=$2
output_tar=$3

case "$artifact_name" in
    '' | . | .. | *[!A-Za-z0-9._-]* | */*)
        fail "invalid artifact name '$artifact_name'"
        ;;
esac

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
# shellcheck disable=SC1090,SC1091 # Resolved next to this script.
source "$script_dir/versions.env"

[[ -d $input_dir && ! -L $input_dir ]] || fail "input directory is not a real directory: $input_dir"
input_dir=$(cd -- "$input_dir" && pwd -P)
binary_path="$input_dir/$artifact_name"
build_info_path="$input_dir/$artifact_name.build-info"
checksum_path="$input_dir/SHA256SUMS"

for input_path in "$binary_path" "$build_info_path" "$checksum_path"; do
    [[ -f $input_path && ! -L $input_path ]] || fail "package input is not a regular file: $input_path"
done

[[ $(stat -c '%a' "$binary_path") == 755 ]] || fail 'input binary mode is not 755'
[[ $(stat -c '%a' "$build_info_path") == 644 ]] || fail 'input build-info mode is not 644'
[[ $(stat -c '%a' "$checksum_path") == 644 ]] || fail 'input checksum mode is not 644'

expected_checksum=$(sha256sum "$binary_path" | awk -v name="$artifact_name" '{print $1 "  " name}')
actual_checksum=$(<"$checksum_path")
[[ $actual_checksum == "$expected_checksum" ]] || fail 'SHA256SUMS does not match the input binary'

output_parent=$(dirname -- "$output_tar")
output_basename=$(basename -- "$output_tar")
[[ -d $output_parent && ! -L $output_parent ]] || fail "output parent is not a real directory: $output_parent"
output_parent=$(cd -- "$output_parent" && pwd -P)
output_tar="$output_parent/$output_basename"
[[ $output_basename == *.tar ]] || fail 'output file must use the .tar suffix'
path_exists "$output_tar" && fail "refusing to replace existing package: $output_tar"

stage_dir=
stage_owned=0
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

stage_dir=$(mktemp -d "$output_parent/.${output_basename}.stage.XXXXXXXX")
case "$stage_dir" in
    "$output_parent/.${output_basename}.stage."*) ;;
    *) fail "mktemp returned unexpected stage path: $stage_dir" ;;
esac
[[ -d $stage_dir && ! -L $stage_dir ]] || fail "stage path is not a real directory: $stage_dir"
stage_owned=1
chmod 0700 "$stage_dir"

names_file="$stage_dir/names"
stage_tar="$stage_dir/$output_basename"
printf '%s\n' SHA256SUMS "$artifact_name" "$artifact_name.build-info" | LC_ALL=C sort >"$names_file"

TZ=UTC LC_ALL=C tar \
    --create \
    --file "$stage_tar" \
    --directory "$input_dir" \
    --no-recursion \
    --numeric-owner \
    --owner 0 \
    --group 0 \
    --mtime="@$SOURCE_DATE_EPOCH" \
    --sort=name \
    --format=gnu \
    --files-from "$names_file"
chmod 0644 "$stage_tar"

mv -nT -- "$stage_tar" "$output_tar"
path_exists "$stage_tar" && fail "destination appeared during packaging: $output_tar"
[[ -f $output_tar && ! -L $output_tar ]] || fail 'package output is not a regular file'
[[ $(stat -c '%a' "$output_tar") == 644 ]] || fail 'package mode is not 644'

printf 'package %s\n' "$output_tar"
printf 'sha256 %s\n' "$(sha256sum "$output_tar" | awk '{print $1}')"
