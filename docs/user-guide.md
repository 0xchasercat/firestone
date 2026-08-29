# Firestone user guide

Firestone is a daemonless VM manager for Linux. This guide covers the Linux x86_64 MVP built around Cloud Hypervisor v53 and the pinned edk2 firmware.

The MVP does not provide aarch64 runtime support, non-Linux host support, non-Linux guests, or cross-architecture emulation. The aarch64 target is compile-only until its own KVM and catalog matrix passes. Custom VMM wrappers are accepted by the configuration model, but hostile wrapper containment and non-Linux process supervision are deferred.

## Install and build

You need Linux x86_64, hardware virtualization, a kernel KVM driver, at least 5 GB free in Firestone's data filesystem, OpenSSH, `qemu-img`, and a recent `passt`. Rust 1.85 or newer is required to build from source.

Build and install the binary in your user account:

```sh
rustup toolchain install stable
cargo build --locked --release --package firestone
install -Dm0755 target/release/firestone "$HOME/.local/bin/firestone"
firestone version
```

Make sure `$HOME/.local/bin` is on `PATH` before opening a new shell.

Install the system packages for your distribution. The package names below were checked in fresh x86_64 containers and are recorded in the [fresh-host doctor matrix](verification/doctor-matrix.md). Containers do not provide KVM and were not used as boot evidence.

Ubuntu 24.04 provides `qemu-img` and OpenSSH under these package names:

```sh
sudo apt-get update
sudo apt-get install -y build-essential ca-certificates git openssh-client qemu-utils util-linux
```

Ubuntu 24.04's `passt` package is `2024_02_20`, older than Firestone's `2025_02_17.a1e48a0` minimum. Build the exact minimum from the upstream HTTPS repository instead:

```sh
passt_work=$(mktemp -d)
trap 'rm -rf -- "$passt_work"' EXIT
git clone --depth 1 --branch 2025_02_17.a1e48a0 https://passt.top/passt "$passt_work/passt"
test "$(git -C "$passt_work/passt" rev-parse HEAD)" = a1e48a02ff3550eb7875a7df6726086e9b3a1213
make -C "$passt_work/passt" passt
sudo install -m0755 "$passt_work/passt/passt" /usr/local/bin/passt
passt --version
```

Fedora 44 carries a new enough `passt`:

```sh
sudo dnf install passt qemu-img openssh-clients util-linux
```

Arch Linux carries a new enough `passt`:

```sh
sudo pacman -S passt qemu-img openssh util-linux
```

## Run doctor first

Run the read-only check, then allow Firestone to perform its unprivileged repairs:

```sh
firestone doctor
firestone doctor --fix
firestone doctor
```

`doctor --fix` creates Firestone-owned directories, downloads and checksum-verifies the pinned Cloud Hypervisor, firmware, and `virtiofsd` binaries, and generates Firestone's SSH key. It does not run `sudo`, install distribution packages, change KVM permissions, enable kernel features, change sysctls, or delete machines.

Each check is `ok`, `warn`, or `fail`. A failed report exits with status 5. Warnings do not block VM use.

| Check | What to do |
|---|---|
| Host architecture | Use Linux x86_64. aarch64 runtime is deferred. |
| `/dev/kvm` missing | Enable virtualization in firmware and load `kvm_intel` or `kvm_amd`. A VM or CI host may also need nested virtualization enabled. |
| `/dev/kvm` permission denied | Run the exact group command printed by doctor, normally `sudo usermod -aG kvm $USER`, then log out and back in. Doctor reads the device's real group name. |
| Runtime directory | Set `XDG_RUNTIME_DIR` to a user-owned mode-0700 directory. Without it, Firestone uses `/tmp/firestone-<uid>` and warns. `doctor --fix` creates the fallback safely. |
| Vendored binaries or Firestone SSH key | Run `firestone doctor --fix`. |
| `passt` | Use the package command printed for Fedora or Arch. On Ubuntu 24.04, use the pinned source build above because the distro package is too old. |
| `qemu-img` | Install `qemu-utils` on Ubuntu, `qemu-img` on Fedora, or `qemu-img` on Arch. |
| OpenSSH | Install `openssh-client` on Ubuntu, `openssh-clients` on Fedora, or `openssh` on Arch. |
| User namespaces | If `unshare -U true` is denied, Firestone warns and runs `virtiofsd` with `--sandbox none`. Whether to enable unprivileged user namespaces is a host security policy decision. |
| Free space | Free space on the filesystem containing the data directory before pulling images. The warning threshold is 5 GB. |
| Stale state | Doctor and ordinary reads reconcile state against live processes and sockets. Repair the named path or lock error if reconciliation itself fails. |

