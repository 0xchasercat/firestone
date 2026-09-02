# Machines

The machine lifecycle, plus snapshots, clone, resize and metrics.

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

Two limits before you clone something important. A source that pins `network.mac` explicitly passes that address to the clone verbatim, and Firestone warns naming both machines; change it before running both on the same L2 segment. And a copied overlay carries the guest's `/etc/machine-id` and `/etc/hostname`, because Firestone does not rewrite guest filesystems. Run `systemd-firstboot --setup-machine-id` inside the clone when a unique guest identity matters, or use `--fresh-disk`.

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

```
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

Related: [images](images.md) for what a machine boots, [networking](networking.md) for forwards and mounts, and [cloud-init](cloud-init.md) for provisioning. The page list is in the [documentation index](./).
