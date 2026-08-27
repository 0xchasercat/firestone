# Dependency pin evidence

This record covers the M0 dependency manifest and the source-level portions of SPEC verify items 1, 2, and 12. All checks below use exact release tags. Runtime boot claims stay open until M1.

## Pinned releases and downloaded hashes

`scripts/pin-deps.sh` uses only the URLs in `deps.toml`. It never queries a latest-release endpoint. The checksums below came from downloaded bytes, then a second `verify --arch all` run downloaded and checked every URL again.

| Dependency | Architecture | Release asset | SHA-256 |
|---|---|---|---|
| cloud-hypervisor v53.0 | x86_64 | `cloud-hypervisor-static` | `448af3d4e59b22c2987f7df94c213ad40fb53a10d437e42b5ee6c4fce7c29ecc` |
| cloud-hypervisor v53.0 | aarch64 | `cloud-hypervisor-static-aarch64` | `f192b510eea1c710cbc439d716bb0573c223fc463dbe3e6523788a2b7ef62850` |
| Rust Hypervisor Firmware 0.5.0 | x86_64 | `hypervisor-fw` | `4a0a1e977368f6b15d2198a216bdedf9a350bf5e5ae07e29e695373ec16ad958` |
| Rust Hypervisor Firmware 0.5.0 | aarch64 | `hypervisor-fw-aarch64` | `2a22aed888572ae319e231b85a7b4de951c7eca8857730300653512d064c8102` |
| cloud-hypervisor edk2 ch-1e1b96f126 | x86_64 | `CLOUDHV.fd` | `9fb511fc0dd423d90a79615a90a8ace9b9e078b4a115ea2c459e0ac2f4e60218` |
| cloud-hypervisor edk2 ch-1e1b96f126 | aarch64 | `CLOUDHV_EFI.fd` | `460cefa75c72461745ac2f8e828ac8646475f93823101980dfc3f5967175c1ef` |
| virtiofsd v1.14.0 | source | `virtiofsd-v1.14.0.tar.gz` | `52b66e449ca583b4f050a2bff327ff812211a2c349b4130279fcfc6a64540f04` |

Primary release records:

