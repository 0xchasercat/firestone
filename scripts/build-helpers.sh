#!/usr/bin/env bash

set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/build-helpers.sh --input-dir DIR --output-dir DIR

Builds passt and qemu-img twice in network-disabled containers, requires every
output byte to match, then copies the first build into the empty output dir.
Run scripts/fetch-helper-inputs.sh first.
EOF
}

fail() {
    printf 'build-helpers: %s\n' "$*" >&2
    exit 1
}

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        fail 'sha256sum or shasum is required'
    fi
}

verify_lock_files() {
    local lock=$1
    local directory=$2
    local expected filename url extra actual line_number=0

    while read -r expected filename url extra; do
        line_number=$((line_number + 1))
        [[ -n ${expected:-} ]] || continue
        [[ $expected != \#* ]] || continue
        [[ -z ${extra:-} ]] || fail "$lock:$line_number has extra fields"
        [[ $expected =~ ^[0-9a-f]{64}$ ]] || fail "$lock:$line_number has an invalid SHA-256"
        [[ $filename =~ ^[A-Za-z0-9][A-Za-z0-9._+-]*$ ]] ||
            fail "$lock:$line_number has an unsafe file name '$filename'"
        [[ $url == https://* ]] || fail "$lock:$line_number URL is not HTTPS"
        [[ -f $directory/$filename ]] || fail "missing input $directory/$filename"
        actual=$(sha256_file "$directory/$filename")
        [[ $actual == "$expected" ]] ||
            fail "$filename checksum mismatch: expected $expected, got $actual"
    done <"$lock"
}

normalize_arch() {
    case "$1" in
        x86_64 | amd64) printf '%s\n' x86_64 ;;
        *) printf '%s\n' "$1" ;;
    esac
}

compare_output_trees() {
    local first=$1
    local second=$2
    local first_list="$temporary_dir/first-files"
    local second_list="$temporary_dir/second-files"
    local relative first_mode second_mode

    (cd "$first" && find . -type f -print | LC_ALL=C sort) >"$first_list"
    (cd "$second" && find . -type f -print | LC_ALL=C sort) >"$second_list"
    cmp "$first_list" "$second_list" || fail 'double builds produced different file sets'

    while IFS= read -r relative; do
        cmp "$first/$relative" "$second/$relative" ||
            fail "double builds differ at ${relative#./}"
        first_mode=$(stat -c '%a' "$first/$relative")
        second_mode=$(stat -c '%a' "$second/$relative")
        [[ $first_mode == "$second_mode" ]] ||
            fail "double builds produced different modes for ${relative#./}"
    done <"$first_list"
}

input_request=
output_request=
while (($# > 0)); do
    case "$1" in
        --input-dir)
            (($# >= 2)) || fail '--input-dir requires a value'
            input_request=$2
            shift 2
            ;;
        --output-dir)
            (($# >= 2)) || fail '--output-dir requires a value'
            output_request=$2
            shift 2
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            usage >&2
            fail "unknown argument '$1'"
            ;;
    esac
done
[[ -n $input_request ]] || fail '--input-dir is required'
[[ -n $output_request ]] || fail '--output-dir is required'

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
repository_root=$(cd -- "$script_dir/.." && pwd -P)
recipe_root=${FIRESTONE_HELPER_RECIPE_ROOT:-"$repository_root/build/helpers"}
docker_command=${FIRESTONE_HELPER_DOCKER:-docker}
readonly script_dir repository_root recipe_root docker_command
command -v "$docker_command" >/dev/null 2>&1 || fail "$docker_command is required"
[[ -f $recipe_root/versions.env ]] || fail "missing $recipe_root/versions.env"

# shellcheck disable=SC1091 # The recipe root is selected above.
source "$recipe_root/versions.env"
[[ $BUILDER_IMAGE =~ @sha256:[0-9a-f]{64}$ ]] || fail 'builder image is not pinned by digest'
[[ $HELPER_ARCH == x86_64 ]] || fail "unsupported helper architecture '$HELPER_ARCH'"

host_arch=$(normalize_arch "$(uname -m)")
[[ $host_arch == x86_64 ]] || fail "native x86_64 host required, got $host_arch"
"$docker_command" image inspect "$BUILDER_IMAGE" >/dev/null 2>&1 ||
    fail 'pinned builder image is absent; run scripts/fetch-helper-inputs.sh first'
daemon_arch=$(normalize_arch "$("$docker_command" info --format '{{.Architecture}}')")
[[ $daemon_arch == x86_64 ]] || fail "native x86_64 Docker daemon required, got $daemon_arch"
image_arch=$(normalize_arch "$("$docker_command" image inspect --format '{{.Architecture}}' "$BUILDER_IMAGE")")
[[ $image_arch == x86_64 ]] || fail "builder image architecture is $image_arch"

[[ -d $input_request ]] || fail "input directory not found: $input_request"
input_dir=$(cd -- "$input_request" && pwd -P)
mkdir -p -- "$output_request"
output_dir=$(cd -- "$output_request" && pwd -P)
readonly input_dir output_dir
case "$input_dir/" in "$repository_root/"*) fail 'input directory must be outside the git worktree' ;; esac
case "$output_dir/" in "$repository_root/"*) fail 'output directory must be outside the git worktree' ;; esac
shopt -s nullglob dotglob
output_entries=("$output_dir"/*)
shopt -u nullglob dotglob
[[ ${#output_entries[@]} -eq 0 ]] || fail 'output directory must be empty'

[[ $(sha256_file "$recipe_root/packages.lock") == "$PACKAGES_LOCK_SHA256" ]] ||
    fail 'packages.lock checksum does not match versions.env'
[[ $(sha256_file "$recipe_root/sources.lock") == "$SOURCES_LOCK_SHA256" ]] ||
    fail 'sources.lock checksum does not match versions.env'
[[ $(sha256_file "$input_dir/$QEMU_SIGNING_KEY_ASSET") == "$QEMU_SIGNING_KEY_SHA256" ]] ||
    fail 'QEMU signing key input is missing or corrupt'
verify_lock_files "$recipe_root/packages.lock" "$input_dir/packages"
verify_lock_files "$recipe_root/sources.lock" "$input_dir/sources"

host_uid=$(id -u)
host_gid=$(id -g)
readonly host_uid host_gid

temporary_dir=$(mktemp -d "${TMPDIR:-/tmp}/firestone-helper-build.XXXXXX")
cleanup() {
    rm -rf "$temporary_dir"
}
trap cleanup EXIT HUP INT TERM

for run in 1 2; do
    work_dir="$temporary_dir/run-$run/work"
    run_output="$temporary_dir/run-$run/output"
    mkdir -p "$work_dir" "$run_output"
    "$docker_command" run \
        --rm \
        --pull never \
        --platform linux/amd64 \
        --network none \
        --security-opt no-new-privileges \
        --tmpfs /tmp:rw,nosuid,nodev,noexec,mode=1777 \
        --env "SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH" --env "HOST_UID=$host_uid" --env "HOST_GID=$host_gid" \
        --mount "type=bind,src=$repository_root,dst=/source,readonly" \
        --mount "type=bind,src=$input_dir,dst=/inputs,readonly" \
        --mount "type=bind,src=$work_dir,dst=/work" \
        --mount "type=bind,src=$run_output,dst=/output" \
        "$BUILDER_IMAGE" \
        /source/build/helpers/build-in-container.sh
done

compare_output_trees "$temporary_dir/run-1/output" "$temporary_dir/run-2/output"
cp -Rp "$temporary_dir/run-1/output/." "$output_dir/"
printf 'byte-identical helper builds verified\n'
cat "$output_dir/SHA256SUMS"
