# Firestone user guide

Firestone runs Linux virtual machines on Cloud Hypervisor, and it runs OCI container images as virtual machines. It is one executable that carries the VMM, `passt` and `qemu-img` inside it. Firestone runs as your user, nothing of it keeps running between your commands, and every machine is a directory of files you can read.

This guide covers the whole shipped surface, in the order you are likely to meet it. The design behind each behavior is in [SPEC.md](../SPEC.md); this document is about using the tool.

Firestone targets Linux x86_64 hosts with KVM. The aarch64 target compiles and its catalog metadata exists, but there is no aarch64 runtime release and `doctor` refuses an aarch64 host. Non-Linux hosts, non-Linux guests, and cross-architecture emulation are out of scope rather than half-supported.

## Install

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

### Build from source

Rust 1.85 or newer builds the workspace. A plain `cargo build` is a development build. It keeps a PATH fallback for the helper binaries, so you can develop against your own `passt` or `qemu-img`.

```sh
cargo build --release
```

For the standalone release build, which embeds the pinned helpers and refuses a missing or mismatched input, use the release script:

```sh
scripts/build-release.sh --target x86_64-unknown-linux-musl
```

## Check the host

Run the read-only check first, then let Firestone repair what it owns:

```sh
firestone doctor
firestone doctor --fix
firestone doctor
```

`doctor --fix` creates Firestone's directories, materializes the embedded Cloud Hypervisor, `passt` and `qemu-img` executables, downloads and checksum-verifies the pinned firmware and `virtiofsd`, and generates Firestone's SSH key. It never changes a sysctl, a device permission, or a machine. The one privileged path is narrow. When Ubuntu's AppArmor blocks the user namespace `passt` needs, an interactive `doctor --fix` prints the literal root-owned helper and profile commands and asks before running them. `--yes` and `--json` never authorize that; a non-interactive run prints the commands and stops.

Each check reports `ok`, `warn` or `fail`. A failed report exits 5. Warnings do not block anything.

| Check | What to do |
|---|---|
| Host architecture | Use Linux x86_64. The aarch64 runtime is deferred. |
| `/dev/kvm` missing | Enable virtualization in firmware and load `kvm_intel` or `kvm_amd`. A guest VM or CI runner may also need nested virtualization turned on. |
| `/dev/kvm` permission denied | Run the group command doctor prints, normally `sudo usermod -aG kvm $USER`, then log out and back in. Doctor reads the device's real group name rather than assuming `kvm`. |
| Runtime directory | Set `XDG_RUNTIME_DIR` to a user-owned mode-0700 directory. Without it Firestone uses `/tmp/firestone-<uid>` and warns. `doctor --fix` creates that fallback safely. |
| Vendored binaries, embedded helpers, or the Firestone SSH key | Run `firestone doctor --fix`. A corrupt embedded helper is refused, never overwritten. |
| `passt` | The standalone binary carries the exact helper. If AppArmor restricts unprivileged user namespaces, review the literal `/usr/libexec/firestone/passt-2025_02_17.a1e48a0` profile commands doctor prints. Firestone never grants `userns,` to `~/.local/share/firestone/bin/*` or any other user-writable path. |
| `qemu-img` | The standalone binary carries qemu-img 8.2.2 and materializes it on the first image operation or on `doctor --fix`. It needs no user namespace. |
| OpenSSH | Install `openssh-client` on Ubuntu, `openssh-clients` on Fedora, or `openssh` on Arch. |
| User namespaces | Doctor probes `passt` in the same foreground one-off vhost-user mode a VM uses. Both `Couldn't create user namespace` and `Failed to detach isolating namespaces` are fatal. A denial that affects only `virtiofsd` warns and falls back to `--sandbox none`. |
| Free space | Free space on the filesystem holding the data directory. The warning threshold is 5 GB. |
| Stale state | Doctor and ordinary reads reconcile recorded state against live processes and sockets. Repair the named path or lock error when reconciliation itself fails. |

A normal `start` does not run the rest of doctor implicitly. It downloads and publishes one missing pinned artifact when the machine needs it: the selected firmware for a disk image, or the direct-boot kernel for a container image. A custom firmware path is used as it is and never rewritten.

## Your first machine

The shortest path creates a machine named `ubuntu`, pulls the default Ubuntu image, boots it, waits for SSH over vsock, and drops you at a root prompt:

```sh
firestone run ubuntu
```

Run one guest command instead of an interactive shell:

```sh
firestone run ubuntu -- uname -a
```

Create a throwaway machine and delete it when the command exits:

```sh
firestone run ubuntu --name scratch --rm -- true
```

The first boot downloads and verifies the image, installs the pinned firmware when it is missing, and runs cloud-init. Later starts reuse the owned firmware, the cached base image, and the machine's own qcow2 overlay.

`run` with no argument uses `ubuntu`. That default is the one opinion Firestone has about which distribution you want.

## The web interface

`firestone ui` serves the same API and an embedded web interface on an ephemeral loopback port, then opens your browser:

