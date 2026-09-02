# Firestone documentation

Firestone runs Linux virtual machines on Cloud Hypervisor, and it runs OCI container images as virtual machines. It is one executable that carries the VMM, `passt` and `qemu-img` inside it. Firestone runs as your user, nothing of it keeps running between your commands, and every machine is a directory of files you can read.

* [Install](install.md): the release one-liner, placing the executable yourself, and building from source.
* [Quick start](quickstart.md): check the host with `doctor`, then boot your first machine.
* [Web interface](web-ui.md): what `firestone ui` serves, including the terminal page.
* [Machines](machines.md): create, start, edit, logs, stop, remove, snapshots, clone, resize, metrics.
* [Images](images.md): the catalog, the owned image store, and OCI container images.
* [Networking](networking.md): passt, port forwards, tap mode, and shared folders.
* [Cloud-init](cloud-init.md): provisioning, keys, passwords, static addressing, and secret handling.
* [CLI and REST](cli-and-rest.md): `cp`, `serve`, pruning, and the `--json` conventions.
* [Troubleshooting](troubleshooting.md): the symptom table and how recovery is decided.
* [Security](security.md): the security model, paths, and on-disk state.

[`openapi.json`](openapi.json) is the OpenAPI 3.1 contract for the REST API. These pages are about using the tool.

Firestone targets Linux x86\_64 hosts with KVM. The aarch64 target compiles and its catalog metadata exists, but there is no aarch64 runtime release and `doctor` refuses an aarch64 host. Non-Linux hosts, non-Linux guests, and cross-architecture emulation are not planned.
