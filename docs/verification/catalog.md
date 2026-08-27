# Built-in image catalog source validation

Retrieval date: 2026-08-28

This record covers source availability for the initial catalog in SPEC.md section 8.1. It does not close `[verify 3]`. No image in this catalog has been booted by this work.

## Source selection

The Ubuntu entries come from the official `current/` directories for [Noble 24.04](https://cloud-images.ubuntu.com/noble/current/) and [Jammy 22.04](https://cloud-images.ubuntu.com/jammy/current/). Both directories identified their 2026-08-26 daily builds when checked.

The Debian entries come from the official `latest/` directories for [Bookworm 12](https://cloud.debian.org/images/cloud/bookworm/latest/) and [Trixie 13](https://cloud.debian.org/images/cloud/trixie/latest/).

The [Fedora Cloud download page](https://fedoraproject.org/cloud/download/) identified Fedora Cloud 44 as the current release, dated 2026-04-28. The page linked build `44-1.7` for both architectures. The catalog uses Fedora's `download.fedoraproject.org` mirror redirector. The `dl.fedoraproject.org` hostname served an HTML proof-of-work page to an automated client during validation, so it is unsuitable for Firestone's checksum fetch path. The validator requires the configured origin to be exactly `https://download.fedoraproject.org/pub/fedora/`, caps redirects at five, and rejects any redirect that leaves HTTPS. The redirector selects community mirrors outside the `fedoraproject.org` domain; those mirrors are not Fedora-operated hosts. The exact targets observed in the final local and Linux runs appear below. Firestone's later pull path must still verify the downloaded bytes against the checksum manifest as section 8.3 requires.

Ubuntu `current/` and Debian `latest/` are moving locations. Their filenames stay stable while file contents and digests change when upstream publishes a new build. Firestone must treat the checksum found during each explicit pull as the image identity, as required by section 8.3. Fedora 44 build `1.7` uses versioned paths; Fedora's redirector may select a different mirror for each request.

## Entries checked

The validator sent `HEAD` to each image URL and received HTTP 200. It fetched each checksum file with `GET`, received HTTP 200, and found one matching digest record for the exact image filename. It did not download image bodies.

| Reference | Architecture | Image | Checksum source | Algorithm | Digest observed on 2026-08-28 |
|---|---|---|---|---|---|
| `ubuntu:24.04` | `x86_64` | [`noble-server-cloudimg-amd64.img`](https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-amd64.img) | [`SHA256SUMS`](https://cloud-images.ubuntu.com/noble/current/SHA256SUMS) | SHA-256 | `d0fe84bb5f80853425fa6be28e2c106f30104c3cfe8611933f2e65c9b63f0e30` |
| `ubuntu:24.04` | `aarch64` | [`noble-server-cloudimg-arm64.img`](https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-arm64.img) | [`SHA256SUMS`](https://cloud-images.ubuntu.com/noble/current/SHA256SUMS) | SHA-256 | `afa139bac6f2629c1e1f2f8f34215f3a9ad9779801bcb945521ba1a45016743f` |
| `ubuntu:22.04` | `x86_64` | [`jammy-server-cloudimg-amd64.img`](https://cloud-images.ubuntu.com/jammy/current/jammy-server-cloudimg-amd64.img) | [`SHA256SUMS`](https://cloud-images.ubuntu.com/jammy/current/SHA256SUMS) | SHA-256 | `c0a5af17e6c0f76351fe07e2fffef3011dab1facb8a8ed5701dcf648dabd4f0a` |
| `ubuntu:22.04` | `aarch64` | [`jammy-server-cloudimg-arm64.img`](https://cloud-images.ubuntu.com/jammy/current/jammy-server-cloudimg-arm64.img) | [`SHA256SUMS`](https://cloud-images.ubuntu.com/jammy/current/SHA256SUMS) | SHA-256 | `81fb9116b507f68650e6a4de369fa4104731c8f205b4c48338e1b7d294a79e8e` |
| `debian:12` | `x86_64` | [`debian-12-genericcloud-amd64.qcow2`](https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-genericcloud-amd64.qcow2) | [`SHA512SUMS`](https://cloud.debian.org/images/cloud/bookworm/latest/SHA512SUMS) | SHA-512 | `c602f42a374c097bafcbc77c2d034fb06cb8a831d791bcbaa5d043f029874b0c32d41cb72ba8b6d50ccfd64c9b4b0dc9ade5b6e4065712f3eb152338e532721f` |
| `debian:12` | `aarch64` | [`debian-12-genericcloud-arm64.qcow2`](https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-genericcloud-arm64.qcow2) | [`SHA512SUMS`](https://cloud.debian.org/images/cloud/bookworm/latest/SHA512SUMS) | SHA-512 | `525c2ead4b8a905cab07106696761fda61e2431480bc70b1b1bcc5cab93823f1c04268918fc5e7a40b128e751a572a346416f5b829c0f926d29d71ccc40baac6` |
| `debian:13` | `x86_64` | [`debian-13-genericcloud-amd64.qcow2`](https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2) | [`SHA512SUMS`](https://cloud.debian.org/images/cloud/trixie/latest/SHA512SUMS) | SHA-512 | `184761b0dad0f9ace02f9298050ca96ce3caa39a461a47706d47ff9698b59933918b91b40177fbd4d392f6446af8b4d18ecb94caca988169b19641606bf34003` |
| `debian:13` | `aarch64` | [`debian-13-genericcloud-arm64.qcow2`](https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-arm64.qcow2) | [`SHA512SUMS`](https://cloud.debian.org/images/cloud/trixie/latest/SHA512SUMS) | SHA-512 | `6db58588c547771a2839a2deaa41b9732f55967ed9378095583fe80f6bfe2783387992dc54385b67019b91b0e1cc2e91160e5d84b985cd736f8208be76387ce7` |
| `fedora:44` | `x86_64` | [`Fedora-Cloud-Base-Generic-44-1.7.x86_64.qcow2`](https://download.fedoraproject.org/pub/fedora/linux/releases/44/Cloud/x86_64/images/Fedora-Cloud-Base-Generic-44-1.7.x86_64.qcow2) | [`Fedora-Cloud-44-1.7-x86_64-CHECKSUM`](https://download.fedoraproject.org/pub/fedora/linux/releases/44/Cloud/x86_64/images/Fedora-Cloud-44-1.7-x86_64-CHECKSUM) | SHA-256 | `28680fe5b371a5a82ebf43a31926e086a168e59949d03969c5093e7071f90b7f` |
| `fedora:44` | `aarch64` | [`Fedora-Cloud-Base-Generic-44-1.7.aarch64.qcow2`](https://download.fedoraproject.org/pub/fedora/linux/releases/44/Cloud/aarch64/images/Fedora-Cloud-Base-Generic-44-1.7.aarch64.qcow2) | [`Fedora-Cloud-44-1.7-aarch64-CHECKSUM`](https://download.fedoraproject.org/pub/fedora/linux/releases/44/Cloud/aarch64/images/Fedora-Cloud-44-1.7-aarch64-CHECKSUM) | SHA-256 | `55c60a3b80d3616a08705afd0459e75fe9f03c54aba7a46e4002a41a72fa0d5b` |

Ubuntu and Debian publish GNU-style checksum lines. Fedora publishes an OpenPGP-clearsigned file with BSD-style `SHA256 (filename) = digest` lines. The validator checks the digest record but does not verify the OpenPGP signature. Signature verification is not part of the section 8.3 pull contract.

## Firmware and release gate

Every entry declares `firmware = "rhf"`. Under section 9.1, `firmware = "auto"` selects that catalog value. This follows the section 21 decision to use RHF by default and keep edk2 as the fallback.

`[verify 3]` remains open for all ten reference and architecture pairs. Before release, the catalog matrix must boot each image with the pinned RHF and reach SSH. A failed RHF boot requires testing the pinned edk2 build and changing the catalog value through the SPEC decision process. Source availability alone does not establish firmware compatibility, cloud-init behavior, vsock SSH readiness, or a successful boot.

## Reproduce the source check

Run from the repository root with Python 3.11 or newer:

```sh
python3 scripts/validate-catalog-images.py
```

The script parses the section 8.1 fields, rejects duplicate references and defaults, probes image URLs without downloading image bodies, limits checksum responses to 2 MiB, and requires one unambiguous checksum record for every exact filename. Fedora source URLs must start at the official `download.fedoraproject.org` redirector. Redirects may end at an HTTPS community mirror selected by Fedora.

## Validation runs

- macOS arm64 local host, Darwin 25.6.0, Python 3.14.7: 10 entries passed. In the final run, the Fedora x86_64 image resolved to `mirror.twds.com.tw` and its checksum to `mirror.freedif.org`, with one redirect each. The aarch64 image resolved to `ftp.yz.yamagata-u.ac.jp` and its checksum to `mirrors.tuna.tsinghua.edu.cn`, also with one redirect each.
- Ubuntu 24.04 x86_64 Azure host, kernel 6.17.0-1022-azure, Python 3.12.3: 10 entries passed. The Fedora x86_64 image resolved to `ftp2.osuosl.org` after two redirects and its checksum to `nocix.mm.fcix.net` after one. The aarch64 image resolved to `mirror.web-ster.com` and its checksum to `mirror.fcix.net`, with one redirect each.

The Linux run checked network and parser behavior only. It did not use `/dev/kvm` or boot an image.