```sh
firestone ui
```

It prints a URL carrying a session token generated for this run only:

```text
Firestone UI   http://127.0.0.1:47318/?token=<64 hex>
Press Ctrl-C to stop.
```

The first page load trades that token for an `HttpOnly`, `SameSite=Strict` cookie and rewrites the address bar, so the token does not sit in browser history. The token lives in process memory; stopping `firestone ui` invalidates it.

On a headless host, print the URL and reach it through an SSH tunnel. Firestone refuses to bind a routable address, so a tunnel is the supported path:

```sh
firestone ui --no-open
```

`--print-url` prints the URL and nothing else, which is what you want in a script. Add `--json` for a machine-readable record of address, port and URL.

### What the interface holds

The overview screen carries a host summary, the doctor report, and panels for machines and the image cache. `/machines` lists every machine with its status, image, CPU and memory, uptime, and forward chips; a forward chip is a link when the machine is running, the protocol is TCP, and the host side is a single port. Anything else renders as a plain chip, because a link that navigates nowhere teaches you to distrust the rest.

The machine detail page has four tabs. `spec` renders the effective specification, `logs` renders the guest console with its own ANSI colors, `snapshots` lists and manages snapshots, and `vmconfig` shows the exact JSON Firestone handed the VMM. Above the tabs, a running machine carries a live utilization strip: CPU per cent, memory against allocation, and disk throughput, drawn as sparklines from samples the browser takes every three seconds. History is a 60-sample ring buffer per browser tab. It is not stored on the host, so a reload starts over. That is the honest consequence of running no metrics daemon.

Machine creation, editing, cloning, snapshot create and restore, image delete, and both prunes are dialogs. Every one of them writes to the documented `/v1` endpoints and renders the resulting NDJSON as it arrives, so what the browser does is what `curl` would do. The system-prune dialog previews before it removes: it runs the same request with `dry_run` set, renders the list it gets back, and only then enables the confirm button.

`⌘K` or `/` opens a command palette over machines, catalog entries, and the actions the screens themselves offer. It deliberately has no start, stop, restart or delete entry. Those four render their progress on the button that dispatched them, and a palette row has no button.

### The terminal page

`/machines/<name>/terminal` is a full-window terminal with Console and Shell tabs. Console attaches to the guest's `hvc0` through a WebSocket, which is the same single-client console `firestone console` takes; if the CLI already holds it, the page says so and names `firestone console <name>`. Shell opens an SSH session on a host pseudo-terminal over the same transport `firestone shell` uses.

Both tabs need a running machine, and the Terminal link appears on the detail page only while a machine is running. The page itself renders for a machine in any state, because a terminal that cannot attach should say why rather than return a 404. Nothing reconnects on its own; every failure overlay offers a Reconnect button and waits for you to decide.

If the browser cannot instantiate the terminal emulator, the page falls back to a plain transcript that strips the sequences it cannot draw and still sends keystrokes. It says in the footer that it is degraded.

Everything the interface needs is compiled into the binary. No CDN, no outbound request, no second process.

## Machine lifecycle

Create without booting:

```sh
firestone create dev ubuntu --cpus 4 --memory 4G --disk 40G
```

On an interactive terminal, `create` opens an arrow-key selector over the merged catalog. The last option takes an HTTPS URL or a local path. It then walks you through name, CPU, memory, disk and network, using any values you supplied as the defaults. `--yes` skips the wizard; `--json` and any non-terminal invocation are always deterministic and non-interactive.

After it writes the file, `create` prints the effective image, resources, network, forwards and mounts, the exact `firestone.toml` path, and the `firestone edit` and `firestone start` commands to run next. `firestone create --help` lists every flag that sets a field.

Start, inspect and enter the machine:

```sh
firestone start dev
firestone ls
firestone show dev
firestone shell dev
firestone shell dev -- id -u
```

`start` waits for boot and SSH readiness. Use `--no-wait` when another process will check readiness itself:

```sh
firestone start dev --no-wait
```

Attach the serial console for boot diagnosis or rescue work:

```sh
firestone console dev
```

Press `Ctrl-]` to detach. The console needs a terminal on stdin, stdout and stderr; it does not work through a pipe or under `--json`. Use `logs --source console` in a script.

Edit the specification and read the logs:

```sh
firestone edit dev
firestone logs dev
firestone logs dev -n 500 --source console
firestone logs dev -n 200 --source vmm
firestone logs dev --follow --source console
```

`edit` opens `firestone.toml` in `$VISUAL`, then `$EDITOR`, otherwise `nano`, and validates on save, reopening the editor on a rejection. Log sources are `console`, `vmm`, `shim`, `passt` and `virtiofsd-N`. Firestone opens only current-user-owned mode-0600 regular files and never follows the final symlink. A reverse tail is capped at 8 MiB, and follow mode reopens a safe rotation and stops on `Ctrl-C`.

Cloud-init contents and private keys never reach a log. A command you run inside the guest can still print a secret to the console, so watch what you run there.

Stop gracefully, force only when you must, and remove:

