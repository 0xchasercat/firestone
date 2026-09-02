# Troubleshooting

Symptom-to-repair table for the thirteen error kinds, and how Firestone decides what is recoverable.

Run `firestone doctor` first. It repairs the host problems it owns and prints the exact command for the ones it will not run itself; the AppArmor repair on Ubuntu is the only one that waits, behind `firestone doctor --fix`. Then use the narrowest log: `firestone logs NAME --source vmm` for a VMM failure, `--source shim` for a supervision failure, `--source console` for a guest failure.

Every Firestone error carries a stable kind, concrete context and, where there is something to do, a hint. The kinds are `usage`, `invalid_spec`, `not_found`, `not_running`, `conflict`, `already_exists`, `already_running`, `busy`, `dependency`, `timeout`, `checksum`, `interrupted` and `generic`. REST returns the same kind in the JSON envelope, so the repair for a 409 from `curl` is the repair for exit code 4 from the CLI.

| Symptom | What it means and what to do |
|---|---|
| `host architecture ... unsupported` | Run on Linux x86_64. Compiling for aarch64 does not make it a runtime target. |
| `/dev/kvm does not exist` | Enable hardware virtualization, load the matching KVM module, or enable nested KVM in the outer hypervisor. A normal container has no KVM. |
| `/dev/kvm does not open read/write` | Run doctor's detected `usermod` command and log in again. Do not make `/dev/kvm` world-writable. |
| `passt not found` or a version rejection | Run doctor; it names the exact helper and probes every option it needs. On a standalone release this means the embedded payload failed to materialize. |
| A helper `has length X; expected Y` | An older release installed different bytes under that name. Any command that needs the helper now replaces it with this build's payload; nothing to do by hand. |
| `image store is busy with another mutation` | Another image operation holds the store lock. A pull of the same reference waits for it and then uses the published image, so this names a different operation: wait for it and retry. |
| `qemu-img`, `ssh` or `ssh-keygen` missing | Run the package command doctor prints. |
| An unsafe config, data, runtime, lock, log or socket path | Move the unexpected node aside and let Firestone recreate its own. Do not chmod or follow an unknown-owner or symlinked node to silence the check. |
| Less than 5 GB free | Free space on the data filesystem, or move the data directory with `FIRESTONE_DATA_DIR`. |
| `unknown image` | Use a catalog reference, a strict HTTPS URL, an existing local path, or an OCI reference such as `docker://nginx` or `ghcr.io/owner/app:v1`. A bare `nginx` is a catalog name. |
| Checksum mismatch | Do not bypass it. Retry from a trusted network and compare the vendor metadata. A direct URL needs `--sha256`. |
| Disk is smaller than the base image | Recreate the machine with a larger `--disk` before an overlay exists. |
| Name already in use | Reuse the machine, or pass a different `--name` when running an image. |
| Machine is busy | Wait for the other action to release the machine lock. Do not delete the lock file. |
| Start timed out | Read `console`, `vmm` and `shim` logs. A slow first cloud-init run may need a larger `--timeout`; a repeated hang needs diagnosis rather than an unlimited timeout. An OCI machine skips the SSH wait entirely, so a timeout there points at the boot itself; read `console`. |
| SSH host key changed | Do not disable host-key checking. Confirm that a seed change you made regenerated the guest. Firestone removes `known_hosts` on a seed rewrite and on `rm`; an unexplained change is a hard failure. |
| `shell` says not running | Start the machine. Vsock SSH works even with `--net none`, but not on an OCI machine, which has no sshd. |
| Console requires a terminal | Run `firestone console NAME` in an interactive terminal. Use `logs --source console` in a script. |
| A log is empty on a machine that never started | Expected. A machine that has not run has written no log, so the read succeeds with no output. |
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

The exit code for each kind is in [CLI and REST](cli-and-rest.md), and the host checks doctor runs are in the [quick start](quickstart.md). The page list is in the [documentation index](README.md).
