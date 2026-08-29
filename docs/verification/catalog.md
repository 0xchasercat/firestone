# Built-in image catalog source validation

Retrieval date: 2026-08-30

This record covers the Linux x86_64 catalog in SPEC section 8.1. It records source and package metadata retrieval. It does not claim that an image booted. E2E 11 remains open because the Linux KVM validation host is unavailable.

## Scope and policy

The built-in catalog contains exactly five references:

- Ubuntu 24.04, default, alias `noble`
- Ubuntu 22.04, alias `jammy`
- Debian 12, default, alias `bookworm`
- Debian 13, alias `trixie`
- Fedora 44, default, no alias

No Arch, AlmaLinux, Rocky Linux, openSUSE, or NixOS image was added. Each authorized release keeps dated x86_64 and aarch64 locators so catalog parsing and compile-only paths work on both build targets. Runtime support and E2E 11 are Linux x86_64 only; the aarch64 locators are not a runtime or boot claim.

Every row has these effective values:

| Field | Value | Basis |
|---|---|---|
| Architecture | dated `x86_64` and `aarch64` source tables; runtime gate is x86_64 only | compile-only portability plus Linux MVP decision |
| Firmware | pinned edk2 `ch-1e1b96f126` | Cloud Hypervisor v53 integration pin; x86_64 E2E 11 must still prove each image |
| Login user | `root` | normative `MachineSpec` default; not a catalog field |
| SSH daemon | `/usr/sbin/sshd` | distribution package ownership checks below |
| Image format | qcow2 | vendor metadata and image filenames |

The validator rejects any extra release, changed alias or default, missing explicit `sshd_path`, missing x86_64 or compile-only aarch64 table, non-edk2 effective firmware, and any image or checksum URL containing `current` or `latest`. Network metadata and checksum retrieval in this M5 audit is limited to x86_64.

## Authoritative release metadata

Ubuntu release discovery uses Canonical's machine-readable [`com.ubuntu.cloud:released:download.json`](https://cloud-images.ubuntu.com/releases/streams/v1/com.ubuntu.cloud:released:download.json). The 18,433,990-byte document identified build `20260826` as the latest released amd64 `disk1.img` for both Noble and Jammy. The catalog stores the direct dated paths rather than the moving release aliases. The validator limits release metadata to 32 MiB and requires the SHA-256 in this document to equal the matching `SHA256SUMS` record.

Debian release discovery uses the official current JSON records for [Bookworm](https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-genericcloud-amd64.json) and [Trixie](https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.json). Their `Build` records identified official amd64 genericcloud builds `20260821-2577` and `20260826-2582`. The catalog then uses those dated directories and their `SHA512SUMS` files. The moving `latest` location is used only to discover the current build during validation, never as the image source.

