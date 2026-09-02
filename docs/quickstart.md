---
icon: bolt
---

# Quick start

Check the host with `doctor`, then boot your first machine.

## Check the host

Run it once:

```sh
firestone doctor
```

Doctor repairs what it owns as it goes. It creates Firestone's directories, materializes the embedded Cloud Hypervisor, `passt` and `qemu-img` executables, downloads and checksum-verifies the pinned firmware and `virtiofsd`, and generates Firestone's SSH key. It never changes a sysctl, a device permission, or a machine.

One repair needs an administrator, and that one waits for you to ask. When Ubuntu's AppArmor blocks the user namespace `passt` needs, `firestone doctor --fix` in an interactive terminal prints the literal root-owned helper and profile commands and asks before running them. `--yes` and `--json` never authorize that; a non-interactive run prints the commands and stops.

Each check reports `ok`, `warn`, `fail`, or `fixed` for one the run just repaired. A failed report exits 5. Warnings do not block anything.

| Check | What to do |
|---|---|
| Host architecture | Use Linux x86_64. The aarch64 runtime is deferred. |
| `/dev/kvm` missing | Enable virtualization in firmware and load `kvm_intel` or `kvm_amd`. A guest VM or CI runner may also need nested virtualization turned on. |
| `/dev/kvm` permission denied | Run the group command doctor prints, normally `sudo usermod -aG kvm $USER`, then log out and back in. Doctor reads the device's real group name rather than assuming `kvm`. |
| Runtime directory | Set `XDG_RUNTIME_DIR` to a user-owned mode-0700 directory. Without it Firestone uses `/tmp/firestone-<uid>` and warns. Doctor creates that fallback safely. |
| Vendored binaries, embedded helpers, or the Firestone SSH key | Run `firestone doctor`; it downloads and generates these itself. A helper an older release installed under the same name is replaced with this build's payload. |
| `passt` | The standalone binary carries the exact helper. If AppArmor restricts unprivileged user namespaces, review the literal `/usr/libexec/firestone/passt-2025_02_17.a1e48a0` profile commands doctor prints. Firestone never grants `userns,` to `~/.local/share/firestone/bin/*` or any other user-writable path. |
| `qemu-img` | The standalone binary carries qemu-img 8.2.2 and materializes it on the first image operation or on `doctor`. It needs no user namespace. |
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

Next: the [web interface](web-ui.md), or [machines](machines.md) for the full command set. The page list is in the [documentation index](README.md).
