---
icon: layer-group
---

# Images

The built-in catalog, the owned image store, and running an OCI container image as a VM.

## Images and the catalog

The built-in catalog holds the releases authorized by [SPEC section 8.1](../SPEC.md). Every entry names a dated vendor build, its matching checksum document, edk2 firmware, and the guest sshd path `/usr/sbin/sshd`. The login user is `root`, which is the spec default rather than a catalog field.

Print the catalog Firestone will actually resolve:

```sh
firestone catalog
```

The table merges the built-in entries with `~/.config/firestone/catalog.toml` and any extra catalogs in your config. It reports canonical references, aliases, available architectures and effective firmware. `images ls` is the separate list of what has already been downloaded.

| Reference | Aliases and default | Vendor build | Verification |
|---|---|---|---|
| `ubuntu:24.04` | `ubuntu`, `ubuntu:noble`; Ubuntu default | released build `20260826` | SHA-256 |
| `ubuntu:22.04` | `ubuntu:jammy` | released build `20260826` | SHA-256 |
| `debian:12` | `debian`, `debian:bookworm`; Debian default | official genericcloud `20260821-2577` | SHA-512 |
| `debian:13` | `debian:trixie` | official genericcloud `20260831-2587` | SHA-512 |
| `fedora:44` | `fedora`; Fedora default | stable Cloud Base `44-1.7` | SHA-256 |

The maintainer keeps a catalog source audit outside this repository, recording the vendor metadata URLs, the exact image URLs, the checksum sources and the observed digests. Availability of a source is not evidence that it boots; that comes from the catalog end-to-end run on a real KVM host.

Manage the owned store:

```sh
firestone images pull ubuntu:24.04
firestone images ls
firestone images inspect image-0123456789abcdef
firestone images rm image-0123456789abcdef
firestone images prune
```

Use the full image id `images ls` prints; the value above only shows the argument position. `images rm` refuses an image a machine or a published snapshot still references unless you pass `--force` or approve the prompt. `images prune` removes only unreferenced images.

A direct HTTPS source carries no catalog checksum. Supply the publisher's SHA-256 rather than accepting an unverified download:

```sh
firestone images pull "$IMAGE_URL" --sha256 "$IMAGE_SHA256"
```

Local raw and qcow2 files are copied into the store. Raw files are converted with `qemu-img`. Firestone never uses your file directly as a backing file, because a base image it does not own can change under a running machine.

## Container images

Firestone runs an OCI image as a VM. It pulls the image from a Registry V2 endpoint, merges the layers into one root filesystem, packs that into an ext4 image with a static `mkfs.ext4`, and boots it on the pinned Cloud Hypervisor kernel with `firestone-init` as PID 1. There is no cloud-init, no systemd and no sshd in the guest.

A reference is an OCI reference when it starts with `oci://` or `docker://`, or when it contains a `/` and its first component contains a `.` or a `:` or is `localhost`:

```sh
firestone create web docker.io/library/nginx -p 8080:80
firestone create app ghcr.io/owner/app:v1
firestone create pinned "docker://nginx@sha256:0000000000000000000000000000000000000000000000000000000000000000"
```

The third form pins a manifest by digest, and the zeroes above stand in for the 64 hexadecimal characters a real one carries. A bare `nginx` is a catalog lookup, not a container image; the error names `docker://nginx`. `--sha256` stays HTTPS-only, because a digest reference is how you pin a registry image. Docker Hub credentials are read from the `auths` object of `~/.docker/config.json`; `credsStore` and `credHelpers` are ignored with a warning. Registries reachable over plain HTTP must be listed literally in `images.insecure_registries` in `~/.config/firestone/config.toml`, and Docker Hub can never be listed.

### What works and what does not

The guest gets one static `/sbin/firestone-init` as PID 1. It mounts `/proc`, `/sys`, `/dev`, `/dev/pts`, `/run`, `/tmp` and `/dev/shm`, brings up loopback, grows the root filesystem online to the machine's `disk` size, sets the hostname, runs a five-second DHCP exchange on `eth0` unless the network mode is `none`, and then runs the image's entrypoint and command as its child. It stays PID 1 for the life of the machine so it can reap orphans, forward `SIGTERM` and `SIGINT` to the child's process group, and power the machine off cleanly when the entrypoint exits.

The console and the logs are the surfaces that work:

```sh
firestone console web
firestone logs web --follow --source console
```

The entrypoint's stdout and stderr reach both `hvc0` and `console.log`, so you read a container image's output exactly where you would read a kernel message.

The things that do not work follow from having no sshd, and Firestone would rather you know them here than discover them at a timeout:

- `firestone shell`, `firestone ssh-config` and `firestone cp` refuse immediately with a usage error that names `console` and `logs`. There is no sshd, no vsock SSH listener, and no guest host key, so Firestone does not try to connect.
- `firestone start` is ready as soon as the machine is running. There is no SSH wait on a container machine, so `start` returns when the boot completes.
- `firestone stop` falls through to the force path. `firestone-init` installs no ACPI power-button handler, so the power button goes unanswered and the stop sequence escalates after its timeout. `stop --force` skips the wait. The guest's own entrypoint exit is the clean shutdown, and init syncs before it powers off.
- Any `[cloud_init]` key on an OCI machine is a validation error naming the offending key, before any registry request. The hostname, network, disk size, entrypoint, command, environment, working directory and user all come from the config disk, which Firestone renders from the image's own metadata.
- SELinux labels are dropped. Only `security.capability` and `gnu.translator` extended attributes survive the layer merge, which is what the packing tool accepts. An SELinux-labeled image boots unlabeled. That is documented behavior, not a defect.
- Only gzip layers are decompressed. A `zstd` layer or a foreign one is refused with a message naming the media type.

Related: [machines](machines.md) for what to do with one once it boots, and [networking](networking.md) for publishing a container port. The page list is in the [documentation index](README.md).