```sh
firestone stop dev
firestone stop dev --force
firestone restart dev
firestone rm dev
```

`stop` presses the ACPI power button and waits; on timeout it escalates to SIGTERM and then SIGKILL. `--force` skips straight to the kill. `rm` deletes the whole machine directory: overlay, seed, snapshots, logs and known-hosts file. Shared base images stay in the store until `images rm` or a prune removes them.

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

The [catalog source audit](verification/catalog.md) records the vendor metadata URLs, the exact image URLs, the checksum sources and the observed digests. Availability of a source is not evidence that it boots; that comes from the catalog end-to-end run on a real KVM host.

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

- `firestone shell` and `firestone cp` cannot connect. There is no sshd, no vsock SSH listener, and no guest host key. Use `console` and `logs`.
- `firestone start` waits for SSH readiness whenever Firestone provisioning is on, which is the default and cannot be turned off for an OCI machine. Start a container machine with `firestone start web --no-wait`, then watch `logs`. The VM boots; it is only the wait that has nothing to wait for. Making start ready as soon as the shim reports running, and making `shell` refuse immediately with a usage error, are both specified and not yet implemented.
- `firestone stop` falls through to the force path. `firestone-init` installs no ACPI power-button handler, so the power button goes unanswered and the stop sequence escalates after its timeout. `stop --force` skips the wait. The guest's own entrypoint exit is the clean shutdown, and init syncs before it powers off.
- Any `[cloud_init]` key on an OCI machine is a validation error naming the offending key, before any registry request. The hostname, network, disk size, entrypoint, command, environment, working directory and user all come from the config disk, which Firestone renders from the image's own metadata.
- SELinux labels are dropped. Only `security.capability` and `gnu.translator` extended attributes survive the layer merge, which is what the packing tool accepts. An SELinux-labeled image boots unlabeled. That is documented behavior, not a defect.
- Only gzip layers are decompressed. A `zstd` layer or a foreign one is refused with a message naming the media type.

## Snapshots

A snapshot is an immutable copy of one machine at one instant. There are two tiers and Firestone always tells you which one it took.

A **cold** snapshot is taken from a `created` or `stopped` machine. It copies the spec, the published VM configuration when one exists, and the qcow2 overlay onto the same base image. It is a file copy of a quiescent machine, so it is guaranteed.

A **warm** snapshot is taken from a running machine. It captures everything a cold snapshot does, plus the Cloud Hypervisor VM state written while the guest is paused. Restoring it resumes the guest at the instruction it was paused on. Warm is verified rather than guaranteed. It depends on the VMM's own snapshot and restore, and it refuses to run unless the machine directory and runtime layout the snapshot baked in are unchanged.

```sh
firestone snapshot create dev
firestone snapshot create dev before-upgrade
firestone snapshot list dev
```

Without a name, the snapshot is `snap-<yyyymmdd>-<hhmmss>` from the UTC instant of the request. The machine's status picks the tier; you do not.

The pause is real and worth naming. A warm snapshot pauses the guest, writes its memory and copies its overlay, then resumes. An idle guest writes roughly a third of its RAM, and both the memory image and the overlay copy preserve holes, so a snapshot costs what the guest actually touched rather than what it was promised. Firestone refuses to start a warm snapshot when free space is below guest memory plus the overlay's allocated size.

If the resume after a snapshot fails, the machine is marked degraded with `vmm paused after a failed snapshot resume`, the action fails, and the hint names `firestone restart`. A machine Firestone cannot resume is a visible fault, never a quiet one.

Restore is a whole-machine rollback, not a disk rollback:

```sh
firestone snapshot restore dev before-upgrade
firestone snapshot restore dev before-upgrade --force --start
firestone snapshot rm dev before-upgrade
```

The snapshot's `disk.qcow2`, `firestone.toml` and VM configuration all replace the machine's own, so the machine returns to the configuration its disk was captured under. Anything else would restore a disk into a machine whose spec had moved on. A running machine is refused unless `--force` stops it first. After a cold restore the machine is stopped and startable, and `--start` starts it. A warm restore always starts the machine, because a memory image only means something resumed; `--start` is redundant there.

Machine identity survives a restore. The MAC and the cloud-init instance id derive from the machine name, and the guest's SSH host keys live inside the restored overlay, so `known_hosts` stays valid and nothing prompts you to re-trust the machine.

A published snapshot pins its base image. `images rm` refuses it and `images prune` keeps it, exactly as they do for a running machine's image. `firestone rm` deletes a machine's snapshots with the machine directory and warns first, naming them, unless you passed `--force`.

## Clone

`clone` copies a machine definition, and by default its writable disk, into a new machine that has never run:

```sh
firestone clone dev dev-experiment
firestone clone dev dev-clean --fresh-disk
```

The source must be `created` or `stopped`; a running machine is refused before any lock is taken, with a hint naming `firestone stop`. The destination's `firestone.toml` is the source document byte for byte, and its `state.json` is written fresh with the source's pinned image identity, so the clone reuses the exact verified base rather than resolving the reference again.

