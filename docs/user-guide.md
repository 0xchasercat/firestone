# Firestone user guide

Firestone is a daemonless VM manager for Linux. This guide covers the Linux x86_64 MVP built around Cloud Hypervisor v53 and the pinned edk2 firmware.

The MVP does not provide aarch64 runtime support, non-Linux host support, non-Linux guests, or cross-architecture emulation. The aarch64 target is compile-only until its own KVM and catalog matrix passes. Custom VMM wrappers are accepted by the configuration model, but hostile wrapper containment and non-Linux process supervision are deferred.

## Install and build

You need Linux x86_64, hardware virtualization, a kernel KVM driver, at least 5 GB free in Firestone's data filesystem, and OpenSSH. The published x86_64 musl executable already carries the pinned static passt and qemu-img helpers; no passt or QEMU package is required. Rust 1.85 or newer is required only when building from source.

Install the published standalone executable in your user account, make sure `$HOME/.local/bin` is on `PATH`, then check its identity:

```sh
install -Dm0755 firestone-v*-x86_64-unknown-linux-musl "$HOME/.local/bin/firestone"
firestone version
```

The published asset is named for its release, so substitute the version you
downloaded if the glob matches more than one file.

The version report identifies the embedded passt `2025_02_17.a1e48a0` and qemu-img `8.2.2` payload hashes. Firestone verifies those bytes again before first-use materialization under its private data directory.

A normal `cargo build` is a development build and retains PATH fallback for helper development. Use `scripts/build-release.sh --target x86_64-unknown-linux-musl` for the strict standalone build; it refuses missing or mismatched helper inputs.

OpenSSH remains a host tool. Install only its client package when it is missing:

```sh
# Ubuntu 24.04
sudo apt-get install openssh-client

# Fedora 44
sudo dnf install openssh-clients

# Arch Linux
sudo pacman -S openssh
```

## Run doctor first

Run the read-only check, then let Firestone apply the repairs you approve:

```sh
firestone doctor
firestone doctor --fix
firestone doctor
```

`doctor --fix` creates Firestone-owned directories, materializes the embedded Cloud Hypervisor, passt, and qemu-img executables, downloads and checksum-verifies the pinned firmware and `virtiofsd` binaries, and generates Firestone's SSH key. It never changes a sysctl, KVM permissions, or machines. When Ubuntu AppArmor blocks passt's mandatory user namespace, an interactive run first prints the exact root-owned helper/profile commands and asks for confirmation. `--yes` and `--json` never authorize elevation; non-interactive runs only print the administrator commands.
A normal `start` never runs the rest of doctor implicitly. When `auto`, `rhf`, or `edk2` selects a missing pinned firmware, start downloads and securely publishes only that artifact before writing VmConfig. A custom firmware path is used as-is and is never downloaded over or rewritten.

Each check is `ok`, `warn`, or `fail`. A failed report exits with status 5. Warnings do not block VM use.

| Check | What to do |
|---|---|
| Host architecture | Use Linux x86_64. aarch64 runtime is deferred. |
| `/dev/kvm` missing | Enable virtualization in firmware and load `kvm_intel` or `kvm_amd`. A VM or CI host may also need nested virtualization enabled. |
| `/dev/kvm` permission denied | Run the exact group command printed by doctor, normally `sudo usermod -aG kvm $USER`, then log out and back in. Doctor reads the device's real group name. |
| Runtime directory | Set `XDG_RUNTIME_DIR` to a user-owned mode-0700 directory. Without it, Firestone uses `/tmp/firestone-<uid>` and warns. `doctor --fix` creates the fallback safely. |
| Vendored binaries, embedded helpers, or Firestone SSH key | Run `firestone doctor --fix`. Embedded helper corruption is refused rather than overwritten. |
| `passt` | The standalone binary includes the exact helper. If AppArmor restricts unprivileged user namespaces, review the literal `/usr/libexec/firestone/passt-2025_02_17.a1e48a0` profile commands printed by doctor. Firestone never grants `userns,` to `~/.local/share/firestone/bin/*` or another user-writable wildcard. |
| `qemu-img` | The standalone binary includes qemu-img 8.2.2 and materializes it on first image operation or `doctor --fix`. It does not need user namespaces. |
| OpenSSH | Install `openssh-client` on Ubuntu, `openssh-clients` on Fedora, or `openssh` on Arch. |
| User namespaces | Doctor probes passt with the same foreground, one-off vhost-user isolation mode used by a VM. Both `Couldn't create user namespace` and `Failed to detach isolating namespaces` are fatal. When host facts point to AppArmor, interactive `doctor --fix` offers the literal root-owned pinned helper/profile repair. A virtiofsd-only denial warns and uses `--sandbox none`. |
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

The first boot downloads and verifies the image, securely installs the selected pinned firmware when missing, and runs cloud-init. Later starts use the owned firmware and image cache plus the machine's qcow2 overlay.

## Machine lifecycle

Create without booting:

```sh
firestone create dev ubuntu --cpus 4 --memory 4G --disk 40G
```
On a terminal, `create` starts with an arrow-key selector over the merged catalog. Choose the final custom option for an HTTPS URL or local path, then continue through name, CPU, memory, disk, and network prompts. Supplied arguments become the shown defaults. Pass `--yes` to skip the wizard; `--json` and non-terminal invocations are always deterministic and non-interactive.

After publication, human output prints the effective image/resources/network, forwards, mounts, exact `firestone.toml` path, and ready-to-run `firestone edit NAME` and `firestone start NAME` commands. `firestone create --help` lists every equivalent configuration flag.

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

Print the catalog Firestone will actually resolve before creating a machine:

```sh
firestone catalog
```

The table merges the built-in entries with `~/.config/firestone/catalog.toml` and configured extra catalogs. It reports canonical references, aliases, available architectures, and effective firmware; `images ls` remains the separate list of artifacts already downloaded into the owned cache.

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
The complete static contract is [`openapi.json`](openapi.json). It is an OpenAPI 3.1 JSON document covering request and response shapes, the default NDJSON streams, `Accept: application/json` aggregation, error statuses, limits, and Unix-socket transport. Firestone does not serve the document as an API endpoint.

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

The socket mode is 0600. Possession of the same user account grants API authority.

A browser cannot open a Unix socket, so `serve` also accepts a loopback TCP listener. It must be loopback and it must carry a token; Firestone refuses anything else before it binds:

```sh
firestone serve --listen tcp:127.0.0.1:8642 --token ~/.local/share/firestone/api-token
```

The token file is created mode 0600 if it does not exist. Authenticate with it as a bearer token:

```sh
curl --fail --silent --show-error \
  -H "Authorization: Bearer $(cat ~/.local/share/firestone/api-token)" \
  http://127.0.0.1:8642/v1/machines
```

## The web UI

`firestone ui` serves the same API and an embedded web interface on an ephemeral loopback port, then opens your browser:

```sh
firestone ui
```

It prints the URL, which carries a fresh session token generated for this run only:

```
Firestone UI   http://127.0.0.1:47318/?token=<64 hex>
Press Ctrl-C to stop.
```

The first page load exchanges the token for an `HttpOnly`, `SameSite=Strict` cookie and rewrites the address bar, so the token does not linger in browser history. The token lives only in the process; stopping `firestone ui` invalidates it.

Use `--no-open` on a headless host and open the printed URL from a machine that can reach it — over an SSH tunnel, not by binding a routable address, which Firestone refuses:

```sh
firestone ui --no-open
```

`--print-url` prints the URL and nothing else, which is convenient in a script. Add `--json` for a machine-readable record of the address, port and URL.

The UI covers the host overview and doctor report, the machine list and detail (spec, logs, generated VM config), the image catalog, and machine creation. Lifecycle actions call the same `/v1` endpoints you would call with curl and render their NDJSON progress as it arrives, so what you see in the browser is the same event stream the CLI prints. Everything it needs is compiled into the binary: no CDN, no network access, no second process.

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
| `unknown image` | Use a reference from the catalog table, a strict HTTPS URL, an existing local path, or an OCI reference such as `docker://nginx` or `ghcr.io/owner/app:v1`. A bare `nginx` is a catalog name, not a container image. Compile-only aarch64 source metadata does not enable runtime support; doctor rejects an aarch64 host in this MVP. |
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
- A TCP listener is loopback-only and always authenticated; Firestone refuses a routable or wildcard bind, and refuses TCP without a token, before it creates the listener. Requests are checked against a `Host` allowlist before the token is compared, which is what stops a rebound DNS name from spending the cookie your browser would attach for it. The transport is plaintext, so anything with local root can still read it — loopback TCP is a convenience for the browser, not a replacement for the 0600 socket.
- The web UI performs no privileged action of its own. It renders the same action results the CLI does and calls the same `/v1` endpoints for every mutation, so it holds exactly the authority your user already has.
- Catalog downloads use vendor checksum documents. Direct HTTPS downloads are unchecked unless you supply `--sha256`.
- Firestone uses key-only SSH with per-machine known-hosts files. Never replace that with `StrictHostKeyChecking=no`.
- A passt forward without a bind address listens on all host addresses. Bind sensitive services to `127.0.0.1`.
- Tap setup is privileged host networking that you own. Firestone does not manage its bridge, addressing, NAT, or firewall policy.
- A read-write mount grants the guest write access to that host directory. Share the smallest tree possible and use `:ro` where it fits.
- Cloud-init inputs and rendered seed files remain on disk in the protected machine directory. Keep secrets out of command arguments and logs, and protect the data directory like other private VM storage.
- `vmm.binary`, `vmm.extra_args`, and `vmm.config_overlay` are advanced authority. A custom executable runs as your user. The MVP validates ordinary binaries and wrappers but does not claim containment against a hostile wrapper.
- The console has root autologin after Firestone provisioning. Its socket is private to the Firestone user; anyone controlling that user already controls the VM.

The Linux x86_64 scope is deliberate. aarch64 runtime, macOS or Windows hosts, non-Linux guests, remote hosts, TCP REST, graphics, snapshots, live migration, and cross-architecture emulation are not half-supported. They are deferred. A routable or unauthenticated TCP listener stays deferred with them.