Fedora release discovery uses [`https://getfedora.org/releases.json`](https://getfedora.org/releases.json), which redirected over HTTPS to `fedoraproject.org/releases.json`. The document listed stable x86_64 Cloud releases 42, 43, and 44. The highest stable release was 44, with one `Cloud_Base` Generic qcow2 record for build `44-1.7`. Its URL and SHA-256 matched the catalog and Fedora's clearsigned checksum document.

## Sources and digests

The validator sent `HEAD` to each image URL. Each returned HTTP 200 without downloading an image body. It fetched each checksum document with `GET`, received HTTP 200, and found one digest for the exact dated filename.

| Reference | Image URL | Checksum source | Algorithm | Digest observed on 2026-08-30 |
|---|---|---|---|---|
| `ubuntu:24.04` | [`ubuntu-24.04-server-cloudimg-amd64.img`](https://cloud-images.ubuntu.com/releases/noble/release-20260826/ubuntu-24.04-server-cloudimg-amd64.img) | [`SHA256SUMS`](https://cloud-images.ubuntu.com/releases/noble/release-20260826/SHA256SUMS) | SHA-256 | `d0fe84bb5f80853425fa6be28e2c106f30104c3cfe8611933f2e65c9b63f0e30` |
| `ubuntu:22.04` | [`ubuntu-22.04-server-cloudimg-amd64.img`](https://cloud-images.ubuntu.com/releases/jammy/release-20260826/ubuntu-22.04-server-cloudimg-amd64.img) | [`SHA256SUMS`](https://cloud-images.ubuntu.com/releases/jammy/release-20260826/SHA256SUMS) | SHA-256 | `c0a5af17e6c0f76351fe07e2fffef3011dab1facb8a8ed5701dcf648dabd4f0a` |
| `debian:12` | [`debian-12-genericcloud-amd64-20260821-2577.qcow2`](https://cloud.debian.org/images/cloud/bookworm/20260821-2577/debian-12-genericcloud-amd64-20260821-2577.qcow2) | [`SHA512SUMS`](https://cloud.debian.org/images/cloud/bookworm/20260821-2577/SHA512SUMS) | SHA-512 | `c602f42a374c097bafcbc77c2d034fb06cb8a831d791bcbaa5d043f029874b0c32d41cb72ba8b6d50ccfd64c9b4b0dc9ade5b6e4065712f3eb152338e532721f` |
| `debian:13` | [`debian-13-genericcloud-amd64-20260826-2582.qcow2`](https://cloud.debian.org/images/cloud/trixie/20260826-2582/debian-13-genericcloud-amd64-20260826-2582.qcow2) | [`SHA512SUMS`](https://cloud.debian.org/images/cloud/trixie/20260826-2582/SHA512SUMS) | SHA-512 | `184761b0dad0f9ace02f9298050ca96ce3caa39a461a47706d47ff9698b59933918b91b40177fbd4d392f6446af8b4d18ecb94caca988169b19641606bf34003` |
| `fedora:44` | [`Fedora-Cloud-Base-Generic-44-1.7.x86_64.qcow2`](https://download.fedoraproject.org/pub/fedora/linux/releases/44/Cloud/x86_64/images/Fedora-Cloud-Base-Generic-44-1.7.x86_64.qcow2) | [`Fedora-Cloud-44-1.7-x86_64-CHECKSUM`](https://download.fedoraproject.org/pub/fedora/linux/releases/44/Cloud/x86_64/images/Fedora-Cloud-44-1.7-x86_64-CHECKSUM) | SHA-256 | `28680fe5b371a5a82ebf43a31926e086a168e59949d03969c5093e7071f90b7f` |

Ubuntu and Debian use GNU checksum lines. Fedora uses BSD-style lines inside an OpenPGP clearsigned document. Firestone's section 8.3 contract validates the digest record; it does not verify Fedora's OpenPGP signature.

The final verification run observed no redirect for Ubuntu. Debian images redirected once to the `acc.umu.se` mirror hosts `gemmei` and `saimei`; checksum documents stayed on `cloud.debian.org`. Fedora's image redirected once to `mirrors.tuna.tsinghua.edu.cn` and its checksum redirected once to `mirror.twds.com.tw`. An earlier successful run selected `ftp.yz.yamagata-u.ac.jp` and `mirrors.ustc.edu.cn`. Fedora's official redirector may choose different HTTPS community mirrors on each request. The validator requires the configured Fedora origin to be exactly `https://download.fedoraproject.org/pub/fedora/` and caps all redirects at five.

## SSH daemon path checks

The dated cloud metadata identifies the target distributions and their OpenSSH packages. Separate x86_64 package ownership checks on 2026-08-30 confirmed the executable path used by Firestone:

| Distribution | Package owner result | Executable |
|---|---|---|
| Ubuntu 24.04 | `openssh-server: /usr/sbin/sshd` | `/usr/sbin/sshd` |
| Ubuntu 22.04 | `openssh-server: /usr/sbin/sshd` | `/usr/sbin/sshd` |
| Debian 12 | `openssh-server: /usr/sbin/sshd` | `/usr/sbin/sshd` |
| Debian 13 | `openssh-server: /usr/sbin/sshd` | `/usr/sbin/sshd` |
| Fedora 44 | `openssh-server-10.2p1-14.fc44.x86_64` from `rpm -qf /usr/sbin/sshd` | `/usr/sbin/sshd` |

These package checks establish the path. E2E 11 must still prove that cloud-init starts SSH over vsock in each image.

## Reproduce the source check

Run from the repository root with Python 3.11 or newer:

```sh
python3 scripts/validate-catalog-images.py
python3 -m unittest scripts.tests.test_validate_catalog_images
```

The script bounds every response, requires strict HTTPS, rejects moving catalog paths, checks the exact allowed rows and policy fields, compares vendor metadata with the checksum document, and probes image availability without reading an image body.

## E2E 11 gate

The gated matrix is `scripts/m5-catalog-kvm-e2e.py`. It refuses a non-Linux, non-x86_64, non-KVM, insecure, or nonempty Firestone home. It installs and verifies pinned dependencies, creates each catalog machine with networking disabled, waits for the boot and SSH readiness steps, runs `id -u` over vsock SSH as root, verifies the stored image and metadata hashes, stops and removes the machine, and writes mode-0600 evidence outside the home. Every command and cleanup action has a deadline. It never records cloud-init contents or SSH key material.

Run only on a real KVM host:

```sh
FIRESTONE_E2E=1 FIRESTONE_HOME=$(mktemp -d) python3 scripts/m5-catalog-kvm-e2e.py
```

Without `FIRESTONE_E2E=1`, the script exits successfully with a skip message. It was exercised in skip mode on the local non-Linux workstation. No boot or SSH result is recorded here because the KVM host was unavailable.