The default copy is a full qcow2 overlay copy that keeps the shared base, so packages you installed in the source guest are in the clone. What it costs is the size of the overlay rather than the size of the base image, which is why a lightly used machine clones in seconds. `--fresh-disk` gives the clone an empty overlay on the same base instead.

Nothing runtime is carried over: no `known_hosts`, no seed, no VM configuration, no logs, no snapshots. The MAC address and the instance id are derived from the machine name at first start, so the clone allocates its own.

Two limits are worth knowing before you clone something important. A source that pins `network.mac` explicitly passes that address to the clone verbatim, and Firestone warns naming both machines; change it before running both on the same L2 segment. And a copied overlay carries the guest's `/etc/machine-id` and `/etc/hostname`, because Firestone does not rewrite guest filesystems. Run `systemd-firstboot --setup-machine-id` inside the clone when a unique guest identity matters, or use `--fresh-disk`.

## Resize

`resize` changes CPU count, memory, or both:

```sh
firestone resize dev --cpus 4
firestone resize dev --memory 8G
firestone resize dev --cpus 4 --memory 8G
```

On a machine that is not running, this is a spec patch and applies at the next start. On a running machine, Firestone changes the live VM and then writes the same values to `firestone.toml`, so desired state matches what is running. The result says which happened, in the `applied_live` field, so a script never has to guess.

A live resize needs headroom, and headroom is a property of how the machine booted rather than of the file it will boot from next. Reserve it when you create the machine:

```sh
firestone create big ubuntu --cpus 2 --cpus-max 8 --memory 4G --memory-max 16G
```

A request above `cpus_max`, above memory plus the hotplug reservation, or below the booted memory size is refused with a hint naming `cpus_max`, `memory_max` and a restart. Raising those numbers in the file does not widen a machine that is already running.

Hotplugged memory comes online by itself. Hotplugged vCPUs arrive offline, so Firestone's cloud-init part installs a udev rule that onlines them, and a live CPU resize reaches the guest scheduler without a login.

Disk is deliberately not part of `resize`. Raise `disk` in the spec and start the machine: Firestone grows the overlay with `qemu-img resize` before it validates it, and the `disk` step reports `grown to <size> overlay`. Growing the disk image does not grow the guest partition; cloud-init's `growpart`, which is already in Firestone's part, extends the root partition and filesystem on that boot. Lowering `disk` below the overlay's virtual size is rejected at validation, because a qcow2 shrink would truncate the guest filesystem.

## Metrics

`metrics` reads one sample on demand from the machine's VMM API socket and the host `/proc` entries for its VMM process:

```sh
firestone metrics dev
```

```text
sampled at 2026-09-02T12:00:00Z
cpu       2 vcpus, 9500000000 ns cpu time
memory    - bytes rss, 2147483648 bytes allocated, 1073741824 bytes guest actual
DEVICE      READ BYTES   WRITTEN BYTES    READ OPS   WRITE OPS
_disk0            4096            8192           2           -
net       none reported
```

Firestone runs no metrics daemon and stores no time series. Every device figure is cumulative since the VMM started, so a rate is two samples divided by the wall clock between their `sampled_at` values. That is exactly what the web interface does in the browser.

An unavailable figure prints as `-` and serializes as `null`. It is never a zero, a guess or a saturating number. A device that reports no counter is left out of a sum rather than counted as idle. Network counters are usually absent, because the default vhost-user `passt` path reports no network devices at all.

A machine that is not running fails with `conflict`, which is exit code 4 and HTTP 409.

## Networking and port forwards

`passt` is the default network mode. It gives the guest outbound access with no root, no bridge and no host firewall change. Inbound needs an explicit forward:

```sh
firestone create web ubuntu -p 8080:80
firestone create dns ubuntu -p udp:5353:53
firestone create private-web ubuntu -p 127.0.0.1:8080:80
firestone create range ubuntu -p 8000-8010:8000-8010
```

A forward with no bind address listens on every host address. Bind to `127.0.0.1` when a service should stay local. Two passt guests reach each other only through forwarded host ports.

### Forwards apply on restart

`passt` fixes its mappings when it spawns and offers no way to change them afterwards, and a Cloud Hypervisor vhost-user session does not survive a `passt` restart. There is no hot-apply for port forwards, and Firestone does not pretend otherwise.

Editing forwards on a running machine leaves the configured set and the applied set different. `ls` marks that row's `FORWARDS` cell with a trailing `*` and prints the legend `* forwards pending restart` after the table; the cell still shows the applied forwards, because those are the ones you can reach right now. `show` prints `forwards pending restart` to stderr, keeping stdout one valid JSON document. A spec write through `edit`, `PUT` or `PATCH` emits the warning `port forwards apply on restart`. The web interface renders a `pending restart` badge beside the forward chips, never instead of them.

`firestone restart NAME` clears it. Nothing else does.

The comparison is canonical rather than textual, so respelling `8080:80` as `tcp:8080:80`, or contracting an IPv6 literal, is not a pending change.