- [cloud-hypervisor v53.0](https://github.com/cloud-hypervisor/cloud-hypervisor/releases/tag/v53.0)
- [Rust Hypervisor Firmware 0.5.0](https://github.com/cloud-hypervisor/rust-hypervisor-firmware/releases/tag/0.5.0)
- [cloud-hypervisor edk2 ch-1e1b96f126](https://github.com/cloud-hypervisor/edk2/releases/tag/ch-1e1b96f126)
- [virtiofsd v1.14.0](https://gitlab.com/virtio-fs/virtiofsd/-/releases/v1.14.0)

The edk2 choice is deliberately older than the latest edk2 release. Cloud Hypervisor v53.0 names ch-1e1b96f126 for both architectures in its [integration asset manifest](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v53.0/scripts/test_assets.yaml), giving this pair better evidence than an edk2 build published after v53.0.

## virtiofsd binary gap

The virtiofsd v1.14.0 release exposes only GitLab-generated source archives. It has no versioned binary links. The pinned tag resolves to commit `c2540f8db14caba81c1e37fba23fc7bf2cd7f0dd`.

The release source includes a static musl build job in [`.gitlab-ci.yml`](https://gitlab.com/virtio-fs/virtiofsd/-/blob/v1.14.0/.gitlab-ci.yml), but that job has two properties that prevent Firestone from using its output:

- It runs only for the `main` branch and produces a mutable CI artifact instead of a versioned release asset.
- It publishes only `x86_64-unknown-linux-musl`. There is no upstream `aarch64-unknown-linux-musl` job or artifact.

The v1.14.0 source does compile and link for both required musl targets. The following release builds used the archive pinned in `deps.toml`, its committed `Cargo.lock`, Rust 1.98.0, static libcap-ng and libseccomp, and the same static-link flags as upstream:

| Target | Builder | Result |
|---|---|---|
| `x86_64-unknown-linux-musl` | `rust:alpine` image manifest `sha256:a10e64dd139b7387337c7fbe8aca31b959b57b2fd4c8ae20a02cf1d6ea424dce`; libcap-ng 0.8.5-r2; libseccomp 2.6.0-r2 | `cargo build --locked --release` succeeded; the output was x86-64 static PIE with no `PT_INTERP` program header |
| `aarch64-unknown-linux-musl` | cross-rs image manifest `sha256:f604e399cbb2154ddeb013db99eb4f123d24f09a579c7e8d6ed631d15ffa8b12`; Alpine 3.22 aarch64 libcap-ng 0.8.5-r0 and libseccomp 2.6.0-r0 | `cargo build --locked --release` succeeded; the output was an AArch64 static executable with no `PT_INTERP` program header |

These builds establish source support for both Firestone release targets. They do not supply a distribution pin. Firestone still needs a repository-owned build recipe that pins the Rust toolchain, builder images, native static libraries, and build flags, then reproduces and publishes checksums for both outputs. The manifest therefore records virtiofsd as `source-only` with both binary architectures missing. The mutable x86_64 CI artifact and distro packages are not substitutes for versioned Firestone artifacts.

## Verify 1: firmware mapping

Evidence from the exact v53.0 source and binary:

- The [v53.0 boot documentation](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v53.0/README.md#booting-linux) identifies `CLOUDHV.fd` for x86_64 and `CLOUDHV_EFI.fd` for aarch64 as edk2 firmware. It also states that `hypervisor-fw` has a Xen PVH entry and may be passed through the kernel input.
- [`PayloadConfig` in the v53.0 OpenAPI document](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v53.0/vmm/src/api/openapi/cloud-hypervisor.yaml) has the exact optional fields `kernel` and `firmware`.
- The downloaded x86_64 binary reported `cloud-hypervisor v53.0`; its help exposes both `--kernel` and `--firmware`.

Firestone will map RHF 0.5.0 to `payload.kernel` and edk2 ch-1e1b96f126 to `payload.firmware`. This resolves the API mapping. It does not show that either firmware boots a catalog image. Both boot checks remain open for M1 and the catalog matrix.

## Verify 2: API paths, methods, and VmConfig

The [pinned OpenAPI document](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v53.0/vmm/src/api/openapi/cloud-hypervisor.yaml) defines server base path `/api/v1` and the methods Firestone needs:

| Method | Path | OpenAPI success |
|---|---|---|
| GET | `/api/v1/vmm.ping` | 200 |
| PUT | `/api/v1/vmm.shutdown` | 204; the v53.0 handler returns 200 |
| GET | `/api/v1/vm.info` | 200 |
| PUT | `/api/v1/vm.create` | 204 |
| PUT | `/api/v1/vm.boot` | 204 |
| PUT | `/api/v1/vm.pause` | 204 |
| PUT | `/api/v1/vm.resume` | 204 |
| PUT | `/api/v1/vm.shutdown` | 204 |
| PUT | `/api/v1/vm.power-button` | 204 |

`ch-remote v53.0 --help` exposes matching `ping`, `create`, `boot`, `info`, `power-button`, `pause`, `resume`, `shutdown`, and `shutdown-vmm` commands.

The OpenAPI document and implementation disagree on the success status for `vmm.shutdown`. The v53.0 `VmmShutdown` HTTP handler constructs `StatusCode::OK`, and a direct request on the Linux validation host returned HTTP 200 before the VMM process exited with status 0. Firestone must accept that exact-version behavior. The ordinary VM action handler returns 204 for successful empty responses, matching the OpenAPI entries for `vm.create`, `vm.boot`, `vm.pause`, `vm.resume`, `vm.shutdown`, and `vm.power-button`.

The section 9.2 JSON names and values map to these v53.0 schemas:

| Firestone JSON | v53.0 schema detail |
|---|---|
| `cpus.boot_vcpus`, `cpus.max_vcpus` | required integer fields of `CpusConfig` |
| `memory.size`, `memory.shared` | byte count and boolean fields of `MemoryConfig` |
| `payload.kernel`, `payload.firmware` | optional path fields of `PayloadConfig` |
| `disks[].path`, `readonly`, `image_type`, `backing_files` | fields of `DiskConfig`; image enum values are case-sensitive `Qcow2` and `Raw` |
| `net[].vhost_user`, `vhost_socket`, `vhost_mode`, `mac` | fields of `NetConfig`; `Client` is the documented enum spelling and default |
| `fs[].tag`, `socket`, `num_queues`, `queue_size` | required fields of `FsConfig` |
| `vsock.cid`, `vsock.socket` | required fields of `VsockConfig`; CID minimum is 3 |
| `serial.mode`, `serial.file` | `SerialConfig`; `File` is an exact `ConsoleMode` enum value |
| `console.mode`, `console.socket` | `ConsoleConfig`; `Socket` is an exact `ConsoleMode` enum value |
| `rng.src` | required path field of `RngConfig` |

Cloud Hypervisor v53.0 auto-detects disk formats only as a deprecated fallback. More importantly, its device manager disables a detected qcow2 backing chain when `backing_files` was not enabled. The v53.0 integration tests pass `image_type=qcow2,backing_files=on` for a qcow2 disk with a backing file. Firestone's overlay entry must therefore use JSON `"image_type": "Qcow2"` and `"backing_files": true`. The vfat `seed.img` is a raw block image and uses `"image_type": "Raw"` with `"readonly": true`.

On `firestone@172.203.242.136`, the downloaded x86_64 binary started with only an API socket and log file. `ch-remote ping` and direct `GET /api/v1/vmm.ping` both returned the v53.0 build response, and the direct request returned HTTP 200. Direct `PUT /api/v1/vmm.shutdown` returned HTTP 200 and stopped the process with exit status 0. No VM was created or booted, so payload acceptance and boot behavior remain M1 checks.

## Verify 12: host-to-guest vsock handshake

The pinned [`docs/vsock.md`](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v53.0/docs/vsock.md) requires the host to send `CONNECT <port>\n` once on the Unix socket. The v53.0 [Unix muxer source](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v53.0/virtio-devices/src/vsock/unix/muxer.rs) adds the response detail:

1. Parse the requested guest port from `CONNECT <guest-port>\n`.
2. Allocate a host-side local port and send a virtio-vsock connection request to the guest.
3. After the guest response moves the connection to `Established`, write `OK <allocated-local-port>\n` to the host Unix stream.
4. Relay application bytes after the acknowledgement. Invalid requests close or reset the connection; the source does not define a textual `ERR` response contract.

The pinned source test asserts the same acknowledgement. A Unix socket connection by itself does not prove that guest port 22 is ready. Firestone's proxy must read the complete `OK` line before starting SSH traffic. A raw `socat` test needs a booted guest listener and remains open for M1.

## Reproduction

From the repository root:

```sh
scripts/pin-deps.sh verify
scripts/pin-deps.sh verify --arch all
```

`verify` selects `x86_64` or `aarch64` from `uname -m`. `refresh --arch all` downloads the same exact URLs and atomically rewrites the manifest. A refresh to a temporary path produced a byte-identical file during this verification.
