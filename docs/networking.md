---
icon: network-wired
---

# Networking

Network modes, port forwards and their restart rule, and shared folders.

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

`passt` fixes its mappings when it spawns and offers no way to change them afterwards, and a Cloud Hypervisor vhost-user session does not survive a `passt` restart. There is no hot-apply for port forwards.

Editing forwards on a running machine creates a mismatch between the configured and active sets. To keep behavior predictable, the system handles pending forward changes across interfaces as follows:

* `ls`: Appends an asterisk (`*`) to the `FORWARDS` cell and displays `* forwards pending restart` beneath the table. The cell continues displaying active forwards since those are the only reachable ports.
* `show`: Prints active forwards to `stdout` (preserving clean JSON output) and writes `forwards pending restart` to `stderr`.
* API / CLI Writes: Mutations via `edit`, `PUT`, or `PATCH` return a `port forwards apply on restart` warning.
* Web UI: Appends a "pending restart" badge next to the active forward chips.

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

Static addressing for a tap guest is in [cloud-init](cloud-init.md), and the trust boundaries a forward or a mount opens are in [security](security.md). The page list is in the [documentation index](./).