### Isolated machines and tap mode

Vsock SSH does not depend on guest networking, so a machine with no network device still has `shell`, `console` and mounts:

```sh
firestone create isolated ubuntu --net none
firestone start isolated
firestone shell isolated
```

For an ad hoc tunnel into such a machine, generate an OpenSSH config and use ordinary OpenSSH forwarding:

```sh
firestone ssh-config isolated > "$HOME/.ssh/firestone-isolated.conf"
ssh -F "$HOME/.ssh/firestone-isolated.conf" -L 8080:127.0.0.1:80 firestone.isolated
```

Tap mode is for a bridge you administer. Firestone never creates the tap, the bridge, a DHCP server, a NAT rule or a firewall rule. An administrator does the one-time setup:

```sh
sudo ip tuntap add dev tap0 mode tap user "$USER"
sudo ip link set tap0 master br0
sudo ip link set tap0 up
```

Then create the machine as the ordinary Firestone user:

```sh
firestone create bridged ubuntu --net tap --tap tap0
```

The tap must exist under `/sys/class/net`, must be a tap device, and `/dev/net/tun` must be openable. Port forwards belong to passt mode and are rejected with `tap` or `none`.

## Shared folders

A mount exposes a host directory to the guest over virtiofs:

```sh
firestone create work ubuntu --mount "$PWD:/work"
firestone create review ubuntu --mount "$PWD:/src:ro"
```

Firestone starts one pinned `virtiofsd` per mount. Treat a read-write mount as guest write access to that host tree, because that is what it is. A `:ro` mount limits guest writes, but it is not a reason to share a tree holding secrets. If user namespaces are unavailable, doctor warns that `virtiofsd` runs with `--sandbox none`.

## Cloud-init, keys, and passwords

Firestone's own cloud-init part enables key-only root SSH over vsock, gives the image's default user the same authorized keys, creates the serial getty, grows the root filesystem, onlines hotplugged CPUs, and mounts shared folders. Password SSH stays off unless you turn it on.

Add your own cloud-config from a file:

```sh
cat > user-data.yaml <<'EOF'
#cloud-config
packages:
  - jq
write_files:
  - path: /etc/motd
    permissions: "0644"
    content: "managed by cloud-init\n"
EOF
firestone create configured ubuntu --user-data user-data.yaml
```

Small user-data can live in the machine specification instead. `--user-data` and `--user-data-inline` are mutually exclusive, and identical bytes produce an identical guest either way:

```sh
user_data=$(printf '#cloud-config\npackages: [htop]\n')
firestone create inline ubuntu --user-data-inline "$user_data"
```

Add public keys from files, from the command line, or both:

```sh
firestone create keyed ubuntu --ssh-key "$HOME/.ssh/id_ed25519.pub"
firestone create pasted ubuntu --ssh-authorized-key "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKg0J8YPh7wARkZSlBzFAoJez6gssTQUuPu4Qy3z8T1P me@laptop"
```

Inline keys are validated exactly like key files, and a key given both ways is written once. Firestone reads its own public key and the public key files you name. It never puts a private key in the seed or in a log.

Set a console login password for `--user` when key-only access is not enough. It is read from a file so it never appears in the process list:

```sh
firestone create console-login ubuntu --password-file ./password.txt
```

The password reaches the guest through cloud-init `chpasswd`, so `firestone console` and a local login accept it immediately. Guest SSH keeps refusing passwords until you also pass `--ssh-pwauth`.

### How secrets are handled

Firestone stores the password as typed, in `machines/<name>/firestone.toml` and in the seed it renders. It is not hashed. Cloud-init's `chpasswd` takes a plaintext value, and a hash Firestone computed would pin one crypt scheme and still be recoverable from the same file. The protection is file permissions, and they are enforced rather than assumed. `firestone.toml`, `seed/meta-data`, `seed/user-data`, `seed/network-config` and `seed.img` are all published mode 0600, inside a mode-0700 directory you own, whatever your umask says.

A password and inline user-data never reach a log line, an event, an error message, a hint or an argument list. `--password-file` is the only spelling of the flag for that reason. Both values stay visible where Firestone is showing your own configuration back to you: `firestone show`, `GET /v1/machines/{name}`, and the `create` result all serialize the effective spec, which is the same data as the file you own. The web interface is bound by the same rule and is not that file. It reports inline user-data as a byte count, authorized keys as a count, and the password as `set` or `unset`, and never renders a submitted password back into the form.

Changing a password, or any effective user-data, keys, network-config, mounts, user, provisioning flag or catalog sshd path, changes the instance id, so the guest re-provisions on the next start.

### Static addressing and no provisioning

Provide NoCloud network-config for a tap guest or anything else that needs static addressing:

```sh
cat > network-config.yaml <<'EOF'
version: 2
ethernets:
  eth0:
    addresses: [192.0.2.10/24]
    routes:
      - to: default
        via: 192.0.2.1
    nameservers:
      addresses: [192.0.2.53]
EOF
firestone create static ubuntu --net tap --tap tap0 --cloud-init-network-config network-config.yaml
```

