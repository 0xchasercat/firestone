---
icon: shield-halved
---

# Security

Firestone's security posture and trust boundaries.

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

| Purpose       | Default                                                     | Override                                                                         |
| ------------- | ----------------------------------------------------------- | -------------------------------------------------------------------------------- |
| Config        | `$XDG_CONFIG_HOME/firestone`, else `~/.config/firestone`    | `FIRESTONE_CONFIG_DIR`                                                           |
| Data          | `$XDG_DATA_HOME/firestone`, else `~/.local/share/firestone` | `FIRESTONE_DATA_DIR`                                                             |
| Runtime       | `$XDG_RUNTIME_DIR/firestone`, else `/tmp/firestone-<uid>`   | `FIRESTONE_RUNTIME_DIR`                                                          |
| Isolated root | Not set by default                                          | `FIRESTONE_HOME`, or `--home`, which maps to `config`, `data` and `run` children |

Use an isolated root for experiments, so nothing you try touches your real machines:

```sh
firestone --home "$PWD/.firestone-sandbox" doctor --fix
firestone --home "$PWD/.firestone-sandbox" create sandbox ubuntu
```

Under the data directory:

* `bin/` holds checksum-verified pinned binaries.
* `ssh/id_ed25519` and its `.pub` are Firestone's identity.
* `images/image-<digest>.qcow2` and a JSON sidecar are one stored base image.
* `machines/<name>/firestone.toml` is desired state, mode 0600.
* `machines/<name>/state.json` is runtime facts.
* `machines/<name>/` also holds `disk.qcow2`, `seed.img` or `config.img`, `vmconfig.json`, `known_hosts`, `snapshots/` and the logs.

Change desired state with `firestone edit NAME`, a REST `PUT` or `PATCH`, or the web interface's edit dialog. Do not hand-edit `state.json`, a lock file, an image sidecar, a socket or a pid file.&#x20;

How cloud-init secrets are stored is in [cloud-init](cloud-init.md), and the listener modes are in [CLI and REST](cli-and-rest.md). The page list is in the [documentation index](./).
