# Fresh-host doctor matrix

Validation date: 2026-08-30

This record covers doctor behavior without KVM. The local matrix used x86_64 distribution containers on Docker 29.4.0. A root-owned mode-0660 regular file at `/dev/kvm`, with group `kvm`, exercised the permission hint. It was deliberately not a character device and was not KVM. No result in this record is boot evidence.

## Environments and observed packages

The local Docker image manifests were:

| Row | Image manifest |
|---|---|
| Ubuntu 24.04.4 | `ubuntu:24.04`, `sha256:33ceb71981b602c1a7443a53469e4dba065f7503eab3078a2d7a57a2ab987517` |
| Fedora 44 | `fedora:44`, `sha256:43b29f65a41eb9c35e1cd5323e3bdf3b655c2357a9f4f1ff2f9c2798e5045d80` |
| Arch Linux rolling build 20260823.0.578598 | `archlinux:base`, `sha256:b860afd5823683f7ea389ba5f00d812f4fe55f6f286dea329d2abeefa535e309` |

The harness queried the native package database and then queried the owner of each executable. It did not infer package names from another distribution.

| Row | `passt` | `qemu-img` package | OpenSSH client package | `util-linux` |
|---|---|---|---|---|
| Ubuntu 24.04 | `0.0~git20240220.1e6f92b-1` | `qemu-utils` `1:8.2.2+ds-0ubuntu1.18` | `openssh-client` `1:9.6p1-3ubuntu13.18` | `2.39.3-9ubuntu6.5` |
| Fedora 44 | `0^20260728.gf8df3f1-2.fc44` | `qemu-img` `10.2.2-1.fc44` | `openssh-clients` `10.2p1-14.fc44` | `2.41.5-1.fc44` |
| Arch Linux | `2026_07_28.f8df3f1-1` | `qemu-img` `11.1.1-1` | `openssh` `10.5p1-1` | `2.42.2-1` |

Ubuntu's package is older than Firestone's pinned passt minimum. Doctor correctly retained a failure after package installation and printed the exact upstream source tag, commit, prerequisite package command, build target, and install command. Fedora's binary reports its version as `passt 0^20260728.gf8df3f1-2.fc44.x86_64`; the matrix exposed that packaging syntax, and doctor now parses it without weakening the commit or date checks. Arch reports the upstream underscore form.

## Missing-tool phase

The harness first gave doctor an empty `PATH` while leaving `/etc/os-release`, `/proc`, the small data filesystem, and the KVM fixture real. All three rows observed the same status shape:

| Check | Status | Required result |
|---|---|---|
| host architecture | ok | Linux x86_64 accepted |
| KVM | fail | device group `kvm`; fix exactly `sudo usermod -aG kvm $USER`; re-login hint |
| nested virtualization | warn | KVM exists but cannot be used |
| runtime directory | fail | fix exactly `firestone doctor --fix` |
| vendored binaries | fail | fix exactly `firestone doctor --fix` |
| virtiofsd | fail | fix exactly `firestone doctor --fix` |
| passt | fail | distribution-specific install or source-build hint; no unverified `fix` field |
| qemu-img | fail | exact apt, dnf, or pacman package command |
| OpenSSH | fail | exact apt, dnf, or pacman package command |
| user namespaces | warn | passt is unavailable, so the result is inconclusive; hint states that generic `unshare` alone is not proof |
| Firestone SSH key | fail | fix exactly `firestone doctor --fix` |
| data space | warn | 1,073,729,536 bytes available, below 5 GiB; free-space hint |
| stale state | ok | no pre-existing Firestone machine state |

The exact package fixes were:

| Row | passt hint | qemu-img fix | OpenSSH fix |
|---|---|---|---|
| Ubuntu 24.04 | install `build-essential ca-certificates git`, clone `https://passt.top/passt` tag `2025_02_17.a1e48a0`, verify commit `a1e48a02ff3550eb7875a7df6726086e9b3a1213`, build `passt`, install it at `/usr/local/bin/passt` | `sudo apt-get install qemu-utils` | `sudo apt-get install openssh-client` |
| Fedora 44 | `sudo dnf install passt` | `sudo dnf install qemu-img` | `sudo dnf install openssh-clients` |
| Arch Linux | `sudo pacman -S passt` | `sudo pacman -S qemu-img` | `sudo pacman -S openssh` |

## Unprivileged fix phase

Each row ran as uid 10001 with a one-GiB tmpfs home. Command traps named `sudo`, `apt-get`, `dnf`, `pacman`, `usermod`, and `sysctl` preceded the system path. None ran.

`doctor --fix` performed only these actions:

- created the user-owned data, binary, SSH, and runtime fallback directories with mode 0700;
- downloaded the exact x86_64 artifacts from `deps.toml` and accepted them only after their SHA-256 checks passed;
- generated an Ed25519 private key at mode 0600 and public key at mode 0644.

The package database and executable ownership facts were byte-for-byte equal before and after. The fake KVM file's device, inode, uid, gid, and mode were unchanged. The runtime result changed from fail to a warning that named the secure `/tmp/firestone-10001` fallback. Vendored binaries, virtiofsd, qemu-img, OpenSSH, and the Firestone key changed to ok.

Ubuntu kept the expected passt failure because the installed package is too old. Fedora and Arch passed passt. User-namespace authority now comes from passt's mandatory namespace stage together with kernel, AppArmor, seccomp/container, and correlated audit facts; the matrix retains `unshare -U true` only as a comparison and does not force doctor status from that generic probe. Inconclusive results must state that generic unshare alone is not proof, and every result records that qemu-img is independent of user namespaces.

## Reproduce locally

Build one Linux x86_64 Firestone binary, install each row's packages in its container, create the unprivileged user and non-KVM fixture, then run:

```sh
python3 scripts/m5-doctor-matrix.py --distro ubuntu --firestone /path/to/firestone
python3 scripts/m5-doctor-matrix.py --distro fedora --firestone /path/to/firestone
python3 scripts/m5-doctor-matrix.py --distro arch --firestone /path/to/firestone
```

The CI definition in `.github/workflows/m5-doctor-matrix.yml` performs the complete setup. Its Ubuntu 24.04 runner builds the Linux x86_64 binary once, then the three container jobs install their own native packages and run the same unprivileged harness. The container option mounts a one-GiB tmpfs at `/matrix-home` so the free-space warning is deterministic.

GitHub Actions run [33276918451](https://github.com/0xchasercat/firestone/actions/runs/33276918451) passed the build, live catalog audit, Python tests, guide validation, and all three doctor rows. The Ubuntu job finished in 28 seconds, Fedora in 33 seconds, and Arch in 17 seconds. These remained non-KVM checks.