Relative cloud-init paths resolve from the machine specification's directory after creation. `--user USER` selects the login and console autologin account; the default is `root`, and a different account must already exist in the image or be created by your own cloud-init data with its own keys.

Turn Firestone's provisioning off only when you will provide all guest access yourself:

```sh
firestone create unmanaged ubuntu --no-provisioning --user-data user-data.yaml
```

Without it there is no SSH readiness, no root key injection, no vsock socket unit, no serial autologin and no automatic mounts.

## Copying files

`cp` copies between the host and a machine over the same vsock transport `shell` uses. There is no guest network involved and nothing to add to `~/.ssh/config`:

```sh
firestone cp ./notes.txt dev:/root/notes.txt
firestone cp dev:/var/log/syslog ./syslog
firestone cp -r ./project dev:/root/project
```

Exactly one operand is remote. An operand is remote when it holds a colon and everything before the first colon is a machine name, which is lowercase letters, digits and dashes. Everything else is local. `./dev:/etc/hostname` is a local file, and so is `/srv/dev:/etc`, because the colon comes after a `/`. An IPv6 literal is not operand syntax: `fe80::1:/etc` reads as machine `fe80`, so write `./fe80::1:/etc` for the local file.

Zero remote operands and two remote operands are both usage errors, and each hint names the `./` escape. A machine that is not running is refused with the same message `shell` gives; `cp` never starts a machine.

Under the hood this is `scp` with Firestone's option block, so `-r`, the progress meter and the exit status are `scp`'s own. OpenSSH 9 `scp` transfers over SFTP, so a remote wildcard is expanded by the guest's SFTP server rather than a shell. Quote a remote glob so your own shell does not expand it first.

## REST API

`firestone serve` is optional and stateless. It projects the same actions, the same locks and the same event stream as the CLI. The default listener is `$XDG_RUNTIME_DIR/firestone/serve.sock`, or `/tmp/firestone-<uid>/serve.sock` when the runtime fallback is active.

The full contract is [`openapi.json`](openapi.json), an OpenAPI 3.1 document covering request and response shapes, the NDJSON streams, `Accept: application/json` aggregation, error statuses, limits and both transports. It is a checked-in artifact, not a runtime endpoint; Firestone does not serve it.

Start the server and find its socket:

```sh
firestone serve &
serve_pid=$!
if test -n "${XDG_RUNTIME_DIR:-}"; then
  firestone_socket="$XDG_RUNTIME_DIR/firestone/serve.sock"
else
  firestone_socket="/tmp/firestone-$(id -u)/serve.sock"
fi
```

List machines and stream a start:

```sh
curl --fail --silent --show-error --unix-socket "$firestone_socket" http://firestone/v1/machines
curl --fail --no-buffer --unix-socket "$firestone_socket" \
  -H 'Content-Type: application/json' \
  -X POST http://firestone/v1/machines/dev/start \
  -d '{"wait":true,"timeout_s":600}'
```

Stop the front end when you are done. Running shims and VMs are independent of it:

```sh
kill "$serve_pid"
wait "$serve_pid"
```

The socket is mode 0600. Holding the same user account is the whole authentication story.

A browser cannot open a Unix socket, so `serve` also takes a loopback TCP listener. It must be loopback and it must carry a token; Firestone refuses anything else before it binds:

```sh
firestone serve --listen tcp:127.0.0.1:8642 --token ~/.local/share/firestone/api-token
```

The token file is created mode 0600 when it does not exist, and validated when it does. Send it as a bearer token:

```sh
curl --fail --silent --show-error \
  -H "Authorization: Bearer $(cat ~/.local/share/firestone/api-token)" \
  http://127.0.0.1:8642/v1/machines
```

Every TCP request passes a `Host` allowlist before the token is compared, which is what stops a rebound DNS name from spending a cookie your browser would attach for it. A WebSocket upgrade must additionally prove same origin. The transport is plaintext, so anything with local root can read it. Loopback TCP is a convenience for the browser, not a replacement for the 0600 socket.

Two routes leave HTTP: `GET /v1/machines/{name}/console/ws` and `GET /v1/machines/{name}/shell/ws` carry a terminal as a byte stream. An attached terminal has no idle point, so shutting `serve` down closes an open terminal rather than waiting for the person at it.

## Reclaiming disk space

`firestone images prune` removes unreferenced base images and nothing else. `firestone system prune` reclaims everything Firestone is holding, arranged as a ladder whose bottom tier cannot destroy work:

```sh
firestone system prune --dry-run
firestone system prune
firestone system prune --images
firestone system prune --all --dry-run
```

With no flags it removes only inert artifacts: a stale runtime directory for a machine that is not active, a rotated `console.log.previous`, an unfinished `.partial` from an interrupted pull or copy, an orphaned removal directory, and an unpublished snapshot working directory. Every one of those is debris from an operation that already finished or died.