## Quickstart

The shortest path creates a machine named `ubuntu`, pulls the default Ubuntu image, starts it, waits for SSH over vsock, and opens a root shell:

```sh
firestone run ubuntu
```

Run a non-interactive guest command instead:

```sh
firestone run ubuntu -- uname -a
```

Create a disposable machine and remove it when the command exits:

```sh
firestone run ubuntu --name scratch --rm -- true
```

The first boot downloads and verifies the image and runs cloud-init. Later starts use the owned image cache and the machine's qcow2 overlay.

## Machine lifecycle

Create without booting:

```sh
firestone create dev ubuntu --cpus 4 --memory 4G --disk 40G
```

Start, inspect, and enter the machine:

```sh
firestone start dev
firestone ls
firestone show dev
firestone shell dev
firestone shell dev -- id -u
```

`start` waits for boot and SSH readiness. Use `--no-wait` only when another process will check readiness:

```sh
firestone start dev --no-wait
```

Attach the serial console for boot diagnosis or rescue work:

```sh
firestone console dev
```

Press `Ctrl-]` to detach. Console attach needs terminal stdin, stdout, and stderr. It does not work through a pipe or under `--json`.

Stop gracefully, force stop only when necessary, and remove the machine:

```sh
firestone stop dev
firestone stop dev --force
firestone rm dev
```

`rm` deletes the complete machine directory, including its overlay, seed, logs, and known-hosts file. Shared base images remain until `images rm` or `images prune` removes them.

## Images and the built-in catalog

The Linux MVP catalog has only the releases authorized by [SPEC section 8.1](../SPEC.md). Every x86_64 source uses a dated vendor build, its matching checksum document, edk2, login user `root`, and `/usr/sbin/sshd`.

| Reference | Aliases and default | Vendor build | Verification |
|---|---|---|---|
| `ubuntu:24.04` | `ubuntu`, `ubuntu:noble`; Ubuntu default | released build `20260826` | SHA-256 |
| `ubuntu:22.04` | `ubuntu:jammy` | released build `20260826` | SHA-256 |
| `debian:12` | `debian`, `debian:bookworm`; Debian default | official genericcloud `20260821-2577` | SHA-512 |
| `debian:13` | `debian:trixie` | official genericcloud `20260826-2582` | SHA-512 |
| `fedora:44` | `fedora`; Fedora default | stable Cloud Base `44-1.7` | SHA-256 |

The [catalog source audit](verification/catalog.md) records the vendor metadata URLs, exact image URLs, checksum sources, and observed digests. Source availability is not boot evidence. E2E 11 must boot every row to SSH on a real Linux x86_64 KVM host before the catalog gate closes.

Manage the owned image store with these commands:

```sh
firestone images pull ubuntu:24.04
firestone images ls
firestone images inspect image-0123456789abcdef
firestone images rm image-0123456789abcdef
firestone images prune
```

Use the real full image id printed by `images ls`; the shortened value above only demonstrates the argument position. `images rm` refuses an image still referenced by a machine unless you pass `--force` or approve the prompt. `images prune` removes only unreferenced images.

A direct HTTPS source has no catalog checksum. Supply the publisher's full SHA-256 to avoid an unchecked download:

```sh
firestone images pull "$IMAGE_URL" --sha256 "$IMAGE_SHA256"
```

Local raw and qcow2 files are copied into the owned store. Raw files are converted with `qemu-img`; Firestone never uses the user file directly as a backing file.

## Networking and port forwards

`passt` is the default. It gives the guest outbound network access without root or host firewall changes. Inbound connections require an explicit forward:

```sh
firestone create web ubuntu --forward 8080:80
firestone create dns ubuntu --forward udp:5353:53
firestone create private-web ubuntu --forward 127.0.0.1:8080:80
firestone create range ubuntu --forward 8000-8010:8000-8010
```

A missing bind address listens on all host addresses. Bind to `127.0.0.1` when a service should remain local. Forwards are fixed at start time; edit the machine and restart it to apply changes. Two passt guests reach each other only through forwarded host ports.

Vsock SSH does not depend on guest networking. You can disable the network device and still use `shell`, `console`, and mounts:

