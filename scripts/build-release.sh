#!/usr/bin/env bash

set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/build-release.sh --target TARGET --output-dir DIR

Build one Firestone Linux musl binary with the pinned native container.

Targets:
  x86_64-unknown-linux-musl
  aarch64-unknown-linux-musl
EOF
}

fail() {
    printf 'build-release: %s\n' "$*" >&2
    exit 1
}

cleanup() {
    if [[ -n ${work_dir:-} && -d $work_dir && $work_dir != / ]]; then
        case "$(basename -- "$work_dir")" in
            firestone-release-build.*) rm -rf -- "$work_dir" ;;
            *) printf 'build-release: refusing to remove unexpected work directory %s\n' "$work_dir" >&2 ;;
        esac
    fi
}

target=
requested_output=
while [[ $# -gt 0 ]]; do
    case "$1" in
        --target)
            [[ $# -ge 2 ]] || fail '--target requires a value'
            target=$2
            shift 2
            ;;
        --output-dir)
            [[ $# -ge 2 ]] || fail '--output-dir requires a value'
            requested_output=$2
            shift 2
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            fail "unknown argument '$1'"
            ;;
    esac
done

case "$target" in
    x86_64-unknown-linux-musl) target_arch=x86_64 ;;
    aarch64-unknown-linux-musl) target_arch=aarch64 ;;
    *) fail '--target must be x86_64-unknown-linux-musl or aarch64-unknown-linux-musl' ;;
esac
[[ -n $requested_output ]] || fail '--output-dir is required'

for command_name in docker git; do
    command -v "$command_name" >/dev/null 2>&1 || fail "$command_name is required"
done

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
repository_root=$(cd -- "$script_dir/.." && pwd -P)
recipe_root="$repository_root/build/firestone"
readonly script_dir repository_root recipe_root

# shellcheck disable=SC1091 # The file is resolved from the repository root above.
source "$recipe_root/versions.env"
[[ $RUST_IMAGE =~ @sha256:[0-9a-f]{64}$ ]] || fail 'Rust image is not pinned by digest'

host_arch=$(uname -m)
case "$host_arch" in
    x86_64 | amd64) host_arch=x86_64 ;;
    aarch64 | arm64) host_arch=aarch64 ;;
    *) fail "unsupported build host architecture '$host_arch'" ;;
esac
[[ $host_arch == "$target_arch" ]] ||
    fail "$target requires a native $target_arch host; refusing emulated release output on $host_arch"

[[ -z $(git -C "$repository_root" status --porcelain --untracked-files=all) ]] ||
    fail 'git worktree must be clean so the embedded revision identifies every source byte'
git_commit=$(git -C "$repository_root" rev-parse --verify HEAD)
source_date_epoch=$(git -C "$repository_root" show -s --format=%ct HEAD)
[[ $git_commit =~ ^[0-9a-f]{40}$ ]] || fail "git returned invalid revision '$git_commit'"
[[ $source_date_epoch =~ ^[0-9]+$ ]] || fail "git returned invalid commit timestamp '$source_date_epoch'"
readonly git_commit source_date_epoch

mkdir -p -- "$requested_output"
output_dir=$(cd -- "$requested_output" && pwd -P)
readonly output_dir
[[ $output_dir != / ]] || fail 'output directory cannot be the filesystem root'
case "$output_dir/" in
    "$repository_root/"*) fail 'output directory must be outside the git worktree' ;;
esac
shopt -s nullglob dotglob
output_entries=("$output_dir"/*)
shopt -u nullglob dotglob
[[ ${#output_entries[@]} -eq 0 ]] || fail 'output directory must be empty'

work_dir=$(mktemp -d "${TMPDIR:-/tmp}/firestone-release-build.XXXXXX")
readonly work_dir
trap cleanup EXIT
mkdir -p "$work_dir/cargo-home" "$work_dir/home" "$work_dir/target"

docker pull "$RUST_IMAGE"
docker run \
    --rm \
    --read-only \
    --cap-drop ALL \
    --security-opt no-new-privileges \
    --tmpfs /tmp:rw,nosuid,nodev,noexec,mode=1777 \
    --user "$(id -u):$(id -g)" \
    --env CARGO_HOME=/work/cargo-home \
    --env CARGO_INCREMENTAL=0 \
    --env FIRESTONE_GIT_COMMIT="$git_commit" \
    --env HOME=/work/home \
    --env SOURCE_DATE_EPOCH="$source_date_epoch" \
    --mount "type=bind,src=$repository_root,dst=/source,readonly" \
    --mount "type=bind,src=$work_dir,dst=/work" \
    --mount "type=bind,src=$output_dir,dst=/output" \
    --workdir /source \
    "$RUST_IMAGE" \
    /source/build/firestone/build-in-container.sh "$target"