`--images` adds base images that nothing references, using the same reference set `images rm` refuses to break: a machine's pinned image and every published snapshot's image both count.

`--machines` is the destructive tier. It removes every machine that is `stopped`, `created` or `failed`, with its disk, spec, snapshots and logs, exactly as `firestone rm` does. On a terminal it prints the machine names and asks; without a terminal it needs `--force` or `--yes`. A machine that is starting, running or stopping is never a candidate. `--all` is `--machines --images`.

`--dry-run` is what makes the ladder usable. It produces the same list, the same per-row byte counts and the same total that a real run against the same state would produce, and deletes nothing. Byte counts are allocated blocks measured immediately before deletion, so a sparse overlay is not reported as its virtual size. The tiers run in ladder order, so a machine removed by the last tier does not release its base image within the same call; that image becomes prunable on the next prune. Doing it the other way would make a real run reclaim more than its own preview promised.

## JSON, pipes, and exit status

Put `--json` on any command for newline-delimited JSON events on stdout:

```sh
firestone ls --json
firestone start dev --json
firestone doctor --json
```

Human progress goes to stderr and data goes to stdout, so pipes work. When stderr is not a terminal, Firestone writes plain lines with no color, cursor control or spinner frame. `NO_COLOR` and `--no-color` disable color explicitly.

Exit status is stable:

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Generic failure |
| 2 | Usage or invalid specification |
| 3 | Machine or image not found |
| 4 | Conflict: already exists, already running, name in use, or a busy lock |
| 5 | Missing or broken host dependency |
| 6 | Timeout |
| 7 | Checksum or verification failure |
| 130 | Interrupted |

`run` and `shell` propagate the guest command's exit status instead.

## Troubleshooting

Run `firestone doctor` first. It answers most host problems and prints the exact command for the ones it will not run itself. Then use the narrowest log: `firestone logs NAME --source vmm` for a VMM failure, `--source shim` for a supervision failure, `--source console` for a guest failure.

Every Firestone error carries a stable kind, concrete context and, where there is something to do, a hint. The kinds are `usage`, `invalid_spec`, `not_found`, `not_running`, `conflict`, `already_exists`, `already_running`, `busy`, `dependency`, `timeout`, `checksum`, `interrupted` and `generic`. REST returns the same kind in the JSON envelope, so the repair for a 409 from `curl` is the repair for exit code 4 from the CLI.

| Symptom | What it means and what to do |
|---|---|
| `host architecture ... unsupported` | Run on Linux x86_64. Compiling for aarch64 does not make it a runtime target. |
| `/dev/kvm does not exist` | Enable hardware virtualization, load the matching KVM module, or enable nested KVM in the outer hypervisor. A normal container has no KVM. |
| `/dev/kvm does not open read/write` | Run doctor's detected `usermod` command and log in again. Do not make `/dev/kvm` world-writable. |
| `passt not found` or a version rejection | Run doctor; it names the exact helper and probes every option it needs. On a standalone release this means the embedded payload failed to materialize. |
| `qemu-img`, `ssh` or `ssh-keygen` missing | Run the package command doctor prints. |
| An unsafe config, data, runtime, lock, log or socket path | Move the unexpected node aside and let Firestone recreate its own. Do not chmod or follow an unknown-owner or symlinked node to silence the check. |
| Less than 5 GB free | Free space on the data filesystem, or move the data directory with `FIRESTONE_DATA_DIR`. |
| `unknown image` | Use a catalog reference, a strict HTTPS URL, an existing local path, or an OCI reference such as `docker://nginx` or `ghcr.io/owner/app:v1`. A bare `nginx` is a catalog name. |
| Checksum mismatch | Do not bypass it. Retry from a trusted network and compare the vendor metadata. A direct URL needs `--sha256`. |
| Disk is smaller than the base image | Recreate the machine with a larger `--disk` before an overlay exists. |
| Name already in use | Reuse the machine, or pass a different `--name` when running an image. |
| Machine is busy | Wait for the other action to release the machine lock. Do not delete the lock file. |
| Start timed out | Read `console`, `vmm` and `shim` logs. A slow first cloud-init run may need a larger `--timeout`; a repeated hang needs diagnosis rather than an unlimited timeout. On an OCI machine there is no sshd to wait for, so use `--no-wait`. |
| SSH host key changed | Do not disable host-key checking. Confirm that a seed change you made regenerated the guest. Firestone removes `known_hosts` on a seed rewrite and on `rm`; an unexplained change is a hard failure. |
| `shell` says not running | Start the machine. Vsock SSH works even with `--net none`, but not on an OCI machine, which has no sshd. |
| Console requires a terminal | Run `firestone console NAME` in an interactive terminal. Use `logs --source console` in a script. |
| A forward cannot bind or overlaps | Stop the conflicting process or pick another host port. Same-protocol host ranges cannot overlap. Bind to loopback when external access is not needed. |
| Forwards look wrong on a running machine | Check for the `*` and the `forwards pending restart` legend in `ls`. Restart the machine to apply them. |
| Tap validation fails | Create and own the tap and bridge outside Firestone, then check `/dev/net/tun` access. |
| A mount is absent in the guest | Read `virtiofsd-N`, the console and the cloud-init status. Confirm the host path existed before start. |
| A write fails on a shared folder | A `:ro` mount is read-only by design. Change it only if guest writes are acceptable. |
| `running!` or a degraded status | Read the named sidecar log, then restart to recreate the sidecar. The VM keeps running when a sidecar exits. |
| `vmm paused after a failed snapshot resume` | A warm snapshot could not resume the guest. Run `firestone restart NAME`. |
| A live resize is refused with a `cpus_max` hint | The machine booted without that headroom. Set `cpus_max` and `memory_max` and restart. |
| A registry pull is refused for a second time | The registry rejected anonymous access. Log in with `docker login` so `~/.docker/config.json` carries an `auths` entry, or use a public reference. |