```sh
firestone create isolated ubuntu --net none
firestone start isolated
firestone shell isolated
```

For an ad hoc SSH tunnel with `network.mode = "none"`, generate an OpenSSH config and use ordinary OpenSSH forwarding:

```sh
firestone ssh-config isolated > "$HOME/.ssh/firestone-isolated.conf"
ssh -F "$HOME/.ssh/firestone-isolated.conf" -L 8080:127.0.0.1:80 firestone.isolated
```

### Tap prerequisite

Tap mode is for a bridge you administer. Firestone never creates the tap, bridge, DHCP server, NAT rule, or firewall rule. A privileged administrator performs the one-time setup:

```sh
sudo ip tuntap add dev tap0 mode tap user "$USER"
sudo ip link set tap0 master br0
sudo ip link set tap0 up
```

Then create the machine as the ordinary Firestone user:

```sh
firestone create bridged ubuntu --net tap --tap tap0
```

The tap must exist under `/sys/class/net`, must be a tap device, and `/dev/net/tun` must be openable. Port forwards belong to passt mode and are rejected with tap or none.

## Shared folders

A read-write mount exposes the host directory to the guest:

```sh
firestone create work ubuntu --mount "$PWD:/work"
```

Add `:ro` for a read-only guest mount:

```sh
firestone create review ubuntu --mount "$PWD:/src:ro"
```

Firestone starts one pinned `virtiofsd` process per mount. Treat a read-write mount as guest write access to that host tree. A read-only mount limits guest writes but is not a substitute for keeping sensitive files outside the shared tree. If user namespaces are unavailable, doctor warns that `virtiofsd` will use `--sandbox none`.

## Cloud-init, SSH keys, and guest network config

Firestone's generated cloud-init part enables key-only root SSH over vsock, gives the image's default user the same authorized keys, creates the serial getty, grows the root filesystem, and mounts shared folders. Password SSH stays disabled.

Add your own cloud-config without placing secrets on the command line:

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

Add one or more public keys. Pass public key files only:

```sh
firestone create keyed ubuntu --ssh-key "$HOME/.ssh/id_ed25519.pub"
```

Firestone reads its own public key and the supplied public key files. It never puts a private key in the seed or logs. The final multipart user-data and network-config are stored in the protected machine directory for inspection, so do not treat that directory as a place to hide plaintext secrets.

Provide NoCloud network-config for a tap guest or another environment that needs static guest addressing:

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

Relative cloud-init paths resolve from the machine specification's directory after creation. Changing effective user-data, network-config, keys, mounts, user, provisioning, or catalog SSH daemon path changes the instance id and triggers provisioning on the next start.

`--user USER` selects the login and console autologin account. The default is `root`. A non-root account must already exist in the image or be created by your cloud-init data with its own authorized key setup.

Disable Firestone provisioning only when you will provide all guest access yourself:

```sh
firestone create unmanaged ubuntu --no-provisioning --user-data user-data.yaml
```

Without Firestone provisioning, SSH readiness, root key injection, the vsock socket unit, serial autologin, and automatic mounts are not guaranteed.

## Logs

The default source is the guest serial console. Select a bounded source explicitly when diagnosing a failure:

```sh
firestone logs dev
firestone logs dev -n 500 --source console
firestone logs dev -n 200 --source vmm
firestone logs dev -n 200 --source shim
firestone logs dev -n 200 --source passt
firestone logs dev -n 200 --source virtiofsd-0
firestone logs dev --follow --source console
```

Log sources are `console`, `vmm`, `shim`, `passt`, and `virtiofsd-N`. Firestone opens only current-user-owned mode-0600 regular files without following the final symlink. A reverse tail is capped at 8 MiB. Follow mode reopens safe rotations and stops on `Ctrl-C`.

Cloud-init contents and private SSH keys are never written to process logs. A user command can still print a secret to the console, so review what you run inside the guest.

## REST over a Unix socket

`serve` is optional and stateless. It projects the same actions and locks as the CLI. The default listener is `$XDG_RUNTIME_DIR/firestone/serve.sock`, or `/tmp/firestone-<uid>/serve.sock` when the runtime fallback is active.

Start the server and locate its socket:

```sh
firestone serve &
serve_pid=$!
if test -n "${XDG_RUNTIME_DIR:-}"; then
  firestone_socket="$XDG_RUNTIME_DIR/firestone/serve.sock"
else
  firestone_socket="/tmp/firestone-$(id -u)/serve.sock"
fi
```

