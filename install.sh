#!/bin/sh
# Firestone installer.
#
#   curl -fsSL https://raw.githubusercontent.com/0xchasercat/firestone/main/install.sh | sh
#
# It downloads one static binary from the latest GitHub release, checks it
# against the SHA256SUMS published beside it, and copies it into a directory
# you own. It never uses sudo and never writes outside that directory.
#
# Environment variables:
#
#   FIRESTONE_VERSION       release tag to install, for example v0.1.4.
#                           Default: the latest release.
#   FIRESTONE_INSTALL_DIR   directory to install into.
#                           Default: $HOME/.local/bin.
#
# Linux x86_64 only. Firestone runs Linux VMs through KVM, so there is no
# macOS build, and there is no aarch64 runtime release yet.

set -eu

repository=0xchasercat/firestone
api_url="https://api.github.com/repos/$repository/releases/latest"
download_base="https://github.com/$repository/releases/download"
target=x86_64-unknown-linux-musl

temporary_dir=
staged_file=

say() {
    printf '%s\n' "$*"
}

fail() {
    printf 'install.sh: %s\n' "$*" >&2
    exit 1
}

cleanup() {
    if [ -n "$staged_file" ]; then
        rm -f -- "$staged_file" 2>/dev/null || true
    fi
    if [ -n "$temporary_dir" ] && [ -d "$temporary_dir" ]; then
        rm -rf -- "$temporary_dir" 2>/dev/null || true
    fi
}

have() {
    command -v "$1" >/dev/null 2>&1
}

# fetch <url> <destination>
fetch() {
    if have curl; then
        curl -fsSL --proto '=https' --proto-redir '=https' -o "$2" "$1"
    elif have wget; then
        wget -q --https-only -O "$2" "$1"
    else
        fail 'neither curl nor wget is installed. Install one of them and run this again.'
    fi
}

# read_tag_name <file> — prints the tag_name field of a GitHub release document.
read_tag_name() {
    tr ',' '\n' <"$1" |
        sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' |
        head -n 1
}

trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

host_os=$(uname -s)
case "$host_os" in
    Linux) ;;
    Darwin)
        fail 'this host runs macOS. Firestone runs Linux VMs through KVM, so install it on a Linux x86_64 host instead.'
        ;;
    *)
        fail "this host runs $host_os. Firestone is released for Linux x86_64 only, so install it on a Linux x86_64 host instead."
        ;;
esac

host_architecture=$(uname -m)
case "$host_architecture" in
    x86_64 | amd64) ;;
    aarch64 | arm64)
        fail 'this host is aarch64. There is no aarch64 runtime release yet, so only Linux x86_64 can be installed.'
        ;;
    *)
        fail "this host is $host_architecture. Firestone is released for Linux x86_64 only, so install it on a Linux x86_64 host instead."
        ;;
esac

have sha256sum || fail 'sha256sum is not installed. Install coreutils and run this again.'

install_dir=${FIRESTONE_INSTALL_DIR:-}
if [ -z "$install_dir" ]; then
    [ -n "${HOME:-}" ] ||
        fail 'HOME is not set. Set FIRESTONE_INSTALL_DIR to the directory to install into and run this again.'
    install_dir="$HOME/.local/bin"
fi

temporary_dir=$(mktemp -d "${TMPDIR:-/tmp}/firestone-install.XXXXXX") ||
    fail 'could not create a temporary directory to download into.'

version=${FIRESTONE_VERSION:-}
if [ -n "$version" ]; then
    say "Installing firestone $version, pinned by FIRESTONE_VERSION."
else
    say 'Resolving the latest firestone release.'
    fetch "$api_url" "$temporary_dir/release.json" ||
        fail 'could not reach the GitHub releases API. Set FIRESTONE_VERSION=vX.Y.Z to skip the lookup, or try again later.'
    version=$(read_tag_name "$temporary_dir/release.json")
    [ -n "$version" ] ||
        fail 'the GitHub releases API answered without a release tag. Set FIRESTONE_VERSION=vX.Y.Z to choose a release directly.'
fi

case "$version" in
    v | v*[!0-9.]*)
        fail "release tag '$version' does not look like v0.1.4. Set FIRESTONE_VERSION to a published tag."
        ;;
    v*) ;;
    *)
        fail "release tag '$version' does not look like v0.1.4. Set FIRESTONE_VERSION to a published tag."
        ;;
esac

artifact="firestone-$version-$target"
say "Downloading $artifact."
fetch "$download_base/$version/$artifact" "$temporary_dir/$artifact" ||
    fail "could not download $artifact from release $version. Check the tag at https://github.com/$repository/releases."
fetch "$download_base/$version/SHA256SUMS" "$temporary_dir/SHA256SUMS" ||
    fail "could not download SHA256SUMS from release $version. Nothing was installed."

if ! grep -e "[ *]$artifact\$" "$temporary_dir/SHA256SUMS" >"$temporary_dir/checksum"; then
    fail "SHA256SUMS in release $version does not list $artifact. Nothing was installed."
fi
if ! (cd "$temporary_dir" && sha256sum -c checksum >/dev/null); then
    fail 'the downloaded binary does not match its published SHA-256 checksum. Nothing was installed.'
fi
say 'Checksum verified.'

mkdir -p -- "$install_dir" 2>/dev/null ||
    fail "could not create $install_dir. Set FIRESTONE_INSTALL_DIR to a directory you can write to and run this again."
[ -w "$install_dir" ] ||
    fail "$install_dir is not writable by this user. This installer never uses sudo, so set FIRESTONE_INSTALL_DIR to a directory you own and run this again."

installed_path="$install_dir/firestone"
staged_file="$install_dir/.firestone.install.$$"
cp -- "$temporary_dir/$artifact" "$staged_file" ||
    fail "could not write to $install_dir. Nothing was installed."
chmod 0755 "$staged_file"
mv -f -- "$staged_file" "$installed_path" ||
    fail "could not replace $installed_path. Nothing was installed."
staged_file=

version_line=$("$installed_path" --version 2>/dev/null) || version_line="firestone $version"
say "Installed $version_line at $installed_path."

case ":${PATH:-}:" in
    *:"$install_dir":*) ;;
    *)
        say "$install_dir is not on PATH. Add it to your shell profile with:"
        say "  export PATH=\"$install_dir:\$PATH\""
        say "Until then, run it as $installed_path."
        ;;
esac
