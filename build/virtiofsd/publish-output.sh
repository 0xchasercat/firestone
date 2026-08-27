#!/bin/sh

set -eu

fail() {
    printf 'publish-output: %s\n' "$*" >&2
    exit 1
}

path_exists() {
    [ -e "$1" ] || [ -L "$1" ]
}

cleanup() {
    if [ "${stage_owned:-0}" -eq 1 ]; then
        case "$stage_dir" in
            "$output_dir/.${artifact_name}.stage."*)
                if [ -d "$stage_dir" ] && [ ! -L "$stage_dir" ]; then
                    rm -rf -- "$stage_dir"
                else
                    printf 'publish-output: refusing to remove invalid stage path %s\n' "$stage_dir" >&2
                fi
                ;;
            *)
                printf 'publish-output: refusing to remove unexpected stage path %s\n' "$stage_dir" >&2
                ;;
        esac
    fi

    if [ "${lock_owned:-0}" -eq 1 ]; then
        if [ "$lock_dir" = "$output_dir/.${artifact_name}.lock" ] && [ -d "$lock_dir" ] && [ ! -L "$lock_dir" ]; then
            rmdir -- "$lock_dir" ||
                printf 'publish-output: could not remove output lock %s\n' "$lock_dir" >&2
        else
            printf 'publish-output: refusing to remove invalid lock path %s\n' "$lock_dir" >&2
        fi
    fi
}

move_exclusive() {
    source_path=$1
    destination_path=$2

    mv -nT -- "$source_path" "$destination_path"
    if path_exists "$source_path"; then
        fail "destination appeared during publication: $destination_path"
    fi
}

[ "$#" -eq 4 ] || fail 'usage: publish-output.sh BINARY BUILD_INFO OUTPUT_DIR ARTIFACT_NAME'
source_binary=$1
source_build_info=$2
output_dir=$3
artifact_name=$4

case "$artifact_name" in
    '' | . | .. | *[!A-Za-z0-9._-]* | */*)
        fail "invalid artifact name '$artifact_name'"
        ;;
esac

[ -d "$output_dir" ] && [ ! -L "$output_dir" ] || fail "output directory is not a real directory: $output_dir"
[ -f "$source_binary" ] && [ ! -L "$source_binary" ] || fail "binary source is not a regular file: $source_binary"
[ -f "$source_build_info" ] && [ ! -L "$source_build_info" ] ||
    fail "build-info source is not a regular file: $source_build_info"

artifact_path="$output_dir/$artifact_name"
checksum_path="$artifact_path.sha256"
build_info_path="$artifact_path.build-info"
lock_dir="$output_dir/.${artifact_name}.lock"
stage_dir=
lock_owned=0
stage_owned=0
readonly source_binary source_build_info output_dir artifact_name
readonly artifact_path checksum_path build_info_path lock_dir

trap cleanup 0
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

if ! mkdir -- "$lock_dir"; then
    fail "output is locked for $artifact_name"
fi
lock_owned=1
chmod 0700 "$lock_dir"

for final_path in "$artifact_path" "$checksum_path" "$build_info_path"; do
    if path_exists "$final_path"; then
        fail "refusing to replace existing output: $final_path"
    fi
done

stage_dir=$(mktemp -d "$output_dir/.${artifact_name}.stage.XXXXXXXX")
case "$stage_dir" in
    "$output_dir/.${artifact_name}.stage."*) ;;
    *) fail "mktemp returned unexpected stage path: $stage_dir" ;;
esac
[ -d "$stage_dir" ] && [ ! -L "$stage_dir" ] || fail "stage path is not a real directory: $stage_dir"
stage_owned=1
chmod 0700 "$stage_dir"

stage_binary="$stage_dir/$artifact_name"
stage_checksum="$stage_dir/$artifact_name.sha256"
stage_build_info="$stage_dir/$artifact_name.build-info"

cp -- "$source_binary" "$stage_binary"
cp -- "$source_build_info" "$stage_build_info"
chmod 0755 "$stage_binary"
chmod 0644 "$stage_build_info"

artifact_sha=$(sha256sum "$stage_binary" | awk '{print $1}')
printf '%s  %s\n' "$artifact_sha" "$artifact_name" >"$stage_checksum"
chmod 0644 "$stage_checksum"

[ "$(stat -c '%a' "$stage_binary")" = 755 ] || fail 'staged binary mode is not 755'
[ "$(stat -c '%a' "$stage_checksum")" = 644 ] || fail 'staged checksum mode is not 644'
[ "$(stat -c '%a' "$stage_build_info")" = 644 ] || fail 'staged build-info mode is not 644'
(cd "$stage_dir" && sha256sum -c "$artifact_name.sha256")

# Publish the binary last. Its presence means both sidecars were already renamed.
move_exclusive "$stage_checksum" "$checksum_path"
move_exclusive "$stage_build_info" "$build_info_path"
move_exclusive "$stage_binary" "$artifact_path"

[ -f "$artifact_path" ] && [ ! -L "$artifact_path" ] || fail 'published binary is not a regular file'
[ -f "$checksum_path" ] && [ ! -L "$checksum_path" ] || fail 'published checksum is not a regular file'
[ -f "$build_info_path" ] && [ ! -L "$build_info_path" ] || fail 'published build-info is not a regular file'
[ "$(stat -c '%a' "$artifact_path")" = 755 ] || fail 'published binary mode is not 755'
[ "$(stat -c '%a' "$checksum_path")" = 644 ] || fail 'published checksum mode is not 644'
[ "$(stat -c '%a' "$build_info_path")" = 644 ] || fail 'published build-info mode is not 644'
(cd "$output_dir" && sha256sum -c "$artifact_name.sha256")

printf 'artifact %s\n' "$artifact_path"
printf 'sha256 %s\n' "$artifact_sha"