List machines and stream a start action with curl:

```sh
curl --fail --silent --show-error --unix-socket "$firestone_socket" http://firestone/v1/machines
curl --fail --no-buffer --unix-socket "$firestone_socket" \
  -H 'Content-Type: application/json' \
  -X POST http://firestone/v1/machines/dev/start \
  -d '{"wait":true,"timeout_s":600}'
```

Stop only the REST front end when finished. Running shims and VMs are independent of it:

```sh
kill "$serve_pid"
wait "$serve_pid"
```

The socket mode is 0600. Possession of the same user account grants API authority. There is no TCP listener or bearer-token mode in the MVP.

## Paths, state, and recovery

Firestone resolves paths once at process startup:

| Purpose | Default | Override |
|---|---|---|
| Config | `$XDG_CONFIG_HOME/firestone`, else `~/.config/firestone` | `FIRESTONE_CONFIG_DIR` |
| Data | `$XDG_DATA_HOME/firestone`, else `~/.local/share/firestone` | `FIRESTONE_DATA_DIR` |
| Runtime | `$XDG_RUNTIME_DIR/firestone`, else `/tmp/firestone-<uid>` | `FIRESTONE_RUNTIME_DIR` |
| Isolated root | Not set by default | `FIRESTONE_HOME`, or `--home`, maps to `config`, `data`, and `run` children |

Use an isolated root for experiments:

```sh
firestone --home "$PWD/.firestone-sandbox" doctor --fix
firestone --home "$PWD/.firestone-sandbox" create sandbox ubuntu
```

Important files under the data directory are:

- `bin/` for checksum-verified pinned binaries.
- `ssh/id_ed25519` and `.pub` for Firestone's identity.
- `images/image-<full-digest>.qcow2` plus its JSON sidecar.
- `machines/<name>/firestone.toml` for desired state.
- `machines/<name>/state.json` for runtime facts.
- `machines/<name>/disk.qcow2`, `seed.img`, `known_hosts`, and logs.

Edit desired state with `firestone edit NAME` or a REST PUT/PATCH. Do not hand-edit `state.json`, lock files, image sidecars, sockets, or pid files.

Recovery is based on live processes and sockets, not stale JSON alone:

- After a host reboot, `firestone ls` reconciles a formerly running machine to stopped because its runtime directory is gone.
- After a VMM crash, `ls` reports failed and `firestone start NAME` may start it again after you inspect `vmm` and `console` logs.
- If the shim dies but the verified VMM remains alive, `ls` reports running without supervision. `firestone stop NAME` uses the VMM API and verified process identity to stop it.
- If a sidecar dies, `ls` shows a degraded running status. Inspect the matching log, stop, and start the machine to recreate sidecars.
- If a machine cannot be recovered, `firestone rm NAME --force` removes its owned machine state. It does not remove a shared base image.

## JSON, pipes, and exit status

Put `--json` on any command for newline-delimited JSON events on stdout:

```sh
firestone ls --json
firestone start dev --json
firestone doctor --json
```

Human progress goes to stderr. Data goes to stdout. When stderr is not a terminal, Firestone emits plain lines without colors, cursor controls, or spinner frames. `NO_COLOR` and `--no-color` disable color explicitly.

Exit status is stable:

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Generic failure |
| 2 | Usage or invalid specification |
| 3 | Machine or image not found |
| 4 | Conflict, already running, name in use, or busy lock |
| 5 | Missing or broken host dependency |
| 6 | Timeout |
| 7 | Checksum or verification failure |
| 130 | Interrupted |

`run` and `shell` propagate the guest command's exit status.

## Troubleshooting

Start with `firestone doctor`, then use the narrowest relevant log.

