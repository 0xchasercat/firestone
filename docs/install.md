---
icon: download
---

# Install

Getting the Firestone executable onto a Linux x86_64 host, from the release or from source.

The published release is a standalone x86_64 musl executable:

```sh
curl -fsSL https://raw.githubusercontent.com/0xchasercat/firestone/main/install.sh | sh
```

To place the executable yourself instead, download the release asset and install it:

```sh
install -Dm0755 firestone-v*-x86_64-unknown-linux-musl "$HOME/.local/bin/firestone"
firestone version
```

The asset is named for its release, so substitute the version you downloaded when the glob matches more than one file. `version` reports the embedded `passt` and `qemu-img` payload hashes, which Firestone verifies again before it materializes them under its own data directory.

You need at least 5 GB free on the filesystem holding Firestone's data directory, and an OpenSSH client on the host. Install only the client package:

```sh
# Ubuntu 24.04
sudo apt-get install openssh-client

# Fedora 44
sudo dnf install openssh-clients

# Arch Linux
sudo pacman -S openssh
```

## Build from source

Rust 1.85 or newer builds the workspace. A plain `cargo build` is a development build. It keeps a PATH fallback for the helper binaries, so you can develop against your own `passt` or `qemu-img`.

```sh
cargo build --release
```

For the standalone release build, which embeds the pinned helpers and refuses a missing or mismatched input, use the release script:

```sh
scripts/build-release.sh --target x86_64-unknown-linux-musl
```

Next: [check the host and boot your first machine](quickstart.md). The full page list is in the [documentation index](README.md).