Recovery is decided from live processes and sockets, not from stale JSON:

- After a host reboot, `firestone ls` reconciles a formerly running machine to stopped, because its runtime directory is gone.
- After a VMM crash, `ls` reports failed. Read the `vmm` and `console` logs, then start it again.
- If the shim dies but the verified VMM is alive, `ls` reports running without supervision, and `firestone stop NAME` still stops it through the VMM API and verified process identity.
- If a sidecar dies, `ls` shows a degraded running status. Read the matching log, then stop and start the machine to recreate it.
- If a machine cannot be recovered, `firestone rm NAME --force` removes its own state. It never removes a shared base image.

## Security model

Firestone avoids privilege escalation. It still runs a hypervisor and hands the guest controlled access to host resources, so the boundaries are worth stating plainly.

Firestone runs as your user, and `doctor --fix` makes no privileged host change without displaying it and asking first. The REST Unix socket, the runtime sockets, the console, the SSH identity, machine disks, logs and state are all your user's authority. Firestone rejects an unsafe owner, mode, file type or symlink instead of quietly repairing it.

A TCP listener is loopback-only and always authenticated. Firestone refuses a routable or wildcard bind, and refuses TCP without a token, before it creates the listener. The web interface performs no privileged action of its own; it renders the same results and calls the same `/v1` endpoints, so it holds exactly the authority you already have.

Catalog downloads are verified against vendor checksum documents. A direct HTTPS download is unverified unless you supply `--sha256`. An OCI pull verifies every manifest, config and layer blob against the digest that referenced it before a byte is used, and only `sha256` digests are accepted.

SSH is key-only with a per-machine known-hosts file. Never replace that with `StrictHostKeyChecking=no`. A passt forward with no bind address listens on every host address, so bind sensitive services to `127.0.0.1`. Tap setup is privileged host networking that you own; Firestone does not manage its bridge, addressing, NAT or firewall policy. A read-write mount grants the guest write access to that host directory, so share the smallest tree you can.

Cloud-init inputs and rendered seed files stay on disk in the machine directory. Keep secrets out of command arguments and logs, and protect the data directory the way you would protect any other private VM storage.

`vmm.binary`, `vmm.extra_args` and `vmm.config_overlay` are advanced authority. A custom executable runs as your user. Firestone validates ordinary binaries and wrappers, and it does not claim containment against a hostile one. The console has root autologin after Firestone provisioning; its socket is private to your user, and anyone controlling that user already controls the VM.

## Paths and state

Firestone resolves every path once, at process startup:

| Purpose | Default | Override |
|---|---|---|
| Config | `$XDG_CONFIG_HOME/firestone`, else `~/.config/firestone` | `FIRESTONE_CONFIG_DIR` |
| Data | `$XDG_DATA_HOME/firestone`, else `~/.local/share/firestone` | `FIRESTONE_DATA_DIR` |
| Runtime | `$XDG_RUNTIME_DIR/firestone`, else `/tmp/firestone-<uid>` | `FIRESTONE_RUNTIME_DIR` |
| Isolated root | Not set by default | `FIRESTONE_HOME`, or `--home`, which maps to `config`, `data` and `run` children |

Use an isolated root for experiments, so nothing you try touches your real machines:

```sh
firestone --home "$PWD/.firestone-sandbox" doctor --fix
firestone --home "$PWD/.firestone-sandbox" create sandbox ubuntu
```

Under the data directory:

- `bin/` holds checksum-verified pinned binaries.
- `ssh/id_ed25519` and its `.pub` are Firestone's identity.
- `images/image-<digest>.qcow2` and a JSON sidecar are one stored base image.
- `machines/<name>/firestone.toml` is desired state, mode 0600.
- `machines/<name>/state.json` is runtime facts.
- `machines/<name>/` also holds `disk.qcow2`, `seed.img` or `config.img`, `vmconfig.json`, `known_hosts`, `snapshots/` and the logs.

Change desired state with `firestone edit NAME`, a REST `PUT` or `PATCH`, or the web interface's edit dialog. Do not hand-edit `state.json`, a lock file, an image sidecar, a socket or a pid file. Those are Firestone's record of what is true, not what you want.