| Symptom | Action |
|---|---|
| `host architecture ... unsupported` | Run on Linux x86_64. Building aarch64 does not establish runtime support. |
| `/dev/kvm does not exist` | Enable hardware virtualization, load the matching KVM module, or enable nested KVM in the outer hypervisor. A normal container has no KVM. |
| `/dev/kvm does not open read/write` | Run doctor's detected `usermod` command and complete a fresh login. Do not make `/dev/kvm` world-writable. |
| `passt not found` or version rejected | Install the distro package named by doctor. On Ubuntu 24.04 use the pinned source build in this guide. Rerun doctor; it also probes every required option. |
| `qemu-img`, `ssh`, or `ssh-keygen` missing | Run the exact package command printed by doctor. |
| Vendored dependency or Firestone SSH key missing | Run `firestone doctor --fix` as the Firestone user. |
| Unsafe config, data, runtime, lock, log, or socket path | Move the unexpected node aside and let Firestone recreate its own path. Do not chmod or follow an unknown-owner or symlinked node just to silence the check. |
| Less than 5 GB free | Free space in the data filesystem or move the data directory with `FIRESTONE_DATA_DIR`. |
| `unknown image` | Use a reference from the catalog table, a strict HTTPS URL, or an existing local path. Compile-only aarch64 source metadata does not enable runtime support; doctor rejects an aarch64 host in this MVP. |
| Checksum mismatch | Do not bypass it. Retry from a trusted network and compare the vendor metadata. A direct URL needs `--sha256`. |
| Disk is smaller than the base image | Recreate the machine with a larger `--disk` before an overlay exists. |
| Name already in use | Reuse the existing machine or pass a different `--name` when running an image. |
| Machine is busy | Wait for the other action to release the machine lock. Do not delete the lock file. |
| Start timed out | Inspect `console`, `vmm`, and `shim` logs. A slow first cloud-init run may need a larger `--timeout`; a repeated hang needs diagnosis, not an unlimited timeout. |
| SSH host key changed | Do not disable host-key checking. Confirm that a trusted seed change regenerated the guest. Firestone removes `known_hosts` for a seed rewrite and on `rm`; an unexplained change is a hard failure. |
| `shell` says not running | Start the machine. Vsock SSH works even with `--net none`. |
| Selected login user fails | Use the default root user or provision the named account and its key before selecting `--user`. |
| Console requires a terminal | Run `firestone console NAME` directly in an interactive terminal. Use `logs --source console` in scripts. |
| Forward cannot bind or overlaps | Stop the conflicting process or choose a different host port. Same-protocol host ranges cannot overlap. Use a loopback bind when external access is not needed. |
| Tap validation fails | Create and own the tap and bridge outside Firestone, then verify `/dev/net/tun` access. |
| Mount is absent | Inspect `virtiofsd-N`, console, and cloud-init status. Confirm the host path exists before start. |
| Write fails on a shared folder | A `:ro` mount is intentionally read-only. Recreate or edit the mount only if guest writes are acceptable. |
| `running!` or degraded status | Read the named sidecar log, then restart to recreate the sidecar. The VM remains running when a sidecar exits. |
| REST returns 404, 409, 502, 503, or 504 | Read the JSON `error.kind` and `hint`. These correspond to not found, conflict, checksum, dependency, and timeout errors. The same repair applies to the CLI action. |

## Security boundaries

Firestone avoids privilege escalation, but it still runs a hypervisor and gives the guest controlled access to host resources. Keep these boundaries explicit:

- Firestone runs as your user. `doctor --fix` never performs privileged host changes.
- The REST Unix socket, runtime sockets, console, SSH identity, machine disks, logs, and state are user authority. Firestone rejects unsafe owners, modes, file types, and symlinks instead of repairing them silently.
- Catalog downloads use vendor checksum documents. Direct HTTPS downloads are unchecked unless you supply `--sha256`.
- Firestone uses key-only SSH with per-machine known-hosts files. Never replace that with `StrictHostKeyChecking=no`.
- A passt forward without a bind address listens on all host addresses. Bind sensitive services to `127.0.0.1`.
- Tap setup is privileged host networking that you own. Firestone does not manage its bridge, addressing, NAT, or firewall policy.
- A read-write mount grants the guest write access to that host directory. Share the smallest tree possible and use `:ro` where it fits.
- Cloud-init inputs and rendered seed files remain on disk in the protected machine directory. Keep secrets out of command arguments and logs, and protect the data directory like other private VM storage.
- `vmm.binary`, `vmm.extra_args`, and `vmm.config_overlay` are advanced authority. A custom executable runs as your user. The MVP validates ordinary binaries and wrappers but does not claim containment against a hostile wrapper.
- The console has root autologin after Firestone provisioning. Its socket is private to the Firestone user; anyone controlling that user already controls the VM.

The Linux x86_64 scope is deliberate. aarch64 runtime, macOS or Windows hosts, non-Linux guests, remote hosts, TCP REST, graphics, snapshots, live migration, and cross-architecture emulation are not half-supported. They are deferred.
