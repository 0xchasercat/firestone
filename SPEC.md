# Firestone — design and implementation spec (v0.1)

Status: implementation-ready draft. This document is the source of truth for v0.1.
Anything not in it is out of scope until it is added to the decision log (§21).

## Contents

0. How to use this document
1. Product philosophy
2. Scope and non-goals
3. Glossary
4. Architecture
5. Core model: spec, actions, events
6. Filesystem layout and state
7. Configuration (`firestone.toml`)
8. Images
9. Boot: firmware, VMM config, start/stop sequences
10. Cloud-init
11. Shell, console, logs
12. Networking
13. Shared folders
14. The shim
15. CLI
16. REST API
17. Dependencies and `doctor`
18. Implementation notes
19. Testing
20. Assumptions to verify before relying on them
21. Decision log
22. Milestones
24. Clone
Appendix A. Example session
Appendix B. Suggested `CLAUDE.md`

---

## 0. How to use this document

- Sections marked **normative** define behavior that must be implemented as written. Everything else is guidance and may be adapted if the philosophy in §1 is respected.
- Items tagged **[verify N]** are assumptions about third‑party tools (cloud‑hypervisor, passt, virtiofsd, cloud‑init, systemd). They are collected in §20 with the concrete check to run. Check the pinned version's `--help`, man page, or OpenAPI spec, or run the tool. Do not guess flag names or JSON field names.
- Where this document is silent, decide by §1 and record the decision in §21 in the same commit.
- Prefer deleting scope over adding it. If something is not needed daily by most users, it waits.

---

## 1. Product philosophy

Firestone is a modern, minimal, product‑grade tool for running Linux virtual machines on Linux. It takes the good parts of incus, quickemu and lima, discards their bad parts, and puts the user experience of ultra‑modern developer tooling on top of raw, low‑level virtualization.

Principles, in priority order. When two conflict, the earlier wins.

1. **Correctness and reliability before polish.** A polished experience that is not backed by true correctness and structural reliability is a scam. Few features, done completely.
2. **Sane defaults, zero paternalism.** The default path just works. Every other window stays open. Firestone never silently changes host policy. A privileged repair is permitted only from an interactive `doctor --fix` after Firestone displays the exact change and the user explicitly confirms it; non-interactive callers receive commands instead. Every failure still says exactly what to run and why.
3. **Frictionless for beginners, unrestricted for power users.** Configurability is designed to minimize friction, not merely to exist. Every knob is reachable from the CLI and from the config file, deterministically.
4. **Never a black box.** Every action produces immediate, continuous, informative feedback: what is happening, what is being waited on, how long it took, what failed and why.
5. **Three identical surfaces.** CLI, config file and REST API project one model. An action through any surface is reflected identically in the others. No exceptions.
6. **Modern only.** No legacy compatibility: KVM, virtio, cloud images, cloud‑init, systemd guests, vhost‑user, vsock. If a technology is legacy, it is not supported rather than half‑supported.
7. **Daemonless.** No long‑running global service. State lives on the filesystem in a form the user can read with `ls` and `cat`.
8. **Transparent.** Image storage, state, sockets and logs are plain files in predictable places.

---

## 2. Scope and non-goals

### 2.1 In scope (v0.1)

- Create, start, stop, restart, delete, list and inspect machines built from stock cloud images.
- Instant context: `firestone run ubuntu` resolves and caches the image for the host architecture, boots it, and drops the user into a root shell.
- Shell access over vsock (works with no networking configured), raw console attach, log viewing.
- Rootless by default. Networking through passt with port forwards; tap mode for users who manage their own bridge.
- Shared folders through virtio‑fs.
- User‑supplied cloud‑init user‑data and network‑config; extra SSH keys.
- Image management: built‑in catalog, pull with checksum verification, list, remove, prune.
- `doctor`: diagnoses the host and prints exact fixes.
- REST API over a unix socket exposing the same actions with the same streaming feedback.
- Linux x86_64 hosts and same-architecture guests. aarch64 remains a compile-only target without runtime or standalone-helper support.

### 2.2 Non-goals (v0.1)

Snapshots and restore; live migration; PCI/GPU passthrough; graphics/VNC/SPICE; non‑Linux guests; cross‑architecture emulation; VM‑to‑VM L2 networking beyond user‑managed tap; remote hosts (`FIRESTONE_HOST`); a routable (non‑loopback) or unauthenticated TCP listener; hotplug of CPU/memory/devices (cloud‑hypervisor supports it; not exposed yet); image building or customization; a provisioning DSL beyond cloud‑init; Windows/macOS hosts.

Each of these is real. None is daily for most users yet.

---

## 3. Glossary

| Term | Meaning |
|---|---|
| machine | A named VM definition plus its disk and runtime state. Identity is its directory name. |
| spec | `firestone.toml` for a machine: the declarative, user‑editable desired state. |
| state | `state.json`: runtime facts (pids, sockets, ports, last exit). A cache, not the truth for liveness. |
| image / base | A pristine, checksum‑verified cloud image stored once, never modified. |
| overlay | The machine's writable qcow2 disk backed by a base image. |
| VMM | The virtual machine monitor process: `cloud-hypervisor`. |
| shim | `firestone _shim <name>`: the one long‑lived process per machine that owns the VMM and sidecars. |
| sidecar | A helper process serving a vhost‑user device to the VMM: `passt` (network), `virtiofsd` (shared folders). |
| catalog | The table mapping image references (`ubuntu:24.04`) to download URLs, checksums and firmware. |
| data dir | `~/.local/share/firestone` (images, machines, vendored binaries, SSH key). |
| runtime dir | `$XDG_RUNTIME_DIR/firestone` (sockets, pids). Cleared by the OS on reboot. |
| action | One imperative operation on the model (`Start`, `Stop`, `Pull`, …). |
| event | One structured progress message emitted by an action. |

---

## 4. Architecture

### 4.1 Process model

```
 firestone (CLI)                     firestone serve (optional)
 runs one action, exits              stateless HTTP front over the same actions
        │                                     │
        └──────────────────┬──────────────────┘
                           │ reads spec, takes the machine lock,
                           │ spawns the shim or talks to the running VMM
                           ▼
 ┌─ firestone _shim <name> — exactly one per machine ───────────────────────────┐
 │  spawns, supervises, reaps, tears down (in order):                           │
 │                                                                              │
 │   passt ────────── vhost-user ── net.sock ──┐                                │
 │   virtiofsd (×N) ─ vhost-user ── fsN.sock ──┤                                │
 │                                             ▼                                │
 │   cloud-hypervisor ── api.sock (REST) ── vsock.sock ── console PTY           │
 │        └── guest: sshd on vsock port 22, autologin getty on hvc0             │
 │                                                                              │
 │  writes state.json while the machine is running                              │
 └──────────────────────────────────────────────────────────────────────────────┘
                           │
                           ▼
 ~/.local/share/firestone/machines/<name>/   firestone.toml  state.json  disk.qcow2  seed.img  console.log
 $XDG_RUNTIME_DIR/firestone/<name>/          api.sock  vsock.sock  console.sock  shim.sock  net.sock  fs0.sock
```

Both entry points are stateless. Every CLI invocation and every REST request:

1. resolves the machine directory,
2. takes the per‑machine lock for mutating actions,
3. reconciles `state.json` against liveness (§4.4),
4. either spawns the shim (start), talks directly to the VMM's `api.sock` (info and liveness), or talks to the shim's `shim.sock` (stop).

`firestone serve` is axum in front of the same `Action` dispatcher the CLI uses. It holds no state, can be killed and restarted at any time, and may run concurrently with CLI invocations; the machine lock serializes them.

### 4.2 Why a shim and not a daemon

"Daemonless" means no global service, not no processes. A VM is a long‑running process by definition. The shim is the per‑machine supervisor that Podman calls `conmon`:

- It is the parent of `cloud-hypervisor`, `passt` and `virtiofsd`, so it gets their exit statuses and reaps them.
- It enforces startup order (sidecar sockets must exist before the VMM connects to them) and teardown order (ACPI power button → wait → escalate → stop sidecars → final state write).
- It owns `state.json` while the machine runs, so there is exactly one writer at a time (§4.3).
- It survives the CLI exiting, terminal closing, and `serve` restarting.

Without it, the CLI would have to double‑fork the VMM and lose exit codes, crash reasons and deterministic teardown. A few hundred lines and ~2 MB of RSS per machine is the right price.

The shim does not proxy the VMM API. `vm.info` and liveness use `api.sock` directly, and `firestone shell` uses `vsock.sock` directly. Cloud Hypervisor v53 rejects socket mode for the virtio-console device, so `firestone console` uses a shim-brokered PTY through `console.sock` (§11.6). Pause and resume are not v0.1 Firestone actions (§5.2, §21).

### 4.3 Locking and state ownership (normative)

- Every machine directory contains a `lock` file. Mutating actions (`create`, `start`, `stop`, `restart`, `rm`, `edit` on save, spec `PUT`/`PATCH`) hold `flock(LOCK_EX)` on it for their duration. Read‑only actions (`ls`, `show`, `logs`, `shell`, `console`) do not take the lock.
- Lock acquisition blocks for up to 10 s, emitting a `Log` event "waiting for another firestone operation on `<name>`" after 1 s, then fails with kind `busy`.
- `state.json` has exactly one writer at any time:
  - while a shim is alive for the machine, only the shim writes it;
  - when no shim is alive, only a CLI/serve action holding the lock writes it (create, rm, reconciliation, and the `starting` transition immediately before spawning the shim).
- All writes are atomic: write to `state.json.tmp` in the same directory, `fsync`, `rename`.
- Machine names are unique because `mkdir` of the machine directory is atomic; `create` uses `create_dir` (not `create_dir_all`) and maps `EEXIST` to kind `already_exists`.

### 4.4 Liveness (normative)

`state.json` records a `status`, but liveness is never inferred from a stored status or a pid alone. A machine is **running** iff a connection to `$RUNTIME/<name>/api.sock` succeeds and `GET /api/v1/vmm.ping` returns 200. Rules:

- Readers call `reconcile(name)` before using state. If `status ∈ {starting, running, stopping}` but the ping fails, and the shim pid is not alive, the reader (holding the lock) rewrites state to `stopped` with `last_exit.reason = "stale"` (or `"host reboot"` if the runtime dir does not exist).
- If the shim pid is alive but the VMM ping fails, the machine is `starting` or `stopping`; readers report that status and do not rewrite.
- Sockets live in the runtime dir (tmpfs, cleared on reboot) precisely so that a host reboot leaves no false positives.
- Pid checks use `/proc/<pid>/cmdline` and require the expected argv (`firestone _shim <name>`) to guard against pid reuse.

---

## 5. Core model: spec, actions, events (normative)

The "three identical surfaces" principle is only enforceable if there is one definition to project from. There are exactly three core types.

### 5.1 `MachineSpec` — one struct, three serializations

```rust
/// Desired state of a machine. TOML on disk, JSON over REST, flags on the CLI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MachineSpec {
    pub image: ImageRef,             // "ubuntu:24.04" | URL | path
    pub arch: Option<Arch>,          // default: host arch; validation: must equal host arch
    pub cpus: u8,                    // default 2
    pub cpus_max: Option<u8>,        // optional vCPU hotplug ceiling; >= cpus
    pub memory: ByteSize,            // default 2G
    pub memory_max: Option<ByteSize>,// optional memory hotplug ceiling; >= memory
    pub disk: ByteSize,              // default 20G (virtual size of the overlay); grows only
    pub user: String,                // default "root"; who `shell` logs in as
    pub network: NetworkSpec,
    #[serde(default, rename = "mount")]
    pub mounts: Vec<MountSpec>,
    pub cloud_init: CloudInitSpec,
    pub vmm: VmmSpec,
}

pub struct NetworkSpec {
    pub mode: NetMode,               // passt (default) | tap | none
    pub forward: Vec<PortForward>,   // "[proto:][bind:]host:guest[-range]"
    pub tap: Option<String>,         // required when mode = tap
    pub mac: Option<MacAddr>,        // generated once and persisted in state if absent
}

pub struct MountSpec {
    pub host: PathBuf,               // "~" expanded at load time
    pub guest: PathBuf,
    pub readonly: bool,              // default false
    pub tag: Option<String>,         // default "share<i>"
}

pub struct CloudInitSpec {
    pub user_data: Option<PathBuf>,      // relative to the machine dir, or absolute
    pub user_data_inline: Option<String>,// the same user part carried in the spec; excludes user_data
    pub network_config: Option<PathBuf>,
    pub ssh_keys: Vec<PathBuf>,          // public key files; contents appended to authorized keys
    pub ssh_authorized_keys: Vec<String>,// inline OpenSSH public keys; deduplicated with ssh_keys
    pub password: Option<String>,        // plaintext guest password for `user` (§10.5)
    pub ssh_pwauth: bool,                // default false; true enables guest SSH password auth
    pub provisioning: bool,              // default true; false = firestone injects nothing (§10.3)
}

pub struct VmmSpec {
    pub binary: Option<PathBuf>,         // override the vendored cloud-hypervisor
    pub firmware: Firmware,              // auto (default) | rhf | edk2 | path
    pub extra_args: Vec<String>,         // appended verbatim to the VMM process argv
    pub config_overlay: Option<serde_json::Value>, // JSON merge-patch applied to the VmConfig (§9.2)
}
```

The CLI does not parse flags into `MachineSpec` directly. It parses into `MachineSpecPatch`, a mirror struct in which every settable leaf is `Option<T>`. The patch also has a typed `clear` list covering every optional leaf and append-vector leaf. Unknown clear paths and setting and clearing the same leaf in one layer are errors. `firestone-core` exposes clap-free field metadata and the CLI crate owns the `clap::Args` projection.

Effective spec layering is deterministic:

1. built-in defaults;
2. global config `[defaults]` over the built-ins;
3. `firestone.toml`, where a present vector replaces the lower-layer vector;
4. the CLI or REST patch, where a present vector appends.

Each layer applies its validated `clear` operations before its set operations. Clearing an optional leaf produces `None`; clearing a vector produces an empty vector. Machine persistence writes the effective spec as a machine-file layer, including clear entries for every effective optional `None`, so reloading it over unchanged global defaults is idempotent. The same `MachineSpecPatch` shape is the body of REST `PATCH`; `--clear FIELD` projects the typed clear list on the CLI.

### 5.2 `Action` — one enum, two transports

```rust
pub enum Action {
    Create { name: String, spec: MachineSpec },
    Start { name: String, wait: bool, timeout: Duration },
    Stop { name: String, timeout: Duration, force: bool },
    Restart { name: String, timeout: Duration },
    Remove { names: Vec<String>, force: bool },
    List,
    Show { name: String, vmconfig: bool },
    SetSpec { name: String, spec: MachineSpec },        // PUT
    PatchSpec { name: String, patch: MachineSpecPatch }, // PATCH
    Logs { name: String, source: LogSource, lines: u32, follow: bool },
    Cp { source: String, target: String, recursive: bool },             // CLI-only (§11.9)
    CatalogList,
    ImageList, ImagePull { r#ref: ImageRef, sha256: Option<String> }, ImageInspect { id: String },
    ImageRemove { id: String, force: bool }, ImagePrune,
    Doctor { fix: bool },
    Version,
    Resize { name: String, cpus: Option<u8>, memory: Option<ByteSize> },  // §9.5
}
```

CLI subcommands and REST routes are thin adapters that construct an `Action` and hand it to `Dispatcher::run(action, &mut EventSink)`. Terminal attachment commands (`shell`, `console`, and `edit`) remain CLI-only. Bounded log reads, including follow, use the shared `Logs` action and `Output`/`Result` events; the CLI owns only terminal signal projection while REST maps the same operation to its documented stream. `cp` (§11.9) is the one action with a CLI-only transport: `Action::Cp` plans the exact `scp` argv and returns it as its `Result`, and only the CLI executes that plan, so it publishes no REST route.

### 5.3 `Event` — one stream, three renderers

```rust
pub enum Event {
    StepStart  { id: StepId, label: String },
    StepUpdate { id: StepId, detail: String },              // "waiting for cloud-init (first boot)"
    Progress   { id: StepId, done: u64, total: Option<u64>, unit: Unit },
    StepDone   { id: StepId, detail: Option<String>, elapsed_ms: u64 },
    StepSkip   { id: StepId, reason: String },              // "cached", "already running"
    StepFail   { id: StepId, error: ErrorInfo },
    Log        { level: Level, message: String },           // secondary; dim in TTY, -v shows debug
    Output     { data: String },                            // primary stdout data, including logs
    Result     { action: String, payload: serde_json::Value }, // exactly one, last, on success
}
```

- The CLI renders events as spinners, bars and colored lines (§15.3).
- `serve` streams them as NDJSON (§16.3).
- `firestone --json` prints them as NDJSON to stdout, unchanged.

Every action emits Result exactly once on success, or returns an error (which the CLI/REST layer turns into the terminal failure output). Start ids stay ordered. M1 emits image, disk, seed, shim, net, fs, and vmm, then returns only after status is running. M2 appends boot and ssh readiness; M1 never claims those checks.

### 5.4 The drift test (normative)

A core unit test derives recursive leaf paths from the serialized/schema shapes of a fully populated `MachineSpec` and `MachineSpecPatch`, excluding only patch control metadata and collapsing documented composite or opaque fields. It asserts spec/patch parity and derives expected field metadata from those paths. A CLI unit test asserts that every core field-metadata entry has a corresponding clap flag by introspecting `clap::Command::get_arguments()`, mapping `a.b.c` → `--a-b-c` with documented exceptions such as `mount` → `--mount host:guest[:ro]` and the typed clear list → repeatable `--clear FIELD`. The surfaces cannot drift without these tests failing.

---

## 6. Filesystem layout and state (normative)

### 6.1 Directories

Resolved once at startup into a `Paths` struct; no other code computes paths.

| Purpose | Default | Override |
|---|---|---|
| config | absolute `$XDG_CONFIG_HOME/firestone/`, else `~/.config/firestone/` | `FIRESTONE_CONFIG_DIR` |
| data | absolute `$XDG_DATA_HOME/firestone/`, else `~/.local/share/firestone/` | `FIRESTONE_DATA_DIR` |
| runtime | `$XDG_RUNTIME_DIR/firestone/` (fallback `/tmp/firestone-<uid>/`, mode 0700) | `FIRESTONE_RUNTIME_DIR` |
| all of the above | | `FIRESTONE_HOME=<dir>` sets `config=<dir>/config`, `data=<dir>/data`, `runtime=<dir>/run` (used by tests) |

`FIRESTONE_HOME` has precedence over individual overrides; individual overrides have precedence over XDG and home defaults. Relative XDG values are invalid under the XDG base-directory specification and are treated as unset. An absolute `XDG_RUNTIME_DIR` must already be a real directory owned by the current uid with mode 0700; Firestone creates only its `firestone` child. When no valid absolute runtime value is set, the fallback is `/tmp/firestone-<uid>` with uid ownership and mode 0700.

`Paths` captures one absolute startup HOME and uses it for every `~` expansion. Explicit runtime roots, including the `FIRESTONE_HOME` run directory, are accepted only when their existing ancestry cannot be renamed by another uid. Root-owned non-writable ancestors and root-owned sticky `/tmp` are safe; symlink ancestors and writable wrong-owner ancestors are errors. Firestone never recursively creates an untrusted runtime ancestry and then validates only the leaf.

Relative user paths keep their `.` and `..` components until the kernel resolves the complete path. Validation may canonicalize an existing complete path after the kernel resolves it; Firestone does not lexically erase missing prefixes or symlink semantics. Owned machine, image, binary and seed file names are single path components without control characters. Arbitrary user-supplied absolute paths are not restricted by the owned-name rule.

Firestone-owned data directories use the same ancestry trust model as runtime paths. Existing ancestors must be real directories owned by the current uid or root and must not be renameable by another uid; root-owned sticky shared directories are allowed. The final data, `machines`, machine, `images`, `bin`, and `ssh` directories must be owned by the current uid and must not be group- or world-writable. Firestone creates owned directories with mode 0700 and refuses unsafe existing paths before reading, writing, fixing, or publishing machine or image data.

```
~/.config/firestone/
  config.toml                  global defaults (§7.3)
  catalog.toml                 optional catalog additions/overrides (§8.1)

~/.local/share/firestone/
  bin/                         vendored binaries: cloud-hypervisor-<ver>, hypervisor-fw-<ver>, CLOUDHV-<ver>.fd, virtiofsd-<ver>
  ssh/id_ed25519, id_ed25519.pub   firestone's own key (0600), generated on first use
  images/
    image-<identity-sha256>.qcow2
    image-<identity-sha256>.json   strict sidecar v1 (§8.3)
  machines/<name>/             see §6.2

$XDG_RUNTIME_DIR/firestone/
  serve.sock                   REST listener (only while `serve` runs)
  <name>/
    shim.sock  api.sock  vsock.sock  console.sock  net.sock  fs0.sock …
    shim.pid
```

### 6.2 Machine directory

```
machines/<name>/
  firestone.toml     the spec (user-editable; the source of truth for desired state)
  state.json         runtime state (§6.3); written atomically
  lock               flock target; empty file
  disk.qcow2         overlay on the base image
  seed.img           cloud-init NoCloud vfat image (§10.1)
  seed/              rendered inputs kept for inspection: meta-data, user-data, network-config
  user-data.yaml     optional; referenced by cloud_init.user_data (relative paths resolve here)
  known_hosts        per-machine SSH host keys
  console.log        serial console (kernel + systemd output), appended across boots
  vmm.log            cloud-hypervisor's own log
  shim.log           the shim's log
  passt.log  virtiofsd-0.log …
```

`firestone rm` deletes the whole directory. Nothing about a machine lives anywhere else except its sockets in the runtime dir and its base image (shared, reference-counted by `images prune` scanning `state.json` files).

Creation takes the per-machine lock before writing the `.creating` publication marker. A complete machine is published only after its spec and state are durable. If a prior creator died before publication, a later `create` may acquire the unlocked incomplete directory, revalidate every owned path, remove only that stale incomplete publication, and retry. A locked creation is busy; Firestone never removes an active creation or a directory containing a complete machine.

### 6.3 `state.json`

```json
{
  "version": 1,
  "status": "running",
  "image": { "ref": "ubuntu:24.04", "id": "ubuntu-24.04-x86_64-1a2b3c4d", "sha256": "…" },
  "mac": "52:54:00:9a:1f:c3",
  "cid": 3,
  "instance_id": "iid-ubuntu-5f3a9c1e2b7d",
  "shim_pid": 41200,
  "vmm_pid": 41207,
  "sidecar_pids": { "passt": 41203, "virtiofsd-0": 41205 },
  "runtime_dir": "/run/user/1000/firestone/ubuntu",
  "started_at": "2026-08-28T09:12:44Z",
  "forwards": ["tcp:0.0.0.0:8080:80"],
  "degraded": [],
  "last_exit": { "at": "2026-08-27T18:02:10Z", "code": 0, "signal": null, "reason": "guest shutdown" }
}
```

For a newly-created machine that has not pulled its image, `image.ref` is the canonical catalog reference (or validated URL/path) and `image.id`/`image.sha256` are null. They become non-null together after a checksum-verified pull and before any overlay references the image.

`status ∈ {created, starting, running, stopping, stopped, failed}`. Transitions:

```
created ──start──▶ starting ──vmm booted──▶ running ──stop──▶ stopping ──▶ stopped
                      │                        │                             ▲
                      └── failure ──▶ failed    └── vmm crash ──▶ failed ─────┘ (start again)
```

`degraded` lists sidecar problems while the VM keeps running (e.g. `"passt exited (code 1)"`); `ls` shows `running!` and `show` prints the list. Firestone does not kill a VM because a sidecar died.

### 6.4 Atomic writes

All files firestone writes that another process may read (`state.json`, `firestone.toml` from `edit`/`PUT`, image metadata, `seed.img`) are written via temp file + `fsync` + `rename` in the same directory. Never truncate‑and‑write in place.

---

## 7. Configuration (normative)

### 7.1 `firestone.toml` — full schema and defaults

```toml
# machines/<name>/firestone.toml
# Every key is optional. Shown with its default.

image  = "ubuntu:24.04"    # catalog ref "distro[:version]", https URL, or local path (qcow2/raw)
cpus   = 2                 # 1..=host cpus (warn above host count; hard error above 255)
# cpus_max   = 8           # optional vCPU hotplug ceiling for `resize`; must be >= cpus
memory = "2G"              # "512M", "4G", "4096M", or integer MiB
# memory_max = "8G"        # optional memory hotplug ceiling for `resize`; must be >= memory
disk   = "20G"             # virtual size of the overlay; must be >= base image virtual size; grows on next start, never shrinks
user   = "root"            # login user for `shell`; root works because provisioning enables it
# clear = ["network.tap"] # explicitly clear inherited optional or vector fields

[network]
mode    = "passt"          # "passt" | "tap" | "none"
forward = []               # ["8080:80", "udp:5353:53", "127.0.0.1:2222:22", "8000-8010:8000-8010"]
# tap   = "tap0"           # required when mode = "tap"; a tap device the user already owns
# mac   = "52:54:00:xx:xx:xx"   # generated once and stored in state.json if omitted

# [[mount]]                # zero or more
# host     = "~/code"      # ~ expands; must exist
# guest    = "/code"       # absolute; created in the guest
# readonly = false
# tag      = "share0"      # default "share<i>"

[cloud_init]
# user_data        = "user-data.yaml"   # relative to the machine dir or absolute; #cloud-config or #!script
# user_data_inline = "#cloud-config\n…" # the same user part inline, max 32 KiB; excludes user_data
# network_config   = "network-config.yaml"
ssh_keys            = []                 # ["~/.ssh/id_ed25519.pub"]; contents appended to authorized keys of `user`
ssh_authorized_keys = []                 # ["ssh-ed25519 AAAA… me@host"]; inline keys, deduplicated with ssh_keys
# password          = "…"                # plaintext password for `user`, applied through chpasswd (§10.5)
ssh_pwauth          = false              # true enables guest SSH password authentication
provisioning        = true               # false: firestone injects nothing; shell/console will not work

[vmm]
# binary       = "/usr/local/bin/cloud-hypervisor"   # default: embedded pinned VMM on x86_64
firmware       = "auto"                  # "auto" | "rhf" | "edk2" | "/path/to/firmware"
extra_args     = []                      # appended verbatim to the cloud-hypervisor argv (process-level flags)
# config_overlay = '''{"memory":{"shared":true},"rng":null}'''
# JSON object merge-patch (RFC 7396), stored as canonical JSON text in TOML
```

Unknown keys are an error (`deny_unknown_fields`) with a did‑you‑mean hint.

`clear = ["arch", "network.tap", ...]` lives at the root of a `MachineSpecPatch`: top-level in `firestone.toml` and REST PATCH, under `[defaults]` in global config, and behind repeatable `--clear FIELD` on the CLI. The allowed values are a closed enum containing every optional leaf and append-vector leaf. Persisting a full effective spec emits clear entries for optional leaves whose value is `None`. A layer may not both clear and set the same leaf.

`vmm.config_overlay` is one object-only merge-patch value on all surfaces. JSON and REST carry the object directly. TOML stores canonical JSON text under the same `config_overlay` key so RFC 7396 nested `null` deletion round-trips without a second key. `--vmm-config JSON` accepts the same JSON text.

### 7.2 Validation rules

Validation runs on every load. Errors carry the TOML key path.

- `image` follows §8.2 order: an existing local candidate wins, then a strict `https://` URL, then catalog resolution. Arbitrary URLs reject parser violations, userinfo, whitespace, a missing host and fragments. A missing-path hint is emitted only after catalog resolution fails.
- `arch`: if set, must equal the host architecture; message explains that cross‑arch emulation is a non‑goal.
- `cpus` ≥ 1. `memory` ≥ 128M. `disk` ≥ base image virtual size (checked at overlay creation, where the base size is known).
- `cpus_max`, when set, ≥ `cpus`; `memory_max`, when set, ≥ `memory`. Both are optional and default to absent, meaning "no hotplug headroom" (§9.2). Errors carry the `cpus_max` / `memory_max` key.
- `disk` may grow but never shrink. Once a machine owns an overlay, a `PUT`, `PATCH`, or `resize` that resolves to a `disk` below the overlay's current virtual size is rejected with `invalid_spec` "disk shrink is not supported" and a hint naming the current size. A machine that has never started owns no overlay and may still change `disk` freely.
- `network.mode = "tap"` requires `network.tap`; the device must exist (`/sys/class/net/<tap>`), and `/dev/net/tun` must be openable. Firestone never creates it.
- `forward` entries parse per §12.4; guest ports 1–65535; host ports 1–65535 (ports < 1024 are allowed and will fail at passt start without privileges; passt's error is surfaced, not pre‑empted).
- At most 16 `mount` entries. Each `mount.host` is an existing canonical UTF-8 absolute directory owned by the current user, without symlink/alias components or group/world write; each ancestor is current-user/root-owned and not renameable by another uid, with root-owned sticky directories allowed. Host sources are pairwise disjoint after canonicalization. Each `mount.guest` is a canonical UTF-8 absolute non-root path, and guest paths are pairwise disjoint. Host and guest paths obey Linux's 4,095-byte path and 255-byte component limits. Effective tags are unique, 1 through 36 safe ASCII bytes, and default to `share<i>`.
- `cloud_init.user_data`: a symlink to a regular file is allowed. Firestone opens the target once with nonblocking regular-file checks and reads at most 1 MiB. The bytes must be UTF-8 and the first line must be `#cloud-config` or start with `#!`; otherwise error with hint (`provisioning = false` plus a raw user-data script is the escape hatch, see §10.2).
- `cloud_init.network_config`: a symlink to a regular file is allowed. Firestone opens the target once with nonblocking regular-file checks and reads at most 1 MiB of UTF-8 bytes.
- `cloud_init.ssh_keys`: each target may be a symlink to a regular file, is opened once with nonblocking regular-file checks, and is limited to 64 KiB; all configured key files together are limited to 256 KiB. Non-comment lines must parse as OpenSSH public keys.
- `cloud_init.user_data_inline`: at most 32 KiB of UTF-8 whose first line is `#cloud-config` or starts with `#!`, exactly as for a user-data file. Setting it together with `cloud_init.user_data` is an error that names both keys; neither value appears in the message.
- `cloud_init.ssh_authorized_keys`: each entry is exactly one non-comment OpenSSH public-key line of at most 64 KiB, parsed by the same `ssh-key` implementation as file-loaded keys. Surrounding whitespace is trimmed. The failing entry's index is reported; its bytes are not.
- `cloud_init.password`: 1 to 256 bytes of UTF-8 with no control characters. Errors report the length or the class of violation and never the value.
- `user`: `[a-z_][a-z0-9_-]*`.
- `vmm.firmware = "edk2"` on x86_64 uses `CLOUDHV.fd`; on aarch64 `CLOUDHV_EFI.fd`; a custom path must identify a readable regular file.
- `vmm.binary`, when set, must identify a bounded regular file executable by the current user, owned by root or the current uid, and not writable by group or other. Start imports the bytes from one no-follow descriptor into the machine-owned mode-0700 `vmm.bin` before hashing or execution. ELF binaries, shebang scripts, and wrappers remain valid; supervision records the actual post-`exec` executable and argv together with the immutable launch artifact and hash.
- Spec changes while running are accepted and saved with a `Log` warning "takes effect on next start". `Action::Resize` (§9.5) is the one exception: it applies CPU and memory live when the running machine has headroom. A spec change that alters `user` or any `cloud_init` field on a running machine additionally emits a `Log` warning naming those fields, because cloud-init reprovisions only when the instance identity changes on the next start (§10.4).

### 7.3 Global config `~/.config/firestone/config.toml`

```toml
[defaults]              # any MachineSpec key; layered under every machine's spec
cpus   = 2
memory = "2G"
disk   = "20G"

[start]
timeout_first_boot = "300s"
timeout            = "60s"

[stop]
timeout = "30s"

[ui]
color = "auto"          # "auto" | "always" | "never"   (NO_COLOR env also respected)

[images]
catalog = []            # extra catalog files, merged over the built-in one
insecure_registries = []  # OCI registries reachable over plain HTTP: "host" or "host:port" (§8.6)
```

`images.insecure_registries` is global only; it is never a machine key. Each entry is a literal `host:port` with a mandatory one-to-five-digit port and no scheme, path, or wildcard, and `docker.io`/`registry-1.docker.io` are rejected outright. The default empty list means every registry is contacted over HTTPS.

### 7.4 CLI flag ↔ field mapping

Generated from `MachineSpecPatch`. Rule: `a.b` → `--a-b`; vectors are repeatable flags. Documented exceptions:

| Field | Flag |
|---|---|
| `image` | positional on `run`/`create`, or `--image` |
| `cpus_max` | `--cpus-max COUNT` |
| `memory_max` | `--memory-max SIZE` |
| `network.mode` | `--net passt\|tap\|none` |
| `network.forward[]` | `-p, --forward SPEC` (repeatable) |
| `network.tap` | `--tap DEV` |
| `mount[]` | `--mount HOST:GUEST[:ro]` (repeatable) |
| `cloud_init.user_data` | `--user-data FILE` |
| `cloud_init.user_data_inline` | `--user-data-inline TEXT` |
| `cloud_init.ssh_keys[]` | `--ssh-key FILE` (repeatable) |
| `cloud_init.ssh_authorized_keys[]` | `--ssh-authorized-key KEY` (repeatable) |
| `cloud_init.password` | `--password-file FILE` (contents, minus one trailing newline; never an argv value) |
| `cloud_init.ssh_pwauth=true` | `--ssh-pwauth` |
| `cloud_init.provisioning=false` | `--no-provisioning` |
| `vmm.extra_args[]` | `--vmm-arg ARG` (repeatable) |
| `vmm.config_overlay` | `--vmm-config JSON` |
| `clear[]` | `--clear FIELD` (repeatable; closed field enum) |

`firestone create` writes the effective spec (defaults + flags) to `firestone.toml` with comments preserved from the template, so a user who started from the CLI can `edit` a fully documented file.

---

## 8. Images

### 8.1 Catalog format

Built into the binary (`catalog/images.toml`, `include_str!`), merged with `~/.config/firestone/catalog.toml` and any files listed in `[images].catalog` (later entries override by `distro:version`).

```toml
[[image]]
distro   = "ubuntu"
version  = "24.04"
aliases  = ["noble"]
default  = true                 # what bare "ubuntu" resolves to
firmware = "rhf"                # rhf | edk2   (what "auto" picks for this image)
format   = "qcow2"

[image.arch.x86_64]
firmware    = "edk2"             # optional architecture override; wins over entry firmware
url          = "https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-amd64.img"
checksum_url = "https://cloud-images.ubuntu.com/noble/current/SHA256SUMS"   # or: sha256 = "…"

[image.arch.aarch64]
url          = "https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-arm64.img"
checksum_url = "https://cloud-images.ubuntu.com/noble/current/SHA256SUMS"

[[image]]
distro   = "debian"
version  = "12"
aliases  = ["bookworm"]
default  = true
firmware = "rhf"
format   = "qcow2"
[image.arch.x86_64]
url          = "https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-genericcloud-amd64.qcow2"
checksum_url = "https://cloud.debian.org/images/cloud/bookworm/latest/SHA512SUMS"
checksum_alg = "sha512"
[image.arch.aarch64]
url          = "https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-genericcloud-arm64.qcow2"
checksum_url = "https://cloud.debian.org/images/cloud/bookworm/latest/SHA512SUMS"
checksum_alg = "sha512"
```

Initial catalog: `ubuntu` (24.04 default, 22.04), `debian` (12 default, 13), `fedora` (current stable). Add `arch`, `alma`, `rocky`, `opensuse`, `nixos` only after each boots in CI under the pinned firmware **[verify 3]**. URLs above are believed current; confirm at implementation time and never ship an entry that has not been booted.

`firestone catalog` and `GET /v1/catalog` expose the fully merged built-in and user catalog in canonical-reference order without reading the local image cache. Each row includes aliases and every available architecture paired with its effective firmware after applying an architecture override. The human table may collapse a firmware shared by every architecture; mixed firmware is labeled per architecture.

### 8.2 Resolution rules

`ImageRef` parsing, in order:

1. Absolute or relative path that exists → local file (raw or qcow2; detected by header).
2. `https://…` → download to the images dir keyed by URL sha; no checksum unless `--sha256` given (warn once).
3. An OCI reference per §8.5 → container registry.
4. `distro:version` or `distro:alias` → catalog entry.
5. `distro` → the catalog entry marked `default` for that distro.
6. Otherwise error `unknown image` listing the closest catalog names.

The host architecture selects the `[image.arch.<arch>]` table; a missing table is an error naming the architectures that exist. An optional architecture-level `firmware` overrides the entry default for that source only. Catalog distro, version, and alias components must start with ASCII alphanumeric and then contain only ASCII alphanumeric, `.`, `_`, `+`, or `-`; path separators, URL syntax, traversal components, colons, and controls are rejected while loading the catalog.

User-entered image arguments retain path-first resolution. Persisted machine references do not probe relative filesystem names: they classify a strict HTTPS URL first, an absolute canonical path second, and otherwise a validated canonical catalog reference. Thus a relative file cannot shadow `ubuntu:24.04` or a malformed `https:/…` reference after creation.

### 8.3 Pull and verify

- HTTPS bodies stream to `images/.pull-<source-key>.source.partial` through a fixed-size buffer while source SHA-256 and any SHA-256/SHA-512 verifier are updated. Progress is emitted at most once per 1 MiB of accumulated bytes regardless of transport frame size, followed by one final byte total. Checksum manifests are limited to 2 MiB, must be UTF-8, and must contain one unambiguous digest for the image URL's exact filename. A missing or conflicting entry, a digest mismatch, or a partial `Content-Length` response deletes every operation partial and returns kind `checksum` for verification failures.
- HTTPS requests use identity encoding, a 30 s connect timeout, a 30 min request timeout, and at most five redirects; every redirect must remain a strict HTTPS URL without credentials or a fragment.
- Local files are opened once relative to a canonical parent with `O_NOFOLLOW|O_NONBLOCK`; the descriptor must identify a current-user- or root-owned regular file without group/world write access. Firestone streams that same descriptor, compares device, inode, size, mtime, and ctime before and after, and rejects replacement or mutation. Parent aliases deduplicate to one canonical absolute source ref. Local bytes are copied into the owned store and never used directly as a backing file. A qcow2 source is published unchanged; a raw source is converted with `qemu-img convert -f raw -O qcow2 SOURCE TARGET`. Every published base is an owned, read-only qcow2 file.
- Source SHA-256 is always computed before conversion. `state.image.sha256` is this source SHA-256, while the sidecar separately records the stored qcow2 SHA-256. Cache comparison uses complete digests and all applicable identity fields; an eight-character display prefix is never a cache identity. `images ls` may remain metadata-only, but every cache hit or pinned base returned for execution re-hashes the stored qcow2 and rejects a mismatch before invoking `qemu-img`.
- Stable image ids are `image-<identity-sha256>`, where the complete 64-hex identity digest hashes length-framed sidecar version, source ref, optional source URL, host architecture, and complete source SHA-256; verifier provenance and firmware are intentionally excluded so validated metadata can strengthen in place. The base is `images/<id>.qcow2`; the strict atomic sidecar `images/<id>.json` is:
  `{version: 1, id, generation, source_ref, source_url, source_sha256, stored_sha256, architecture, firmware, source_format, stored_format: "qcow2", verification_algorithm, verification_digest, size, pulled_at}`. Every key is required; nullable values are explicit JSON `null`. The exact pretty-JSON-plus-newline bytes must be at most 64 KiB and are serialized and validated before the base path is published; atomic publication writes those same bytes. `generation` is positive and monotonically increases for each canonical source ref and architecture, including same-byte firmware or verifier upgrades. Local/direct-URL firmware is null; catalog firmware is required and persists independently of later catalog edits. Verification fields are both null only for an unchecked local/direct URL source; a later validated pull upgrades provenance, while an unchecked pull never downgrades it.
- Sidecar reads are capped at 64 KiB and machine-state reads at 1 MiB from the already-open no-follow descriptor. One `images/.lock` serializes pull publication, machine image pinning, remove, and prune. New owned directories and locks are forced to 0700/0600 despite umask. A current-user regular lock whose owner permissions are a strict subset of 0600 is recovered to 0600 after an interrupted creation; wrong-owner, special, symlinked, owner-executable, or group/world-accessible existing locks are rejected unchanged.
- Under that lock, recovery removes bounded, correctly typed unreferenced base-only, sidecar-only, and sidecar-temp publications, plus recognized pull partials, and completes deterministic `.removing` tombstones. A half-pair referenced by complete machine state is preserved and reported as checksum corruption rather than silently deleted.
- Events: `StepStart image` → `Progress` (bytes, total from `Content-Length`) → `StepDone image "ubuntu:24.04 · x86_64 · 613 MB"`, or `StepSkip image "cached"`. An unchecked HTTPS URL emits one warning naming `--sha256`.
- "Current" URLs (Ubuntu `current/`, Debian `latest/`) change over time. An explicit `images pull` refreshes the manifest and may publish a new id. Machine start resolves the canonical ref before a cache-first lookup: a manifest-backed catalog cache ignores a moved source URL and unresolved digest but requires canonical ref, architecture, declared format, and checksum algorithm; explicit digests require exact verifier and stable locator; an unchecked direct source requires the same locator and may reuse stronger stored verifier provenance. Explicit unchecked pulls still refresh mutable bytes and never downgrade provenance for the same identity. The unique highest generation wins and duplicate maxima fail closed. The first successful `start` atomically records canonical `image.ref`, immutable `image.id`, and source `image.sha256` before overlay creation. Every later start uses firmware from the pinned sidecar, requires its architecture and canonical source ref to match state, and does not require a deleted original local source or a current catalog entry; later pulls do not change the pin.

### 8.4 Overlays

- `create` records the canonical image reference in `state.json`; `image.id` and source `image.sha256` are null until first start resolves immutable content. Start writes both under the machine and image-store locks before lazily invoking:
  `qemu-img create -f qcow2 -F qcow2 -b <absolute base path> <overlay partial> <disk bytes>`
  The partial is inspected with `qemu-img info --output=json -f qcow2 PATH`. Immutable bases must be clean and non-corrupt. Writable overlays may report `dirty-flag: true` after a crash so qcow2/Cloud Hypervisor can recover them, but corrupt overlays, any external data file, malformed dependency-shaped fields, and unexpected backing metadata are rejected. An overlay must report both `backing-filename` and `full-backing-filename` as the exact owned base string, `backing-filename-format: qcow2`, and the requested virtual size before atomic publication or cached reuse as `<machine>/disk.qcow2`. `qemu-img info`, overlay creation, and raw conversion run through `Cmd` with respective 30 s, 60 s, and 30 min timeouts while the relevant locks remain held.
- Cloud Hypervisor v53 accepted the exact qcow2 overlay with `backing_files: true` and booted the converted Ubuntu 24.04 x86_64 source through edk2 to a serial login prompt. The M1-06 guest fio run records overlay and raw auxiliary-disk measurements in §21. This specification defines no performance threshold, so none is inferred. The aarch64 result remains open outside the M1 gate.
- Base images are mode 0400 and never attached read-write. `images rm` refuses (without `--force`) while any complete `state.json` references the full id; `images prune` removes only valid unreferenced pairs and reports stored bytes freed.

### 8.5 OCI images (normative)

An OCI image is pulled from a Registry V2 endpoint, merged into one root filesystem, packed into an ext4 image and published into the same owned image store as a catalog or URL image. It then boots through the pinned kernel (§9.5) with `firestone-init` as PID 1 (§10.5) instead of firmware plus cloud-init. Everything in §8.3 and §8.4 that concerns the store — the `images/.lock`, the owned qcow2 base, mode 0400, overlays, `images rm`/`prune` — applies unchanged.

**Reference grammar.** §8.6 is the authoritative grammar, classification, and normalization contract; in summary an OCI reference is `[REGISTRY/]NAME[:TAG][@DIGEST]`. `NAME` is one or more `/`-separated components; each component starts and ends with a lowercase alphanumeric and may contain single `.`, single or double `_`, and one to three `-` as separators. `TAG` matches `[A-Za-z0-9_][A-Za-z0-9._-]{0,127}`. `DIGEST` is `sha256:` followed by exactly 64 lowercase hex digits. `REGISTRY` is a DNS host or `localhost`, with an optional `:PORT` of one to five digits.

Normalization is applied once, at parse time, and its result is the canonical reference persisted in `state.json` and the sidecar:

- an absent `REGISTRY` becomes `docker.io`;
- a single-component `NAME` under `docker.io` becomes `library/NAME`;
- an absent tag and digest become `:latest`;
- `docker://NAME` therefore normalizes to `docker.io/library/NAME:latest`;
- a reference carrying both a tag and a digest keeps both, and the digest alone selects the manifest.

`docker.io` is contacted at the host `registry-1.docker.io`; the canonical reference keeps `docker.io`. Uppercase in `NAME`, an empty component, a `..` component, a percent-encoded byte, a userinfo `@` before the digest, a query, a fragment, or any control character is rejected while parsing with kind `invalid_spec`.

**Resolution.** `oci://REF` and `docker://REF` are explicit: the prefix is stripped and the remainder is parsed as an OCI reference only. It is never probed as a path and never matched against the catalog. An unprefixed argument is an OCI reference if and only if it contains `/` **and** its first `/`-separated component contains `.` or `:` or equals `localhost`. So `nginx`, `ubuntu`, and `ubuntu:24.04` are never OCI references, while `ghcr.io/org/app:1`, `localhost:5000/app`, and `docker.io/library/alpine` are.

The rule joins §8.2's ordered list between the HTTPS URL rule and the catalog rules, which becomes (the new step in bold):

1. Absolute or relative path that exists → local file.
2. `https://…` → download to the images dir.
3. **`oci://…`, `docker://…`, or an unprefixed reference matching the registry heuristic → OCI reference.**
4. `distro:version` or `distro:alias` → catalog entry.
5. `distro` → the catalog entry marked `default` for that distro.
6. Otherwise error `unknown image` listing the closest catalog names.

The persisted-reference classifier (§8.2) gains the same step in the same position: it classifies a strict HTTPS URL, then a canonical OCI reference, then an absolute canonical path, and otherwise a validated catalog reference. A relative filesystem name can therefore never shadow a stored OCI reference.

**Registry V2 protocol subset.** Firestone speaks only what a read-only pull needs, over the §8.3 HTTPS transport rules (identity encoding, 30 s connect timeout, 30 min request timeout, at most five redirects, no credentials in a redirect target):

- `GET /v2/`, `GET /v2/<name>/manifests/<reference>`, and `GET /v2/<name>/blobs/<digest>`. Nothing is ever pushed, deleted, or mounted.
- Manifest requests send `Accept: application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json`. Any other returned media type is an error naming what the registry sent.
- **Anonymous Bearer flow.** The first request carries no credentials. On `401` with a `WWW-Authenticate: Bearer …` challenge, Firestone parses `realm`, `service`, and `scope`, performs one `GET <realm>?service=…&scope=…` over HTTPS, accepts `token` or `access_token` from the JSON body (at most 64 KiB), and retries the original request once with `Authorization: Bearer <token>`. Exactly one token fetch and one retry per request. A second `401` is kind `permission` naming the registry and repository, with a hint about `docker login`. A challenge scheme other than `Bearer` or `Basic` is an error.
- **Basic auth** is read from `~/.docker/config.json`, at most 1 MiB, and only from its `auths` object and only from each entry's `auth` field, which must be base64 of `user:password`. `credsStore`, `credHelpers`, and `identitytoken` are ignored with one warning naming the host. The lookup key is the normalized registry host, plus `https://index.docker.io/v1/` for `docker.io`. Credentials are used only over HTTPS, only against the matching host, are sent to a token realm only on that host's challenge, and are never written to any log, event, or error.
- **Platform selection.** When the fetched document is an index or manifest list, Firestone selects the one manifest whose `platform.os` is `linux` and whose `platform.architecture` is `amd64` for an x86_64 host or `arm64` for an aarch64 host; on aarch64 a present `platform.variant` must be `v8`. Entries with `platform.os.features`, an `artifactType`, or an unknown `mediaType` are skipped. No match is an error listing every `os/architecture[/variant]` the index offers. Two matches take the first in document order.
- **Digest verification.** Every manifest, index, config, and layer blob is streamed through SHA-256 and compared with the digest that referenced it before a byte of it is used; the tag-fetched top-level document is compared with the registry's `Docker-Content-Digest` when present, and always re-hashed to produce the recorded `manifest_digest`. A mismatch deletes every partial and returns kind `checksum`.
- **Bounds.** An index or manifest document is at most 4 MiB, an image config at most 1 MiB, a manifest at most 128 layers, and the merged tree at most 64 GiB unpacked. Layer blobs stream to `images/.pull-<source-key>.layer<N>.partial` and emit §8.3's progress events, at most one per 1 MiB.
- **Compression.** v0.2 decompresses gzip only: `application/vnd.oci.image.layer.v1.tar+gzip`, `application/vnd.docker.image.rootfs.diff.tar.gzip`, and the uncompressed `…tar` forms. A `+zstd` layer returns kind `dependency` with the message naming the media type and a hint that this Firestone release decompresses gzip layers only. Foreign/"nondistributable" layers are rejected the same way.
- **`images.insecure_registries`** is a global-config list (§7.3) of `host:port` entries. Only a listed entry may be contacted over plain HTTP, and only for exactly that host and port; the port is mandatory and compared literally, no wildcards, no bare hosts, no scheme. `docker.io` and `registry-1.docker.io` may never be listed. The default is empty, and a plain-HTTP attempt against an unlisted host is kind `permission` naming the entry the user would have to add.

**Layer merge rules.** Layers are applied in manifest order to one accumulated logical tree, and the merged result is serialized once as a canonical tar consumed by `mkfs.ext4 -d`:

- `.wh.<name>` in a directory removes `<name>` and its whole subtree from the accumulated tree; the whiteout entry is never materialized.
- `.wh..wh..opq` in a directory removes every entry accumulated under that directory from earlier layers before the rest of the current layer applies; the marker is never materialized.
- Every member path is normalized and rejected unless it is relative, non-empty, free of `.` and `..` components, and resolves inside the root. A symlink or hard-link target that leaves the root, and a hard link to a member that does not exist in the accumulated tree, are rejected with kind `checksum` naming the entry.
- Regular files, directories, symlinks and hard links are preserved with their mode, uid, gid and mtime. Character devices, block devices, FIFOs and sockets are dropped with one warning per kind and never created.
- Extended attributes are limited by design to `security.capability` and `gnu.translator`, the pair the pinned `mkfs.ext4` tar input accepts. Every other xattr is dropped, including `security.selinux`: an SELinux-labeled image boots unlabeled, which is documented behavior, not a defect.
- The canonical tar is deterministic for a given manifest: entries in sorted path order, one entry per path, no global or per-entry PAX records beyond the retained xattrs.

**Rootfs packing.** The pinned static `mkfs.ext4` (e2fsprogs 1.47.3, §17.2) writes the canonical tar into a fresh raw ext4 image in one pass with `-d`, without a loop device, a mount, or any privilege. `firestone-init` is injected into that tree at `/sbin/firestone-init` before packing. The raw image is converted with the pinned `qemu-img` and published as an owned read-only qcow2 base through the §8.3 store lock, sidecar, mode and re-hash rules.

**Base image sizing.** The ext4 image size is a pure function of the merged tree: `unpacked_bytes × 1.15 + 256 MiB`, rounded up to a 4 MiB multiple. `unpacked_bytes` is the sum of every regular file's size plus 4096 bytes for each directory, symlink and hard link in the merged tree. The multiplication is integer `bytes × 23 / 20`, so the same manifest always yields the same size on every host. The machine's own `disk` size still governs the overlay (§8.4) and is grown inside the guest by `firestone-init` (§10.5).

**Sidecar metadata v2.** The image sidecar gains `version: 2`, a required `kind` of `"disk"` or `"oci"`, and a required `oci` object that is present exactly when `kind` is `"oci"` and JSON `null` otherwise:

```json
{"registry_ref": "docker.io/library/nginx:latest",
 "manifest_digest": "sha256:…", "config_digest": "sha256:…",
 "entrypoint": ["/docker-entrypoint.sh"], "cmd": ["nginx", "-g", "daemon off;"],
 "env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
 "workdir": "/", "user": "root", "boot": "firestone-init"}
```

`entrypoint`, `cmd` and `env` are arrays of strings and may be empty; `workdir` and `user` are strings or `null`; `boot` is `"firestone-init"` in v0.2. Every key is required, nullable values are explicit `null`, and the 64 KiB byte cap, atomic publication and validate-before-publish rules of §8.3 are unchanged. A `version: 1` sidecar remains readable and is interpreted as `kind: "disk"` with `oci: null`; it is never rewritten in place, and a `version: 1` sidecar carrying an `oci` object is corrupt.

For an OCI image the stable id `image-<identity-sha256>` hashes length-framed sidecar version, canonical registry reference, manifest digest, and host architecture; there is no source file or source URL to digest. A cache hit therefore requires the identical canonical reference and manifest digest, so a moved `:latest` publishes a new id and leaves the pinned base of a running machine untouched, exactly as §8.3 specifies for a moved catalog URL.
### 8.6 OCI image references (normative)

Firestone accepts container images alongside catalog references, HTTPS URLs, and local files. This section defines only reference syntax and where the OCI branch sits in resolution; the registry client, layer merge, and pull pipeline are specified separately.

**Classification.** A reference is an OCI reference exactly when it starts with `oci://` or `docker://` (the prefix is stripped before parsing), or when it contains `/` and its first `/`-separated component contains `.` or `:` or equals `localhost`. Nothing else is an OCI reference: `ubuntu:24.04`, `nginx`, `owner/app`, `./disk.qcow2`, `~/disk.qcow2`, and absolute paths are not.

**Resolution order.** The OCI branch sits between the HTTPS-URL check and the catalog lookup, so local-file probing still runs first for user-entered arguments (§8.2) and a strict HTTPS URL is still a direct download. When only the registry-host heuristic classified a reference and it does not parse, resolution falls through to the existing path and catalog behavior unchanged; an explicit `oci://`/`docker://` reference that does not parse is an `invalid_spec` error, because no other interpretation exists. A bare name such as `nginx` therefore remains an `unknown image` catalog error, whose hint names `docker://nginx`.

**Normalization.** A parsed reference is `registry/repository:tag` or `registry/repository@sha256:<64 lowercase hex>`, and its rendering re-parses to itself.

- The registry is `host` or `host:port`, lowercased. A reference with no registry component uses `docker.io`, and `index.docker.io` normalizes to `docker.io`.
- A `docker.io` repository with a single component is prefixed with `library/`, so `docker://nginx` is `docker.io/library/nginx:latest`.
- A reference with neither tag nor digest uses the tag `latest`. A digest reference carries no tag.
- Repository components are lowercase `[a-z0-9]` runs separated by `.`, `_`, `__`, or one or more `-`, and start and end with a letter or digit. Tags are at most 128 characters, start with a letter, digit, or `_`, and otherwise contain letters, digits, `.`, `_`, and `-`. Only `sha256` digests are accepted.

**Persisted references.** Validation rewrites an accepted OCI reference in the machine spec to its normalized form, exactly as it rewrites an existing local path to its canonical absolute path. Persisted resolution classifies an OCI reference after the strict HTTPS URL and the absolute canonical path and before the catalog, so a relative filesystem name still cannot shadow it.

**`--sha256`.** The flag stays HTTPS-only. Supplying it with an OCI reference is a `usage` error whose hint points at `repo@sha256:…` digest references.

**Configuration.** `[images].insecure_registries` in `~/.config/firestone/config.toml` lists registries reachable over plain HTTP as bare `host` or `host:port` entries. An entry carrying a scheme, a path, credentials, an empty host, or a non-numeric or out-of-range port is an `invalid_spec` error naming `images.insecure_registries`.

---

## 9. Boot: firmware, VMM config, start/stop sequences

### 9.1 Firmware

cloud‑hypervisor boots stock cloud images through a firmware; it supports Rust Hypervisor Firmware (RHF, a small PVH payload) and an edk2 UEFI build (`CLOUDHV.fd` on x86_64, `CLOUDHV_EFI.fd` on aarch64). Firmware remains image-specific. On the observed Ubuntu 24.04 x86_64 source, RHF 0.5.0 panicked while resolving the `LABEL=root` device and edk2 reached systemd and `ssh.socket`, so that architecture source is gated to edk2. The entry fallback and unverified aarch64 source remain RHF; this observation does not close the aarch64 firmware gate.

Policy:

- `firmware = "auto"` (default) uses the catalog entry's `firmware` field; local/URL images default to `rhf` on x86_64 and `edk2` on aarch64.
- RHF is passed as the VMM `payload.kernel`; edk2 as `payload.firmware`. Ubuntu 24.04 x86_64 selects the accepted edk2 path; RHF remains a source-mapped or separately observed alternative, not the accepted default for that image.
- Both firmwares are vendored and pinned (§17.2). Before publishing `vmconfig.json`, start downloads only the selected `auto`/`rhf`/`edk2` manifest artifact and publishes it with no-follow ownership, exact hash/mode, lock, fsync, and no-replace checks. A custom firmware path is never installed or rewritten. `doctor` also verifies their checksums.

### 9.2 VmConfig mapping (normative)

The VMM is started with only `--api-socket` (plus `--log-file` and `vmm.extra_args`). The machine itself is created through the API with a JSON `VmConfig` (`PUT /api/v1/vm.create`, then `PUT /api/v1/vm.boot`). This keeps the spec → VMM mapping as data (serde struct → JSON), makes `config_overlay` a clean escape hatch, and avoids building shell argv.

The pinned Cloud Hypervisor v53.0 OpenAPI document at `vmm/src/api/openapi/cloud-hypervisor.yaml` defines the target mapping for the default machine:

```json
{
  "cpus":    { "boot_vcpus": 2, "max_vcpus": 2 },
  "memory":  { "size": 2147483648, "shared": true },
  "payload": { "kernel": "/home/u/.local/share/firestone/bin/hypervisor-fw-0.5.0" },
  "disks": [
    { "path": "/home/u/.local/share/firestone/machines/ubuntu/disk.qcow2",
      "image_type": "Qcow2", "backing_files": true },
    { "path": "/home/u/.local/share/firestone/machines/ubuntu/seed.img",
      "readonly": true, "image_type": "Raw" }
  ],
  "net": [
    { "vhost_user": true, "vhost_socket": "/run/user/1000/firestone/ubuntu/net.sock",
      "vhost_mode": "Client", "mac": "52:54:00:9a:1f:c3" }
  ],
  "fs": [
    { "tag": "share0", "socket": "/run/user/1000/firestone/ubuntu/fs0.sock", "num_queues": 1, "queue_size": 1024 }
  ],
  "vsock":   { "cid": 3, "socket": "/run/user/1000/firestone/ubuntu/vsock.sock" },
  "serial":  { "mode": "File",   "file":   "/home/u/.local/share/firestone/machines/ubuntu/console.log" },
  "console": { "mode": "Pty" },
  "rng":     { "src": "/dev/urandom" }
}
```

Rules:

- `memory.shared = true` is mandatory whenever any vhost‑user device (passt, virtiofsd) is attached. Always set it; the cost is negligible and it removes a class of "works without mounts, breaks with mounts" bugs.
- `cpus.boot_vcpus` is `cpus`. `cpus.max_vcpus` is `cpus_max` when set, otherwise `cpus`: the default machine keeps the two equal.
- `memory.hotplug_size` is emitted **only** when `memory_max` is set, and equals `memory_max - memory` in bytes. `memory.shared` stays `true`. Cloud Hypervisor v53 requires the headroom at boot, so a machine without `memory_max` produces byte-identical `vm.create` input to a machine defined before these fields existed. A unit test asserts that exact byte sequence.
- `network.mode = "tap"` → `"net": [{ "tap": "<dev>", "mac": "…" }]` with no `ip`/`mask` so the VMM does not try to configure the device **[verify 8]**.
- `network.mode = "none"` → no `net` entry.
- Each mount → one `fs` entry with its own virtiofsd socket.
- `serial` captures kernel and systemd output because cloud images put `console=ttyS0` on the kernel command line. `console` (virtio‑console, `hvc0`) is the interactive rescue console (§11.6).
- `config_overlay`, if set, is applied last as an RFC 7396 merge‑patch. The resulting JSON is written to `machines/<name>/vmconfig.json` on every start for inspection, and `firestone show --vmconfig` prints it.

The API client is a small hand-written HTTP/1.1-over-unix-socket client built on `std::os::unix::net::UnixStream` and the existing `nix` poll/socket dependency; it does not add `hyper` or `hyperlocal`. It opens one fresh stream per request and applies one absolute deadline across connect, every partial write, and every read. Endpoints used in v0.1 are `GET /api/v1/vmm.ping`, `PUT /api/v1/vm.create`, `PUT /api/v1/vm.boot`, `GET /api/v1/vm.info`, `PUT /api/v1/vm.power-button`, `PUT /api/v1/vm.shutdown`, and `PUT /api/v1/vmm.shutdown`.

The serialized `vm.create` body is capped at 51,200 bytes, the default maximum of the exact micro-http revision pinned by cloud-hypervisor v53.0. Response headers are capped at 16 KiB; `vmm.ping` success and all error bodies at 64 KiB; `vm.info` success at 1 MiB; and every expected-empty success at zero bytes. The client requires HTTP/1.1, reads through `\r\n\r\n`, then reads exactly one valid `Content-Length`; it never waits for EOF. It rejects transfer encoding, duplicate or conflicting lengths, malformed status/header lines, truncated or extra buffered bytes, non-UTF-8 error diagnostics, and any status or body outside the endpoint contract. A 204 has neither `Content-Length` nor a body; the v53 `vmm.shutdown` exception is 200 with `Content-Length: 0`.

### 9.3 Start sequence (normative)

`Action::Start`, holding the machine lock:

| # | Step id | What happens | Done detail |
|---|---|---|---|
| 1 | — | `reconcile`; if `running` → error `already_running` (for `run`, this is the "just shell in" branch, §15.2) | |
| 2 | `image` | resolve ref; pull if not cached (§8.3) | `ubuntu:24.04 · x86_64 · cached` |
| 3 | `disk` | create overlay if `disk.qcow2` is missing (§8.4) | `20G overlay` / skip `exists` |
| 4 | `seed` | render cloud‑init inputs; compute instance id; rewrite `seed.img` only if content changed (§10) | `instance iid-…` / skip `unchanged` |
| 5 | — | create runtime dir (0700); allocate mac if absent; write `state.json` `starting` (CLI is the writer: no shim yet) | |
| 6 | `shim` | spawn `firestone _shim <name>` detached (§14.2); connect to `shim.sock`; wait for its `ready`/`failed` message. Inside the shim, `net`/`fs`/`vmm` steps are executed and their events are relayed to the CLI over the same connection | `pid 41200` |
| 6a | `net` | (shim) start passt; wait for `net.sock` | `passt · 8080→80` |
| 6b | `fs` | (shim) start one virtiofsd per mount; wait for each `fsN.sock` | `~/code → /code` |
| 6c | `vmm` | (shim) spawn cloud‑hypervisor; wait for `vmm.ping`; `vm.create`; `vm.boot`; write `state.json` `running` | `cloud-hypervisor vNN` |
| 7 | `boot` | (CLI) if `--no-wait`, finish here. Otherwise watch `console.log` growth as a heartbeat and show elapsed time | `firmware+kernel 1.3s` (time to first login prompt line or to vsock accepting) |
| 8 | `ssh` | (CLI) loop until timeout: vsock `CONNECT 22` succeeds → `ssh … true` with `BatchMode=yes` succeeds. Emit `StepUpdate` with the current wait reason (`waiting for sshd on vsock`, `waiting for cloud-init (first boot)` when `seed.img` was just created) | `ready · 6.8s` |
| 9 | — | `Result { name, status: running, elapsed_ms, forwards, mounts }` | |

Timeouts: `[start].timeout_first_boot` when the seed was (re)written, else `[start].timeout`; `--timeout` overrides. On timeout the VM is left running, `StepFail ssh` carries the hint `firestone logs <name>` / `firestone console <name>`, exit code 6.

Ctrl‑C during steps 7–8 cancels the wait only; the VM keeps running and the CLI prints "still booting in the background; `firestone stop <name>` to stop it". Ctrl‑C during steps 2–6 aborts and rolls back to `stopped`/`created` (partial downloads deleted; shim told to `stop`).

### 9.4 Stop sequence (normative)

`Action::Stop`, holding the lock:

1. `reconcile`; if not running → `StepSkip "not running"` and `Result`.
2. Connect to `shim.sock`, send `{"op":"stop","timeout_s":30,"force":false}`; relay the shim's events.
3. Shim: write `stopping`; `PUT vm.power-button` (ACPI; systemd guests shut down cleanly); wait for the VMM process to exit or `vm.info` to report the VM stopped, then `PUT vmm.shutdown` if the process is still alive. On timeout: SIGTERM the VMM, wait 5 s, SIGKILL. `force: true` skips ACPI and goes straight to SIGKILL.
4. Shim: sidecars exit on their own when the VMM disconnects (`passt --one-off`; virtiofsd exits on client disconnect **[verify 7]**); any still alive after 5 s get SIGTERM then SIGKILL.
5. Shim: write final `state.json` (`stopped`, `last_exit` with code/signal/reason/time), remove its sockets and pid file, exit 0.
6. CLI: `Result { name, status: stopped, elapsed_ms }`.

`restart` = `stop` then `start` under one lock acquisition.

`rm` = `stop` (prompting if running and interactive; refusing without `--force` if non‑interactive and running) then delete the machine directory and runtime dir.

### 9.5 Direct kernel boot for OCI machines (normative)

An OCI machine (§8.5) does not boot a firmware. It boots the pinned Cloud Hypervisor kernel release directly, with `firestone-init` as PID 1 (§10.5).

`PayloadConfig` gains a third field beside `firmware` and `kernel`:

```json
"payload": {
  "kernel": "/home/u/.local/share/firestone/bin/bzImage-ch-release-v6.16.9-20260508",
  "cmdline": "console=hvc0 console=ttyS0 root=/dev/vda rw init=/sbin/firestone-init"
}
```

Rules:

- The command line is fixed and byte-exact: `console=hvc0 console=ttyS0 root=/dev/vda rw init=/sbin/firestone-init`. Both consoles are listed so kernel output reaches the interactive virtio-console (`hvc0`, §11.6) and the captured serial file (`console.log`, §9.2). There is no `ip=` parameter; §10.5 explains why the guest runs a userspace DHCP client instead.
- `payload.firmware` is absent on an OCI machine and `payload.cmdline` is absent on a firmware machine. Payload exclusivity is otherwise unchanged: `kernel` and `firmware` are still never both present.
- `vmm.firmware` set to `rhf`, `edk2`, or a custom path on an OCI machine is a validation error (kind `invalid_spec`, `field` `vmm.firmware`) whose message names direct kernel boot and whose hint says to remove the key. `firmware = "auto"` is accepted and ignored.
- **Overlay boundary.** Extending the §21 canonical-overlay invariant: `config_overlay` may not add, change, or remove `payload.cmdline`, and may not flip a machine between boot modes — it may not introduce `payload.firmware` on an OCI machine, nor `payload.kernel` or `payload.cmdline` on a firmware machine. A violation fails before `vm.create` with the existing overlay invariant error naming the offending pointer.
- **Kernel installation.** Before `vmconfig.json` is published, start resolves `cloud-hypervisor-kernel` for the host architecture from `deps.toml` and publishes it through the same locked, no-follow, exact length/hash/mode, fsynced, no-replace publisher used for the selected firmware (§17.2). The kernel is data, not an executable: its published mode is 0644. Only the kernel is fetched for an OCI start; no firmware artifact is downloaded. A firmware machine never fetches the kernel.
- **Explicit raw disk types.** Cloud Hypervisor v53 disables sector-0 writes on a raw disk image whose type it autodetected, which silently corrupts the first sector's writes. Every disk Firestone emits therefore declares `image_type` explicitly — `"Qcow2"` for the overlay, `"Raw"` for the config disk (§10.5) and for any raw auxiliary disk — and `config_overlay` may not remove an `image_type` field.
### 9.5 Resize (normative)

`Action::Resize { name, cpus, memory }` changes one machine's CPU count, memory, or both. At least one must be present; a request that names neither is `usage`. `disk` is deliberately not part of this action: disk capacity is a spec field that grows at the next start (below).

1. Read reconciled state. `starting` and `stopping` are `conflict`: the machine has no settled sizing to change.
2. Layer `{cpus, memory}` over the machine's `firestone.toml` exactly as `PATCH` does, validate the result (§7.2), and refuse a `disk` shrink the file may already contain. The effective spec is what the rest of the action uses.
3. **Not running** → this is exactly a spec patch. Persist it under the machine lock, and answer `applied_live: false`.
4. **Running** → the machine's shim owns the machine lock for its whole lifetime, so the live path takes no lock. It changes only the VMM's live sizing and `firestone.toml`, which the shim never writes.
   1. Read the published `machines/<name>/vmconfig.json` — the exact bytes the running VM booted with — and parse `cpus.boot_vcpus`, `cpus.max_vcpus`, `memory.size`, and the optional `memory.hotplug_size`. A machine whose VmConfig is missing, non-canonical, or missing those fields is `dependency` with a hint to restart.
   2. Refuse, as `conflict` with the hint `set cpus_max/memory_max and restart <name>`, any request above `max_vcpus`, above `memory.size + hotplug_size`, or below `memory.size`. Headroom is a property of the boot configuration, not of the current spec: raising `cpus_max` in the file does not widen a machine that is already running.
   3. `PUT /api/v1/vm.resize` with `{"desired_vcpus": u32?, "desired_ram": u64?}`, omitting whatever the caller did not ask for. v53 answers `204` with no body.
   4. Persist the same values to `firestone.toml` so desired state matches observed state, and answer `applied_live: true`.
5. `Result { name, applied_live, cpus, memory }` carries the effective values either way.

Guest-side consequences, verified against Cloud Hypervisor v53 on Ubuntu 24.04 x86_64: hotplugged memory auto-onlines, while hotplugged vCPUs arrive **offline**. Firestone's cloud-init part therefore installs `/etc/udev/rules.d/80-firestone-hotplug-cpu.rules` (`ACTION=="add", SUBSYSTEM=="cpu", ATTR{online}="1"`) and reloads udev, so a live CPU resize reaches the guest scheduler without a login.

**Disk grow.** `disk` is not resized live. Raising it in the spec and starting the machine grows the overlay: when the existing `disk.qcow2` reports a virtual size below `disk`, start runs `qemu-img resize <overlay> <bytes>` before validating the overlay, and the `disk` step reports `grown to <size> overlay`. Lowering `disk` below the overlay's virtual size is rejected at validation time (§7.2); qcow2 shrink would truncate the guest filesystem. Growing the container does not grow the guest partition: cloud-init's `growpart` (already in Firestone's part, §10.3) extends the root partition and filesystem on the next boot.

---

## 10. Cloud-init

### 10.1 Seed disk

The NoCloud datasource is fed from a small vfat image labeled `CIDATA` containing `meta-data`, `user-data` and optionally `network-config`. It is generated in Rust with the `fatfs` crate (no `genisoimage`/`xorriso` dependency) and attached read-only as the second disk. Rendered inputs are also written to `machines/<name>/seed/` for inspection.

`meta-data`:

```yaml
instance-id: iid-<name>-<identity-digest[0..12]>
local-hostname: <name>
```

### 10.2 Multipart user-data and merge rules (normative)

Except for `provisioning = false` with no user part, the user-data written to the seed is canonical MIME multipart (`multipart/mixed`, `MIME-Version: 1.0`) with the fixed boundary `===============firestone==`. MIME headers and delimiter lines use CRLF. A delimiter-owning CRLF follows each raw body, so parsing returns the user's bytes unchanged even when the source has no final newline.

Parts, in order:

1. **The user's part**, if `cloud_init.user_data` or `cloud_init.user_data_inline` is set: `text/cloud-config` if the content starts with `#cloud-config`, `text/x-shellscript` if it starts with `#!`. Content is passed byte‑for‑byte; firestone never edits it. The two keys are mutually exclusive, and identical bytes produce an identical part and instance id whichever key carried them, so a UI form and a file are the same input.
2. **Firestone's part** (`text/cloud-config`), only if `provisioning = true`. It declares its own merge behavior so that it is merged *into* the user's config without overriding anything the user set:

```yaml
#cloud-config
merge_how: "list(append)+dict(recurse_dict,recurse_list,no_replace)+str()"
```

Ordering rationale: cloud‑init merges each later part into the accumulated result using the later part's merge settings. Putting firestone's part second with `no_replace` means user scalars win (`hostname`, `disable_root`, …) while lists (`ssh_authorized_keys`, `write_files`, `runcmd`, `mounts`) are appended. The exact `merge_how` grammar must be confirmed against the cloud‑init version in the target images and covered by a golden test **[verify 10]**.

Escape hatches: `provisioning = false` writes only the user's part (or an empty user‑data if none) and disables `shell`/`console` readiness checks (`start` finishes after `vmm`). Users who need a raw, non‑cloud‑config user‑data format can combine `provisioning = false` with a `#!` script.

### 10.3 Firestone's part (rendered template)

```yaml
#cloud-config
merge_how: "list(append)+dict(recurse_dict,recurse_list,no_replace)+str()"
hostname: {{ name }}
disable_root: false
ssh_pwauth: {{ "true" if ssh_pwauth else "false" }}
{%- if chpasswd_entry %}
chpasswd:
  expire: false
  list:
    - {{ chpasswd_entry }}          # JSON-quoted "<user>:<password>"
{%- endif %}
ssh_authorized_keys:            # applies to the image's default user
  - {{ firestone_pubkey }}
{%- for key in user_keys %}
  - {{ key }}
{%- endfor %}
users:
  - default
  - name: root
    ssh_authorized_keys:
      - {{ firestone_pubkey }}
{%- for key in user_keys %}
      - {{ key }}
{%- endfor %}
growpart:
  mode: auto
  devices: ["/"]
write_files:
  - path: /etc/systemd/system/firestone-sshd.socket
    permissions: "0644"
    content: |
      [Unit]
      Description=firestone: sshd over vsock
      After=sshd-vsock.socket
      ConditionPathExists=!/run/systemd/generator/sshd-vsock.socket
      [Socket]
      ListenStream=vsock::22
      Accept=yes
      [Install]
      WantedBy=sockets.target
  - path: /etc/systemd/system/firestone-sshd@.service
    permissions: "0644"
    content: |
      [Unit]
      Description=firestone: sshd over vsock (%i)
      After=sshd-keygen.service ssh.service
      [Service]
      RuntimeDirectory=sshd
      RuntimeDirectoryMode=0755
      RuntimeDirectoryPreserve=yes
      ExecStart={{ sshd_path }} -i
      StandardInput=socket
      StandardError=journal
  - path: /etc/udev/rules.d/80-firestone-hotplug-cpu.rules
    permissions: "0644"
    content: |
      ACTION=="add", SUBSYSTEM=="cpu", ATTR{online}="1"
  - path: /etc/systemd/system/serial-getty@hvc0.service.d/firestone-autologin.conf
    permissions: "0644"
    content: |
      [Service]
      ExecStart=
      ExecStart=-/sbin/agetty --autologin {{ user }} --noclear %I $TERM
{%- if mounts %}
mounts:
{%- for m in mounts %}
  - [ "{{ m.tag }}", "{{ m.guest }}", "virtiofs", "{{ 'ro' if m.readonly else 'defaults' }}", "0", "0" ]
{%- endfor %}
{%- endif %}
runcmd:
  - udevadm control --reload
  - systemctl daemon-reload
  - systemctl enable --now firestone-sshd.socket
  - systemctl is-active --quiet sshd-vsock.socket || systemctl is-active --quiet firestone-sshd.socket
  - systemctl enable serial-getty@hvc0.service
  - systemctl restart serial-getty@hvc0.service
```

Notes:

- Root login works because `disable_root: false` plus a key while `ssh_pwauth: false` keeps it key-only (`PermitRootLogin prohibit-password`/`without-password`, `PasswordAuthentication no`). The image's default user receives the same Firestone and user keys.
- Firestone opens its own public identity key with no symlink following, requires current-user ownership and exact mode 0644, and reads at most 16 KiB. Private-key bytes never enter seed rendering.
- User key files and lines are traversed in configuration order, then `cloud_init.ssh_authorized_keys` entries in configuration order. Blank and comment lines are ignored. Duplicate key material, including a user entry equal to Firestone's identity key or the same key with a different comment, is omitted while the first spelling and order are preserved. Deduplication compares key data across both sources, so the same key supplied as a file and inline is rendered once.
- `chpasswd` is emitted only when `cloud_init.password` is set, with `expire: false` so the guest is not forced to change the password at first login. `list` is used rather than `users` for compatibility with the cloud-init releases in the catalog images. The entry is one JSON-quoted `"<user>:<password>"` scalar, so a password containing `:`, `"`, `#`, or a backslash cannot change the document's structure.
- A password alone does not enable SSH password authentication: `ssh_pwauth` stays `false` unless `cloud_init.ssh_pwauth` is set, and the password reaches console and local login through `chpasswd`.
- On guests with systemd ≥ 256, the generated `/run/systemd/generator/sshd-vsock.socket` owns vsock port 22. `firestone-sshd.socket` is ordered after it and has the inverse path condition, so the Firestone socket starts only when the native unit is absent. The final `is-active` command requires one listener; unrelated bind/start failures remain failures **[verify 11]**.
- The per-connection service owns and preserves `/run/sshd`, which stock OpenSSH requires before `sshd -i`; its `ExecStart` is not failure-prefixed.
- The `sshd` path differs between distros only rarely (`/usr/sbin/sshd` on Debian/Ubuntu/Fedora); the typed catalog entry may override `sshd_path`, which must be a safe absolute POSIX executable path.
- The hotplug-cpu udev rule exists because Cloud Hypervisor v53 brings hotplugged vCPUs up **offline** (§9.5). Adding it changed the rendered user-data bytes, and therefore the instance id (§10.4), for every machine: the next start of an existing machine reprovisions it and invalidates its `known_hosts`. The goldens were regenerated deliberately.
- Templates are rendered with `minijinja`; multipart bytes and deterministic seed images are golden-tested per typed input.

### 10.4 Instance id and re-provisioning

`identity-digest` preserves the M1 formula when `network-config` is absent: `SHA-256(user-data)`. When `network-config` is present it is `SHA-256(b"firestone-instance-v1" || 0x00 || be64(len(user-data)) || user-data || be64(len(network-config)) || network-config)`. Length framing and a versioned domain separate the two byte strings and distinguish an absent network file from a present empty file.

Only effective seed input bytes change the id. Changing user-data or network-config bytes, the effective de-duplicated key sequence, `user`, a rendered mount tuple, the Firestone identity key, `provisioning`, or the catalog `sshd_path` changes it. A different source pathname with the same bytes, a duplicate key, CPU/memory changes, or a host-only mount path change leaves it stable. A changed id makes cloud-init run its per-instance modules again. Because it also regenerates the guest's SSH host keys (`ssh_deletekeys` default), `start` deletes `machines/<name>/known_hosts` before accepting the new seed identity.

### 10.5 `firestone-init` (normative)

`firestone-init` replaces cloud-init on OCI machines. An OCI guest has no cloud-init, no NoCloud seed, no systemd, and no sshd; it has one static Firestone binary at `/sbin/firestone-init` running as PID 1 and the image's entrypoint running as its child.

**Config disk.** The disk in the seed slot — `disks[1]` in §9.2 — is `machines/<name>/config.img`, attached `readonly: true` with an explicit `"image_type": "Raw"` (§9.5). It is a magic-framed, length-prefixed JSON document:

```
offset  0   8 bytes   ASCII magic "FSTNINIT"
offset  8   4 bytes   little-endian u32 format version; 1 in v0.2
offset 12   4 bytes   little-endian u32 JSON length in bytes; at most 65536
offset 16   N bytes   the JSON document, UTF-8, no trailing NUL
```

The remaining bytes up to the image's 4 KiB-aligned size are zero. `firestone-init` refuses a wrong magic, an unknown format version, or a length above the cap, and prints the reason to the console before powering off. The document is:

```json
{
  "hostname": "app",
  "entrypoint": ["/docker-entrypoint.sh"],
  "cmd": ["nginx", "-g", "daemon off;"],
  "env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
  "workdir": "/",
  "user": "root",
  "network": "dhcp",
  "disk_size_bytes": 21474836480
}
```

Every key is required. `entrypoint`, `cmd` and `env` are arrays of strings and may be empty; `workdir` and `user` are strings or `null`; `network` is `"dhcp"` or `"none"`; `disk_size_bytes` is the machine's configured disk size. The values default to the sidecar's `oci` object (§8.5) and are overridden per machine by the spec. `start` rewrites `config.img` only when its bytes change, exactly as step 4 of §9.3 rewrites `seed.img`, and the machine's identity digest covers those bytes the same way.

**Boot sequence.** As PID 1, `firestone-init`, in order:

1. mounts `proc` on `/proc`, `sysfs` on `/sys`, `devtmpfs` on `/dev`, `devpts` on `/dev/pts`, and `tmpfs` on `/tmp` and `/run`;
2. brings `lo` up;
3. grows the root filesystem: `/dev/vda` already presents the full virtual size, so init resizes the mounted ext4 root online with the `EXT4_IOC_RESIZE_FS` ioctl to `disk_size_bytes` divided by the filesystem block size. Kernel ext4 is builtin, so no `resize2fs` binary is required in the guest. A filesystem already at or above the target is a no-op;
4. sets the hostname;
5. when `network` is `"dhcp"`, runs a minimal userspace DHCP client on `eth0` with a short bounded timeout, then applies the address, netmask, gateway and resolvers and writes `/etc/resolv.conf`. A timeout prints one warning to the console and boot continues; `"none"` skips the step entirely;
6. spawns `entrypoint ++ cmd` as a direct child in its own new process group, with `env` as the child's complete environment, `workdir` as its working directory, and `user` resolved through the image's own `/etc/passwd` when it is not `root`.

**Staying PID 1.** `firestone-init` does not `exec` the entrypoint. It remains PID 1 for the life of the machine so that it can: reap every orphaned process the guest leaves behind; forward `SIGTERM` and `SIGINT` to the child's process group rather than dying itself; and, when the direct child exits, print its exit status to the console, `sync()`, and issue `reboot(RB_POWER_OFF)` so Cloud Hypervisor exits and Firestone observes a clean, attributable machine exit instead of a hang.

**Verified kernel facts** (pinned `ch-release-v6.16.9-20260508`, §17.2). The kernel has no loadable-module support at all, so everything the guest needs is builtin, and nothing else can be added at runtime. Builtin: `EXT4`, `VIRTIO_BLK`, `VIRTIO_NET`, `VIRTIO_CONSOLE`, `VSOCKETS` with `VIRTIO_VSOCKETS`, `PACKET`, `IP_PNP` with `IP_PNP_DHCP`, `DEVTMPFS` with `DEVTMPFS_MOUNT`, `TMPFS`, `UNIX`, `PROC_FS` with `SYSFS`, `OVERLAY_FS`, `VFAT`, and `VIRTIO_FS`. squashfs is absent, so no image may depend on it.

`IP_PNP_DHCP` being builtin does not make kernel `ip=dhcp` usable: with no server answering, the in-kernel client retries for about 176 s, which would dominate the boot of every `network.mode = "none"` machine and of any machine whose passt start is slow. Firestone therefore never puts `ip=` on the command line (§9.5) and runs its own client in userspace with a short timeout; builtin `CONFIG_PACKET` is what makes that client possible without a module.

**Cloud-init inputs are refused.** Any `[cloud_init]` input from §7.1 on an OCI machine is a validation error (kind `invalid_spec`, with `field` naming the offending key) whose hint points at the config-disk keys above. The failure happens during validation, before any registry request, image pull, or boot — never as a silently ignored file.
`cloud_init.password` and `cloud_init.ssh_pwauth` render into Firestone's part, so they are ordinary user-data bytes: changing, adding, or removing a password changes the instance id and re-provisions the guest on the next start. No separate credential-change mechanism exists.

### 10.5 Credentials and secret handling (normative)

`cloud_init.password` is the plaintext guest password for `user`. Firestone stores it as typed, in `machines/<name>/firestone.toml`, and renders it into `machines/<name>/seed/user-data` and the CIDATA image. It is not hashed: cloud-init's `chpasswd` list accepts a plaintext value, and a hash computed by Firestone would pin one crypt scheme and still be recoverable from the same files. The protection is filesystem permissions, and it is enforced rather than assumed:

- `machines/<name>/firestone.toml`, including the `firestone.toml.edit` candidate `edit` opens in an editor, is published with mode 0600 regardless of the caller's umask.
- `seed/meta-data`, `seed/user-data`, `seed/network-config` and `seed.img` are published with mode 0600 inside the mode-0700 seed directory.
- The machine directory and its ancestors are already current-user owned and non-group/world-writable (§6.1).

Redaction rule: a password and inline user-data never reach a log line, an event, an error message, a hint, or a process argument list.

- `CloudInitSpec` and `CloudInitSpecPatch` implement `Debug` manually and print `Some("<redacted>")` for `password` and `user_data_inline`, so any spec formatted into a trace or panic message is safe by construction.
- Validation and rendering errors report an index, a byte length, or a violated rule, never the offending bytes.
- `--password-file FILE` is the only CLI spelling: the value is read from the file, not from `argv`, which is world-readable on Linux.
- The values remain visible where the user's own configuration is being shown back: `firestone show`, `GET /v1/machines/{name}`, `PUT`/`PATCH` responses, and the `create` `Result` payload serialize the effective `MachineSpec`. That is the same data as the machine file the user owns; the boundary is the 0600 file and the 0600 socket, not selective serialization, because a redacted spec would not round-trip back into `firestone.toml`.

---

## 11. Shell, console, logs

### 11.1 SSH over vsock (normative)

Shell access does not depend on guest networking. cloud‑hypervisor implements virtio‑vsock in userspace and exposes it as a unix socket on the host (`vsock.sock`); no `/dev/vhost-vsock`, no kernel module, no CID coordination — every machine uses CID 3. The guest runs `sshd` on vsock port 22 via the socket unit in §10.3.

### 11.2 Host proxy

`firestone _vsock-proxy <name> <port>` (hidden subcommand):

1. Connect to `$RUNTIME/<name>/vsock.sock` (error kind `not_running` with a hint if absent).
2. Write `CONNECT <port>\n`; read one line; expect `OK <n>\n`. Anything else → exit 1 with the line on stderr.
3. Splice stdin → socket and socket → stdout until either side closes.

Used as the `ProxyCommand` for ssh, and by `start` for readiness probing (step 8).

### 11.3 SSH invocation and host keys

`firestone shell <name> [--user U] [-- CMD…]` execs (replaces the process with) the system `ssh`:

```
ssh -o ProxyCommand="firestone _vsock-proxy <name> 22"
    -o IdentityFile=<data>/ssh/id_ed25519 -o IdentitiesOnly=yes
    -o UserKnownHostsFile=<machine>/known_hosts -o StrictHostKeyChecking=accept-new
    -o LogLevel=ERROR
    [-t]                      # when stdin is a TTY
    <user>@firestone.<name> [CMD…]
```

- Firestone's key is generated on first use with `ssh-keygen -t ed25519 -N "" -C firestone@<host> -f <data>/ssh/id_ed25519`.
- `accept-new` trusts the first host key per machine; `known_hosts` is deleted on `rm` and on seed rewrite (§10.4). A host key *change* without a seed rewrite is a hard error (`ssh` will refuse), which is the correct behavior.
- If the machine is not running: `shell` starts it first when interactive (this is the `run` path), else errors `not_running`.

### 11.4 `ssh-config`

`firestone ssh-config <name>` prints an OpenSSH `Host` block the user can `Include` from `~/.ssh/config`; VS Code Remote‑SSH and `rsync`/`scp` then work unchanged:

```
Host firestone.ubuntu
  User root
  ProxyCommand firestone _vsock-proxy ubuntu 22
  IdentityFile ~/.local/share/firestone/ssh/id_ed25519
  IdentitiesOnly yes
  UserKnownHostsFile ~/.local/share/firestone/machines/ubuntu/known_hosts
  StrictHostKeyChecking accept-new
```

### 11.5 Ad-hoc port forwards without networking

Because ssh runs over vsock, `ssh -L`/`-R` work with `network.mode = "none"`. Not exposed as a firestone command in v0.1; documented in the user guide.

### 11.6 `console`

`firestone console <name>` connects to `$RUNTIME/<name>/console.sock`, puts the terminal in raw mode, and relays bytes from Cloud Hypervisor's virtio-console PTY at `hvc0`. Escape sequence `Ctrl-]` detaches. On connect it prints to stderr: `connected to <name> console · escape: Ctrl-]`. The guest side has an autologin getty on `hvc0` (§10.3), so the console is a rescue path that works when SSH does not. The shim owns the PTY master and must permit attach, detach, and a later reattach over `console.sock` **[verify 13]**.

### 11.7 `logs`

`firestone logs <name> [-f] [--source console|vmm|shim|passt|virtiofsd-N] [-n LINES]` opens only the selected current-user-owned mode-0600 regular file with no-follow and nonblocking flags. It prints the last 200 lines by default; `LINES` is 0 through 100,000. The reverse tail scan is capped at 8 MiB and refuses an individual or requested tail beyond that bound instead of truncating it. Follow reopens a safely rotated path, reads at most 256 KiB per pass, and sleeps 100 ms between passes. `SIGINT` cancels within one polling interval and returns the shared interrupted error without a terminal `Result`. Over REST: `GET /v1/machines/{name}/logs?source=&follow=`.

### 11.8 OCI machines (normative)

An OCI guest (§8.5, §10.5) runs `firestone-init` and the image's entrypoint. It has no sshd, no vsock SSH listener, and no guest SSH host key, so every SSH-dependent surface is unavailable rather than merely failing to connect:

- `firestone shell <name>` and `firestone ssh-config <name>` on an OCI machine return a usage error (kind `usage`, exit code 2) stating that the machine has no sshd and naming `firestone console <name>` and `firestone logs <name>`. Over REST the same actions return `400` with the same message and hint. Both fail before any connection attempt, so there is no timeout to wait through.
- `firestone run` on an OCI machine performs create and start and then reports that same usage error instead of attaching a shell; the machine is left running.
- `console` and `logs` work unchanged. The entrypoint's stdout and stderr reach `hvc0` and `console.log`, so the console is the interactive surface and `logs` is the durable one.
- **Readiness.** Steps 7 and 8 of §9.3 do not apply. `start --wait` on an OCI machine is ready when the shim reports `running` after `vm.boot` — the shim is running and the VM was created and booted — not when SSH answers. `Result.status` is `running` and no `ssh` step is emitted. `[start].timeout` and `--timeout` still bound steps 2 through 6.

### 11.9 `cp` (normative)

`firestone cp [-r] SRC DST` copies files between the host and one machine by exec'ing the system `scp` over the same vsock `ProxyCommand` as §11.3.

**Operand grammar.** Exactly one of `SRC` and `DST` is remote. An operand is remote if and only if it contains a colon and everything before its *first* colon is a non-empty machine name — lowercase ASCII letters, digits, and dashes (`[a-z0-9-]+`). Every other operand is local:

| Operand | Classification |
|---|---|
| `dev:/etc/hostname` | remote, machine `dev`, path `/etc/hostname` |
| `dev:` | remote, machine `dev`, empty path (the guest login directory) |
| `./dev:/etc/hostname` | local (the `./` escape) |
| `/srv/dev:/etc` | local (a colon after the first `/`) |
| `notes.txt` | local (no colon) |
| `Dev:/etc`, `dev_1:/etc`, `:/etc`, `::1:/etc` | local (not a machine name before the first colon) |
| `fe80::1:/etc` | remote, machine `fe80` — an IPv6 literal is not operand syntax; use `./fe80::1:/etc` for the local file |

Zero remote operands and two remote operands are both `usage` errors; each hint names the `./` escape. A remote operand whose machine does not exist is `not_found`; a machine that is not running is `not_running` with the same message and hint family as `shell` (`machine <name> is not running` / `start it with firestone start <name>`). `cp` never starts a machine.

**Invocation.** The remote operand renders as `<user>@firestone.<machine>:<path>` with the machine's spec `user`; a local operand that `scp` would itself read as remote (a colon before any `/`) is passed with a `./` prefix so host and Firestone agree on the same file. The argv is:

```
scp [-r] -o ProxyCommand="firestone _vsock-proxy <name> 22"
    -o IdentityFile=<data>/ssh/id_ed25519 -o IdentitiesOnly=yes
    -o UserKnownHostsFile=<machine>/known_hosts -o StrictHostKeyChecking=accept-new
    -o LogLevel=ERROR
    <src> <dst>
```

The option block is the one shared by `shell` and the start readiness probe; `cp` never sets `BatchMode` and never allocates a TTY. Everything after the option block is `scp`'s own behavior: `-r` for directories, its progress meter and its exit status pass through unchanged, and firestone adds no other flag. Because OpenSSH 9 `scp` transfers over the SFTP protocol by default, a wildcard in a remote path is expanded by the guest's SFTP server rather than a remote shell, so brace expansion and shell-only globs do not apply; quote a remote glob so the host shell does not expand it first.

`cp` is CLI-only. The process becomes `scp`, so there is nothing for an HTTP response to carry; `Action::Cp` returns the planned argv and the CLI execs it. `--json` is a `usage` error, exactly as for `shell`. Adding no route keeps `docs/openapi.json` and the §5.4 drift test unchanged, since that test compares the axum route table with the OpenAPI document and not the CLI command table.
**Log sanitization (normative).** A guest writes its console; Firestone decides what leaves the logs surface. Every consumer of the logs surface — the `logs` REST route, both streaming and aggregated, and the web UI that renders the same bytes — receives text filtered by one server-side allowlist, applied once, in one place:

- Printable characters, `\n`, `\r` and `\t` pass through unchanged.
- A CSI sequence passes through **verbatim** only when it is `ESC [`, then at most 16 bytes drawn from `[0-9;]`, then the final byte `m`. That is SGR, the sequence family that only paints.
- Every other escape sequence is swallowed whole and replaced by exactly one `U+FFFD`: any other CSI final byte, a CSI with a private or intermediate byte, an overlong CSI parameter run, an OSC with either its `BEL` or its `ESC \` terminator (including an OSC 52 clipboard write), DCS, APC, PM, SOS, and a lone `ESC` before an ordinary byte. The swallowed body is discarded, not printed.
- Every other control character, including a bare C1 code point in `U+0080`–`U+009F`, becomes one `U+FFFD`.

The filter is stateful and chunk-boundary safe: a sequence split across two `Output` events parses as one sequence, and a sequence still open when the stream ends is flushed as one `U+FFFD` rather than leaking its bytes. An unterminated string sequence is abandoned at the next control character, so it cannot swallow the rest of the log.

This applies to the logs surface only. CLI table cells, `show` payloads and every other rendered field keep the stricter rule that replaces every control character, escape included.

---

## 12. Networking

### 12.1 `passt` (default)

passt provides user‑mode connectivity with no capabilities or privileges: the guest gets the host's own address by DHCP, the host's routes and resolvers, and outbound connections are made by passt with ordinary sockets; inbound reaches the guest through configured port forwards. In `--vhost-user` mode the data path is shared memory and the passt process looks like any vhost‑user network backend to the VMM.

Spawn (by the shim, before the VMM):

```
passt --foreground --one-off --vhost-user
      --socket $RUNTIME/<name>/net.sock
      --log-file $MACHINE/passt.log
      [-t <tcp forwards>] [-u <udp forwards>]
      --repair-path none       # must be the final option
```

- `--foreground` keeps passt a child of the shim (passt daemonizes by default).
- `--one-off` makes passt exit when the VMM disconnects, so teardown is automatic.
- The VMM connects as a vhost‑user client (`vhost_mode: "Client"`) **[verify 14]**.
- The guest reaches host loopback services through its gateway address (passt default). Documented for users; not configurable in v0.1.
- Forwards are fixed for the life of the process; changing `network.forward` takes effect on the next start, and the divergence is surfaced as `forwards_pending` until then (§12.5). `ssh -L` over vsock covers ad‑hoc needs (§11.5).
- Limitation to document plainly: two passt machines on one host reach each other only through forwarded host ports. Multi‑VM clusters need tap mode.

### 12.2 `tap`

For users who run their own bridge. Firestone opens an existing tap device it does not create or configure. One‑time setup the user performs (root), shown by `doctor` when `mode = "tap"` fails validation:

```
sudo ip tuntap add dev tap0 mode tap user $USER
sudo ip link set tap0 master br0
sudo ip link set tap0 up
```

Guest addressing is then whatever the bridge provides (DHCP) or a user `network_config`. No DHCP server, NAT or firewall rules are ever created by firestone.

### 12.3 `none`

No `net` device. Shell, console, mounts still work.

### 12.4 Port forward syntax (normative)

`[proto:][bind:]HOST:GUEST` where `proto ∈ {tcp,udp}` (default tcp), `bind` is an IPv4/IPv6 literal (default all addresses), and `HOST`/`GUEST` are ports or equal‑length ranges `a-b`. Examples: `8080:80`, `udp:5353:53`, `127.0.0.1:2222:22`, `8000-8010:8000-8010`. Parsing is a pure function with exhaustive unit tests; the mapping to passt's `-t`/`-u` grammar is a second pure function tested against the pinned passt man page **[verify 15]**. `ls` displays forwards as `8080→80`.

Firestone emits one repeated `-t` or `-u` option per forward. The passt value is `[bind/]HOST:GUEST`: IPv4 is emitted directly, IPv6 is bracketed, and omitted bind means all addresses. TCP mappings precede UDP mappings while preserving configuration order within each protocol. Overlapping host ranges for one protocol are rejected even when bind literals differ because the pinned passt mapping table has one translation slot per protocol and host port. `--repair-path none` is emitted last because the pinned parser ends its second option pass at that option.

### 12.5 Pending forwards (normative)

passt fixes its `-t`/`-u` mappings at spawn and offers no runtime interface to change them, and the Cloud Hypervisor v53 vhost-user session does not survive a passt restart: `--one-off` passt exits when the VMM disconnects, and the VMM does not reconnect to a replacement backend. There is therefore no hot-apply for port forwards, and Firestone never pretends otherwise. The honest contract is apply-on-restart, made visible rather than silent.

`state.json` `forwards` is the applied set: the shim writes the forwards it actually passed to passt at spawn, in canonical `[proto:][bind:]HOST:GUEST` form. `network.forward` in `firestone.toml` is the configured set. The two diverge whenever the spec is edited while the machine runs.

`forwards_pending` is a boolean on `MachineSummary` (`ls`, `GET /v1/machines`) and on the show payload (`show`, `GET /v1/machines/{name}`). It is true exactly when the machine's status is `running` and the configured set differs from the applied set. It is false for every other status, because a machine that is not running has applied nothing.

The comparison is canonical, not textual. Both sides are normalized through the §12.4 parser and its display form and compared as multisets, so configuration order is irrelevant and an equivalent respelling (`tcp:8080:80` for `8080:80`, a contracted IPv6 literal) is not a pending change. A recorded value that no longer parses is compared verbatim rather than discarded.

Surfaces:

- `ls` marks a pending row with a trailing `*` in the `FORWARDS` column and, when at least one row is marked, prints the single legend line `* forwards pending restart` after the table. The column still shows the applied forwards, since those are the ones a client can reach right now.
- `show` prints `forwards pending restart` to stderr, keeping stdout a single valid JSON document (§15.3).
- `PUT`/`PATCH` on the spec of a running machine, and `firestone edit`, emit the warning `Event::Log` `port forwards apply on restart` whenever the write leaves forwards pending. It is additional to, not a replacement for, the general `machine is running; spec changes take effect on next start` warning.

`restart` clears the condition because the shim rewrites `state.json` `forwards` from the new spec at spawn. Nothing else does.

---


## 13. Shared folders

One virtiofsd per mount, spawned by the shim before the VMM:

```
virtiofsd --socket-path $RUNTIME/<name>/fs<i>.sock
          --shared-dir <host path>
          --sandbox namespace            # falls back to "none" when user namespaces are unavailable (doctor reports which)
          --cache auto --announce-submounts
          [--readonly]                    # [verify 16]
          --log-level warn                # stderr → $MACHINE/virtiofsd-<i>.log
```

- Rootless: virtiofsd runs as the user; guest root maps to the host user's permissions on the shared tree. This is documented, not hidden.
- Guest mount comes from the `mounts:` entry in firestone's cloud‑init part (`virtiofs` filesystem, tag `share<i>`), which also creates the mount point.
- No default mounts. `run --mount .:/work` is the ergonomic path for "I want my current directory".

---

## 14. The shim (normative)

### 14.1 Responsibilities

Exactly one `firestone _shim <name>` per running machine. It:

1. starts sidecars, waits for their sockets, starts the VMM, creates and boots the VM (§9.3 steps 6a–6c), relaying `Event`s to the launching CLI over `shim.sock`;
2. supervises all children, records exits, and writes `state.json` (sole writer while alive);
3. serves `shim.sock` (§14.3) for `stop`, `status` and event relay;
4. tears everything down in order on `stop`, on `SIGTERM`, or when the VMM exits on its own;
5. never restarts anything in v0.1 (restart policies are a later feature).

### 14.2 Spawn

The CLI spawns the shim as a detached process: `setsid`, working directory `/`, stdin `/dev/null`, stdout/stderr appended to `$MACHINE/shim.log`, environment reduced to `PATH`, `HOME`, `XDG_*`, `FIRESTONE_*`. The shim writes `shim.pid`, listens on `shim.sock`, and the CLI connects and sends `{"op":"launch"}`; the shim runs the launch sequence and streams events back, finishing with `{"ok":true}` or `{"ok":false,"error":…}`. If the CLI disconnects mid‑launch, the shim continues (the VM is not tied to the terminal). Optional later improvement: wrap in `systemd-run --user --scope` when available for cgroup tracking.

### 14.3 Protocol over `shim.sock`

Newline‑delimited JSON, one request per connection, responses streamed as `Event` objects followed by a terminal `{"ok":…}` line.

| Request | Behavior |
|---|---|
| `{"op":"launch"}` | run the launch sequence (only valid once, right after spawn) |
| `{"op":"status"}` | `{"ok":true,"status":"running","pids":{…},"started_at":…,"degraded":[…]}` |
| `{"op":"stop","timeout_s":30,"force":false}` | §9.4 steps 3–5, streaming events; the shim exits after replying |
| `{"op":"ping"}` | `{"ok":true}` |

### 14.4 Failure handling

- VMM exits unexpectedly → `state.json` `failed` with `last_exit` (code/signal), `reason` derived from the last lines of `vmm.log`; sidecars torn down; shim exits. `ls` shows `failed`; `show` prints the reason; `start` is allowed again.
- Sidecar exits while the VMM runs → `degraded` entry in `state.json`, `Log` line in `shim.log`; the VM is left running.
- Shim itself is killed (`kill -9`) → children are orphaned and keep running; the next `reconcile` sees `api.sock` alive but no shim, reports status `running (unsupervised)`, and `stop` falls back to driving `api.sock` directly then signaling the recorded `vmm_pid` after verifying its cmdline.
- Host reboot → runtime dir gone → `reconcile` marks `stopped` with reason `host reboot`.

---

## 15. CLI (normative)

### 15.1 Command reference

Global flags on every command: `--json` (NDJSON events on stdout, human output off), `-q/--quiet` (errors and result only), `-v/--verbose` (repeatable; `-vv` = debug), `--no-color`, `-y/--yes` (assume yes to prompts), `--home DIR` (= `FIRESTONE_HOME`).

| Command | Purpose |
|---|---|
| `run [IMAGE\|NAME] [spec flags] [--name N] [--rm] [-- CMD…]` | idempotent: create → start → shell (§15.2) |
| `create [NAME] [IMAGE] [--image IMAGE] [spec flags] [-f SPEC.toml] [--edit]` | write a spec and never boot. With interactive stdin and stderr, and without `--yes` or `--json`, select an image from the merged catalog with arrow keys or choose the final custom URL/local-path option, then guide name, CPU, memory, disk, and network selection; supplied values become defaults. Non-interactive forms require the same resolved name/image contract as before |
| `start NAME [--no-wait] [--timeout D]` | boot and wait for ssh |
| `stop NAME [--timeout D] [--force]` | graceful ACPI stop, escalate on timeout |
| `restart NAME` | stop + start |
| `resize NAME [--cpus N] [--memory SIZE]` | change CPU and memory; live within the booted headroom, otherwise on next start (§9.5) |
| `rm NAME… [--force]` | stop if needed, delete everything |
| `clone SRC DEST [--fresh-disk]` | copy a stopped or created machine's spec and disk to a new machine (§24) |
| `ls` (alias `list`) | table of machines (§15.3) |
| `show NAME [--vmconfig]` | spec + state (+ generated VmConfig) |
| `edit NAME` | open `firestone.toml` in the selected editor; validate on save, re-open on error |
| `shell NAME [--user U] [-- CMD…]` (alias `ssh`) | ssh over vsock |
| `cp [-r] SRC DST` | copy files over vsock with `scp`; exactly one operand is `<machine>:<path>` (§11.9) |
| `ssh-config NAME` | print an OpenSSH Host block |
| `console NAME` | attach to hvc0 |
| `logs NAME [-f] [--source S] [-n N]` | view logs |
| `metrics NAME` | one cumulative resource sample for a running machine (§25) |
| `catalog` | deterministic table of the merged built-in and user catalog, including aliases, effective firmware, and available architectures |
| `images ls` / `images pull REF [--sha256 HEX]` / `images inspect ID` / `images rm ID [--force]` / `images prune` | image management; `--sha256` is valid only for an HTTPS URL |
| `doctor [--fix]` | diagnose host; `--fix` downloads vendorable binaries and prints the rest |
| `serve [--listen unix:PATH|tcp:HOST:PORT] [--token FILE]` | REST listener; a TCP listener must be loopback and must carry a token |
| `ui [--port PORT] [--no-open] [--print-url]` | serve the web UI on an ephemeral loopback port with a per-invocation session token, and open a browser |
| `completions SHELL` | shell completions |
| `version` | version, pinned dependency versions, paths |
| `_shim NAME`, `_vsock-proxy NAME PORT` | hidden internals |

`edit NAME` and `create --edit` use the first nonempty `$VISUAL`, then `$EDITOR`, otherwise literal `nano`. Firestone parses the selected command into argv with `shlex` and launches it directly through `Cmd`, never through a shell.

Spec flags are those in §7.4 and are accepted by `run` and `create` only.

### 15.2 `run` semantics

1. If the argument names an existing machine → use it. Spec flags are an error here (`edit` instead), except `--user`.
2. Otherwise the argument is an image ref; the machine name is `--name` or the distro part of the ref (`ubuntu:24.04` → `ubuntu`). If a machine with that name exists but was created from a different image → error `name in use` with hint `--name`.
3. Missing → create (spec = defaults ← global ← flags). Stopped/failed → start. Running → nothing.
4. Then `shell` (with `-- CMD…` if given). With `--rm`, the machine is removed when the shell exits, but only if this invocation created it.
5. No argument → `run` uses `ubuntu` (the catalog's flagship default); this is the one opinion firestone has.

Exit code of `run` is the exit code of the shell/command.

### 15.3 Output and feedback conventions

TTY (stderr is a terminal):

```
$ firestone run ubuntu
  ✓ image    ubuntu:24.04 · x86_64 · cached
  ✓ disk     20G overlay
  ✓ seed     instance iid-ubuntu-5f3a9c1e2b7d
  ✓ shim     pid 41200
  ✓ net      passt · 8080→80
  ✓ vmm      cloud-hypervisor v48.0
  ✓ boot     firmware+kernel 1.3s
  ⠸ ssh      waiting for cloud-init (first boot) · 4.1s
```

- One line per step: spinner (braille, 80 ms) while active; `✓` green, `✗` red, `-` dim for skipped, `!` yellow for warnings. Label column fixed width; detail right of it; elapsed appended dim when > 1 s.
- Progress steps (`image`) render a bar with bytes and rate.
- All feedback goes to stderr; stdout carries only data (`create` summary, `ls` table, `show`, `ssh-config`, `--json` streams), so pipes work.
- Non‑TTY: no control characters or spinner frames; each step prints `[image] ubuntu:24.04 · x86_64 · cached` on start/done; NO_COLOR respected.
- `--json`: NDJSON `Event`s on stdout, one per line, and nothing else on stdout.
- `ls` table: `NAME  STATUS  IMAGE  CPUS  MEM  UPTIME  FORWARDS`; statuses `running`, `running!` (degraded), `stopped`, `failed`, `starting`, `stopping`, `created`. Never truncates names. A `FORWARDS` cell ends in `*` when the machine's configured forwards are not the applied ones, and the table is then followed by the legend `* forwards pending restart` (§12.5).
- Time, sizes, rates use short human units (`1.3s`, `613 MB`, `48 MB/s`).
- Success ends with a single result line where useful (`start`: `ubuntu is running · shell: firestone shell ubuntu`). No decorative banners.
- Human `create` prints a labeled effective-spec block containing name, canonical image, CPUs, memory, disk, network, forwards, mounts, the resolved `firestone.toml` path, and exact `edit` and `start` commands. Quiet mode still prints this terminal result. JSON and REST retain the canonical `MachineRecord` payload without CLI-only path or command fields.

### 15.4 Prompts

Prompts appear only when stdin and stderr are TTYs and `--yes` is absent. `create` runs the guided configuration flow described in §15.1; `--yes` accepts supplied values and configured defaults without prompting, while `--json` is always non-interactive. Destructive confirmation remains limited to `rm` of a running machine and `images rm` of an image in use. `doctor --fix` may separately request explicit approval immediately before the bounded AppArmor elevation in §17.3. EOF or cancellation makes no mutation and exits 130. Non-interactive destructive invocations fail with a hint to pass `--force`/`--yes`.

### 15.5 Exit codes

| Code | Meaning |
|---|---|
| 0 | success |
| 1 | generic failure |
| 2 | usage / invalid spec |
| 3 | not found (machine, image) |
| 4 | conflict (already exists, already running, name in use, busy) |
| 5 | host dependency missing or broken (`doctor` explains) |
| 6 | timeout |
| 7 | checksum or verification failure |
| 130 | interrupted |
| N | `run`/`shell` propagate the remote command's code |

### 15.6 Errors

One error type in core:

```rust
pub struct FirestoneError { kind: ErrorKind, message: String, hint: Option<String>, source: Option<Box<dyn Error>> }
```

Rendered as:

```
error: cannot start ubuntu: /dev/kvm is not accessible
cause: Permission denied (os error 13)
hint:  run `sudo usermod -aG kvm $USER` and log in again, or see `firestone doctor`
```

Every external process failure includes the program name, its exit status and the last 10 lines of its log. Errors from the VMM API include the HTTP status and body.

---

## 16. REST API (normative)

### 16.1 Transport

`firestone serve` listens on `unix:$XDG_RUNTIME_DIR/firestone/serve.sock` (mode 0600) by default. Authentication is the socket's file permissions.

The reserved TCP form is now implemented, unchanged in shape: `--listen tcp:HOST:PORT --token FILE`. It exists because a browser cannot open a Unix socket, and it is constrained so that it cannot become a weaker boundary than the socket it supplements:

- The bind address must be loopback. `127.0.0.1` and `::1` are accepted; `0.0.0.0`, `[::]`, `::ffff:127.0.0.1` and every routable address are refused with `usage` before the listener is created. No flag lifts this.
- A TCP listener without `--token FILE` is a usage error, and `--token` with a Unix listener is a usage error. Both are refused before binding, so an unauthenticated TCP listener is unspellable.
- The token file is mode 0600 and current-user owned; an existing file is validated, and a missing one is created with `O_CREAT|O_EXCL`. The token is never logged, never echoed in an error, and never passed as a plain process argument.

`firestone ui` is the ergonomic wrapper: it binds an ephemeral loopback port, generates a fresh 256-bit session token that lives only in process memory, prints the URL, and opens a browser through `Cmd`.

Every request on a TCP listener passes an authentication gate, in this order. A Unix listener has no gate: mode 0600 already is one.

1. **Host allowlist.** `Host` must equal `127.0.0.1:PORT`, `localhost:PORT` or `[::1]:PORT` for the bound port. This is the DNS-rebinding defense and runs before authentication.
2. **Token.** Either `Cookie: firestone_session=<64 hex>` or `Authorization: Bearer <64 hex>`, compared in constant time over the raw 32 bytes.
3. **Bootstrap.** A `GET` carrying a matching `?token=<hex>` answers `303` to the same path with the query stripped and sets `firestone_session=<hex>; Path=/; HttpOnly; SameSite=Strict; Max-Age=86400`. No `Secure`: the transport is plaintext loopback. This gets the token out of the address bar and browser history on the first navigation.
4. **Protocol upgrade.** A request carrying `Upgrade: websocket` or `Connection: upgrade` must additionally prove same-origin even though it is a `GET`: `Sec-Fetch-Site: same-origin` or `none`, or an `Origin` in the same loopback allowlist the `Host` check uses. A request carrying neither header is refused. The `SameSite=Strict` session cookie already fails closed cross-site; this is the second lock on the same door, because an upgraded connection is a long-lived bidirectional channel to a machine's console or shell rather than one request. Browsers do not attach `Sec-Fetch-*` to a WebSocket handshake, so in practice the `Origin` allowlist is what answers; a non-browser WebSocket client must send `Origin: http://127.0.0.1:PORT`.
5. **Cross-origin.** A non-GET request must carry `Sec-Fetch-Site: same-origin`, or a matching `Origin` when `Sec-Fetch-Site` is absent. Non-browser clients that send neither are already token-gated.

Failures answer `401` (token) or `403` (host, origin, upgrade) with the same `ErrorEnvelope` shape as every other error. No `WWW-Authenticate` header: it would raise a browser credential dialog for a token this UI already holds.

The server holds no state and takes the same machine locks as the CLI. `curl --unix-socket … http://firestone/v1/machines` is the smoke test.

The server holds no state and takes the same machine locks as the CLI. `curl --unix-socket … http://firestone/v1/machines` is the smoke test.

[`docs/openapi.json`](docs/openapi.json) is the dependency-free OpenAPI 3.1 contract for the existing routes. It records exact request and response schemas, Unix-socket transport, aggregation and stream framing, limits, statuses, nullability, and examples. It is a static artifact, not a runtime endpoint. A behavior-level test parses it and compares its 23 explicitly authored operations with the configured axum router; axum's synthesized `HEAD` handling for `GET` remains framework behavior rather than a separately authored Firestone operation.
[`docs/openapi.json`](docs/openapi.json) is the dependency-free OpenAPI 3.1 contract for the existing routes. It records exact request and response schemas, Unix-socket transport, aggregation and stream framing, limits, statuses, nullability, and examples. It is a static artifact, not a runtime endpoint. A behavior-level test parses it and compares its explicitly authored operations (19 as of M6-03) with the configured axum router; axum's synthesized `HEAD` handling for `GET` remains framework behavior rather than a separately authored Firestone operation.
[`docs/openapi.json`](docs/openapi.json) is the dependency-free OpenAPI 3.1 contract for the existing routes. It records exact request and response schemas, Unix-socket transport, aggregation and stream framing, limits, statuses, nullability, and examples. It is a static artifact, not a runtime endpoint. A behavior-level test parses it and compares its 23 explicitly authored operations with the configured axum router; axum's synthesized `HEAD` handling for `GET` remains framework behavior rather than a separately authored Firestone operation.

### 16.2 Routes

| Method | Path | Body | Response |
|---|---|---|---|
| GET | `/v1/version` | | `{version, identity, architecture, dependencies, paths}` |
| GET | `/v1/doctor` | | doctor report |
| GET | `/v1/machines` | | `[MachineSummary]` (same rows as `ls`, including `forwards_pending`) |
| POST | `/v1/machines` | `{name, spec}` (spec = `MachineSpecPatch` layered on defaults) | 201 `{name, spec, state}` |
| GET | `/v1/machines/{name}` | | `{spec, state, supervision, forwards_pending}` |
| PUT | `/v1/machines/{name}` | `MachineSpec` (omitted fields take built-in defaults) | 200 `{spec, warnings}` |
| PATCH | `/v1/machines/{name}` | `MachineSpecPatch` | 200 `{spec, warnings}` |
| DELETE | `/v1/machines/{name}?force=` | | 204 (stream if a stop is needed) |
| POST | `/v1/machines/{name}/start` | `{wait?, timeout_s?}` | event stream → `Result` |
| POST | `/v1/machines/{name}/stop` | `{timeout_s?, force?}` | event stream → `Result` |
| POST | `/v1/machines/{name}/restart` | | event stream → `Result` |
| POST | `/v1/machines/{name}/clone` | `{name, fresh_disk?}` | event stream → `Result` (§24) |
| POST | `/v1/machines/{name}/resize` | `{cpus?, memory?}` (at least one) | event stream → `Result {name, applied_live, cpus, memory}` |
| GET | `/v1/machines/{name}/logs?source=&follow=&lines=` | | `text/plain`, chunked |
| GET | `/v1/machines/{name}/vmconfig` | | generated VmConfig JSON |
| GET | `/v1/machines/{name}/metrics` | | `MetricsResult` (§25); 409 when the machine is not running |
| GET | `/v1/machines/{name}/console/ws` | | `101` WebSocket carrying the console; `409` when the console is already attached |
| GET | `/v1/machines/{name}/shell/ws` | | `101` WebSocket carrying one SSH shell on a host pseudo-terminal |
| GET | `/v1/catalog` | | `[{reference, aliases, architectures: [{architecture, firmware}]}]` |
| GET | `/v1/images` | | `[StoredImage]` |
| POST | `/v1/images/pull` | `{ref, sha256?}` | event stream → `Result` |
| DELETE | `/v1/images/{id}?force=` | | 204 |
| POST | `/v1/images/prune` | | `{removed, bytes_freed}` |

The user's example holds: `POST /v1/machines/ubuntu/start` returns the same events the CLI shows, ending in `{"type":"Result",…}`, and `GET /v1/machines` then reports `running`.

### 16.3 Streaming

Action routes respond with `Content-Type: application/x-ndjson` and stream `Event`s as they happen, one JSON object per line, the last being `Result` (or an error object). Clients that send `Accept: application/json` get a single JSON response after completion containing `{events: [...], result}` or an error. Requests are canceled by closing the connection; the underlying action continues to a safe point (a started VM stays started).

#### WebSocket terminal transports (normative)

Two routes leave HTTP entirely, because a terminal is a byte stream and not a sequence of events: `GET /v1/machines/{name}/console/ws` and `GET /v1/machines/{name}/shell/ws`. They live under `/v1` because the path split sends every `/v1*` request to the REST router. A successful handshake answers `101 Switching Protocols`; Firestone negotiates no subprotocol and no extension.

**Framing is identical on both routes.**

- **Binary frames are raw terminal bytes**, in both directions, unmodified. Nothing is line-buffered, escaped, or sanitized: this is the same byte stream `firestone console` and `firestone shell` put on a TTY.
- **Text frames are JSON control messages.** `{"resize":{"rows":R,"cols":C}}` is the only one defined. Any other text frame — an unknown member, an unknown message, malformed JSON, a zero dimension — is **ignored**, never fatal. A control message is advisory; the byte stream is the contract, and an older server must not drop a session because a newer client asked for something it does not know.
- **The server closes with a descriptive close frame.** End of the byte stream closes with code `1000` and the reason the route can mean: `machine stopped` on the console, whose broker outlives every client and only stops with the machine, and `session ended` on the shell, whose usual ending is the person typing `exit`. A transport failure closes with code `1011` and the failure detail, truncated on a character boundary to fit one control frame.

**Console.** Everything that can fail is settled *before* the upgrade, so a failure is an ordinary REST error a browser can read rather than a WebSocket that closes immediately. The machine must be `running`; then the shared `console_plan` validates the mode-0600 broker socket, the server connects it and reads the broker's short acknowledgement line (at most 32 bytes). `OK` upgrades and bridges the socket to the WebSocket with a 16 KiB read buffer. `BUSY` answers `409` with a hint naming `firestone console <name>`, because the broker is single-client and the CLI shares it. Backpressure is the relay's own await: the next read cannot start until the WebSocket has accepted the previous chunk. A resize is accepted and ignored — the guest owns its serial geometry.

**Shell.** The machine must be `running` before the upgrade. After it, the server allocates a host pseudo-terminal, and runs the same OpenSSH argv `firestone shell` execs (interactive, forced TTY) through the shared `Cmd` wrapper with the pseudo-terminal slave as the child's stdin, stdout and stderr, in its own process group. The master is relayed to the WebSocket. A resize applies `TIOCSWINSZ` to the master and then raises `SIGWINCH` on the child's process group explicitly, because the child never claims the pseudo-terminal as its controlling terminal — that needs `setsid` plus `TIOCSCTTY` between fork and exec, which cannot be written where `unsafe_code` is forbidden — so the kernel's own `SIGWINCH` would reach no one. The slave is opened `O_NOCTTY` for the mirror-image reason: a daemonized `serve` must not adopt a machine's shell terminal as its own. One session per connection: when the WebSocket ends, the child's process group is killed and reaped. The server-side pseudo-terminal exists because a browser terminal cannot speak the SSH wire protocol; it is the same transport, terminated one hop earlier.

### 16.4 Errors

```json
{ "error": { "kind": "not_found", "message": "no machine named 'ubunut'", "hint": "firestone ls" } }
```

HTTP status by kind: `usage`/`invalid_spec` 400, `not_found` 404, `conflict`/`already_running`/`busy` 409, `timeout` 504, `dependency` 503, `checksum` 502, everything else 500.

`ErrorInfo` carries one more optional member, `field`: the dotted spec path the failure belongs to (`memory`, `network.forward`, `mount[0].host`, `cloud_init.password`, …). It is omitted entirely when the error is not about one spec leaf, so existing bodies are unchanged. Every §7.2 spec-validation error carries it; the same envelope is used for JSON responses and for the terminal error record of an NDJSON stream. This is the form-error contract: the web UI answers beside the offending input instead of parsing messages. CLI rendering ignores `field` and keeps printing the message and hint.

### 16.5 Web UI (normative)

The same axum application serves an embedded web UI beside `/v1`. Templates, stylesheet, scripts and fonts are compiled into the executable: Firestone still ships one file, and a host with no outbound network renders the UI exactly as designed.

**Route split.** `/v1` and `/v1/…` reach the REST router and keep its stable JSON `ErrorEnvelope` 404 unchanged. Every other path reaches the UI router. `/`, `/machines`, `/machines/{name}` and `/catalog` are real URLs: one handler returns the whole document, or the fragment alone when htmx asks for it, so deep links, reload and the back button work without a parallel route table.

**The UI is a read surface.** It renders the same shared action results the REST routes serialize. It does not implement lifecycle mutations. Start, stop, restart, delete and image pull go from the browser to the documented `/v1` endpoints and render their NDJSON progress as it arrives. There is one mutation surface and one contract.

The single exception is `POST /ui/machines`, which layers a form submission into a `MachineSpecPatch` and dispatches the same `Action::Create` as `POST /v1/machines`. It exists so a rejected field is answered beside that field rather than as a notification. It adds no capability the REST route lacks.

**State truth.** The server is the only source of terminal state. The UI renders a transitional `starting` or `stopping` because the action provably dispatched, and replaces it with a server read on the terminal record. It never writes a terminal status from the client.

**Content-Security-Policy.** Every response carries `default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'`, plus `Referrer-Policy: no-referrer`, `X-Content-Type-Options: nosniff` and same-origin `Cross-Origin-Opener-Policy`/`Cross-Origin-Resource-Policy`. There is no `'unsafe-inline'` and no `'unsafe-eval'`. Consequently the UI uses no inline `style` attribute, no inline script, and none of htmx's `new Function()`-compiled constructs (trigger filters, `hx-on:*`, `js:` values).

**Log colour.** The logs tab renders the SGR sequences §11.7 lets through as colour, and never as markup. Rendering is line-oriented: a completed line becomes an immutable node, only the trailing partial line is re-rendered as bytes arrive, and the same renderer serves both the server-rendered first screenful and the follow stream, so the two paths cannot diverge. `\r` inside a line is applied as a last-writer-wins column overlay, which is how a progress line is meant to read. SGR 38 and 48 extended-colour parameters are consumed and discarded — the palette is sixteen themed tokens, and a truecolour triple has no token — so that the codes after them still parse. Colour is carried by class, never by an inline `style` attribute, because the CSP forbids one.
**Terminals.** The console and shell WebSocket transports in section 16.3 are the UI's terminal surface, and the only place the UI opens a connection that is not one request. Two consequences follow. Serve's per-connection HTTP builder enables upgrades, which is what lets a handler answer `101` and keep the socket. And an attached terminal has no idle point, so it holds its connection open until the graceful-shutdown drain timeout aborts it: `firestone serve` shutting down closes an open terminal rather than waiting for the person at it.

**Assets.** Third-party web assets are vendored under `crates/firestone/assets/ui/` and pinned by SHA-256 in `web-assets.toml`. A test recomputes every hash and fails when a vendored file is unpinned or a pinned file is missing. Static assets are served from a closed table matched by exact name, never by joining a request path onto a directory.

---

## 17. Dependencies and `doctor`

### 17.1 Host requirements

- Linux, x86_64 or aarch64, KVM available (`/dev/kvm` readable and writable by the user).
- `$XDG_RUNTIME_DIR` (or the `/tmp` fallback) writable.
- Unprivileged user namespaces for virtiofsd's default sandbox (optional; degrades to `--sandbox none`).
- Rootless by default; no capabilities or kernel modules beyond KVM. The only privileged path is the explicit, confirmed AppArmor helper installation in §17.3.

### 17.2 Binaries

| Binary | Role | Source | How obtained |
|---|---|---|---|
| `cloud-hypervisor` | VMM | GitHub releases, static (`cloud-hypervisor-static`, `-aarch64`) | x86_64 musl payload embedded in Firestone, checksum-verified and materialized on first use; aarch64 retains vendored download fallback |
| `hypervisor-fw` | RHF firmware | rust-hypervisor-firmware releases | vendored, pinned |
| `CLOUDHV.fd` / `CLOUDHV_EFI.fd` | edk2 firmware | cloud-hypervisor edk2 releases | vendored, pinned |
| `virtiofsd` | shared folders | virtio-fs/virtiofsd releases (static) | vendored, pinned |
| `passt` | networking | Firestone static helper release from pinned commit `a1e48a02ff3550eb7875a7df6726086e9b3a1213` | x86_64 musl payload embedded in Firestone, checksum-verified and materialized on first use; no host package |
| `qemu-img` | overlays, raw→qcow2 | Firestone static helper release from signed QEMU 8.2.2 source | x86_64 musl payload embedded in Firestone, checksum-verified and materialized on first use; no host package |
| `bzImage-x86_64` / `Image-arm64` | direct-boot kernel for OCI machines (§9.5) | cloud-hypervisor/linux release `ch-release-v6.16.9-20260508` | vendored, pinned; downloaded and published mode 0644 on the first OCI start, never embedded |
| `mkfs.ext4` | packs the merged OCI rootfs (§8.5) | Firestone static helper release built from e2fsprogs 1.47.3 | vendored, pinned; downloaded and published on the first OCI pull, never embedded |
| `firestone-init` | guest PID 1 for OCI machines (§10.5) | built from this repository | static musl payload embedded in Firestone, checksum-verified and injected into the packed rootfs at `/sbin/firestone-init` |
| `ssh`, `ssh-keygen` | shell | distro package (`openssh-client`) | system |

Pins live in `deps.toml` in the repository (name, version, runtime architectures, immutable URL, sha256, source and license provenance). Checksums come from real downloaded or twice-built bytes. The x86_64 release build downloads and verifies Cloud Hypervisor, passt, and qemu-img before Cargo, then `build.rs` independently matches all three to `deps.toml` and embeds them with `include_bytes!`; a strict release build fails when any payload or hash is absent. First use publishes a versioned `<data>/bin/<name>-<version>` executable under a current-user lock with no-follow validation, exact length/hash/mode readback, fsync, and no-replace publication. A verified literal root-owned passt copy installed for AppArmor takes precedence. Development and compile-only aarch64 builds retain their existing installed/PATH/download fallbacks; the x86_64 standalone release requires no host VMM, passt, or qemu-img package.
The x86_64 static helpers are built by one reproducible recipe in `build/helpers`: a digest-pinned Alpine 3.22 container with no network installs only the SHA-256-locked Alpine package closure, builds `passt`, `qemu-img` and `mkfs.ext4` from SHA-256-locked sources, and runs twice with every published byte required to match. `mkfs.ext4` is e2fsprogs 1.47.3 `misc/mke2fs` configured `--with-libarchive=direct --disable-nls --disable-uuidd --disable-fuse2fs` and linked statically against the full Alpine libarchive 3.8.3 closure, which `misc/Makefile` cannot supply on its own because it hardcodes `LIBARCHIVE=-larchive`; only `misc/mke2fs` is built, because a static link of `debugfs` fails. The resulting binary is published under the name `mkfs.ext4` and accepts `mke2fs -d <tar>`, so an OCI layer tar becomes an ext4 image with its ownership, modes, symlinks, hard links and device nodes intact; the build proves that property on every run. The helper release publishes the corresponding source for e2fsprogs (GPL-2.0) and for every statically linked library, including libarchive (BSD-2-Clause) and libacl (LGPL-2.1-or-later), together with their license texts.
On start, Firestone resolves only the effective built-in firmware selected by the machine and image, downloads that pinned artifact when missing, and sends its checksum-verified current-user-owned mode-0644 path to VmConfig. This targeted install uses the same locked, no-follow, fsynced, no-replace publisher as embedded helpers. Custom firmware paths bypass publication and remain unchanged.

### 17.3 `doctor` checks

Each check prints `ok`, `warn` or `fail`, a one-line reason, and exact remediation. `--fix` performs ordinary current-user repairs directly. When Ubuntu AppArmor blocks passt's mandatory user namespace, an interactive `doctor --fix` first displays the literal root-owned helper/profile changes and requires an explicit confirmation; `--yes` never authorizes elevation. Confirmed repair invokes only `install` and `apparmor_parser` through `sudo` or `pkexec`, never changes a sysctl, and verifies helper hash/version/root ownership/non-writability, exact profile bytes, and loaded profile state. Non-interactive or declined repair prints the exact administrator commands without invoking them.

1. host architecture supported
2. `/dev/kvm` exists and opens O_RDWR → fix: `sudo usermod -aG kvm $USER` (group name detected from the device's owner group) + re‑login
3. KVM nested/virtualization enabled (informational if the device is missing: BIOS/`kvm_intel`/`kvm_amd`)
4. `XDG_RUNTIME_DIR` set and writable → warn and name the fallback
5. embedded `cloud-hypervisor v53.0` is present in x86_64 standalone builds and matches the pinned manifest hash; RHF and edk2 firmware remain vendored files → fix: `doctor --fix` materializes the VMM and downloads missing firmware
6. vendored `virtiofsd` present with the matching checksum → fix: `doctor --fix`
7. embedded `passt 2025_02_17.a1e48a0` has the pinned command grammar; runtime selection prefers a verified literal root-owned AppArmor copy, then the checksum-verified materialization, with PATH fallback only in development
8. embedded `qemu-img 8.2.2` is present in x86_64 standalone builds and materializes with the pinned hash; qemu-img does not require user namespaces
9. `ssh`, `ssh-keygen` on PATH → fix: distro install command
10. user namespaces: inspect `max_user_namespaces`, optional `unprivileged_userns_clone`, AppArmor enable/restriction state, self seccomp/container facts, readable PID-correlated audit evidence, and a bounded passt probe using the runtime `--foreground --one-off --vhost-user --socket ... --log-file ... --repair-path none` mode. `Couldn't create user namespace` is a first-stage failure. `Failed to detach isolating namespaces` proves the first user namespace was created, but it is still fatal and makes passt unusable. Either failure is `fail`; when host facts are consistent with AppArmor, interactive `doctor --fix` offers the existing literal root-owned profile repair. Report AppArmor as confirmed only with correlated evidence, otherwise `consistent with`. A virtiofsd-only denial may warn and use `--sandbox none`.
11. firestone SSH key present → fix: generated by `--fix`
12. free space in the data dir ≥ 5 GB → warn
13. stale machine states (runtime dir missing while `state.json` says running) → info, reconciled

---

## 18. Implementation notes

### 18.1 Language and crates

Rust, edition 2024, stable toolchain. The accepted `x86_64-unknown-linux-musl` release is one static, standalone executable carrying Cloud Hypervisor, passt, and qemu-img payloads. `aarch64-unknown-linux-musl` remains a compile-only Firestone target without embedded-helper or runtime support; glibc builds are for development.

| Concern | Crate |
|---|---|
| CLI | `clap` (derive), `clap_complete` |
| serialization | `serde`, `serde_json`, `toml` (with `toml_edit` for comment‑preserving writes), `schemars` |
| async runtime | `tokio` (multi-thread for serve/shim; current-thread is fine for CLI) |
| HTTP server | `axum`, `hyper`, `hyperlocal` (unix sockets) |
| HTTP client | `reqwest` (downloads, streaming); `std::os::unix::net::UnixStream` + `nix` (VMM API) |
| hashing | `sha2` |
| terminal UI | `indicatif`, `console`, `owo-colors`, `crossterm` (raw mode for `console`), `unicode-width` (table layout) |
| processes / OS | `nix` (flock, setsid, credentials, signals) plus `rustix` (`waitid` non-reaping observation and Linux pidfds); `libc` constants only |
| command argv parsing | `shlex` (`VISUAL`/`EDITOR` only; commands still execute without a shell) |
| timestamps | `jiff` |
| seed image | `fatfs` |
| templates | `minijinja` |
| paths | `directories` |
| errors | `thiserror` (core), `anyhow` at binary edges only |
| logging | `tracing`, `tracing-subscriber` |
| json merge‑patch | `json-patch` |
| tests | `insta` (snapshots), `assert_cmd`, `tempfile`, `wiremock` (download tests) |

### 18.2 Repository layout

```
firestone/
  Cargo.toml                 workspace
  deps.toml                  pinned third-party binaries (§17.2)
  catalog/images.toml        built-in image catalog (§8.1)
  crates/
    firestone-core/          spec, patch, validation, paths, state, lock, catalog, images, cloudinit,
                             vmm (api client + VmConfig mapping), net (passt), fs (virtiofsd),
                             ssh (keys, vsock proxy), events, actions (Dispatcher)
    firestone/               the binary: cli (clap + renderer), serve (axum), shim, hidden subcommands
  templates/                 cloud-init part, ssh-config, firestone.toml template with comments
  tests/                     e2e tests (require KVM; see §19)
  docs/                      user guide; this SPEC.md at the repo root
  CLAUDE.md                  see Appendix B
```

`firestone-core` must not depend on `clap`, `indicatif` or `axum`; it exposes `Dispatcher`, `Action`, `Event`, `EventSink` and typed errors only. This is what keeps the three surfaces identical.

### 18.3 Conventions (normative)

- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test` must pass on every commit.
- No `unwrap`/`expect` outside tests; every error carries context and, where a user can act, a hint.
- All paths come from `Paths`; all external processes go through one `Cmd` wrapper that logs the full argv at debug level and captures stderr into the relevant log file.
- Every function that talks to a third‑party tool cites the `[verify N]` item it depends on in a doc comment.
- Everything the user can see is deterministic under `--json` and non‑TTY; snapshot tests cover the renderer.
- No feature outside this document without a decision‑log entry in the same PR.
- Commit messages: `area: what` (`vmm: map mounts to fs entries`), body explains why.

### 18.4 Logging

`-v` → `info`, `-vv` → `debug` (includes every argv and API request/response), `-vvv` → `trace`. `RUST_LOG` overrides. The shim always logs at `info` to `shim.log`, the VMM to `vmm.log` via `--log-file`, sidecars to their own files. Logs never contain the user's cloud‑init contents (they may hold secrets); they reference the `seed/` directory instead.

---

## 19. Testing

### 19.1 Unit (no KVM)

- Spec: TOML round‑trip, defaults, unknown key errors with suggestions, every validation rule in §7.2, patch layering order.
- Drift test (§5.4).
- Catalog: resolution rules (§8.2), arch selection, merge/override.
- Port forward parser and passt argument mapper (§12.4).
- Pending forwards (§12.5): canonical diff matrix (equal, added, removed, reordered-equal, respelled, unparsable), the not-running gate, the `port forwards apply on restart` warning, the marked `ls` row with its legend, and `forwards_pending` in the `ls`/`show` REST payloads and their OpenAPI schemas.
- Cloud‑init: golden renders for {no user‑data, cloud‑config user‑data, shellscript user‑data, keys, mounts, provisioning=false}; instance id stability.
- VmConfig mapping: golden JSON for {default, tap, none, mounts, edk2, config_overlay}.
- State: reconcile matrix (status × socket alive × shim pid alive × runtime dir exists) → expected status/rewrite.
- Lock: contention and timeout behavior with two processes.
- vsock proxy: handshake against a fake unix server (`OK`, error line, EOF).
- Renderer: `insta` snapshots for TTY and non‑TTY output of a canned event stream; `--json` is byte‑exact NDJSON.
- REST: axum handlers with a mocked `Dispatcher`; NDJSON framing; error mapping (§16.4); static OpenAPI JSON parsing and behavior-level route/method drift against the configured router.

### 19.2 End-to-end (KVM required)

Run `FIRESTONE_E2E=1 FIRESTONE_HOME=$(mktemp -d) scripts/m1-kvm-e2e.py`. The script refuses an insecure or nonempty home, stages an optional `FIRESTONE_BIN` under the required `firestone` basename, installs and verifies the pinned artifacts through `doctor --fix`, gives every command and guest interaction an absolute bound, and removes every machine on success or failure. `FIRESTONE_E2E_EVIDENCE` selects the mode-0600 JSON evidence path. Scenarios are independent and use `network.mode=none` for M1.
Run M2 acceptance with `FIRESTONE_E2E=1 FIRESTONE_HOME=$(mktemp -d) scripts/m2-kvm-e2e.py`. It applies a mode-0600 global `network.mode=none` fixture after the empty-home preflight so the exact E2E 2 argv stays `firestone run ubuntu -- true`; verifies pinned dependency and image hashes; bounds subprocess, PTY, guest, and cleanup work; refuses insecure or nonempty homes; writes atomic mode-0600 JSON evidence; and removes every machine and the home on every normal, error, timeout, or handled-signal path. It never records cloud-init bytes or key material.
Run M4 E2E 9 with `FIRESTONE_E2E=1 FIRESTONE_HOME=$(mktemp -d) scripts/m4-kvm-e2e.py`. It refuses a non-Linux, non-x86_64, non-KVM, insecure, or nonempty home; stages an optional `FIRESTONE_BIN` under the required `firestone` basename; verifies exact dependency and image pins and hashes; observes only a current-user mode-0600 Unix socket; bounds every command and HTTP exchange; and writes atomic mode-0600 JSON evidence outside the home. It starts through REST, requires streamed progress followed by one terminal `Result`, compares the actual CLI `show --json` Result payload bytes with the REST machine payload, kills and restarts only `serve` while proving the shim and VMM stay alive, then stops, removes, checks the 204 body, and cleans every process and owned file.

1. `doctor` passes.
2. `run ubuntu -- true` from an empty home: pulls, boots, ssh works; second `run` is warm and under 5 s.
3. Forward: `-p 8080:80`, guest `python3 -m http.server 80`, host `curl localhost:8080`.
4. Mount: `--mount $tmp:/work`, file written in the guest appears on the host.
5. `stop` is graceful: console.log shows a clean shutdown; `last_exit.reason = "guest shutdown"`.
6. Crash handling: `kill -9` the VMM pid; `ls` shows `failed` within 2 s; `start` recovers.
7. Stale state: `kill -9` the shim; `ls` shows `running (unsupervised)`; `stop` still works.
8. Config change: edit `cloud_init.ssh_keys`, `restart`, new key works, old `known_hosts` replaced.
9. REST: `serve` in background; `POST …/start` streams events and ends with `Result`; `GET /v1/machines` shows `running`.
10. `--json` and non‑TTY outputs contain no control characters.
11. Each catalog image boots to ssh under its declared firmware (matrix; this is the gate for adding catalog entries).

### 19.3 CI

GitHub Actions: unit tests on every push; e2e on a KVM‑capable runner **[verify 18]** nightly and on release branches; release builds for both musl targets with `deps.toml` checksums re‑verified.

---

## 20. Assumptions to verify before relying on them

Do these in the first milestone, against the pinned versions, and record results in §21.

| # | Status | Assumption | How to check |
|---|---|---|---|
| 1 | resolved | RHF maps to `payload.kernel`; edk2 maps to `payload.firmware`; Ubuntu 24.04 x86_64 requires edk2 for its accepted default boot | pinned v53 CLI, README, and payload source; M1-06 edk2 boot |
| 2 | resolved | VmConfig field names and the `/api/v1/…` endpoint set used in §9.2 | pinned OpenAPI and handlers; exact client tests; M1-06 runtime ping/info and lifecycle |
| 3 | resolved | Each catalog image boots to ssh on its declared firmware | Exact-main `47daf52` E2E 11 booted Ubuntu 24.04/22.04, Debian 12/13, and Fedora 44 with edk2 and completed root SSH; `m5-catalog.json` SHA-256 `26d989ddaeb17628bd2815862b1b9ea0dbf3c9ed8881d1a130132f5909311c68` |
| 4 | resolved | `qemu-img convert -f raw -O qcow2` output boots under CH; raw bases are not needed | M1-06 converted-image boot |
| 5 | resolved | CH opens a qcow2 overlay with a qcow2 backing file; fio numbers are recorded without an invented pass threshold | M1-06 overlay boot and guest fio against a raw auxiliary disk |
| 6 | resolved | A systemd guest powers off after `vm.power-button`; Firestone retains `vmm.shutdown` and signal fallbacks | pinned v53 source and M1-06 graceful stop |
| 7 | resolved | `passt --one-off` and `virtiofsd` exit when the VMM disconnects | Exact-main `47daf52` M3 normal-stop, restart, and injected-VMM-exit teardown verified every recorded sidecar process gone |
| 8 | resolved | CH opens a user-owned tap without CAP_NET_ADMIN when `ip`/`mask` are unset | Exact-main `47daf52` M3 created a user-owned TAP through bounded host setup, launched CH without effective capabilities, and removed the TAP after stop |
| 9 | resolved | cloud-init NoCloud accepts a vfat `CIDATA` volume built by `fatfs`; no ISO is needed | M1-06 first boot and `cloud-init status --long` |
| 10 | resolved | `merge_how` grammar and part ordering yield "user scalars win, lists append" | exact target cloud-init 26.1 schema validation and `CloudConfigPartHandler` merge of the byte-exact two-part golden; §21 evidence |
| 11 | resolved | `firestone-sshd.socket` coexists with systemd-256+'s generated `sshd-vsock.socket` | Debian 13 systemd 257 KVM boot; inspect conditions/socket ownership and connect over native vsock SSH |
| 12 | resolved | The pinned v53 host protocol is `CONNECT <port>\n` followed by `OK <allocated-host-port>\n` after guest acceptance | exact v53 `docs/vsock.md`, muxer source, and muxer unit test |
| 13 | resolved | The shim's PTY broker permits console attach, detach, and reattach | attach, detach, attach again |
| 14 | resolved | `passt --vhost-user` and CH `vhost_mode: "Client"` interoperate at the pinned versions | Exact-main `47daf52` E2E 3 completed host-to-guest TCP and UDP forwarding while guest SSH remained on vsock |
| 15 | resolved | passt `-t`/`-u` grammar for bind addresses and ranges | pinned `2025_02_17.a1e48a0` man page and `conf.c`; exact argv and boundary tests |
| 16 | resolved | virtiofsd supports read-only mode and `--sandbox namespace` rootless | Exact-main `47daf52` E2E 4 mounted namespace-sandboxed read-write and read-only virtio-fs shares and proved host/guest I/O plus read-only denial |
| 17 | resolved | target-image systemd supports `ListenStream=vsock::22` and `serial-getty@hvc0` | Ubuntu 24.04.4 KVM boot; inspect loaded units/listeners and exercise SSH plus hvc0 |
| 18 | open | The CI runner exposes `/dev/kvm` | `ls -l /dev/kvm` in a workflow |
| 19 | resolved | pinned passt and qemu-img sources produce reproducible static x86_64 helper payloads with complete corresponding source and license provenance | Native x86_64 double build from digest-pinned Alpine and a 91-APK hash lock produced byte-identical passt SHA-256 `40e59201765c60a0a5bbd0f2caae1aae3fd8f9a9a0628a835159fb2f17ff7025` (322,144 bytes) and qemu-img SHA-256 `30bff329fe1001635cafcfebddc68a1c824d25110c66f968b428c4cf4785d75d` (3,065,192 bytes), both without PT_INTERP, DT_NEEDED, or build-id; corresponding-source SHA-256 `e0195a3ea6c7448e6de07e829347dee2e49eb86f9ff529b49e852ea8a1a38fac` |
| 20 | resolved | the literal root-owned passt AppArmor profile permits passt userns on Ubuntu 24.04 without granting userns to a user-writable path | On bare-metal `w` (Ubuntu 24.04 x86_64, AppArmor enabled), the exact `abi <abi/4.0>,` profile parsed and loaded as `firestone-passt-2025_02_17.a1e48a0 (unconfined)` against the root-owned mode-0755 helper. With `kernel.apparmor_restrict_unprivileged_userns=1`, unprofiled passt run as uid 65534 exited 1 at `Failed to detach isolating namespaces`; the literal profiled copy passed that stage and remained running until the bounded 2 s timeout (124), with no mandatory-stage userns denial. The profile, helper, sockets and test files were removed, the sysctl restored to 0, and loaded-profile absence verified. |
| 21 | resolved | the pinned direct-boot kernel carries every OCI guest device and filesystem as a builtin, with no loadable-module support | Probe of the pinned `ch-release-v6.16.9-20260508` image (x86_64 `bzImage-x86_64` SHA-256 `58088758f601a04ef85b09cf23db5530d51edc039ed47afbf2264c5b762cb568`, 8,385,024 bytes) confirmed builtin `EXT4`, `VIRTIO_BLK`, `VIRTIO_NET`, `VIRTIO_CONSOLE`, `VSOCKETS`+`VIRTIO_VSOCKETS`, `PACKET`, `IP_PNP`+`IP_PNP_DHCP`, `DEVTMPFS`+`DEVTMPFS_MOUNT`, `TMPFS`, `UNIX`, `PROC_FS`+`SYSFS`, `OVERLAY_FS`, `VFAT`, `VIRTIOFS`; no module support and no squashfs. Builtin ext4 is what lets `firestone-init` grow the root with `EXT4_IOC_RESIZE_FS` (§10.5) |
| 22 | resolved | kernel `ip=dhcp` cannot be the OCI network path | Same probe: `IP_PNP_DHCP` is builtin, but with no server answering the in-kernel client retries for about 176 s before giving up, which would dominate every `network.mode = "none"` boot. Firestone omits `ip=` from the fixed cmdline (§9.5) and runs a short-timeout userspace client over builtin `CONFIG_PACKET` (§10.5) |
| 23 | resolved | Cloud Hypervisor v53 disables sector-0 writes on an autodetected raw disk image | Pinned v53 disk source: autodetection marks the image so that writes to sector 0 are dropped. Every raw disk Firestone emits therefore declares `"image_type": "Raw"` explicitly and `config_overlay` may not remove it (§9.5) |
| 24 | open | passt answers a guest DHCP request for an OCI machine | Probes observed kernel DHCP messages reaching a guest under passt, which is suggestive but not proof for the userspace client. Close it only with the `firestone-init` end-to-end run (M6-30) showing an OCI guest that leased an address and reached the network |
| 25 | open | an OCI machine stops gracefully | v0.2's `firestone-init` installs no ACPI power-button handler, so `vm.power-button` is not answered and §9.4 falls through to the force path after the timeout. Close it by handling the Cloud Hypervisor power button inside init, or by accepting the documented behavior in §21 |

---

## 21. Decision log

| Decision | Chosen | Alternatives considered | Why |
|---|---|---|---|
| OCI registry client | Hand-rolled Registry V2 subset over the blocking `reqwest` client Firestone already uses for image pulls: three GET endpoints, one anonymous Bearer retry, `auths`-only basic auth, index platform selection, digest-verified blob streaming | the `oci-client` crate; `oci-distribution`; shelling out to `skopeo` or `crane` | Every OCI crate is async and pulls tokio into `firestone-core`, which the architecture rules forbid, and each adds a large transitive surface for three read-only endpoints. The hand-rolled client reuses the existing bounded HTTPS transport, its limits, and its error kinds, and stays auditable in a few hundred lines. |
| OCI layer compression in v0.2 | gzip and uncompressed tar layers only; a `+zstd` layer is a clean `dependency` error naming the media type | add a zstd decompressor now; silently skip zstd layers; fall back to a `zstd` host binary | Registries overwhelmingly still publish gzip, and a partially applied layer set is a corrupt rootfs, not a degraded one. A named error tells the user exactly why their image is unsupported and leaves adding zstd a purely additive change. |
| `mkfs.ext4` distribution | Firestone-owned static e2fsprogs 1.47.3 helper, pinned in `deps.toml` and downloaded on the first OCI pull | embed it in the binary like passt and qemu-img; require a host `e2fsprogs`; build ext4 images in Rust | Only OCI users need it, so embedding it would grow every standalone release for a feature most machines never touch. The same locked, hash-verified, no-replace publisher that installs firmware installs it, so laziness costs no integrity. |
| Direct-boot kernel distribution | Pinned `ch-release-v6.16.9-20260508` kernels for both architectures in `deps.toml`, downloaded and published mode 0644 on the first OCI start | embed the kernel; build a kernel from source; reuse a host `/boot` kernel | An 8 MB x86_64 and 23 MB aarch64 image would dominate a 14 MB release for a feature only OCI machines use. Cloud Hypervisor publishes the kernels it tests against, which is exactly the configuration §10.5 depends on, and a host kernel has no guaranteed builtin set. |
| `firestone-init` process model | Stay PID 1 for the life of the machine, spawn the entrypoint as a child in its own process group, reap orphans, forward `SIGTERM`/`SIGINT` to that group, then `sync` and `RB_POWER_OFF` on child exit | `exec` the entrypoint as PID 1; ship a real init such as tini or systemd; let the VM idle after the entrypoint exits | An `exec`ed container entrypoint is almost never signal- or reaper-aware, so zombies accumulate and `stop` cannot be delivered. Powering off on child exit is what makes a one-shot OCI machine terminate on its own and gives Firestone an attributable exit instead of a hang. |
| OCI guest DHCP | A minimal userspace DHCP client on `eth0` with a short timeout, over builtin `CONFIG_PACKET` | kernel `ip=dhcp` via builtin `IP_PNP_DHCP`; a static address from the host; ship `udhcpc` in the rootfs | Verify 22: the in-kernel client retries for about 176 s when nothing answers, which would dominate every `network.mode = "none"` boot and any slow passt start. A bounded userspace client fails fast, warns, and lets the machine boot; shipping a third-party client would add a binary to every OCI rootfs. |
| OCI guest configuration transport | A magic-framed, length-prefixed JSON document on a raw read-only disk in the existing seed slot | reuse the cloud-init NoCloud vfat seed; pass configuration on the kernel command line; a vsock handshake at boot | The guest has no cloud-init to read a NoCloud seed and `firestone-init` should not need a vfat parser. A framed length prefix makes truncation, a stale disk, and a future format version all detectable, and the command line is size-limited and world-readable in the guest. |
| OCI rootfs xattrs | Preserve only `security.capability` and `gnu.translator`; drop everything else, SELinux labels included, and document it | preserve every xattr; fail an image that carries an unsupported xattr; relabel in the guest | Those two are what the pinned `mkfs.ext4` tar input accepts, so anything else cannot survive packing regardless of intent. An SELinux-labeled image boots unlabeled under a kernel with no SELinux policy anyway, so dropping labels is honest rather than lossy, and failing the pull would reject most distro base images. |
| OCI base image sizing | `unpacked_bytes × 1.15 + 256 MiB`, rounded up to 4 MiB, computed from the merged tree with integer arithmetic | size to the exact unpacked bytes; a fixed size; ask the user for a size | ext4 metadata, the journal, and any first-boot writes need headroom that exact sizing does not leave, and a fixed size either wastes space or fails large images. Integer arithmetic over a deterministic merged tree makes the same manifest produce the same base size on every host, which the image identity depends on. |
| Plain-HTTP registries | Opt-in only, through an `images.insecure_registries` list of exact `host:port` entries that can never include Docker Hub | allow plain HTTP whenever HTTPS fails; a global `--insecure` flag; refuse plain HTTP entirely | A silent HTTPS-to-HTTP fallback is a downgrade attack, and a per-invocation flag gets pasted into scripts. An exact host-and-port allowlist in the global config makes the exposure explicit, reviewable, and narrow, while still supporting the local development registries that need it. |
| OCI stop semantics in v0.2 | `stop` sends `vm.power-button` as always, and, because `firestone-init` installs no ACPI handler, falls through §9.4's timeout to the force path; `stop --force` skips straight to it | claim graceful stop and hide the timeout; make `stop` force-only for OCI machines; block OCI boot until an ACPI handler exists | The existing stop sequence already degrades correctly and the guest syncs on its own entrypoint exit, so the force path is safe rather than merely tolerated. Recording it as verify 25 keeps the honest cost visible instead of advertising a graceful stop the guest cannot perform. |
| Static `mkfs.ext4` helper | Extend the existing `build/helpers` recipe with e2fsprogs 1.47.3: configure `--with-libarchive=direct --disable-nls --disable-uuidd --disable-fuse2fs`, build only `libs` and `misc/mke2fs` with the full `pkg-config --static --libs libarchive` closure passed as a make-time `LIBARCHIVE` override, publish the stripped static binary as `mkfs.ext4` | a host `e2fsprogs` package; `make all` in the e2fsprogs tree; loop-mount plus `cp` as root; writing ext4 from Rust; libguestfs | The OCI pipeline needs an unprivileged, deterministic way to turn a layer tar into an ext4 image, and `mke2fs -d <tar>` with libarchive is the only pinned tool that preserves ownership, modes, symlinks, hard links and device nodes without root. `misc/Makefile` hardcodes `LIBARCHIVE=-larchive`, which cannot close a static link, and `make all` fails because `debugfs` cannot link statically, so the recipe must name the one target it needs. Reusing the passt/qemu-img container keeps one twice-built, byte-identity-checked, GPL-corresponding-source release instead of a second distribution channel. |
| Port forward changes on a running machine | Apply on restart, surfaced: `forwards_pending` on the `ls` and `show` payloads compares the spec's forwards against the applied set in `state.json` as a canonical order-insensitive multiset, `ls` marks the row and prints one legend, `show` notes it on stderr, and a spec write on a running machine warns `port forwards apply on restart` | restart passt in place and reattach the VMM; add and remove mappings through a passt control interface; hide the divergence and let the next start apply it silently; report any spec edit as pending regardless of content | passt fixes `-t`/`-u` at spawn and exposes no runtime interface to change them, and the Cloud Hypervisor v53 vhost-user session cannot survive a passt restart — `--one-off` passt exits with the VMM and the VMM never reconnects to a replacement backend. There is no hot-apply to implement, so the honest contract is apply-on-restart made visible. Comparing canonical sets rather than text keeps a reorder or a respelling from raising a false pending flag. |
| `cp` transport and operand grammar | Wrap the system `scp` through the existing vsock `ProxyCommand`, sharing one option block with `shell` and the readiness probe. Exactly one operand is remote, and an operand is remote only when the text before its first colon is a machine name (`[a-z0-9-]+`); `./` keeps an ambiguous local path local in both directions. `Action::Cp` returns the planned argv and the CLI alone execs it, so `cp` publishes no REST route. | implement a Firestone file-transfer protocol over vsock; require `firestone cp NAME SRC DST` with no `<machine>:<path>` grammar; accept any text before a colon as a host, as `scp` does; add `POST /v1/machines/{name}/files` | `scp` already speaks the guest's SFTP subsystem, and reusing the one option block means host-key trust, identity, and proxy policy cannot drift from `shell`. The colon grammar is what users already type for `scp` and `rsync`, while the machine-name charset makes classification decidable without asking the store, and `./` is OpenSSH's own escape. A REST route would have to stream a transfer the CLI performs by becoming `scp`, which is the same reason `shell` and `console` stay CLI-only; the §5.4 drift test compares routes to `docs/openapi.json` and is unaffected. |
| Machine metrics sampling (M6-01) | One on-demand `Action::Metrics` sample per call: reconciled state and spec, `/proc/<vmm_pid>/stat` fields 14+15 and `VmRSS` (Linux only, `null` elsewhere), plus v53 `vm.counters` and `vm.info`; counters stay cumulative, `u64::MAX`-family sentinels are projected as absent, block counters are typed while network counters pass through under `net: null` when the VMM reports none | run a sampling daemon or ring buffer; compute rates on the host; surface raw v53 counter maps verbatim; type network counters from their presumed names; report `0` for an unavailable figure | A daily-driver metric needs no background process: both sources are already open for a running machine, and two client samples give a rate without Firestone owning a time series. Publishing `u64::MAX` latency sentinels or a fabricated `0` would poison every derived rate, and the verified v53 fact that vhost-user `passt` emits no network entries means typed network fields would be unverified invention. |
| `clone` disk semantics | Copy the source's qcow2 overlay with `qemu-img convert -B <base>` by default so installed guest state carries over while the immutable base stays shared; require the source to be `created` or `stopped`; offer `--fresh-disk` for an empty overlay on the same base; copy `firestone.toml` byte for byte and never copy `state.json`, `known_hosts`, seed artifacts, logs or snapshots | reference the source overlay as a new backing file; copy the whole machine directory; snapshot-and-restore into the destination; allow cloning a running machine | A copied overlay is a real independent disk: removing the source cannot corrupt the clone, which a backing-file chain would allow. Refusing a running source is the only way to get a crash-consistent copy without a VMM-level quiesce, and the M6-04 snapshot work reuses the same `copy_overlay` primitive. Excluding runtime files keeps MAC and cloud-init instance id derived from the destination name, so the clone is a new machine by construction; the duplicated guest `/etc/machine-id` is documented in §24.4 rather than papered over by rewriting guest filesystems. |
| M6-03 resize headroom is opt-in and read back from the boot config | Add optional `cpus_max`/`memory_max` to `MachineSpec`. Map them to `cpus.max_vcpus` and `memory.hotplug_size`, emitting `hotplug_size` only when `memory_max` is set. Validate a live `Action::Resize` against the published `vmconfig.json` — the bytes the VM actually booted with — never against the current spec file, and refuse anything outside it as `conflict` with the hint `set cpus_max/memory_max and restart <name>`. The live path takes no machine lock, because the shim holds that lock for the machine's whole lifetime; it changes only the VMM's live sizing and `firestone.toml`, which the shim never writes. Persist the same values so desired state matches observed state. | always reserve headroom (e.g. `max_vcpus = host cpus`); infer headroom from the current spec; query `vm.info` for the ceiling; take the machine lock and fail `busy` on every running machine; report live success without persisting the spec | Verified on bare metal against Cloud Hypervisor v53: both resize calls return 204 instantly and `vm.info` reflects the change, but RAM hotplug requires `hotplug_size` **declared at boot**. Headroom is therefore a property of the boot configuration and reading it from anywhere else can only lie. Making it opt-in keeps the default machine's `vm.create` bytes byte-identical to before this feature — asserted by a regression test — so no existing machine is disturbed. A `conflict` naming the two fields and the restart tells the user the exact next action instead of surfacing a VMM 500. |
| M6-03 hotplugged CPUs are onlined by a udev rule in Firestone's cloud-init part | Ship `/etc/udev/rules.d/80-firestone-hotplug-cpu.rules` (`ACTION=="add", SUBSYSTEM=="cpu", ATTR{online}="1"`) plus `udevadm control --reload` in the rendered Firestone part, and regenerate the multipart and seed goldens. | online CPUs from the host over SSH after each resize; document a manual `chcpu -e`; a systemd path unit or a polling service; leave hotplugged CPUs offline | The v53 probe showed hotplugged RAM auto-onlines on Ubuntu 24.04 while hotplugged **vCPUs arrive offline**, so without this a successful `vm.resize` would add CPUs the guest scheduler never uses — a resize that reports success and does nothing. udev is the mechanism the kernel already fires for the event, needs no host round-trip, and works on a machine with no network. The cost is real and is accepted deliberately: the rendered user-data bytes changed, so §10.4 instance ids changed, so the next start of an existing machine reprovisions it and invalidates its `known_hosts`. |
| M6-03 disk grows at start and never shrinks | Refuse any `PUT`/`PATCH`/`resize` whose effective `disk` is below the existing overlay's virtual size (`invalid_spec`, "disk shrink is not supported"). On start, when the overlay is smaller than `disk`, run `qemu-img resize` before overlay validation and report `grown to <size> overlay`. Leave the guest partition to cloud-init `growpart`. | resize the overlay live through the VMM; shrink with a warning or `--force`; grow the guest filesystem from the host; refuse the grow and make the user recreate the machine | qcow2 shrink truncates the guest filesystem, and there is no safe way to know the guest has freed those blocks — so it is not a flag, it is a refusal. Growing the container is safe and offline, and `growpart` is already in Firestone's part, so the partition follows on the next boot with no new host tooling. Doing it at start keeps `disk` a plain spec field instead of a fourth lifecycle verb. |
| Guest password storage and exposure | Store `cloud_init.password` as plaintext in the spec and rendered user-data, and defend it with enforced mode 0600 on `firestone.toml`, its edit candidate, and every seed artifact inside the mode-0700 seed directory; render `chpasswd` with `expire: false` and a JSON-quoted `"<user>:<password>"` scalar; keep `ssh_pwauth` false unless explicitly set; redact the value from `Debug`, logs, events, errors and argv, and expose it on the CLI only through `--password-file` | hash the password in Firestone; keep the umask mode and rely on the 0700 machine directory; redact the password from `show`/REST spec payloads; enable `ssh_pwauth` implicitly whenever a password is set; accept `--password VALUE` | cloud-init's `chpasswd` list takes a plaintext value, and any Firestone-side hash would pin one crypt scheme while remaining recoverable from the same 0600 files, so permissions are the real boundary and must not depend on the caller's umask. Redacting the spec payloads would break `firestone.toml` round-tripping, while quoting, `--password-file`, and the explicit `ssh_pwauth` opt-in remove the exposures that are not the user reading back their own configuration. |
| Structured spec field errors | `ErrorInfo` gains an optional `field` carrying the dotted spec path, populated by the single `invalid()` constructor in `spec/validation.rs`, serialized only when present, surfaced by the REST envelope and ignored by CLI rendering | parse the key out of the existing `invalid '<key>': …` message; add a separate validation-error type or a `Vec<FieldError>` result; give every error kind a field | One constructor already owns every §7.2 failure, so one builder call makes the whole matrix field-addressed with no per-site churn. An optional member keeps existing bodies byte-identical and keeps one error type across CLI, config and REST, while message parsing would make a human string a machine contract. |
| OCI reference classification and docker.io normalization | A reference is OCI exactly when it carries an `oci://`/`docker://` prefix or contains `/` with a first component holding `.` or `:` or equal to `localhost`; parsed references normalize to `registry/repository:tag` or `repository@sha256:…` with `docker.io`, the `library/` namespace, and the `latest` tag filled in; the branch sits between the HTTPS check and the catalog, and a heuristic-only reference that fails to parse falls through unchanged | require an explicit scheme for every container image; ask the registry whether a name exists; treat any `a/b` name as OCI; hard-fail every classified reference that does not parse | The heuristic is the one users already know from Docker and Podman, so `ghcr.io/owner/app` works without ceremony while `ubuntu:24.04`, `nginx`, and `owner/app` keep their current meaning. Normalizing at parse time gives the registry client, the cache key, and `state.image.ref` one canonical spelling. Falling through on a heuristic-only parse failure is what keeps `./file.qcow2` and other dotted relative paths reporting the same missing-path error they reported before. |
| OCI layer merge to a canonical tar | Two passes over the layer blobs: pass one applies the `.wh.` whiteout, `.wh..wh..opq` opaque, same-path replacement and dropped-subtree rules into a sorted path map; pass two re-reads only the surviving members and streams them out in path order, copying every member that needs no repair block for block. Entry paths that are absolute, hold a `..` component, or would escape the root are rejected, and the merge stops above a configurable 8 GiB uncompressed and 1M entry cap, all with kind `dependency`. Character and block device entries are emitted unchanged | extract each layer onto the host and re-tar the result; one pass holding file contents in memory; decompress every layer to a temporary seekable copy; drop device entries | Nothing is extracted on the host, so a hostile layer cannot escape the image root and device entries stay inert data that `mkfs.ext4` consumes directly. Sorted output is byte-deterministic and places parents before children, while the block-for-block copy preserves mode, owner, mtime, symlink targets, PAX (including `SCHILY.xattr.*`) records and GNU long names without re-encoding them. The caps refuse a decompression bomb before its bytes are read. |
| Log colour | One server-side SGR allowlist on the logs surface: `ESC [` + at most 16 bytes of `[0-9;]` + `m` passes verbatim, every other sequence is swallowed as one U+FFFD; a stateful sanitiser spans chunk boundaries and flushes a dangling sequence at end of stream; the browser only maps the surviving SGR to classes | keep replacing every control byte; strip escapes in the browser and stream them raw; ship a full terminal emulator (vt100/xterm.js) for the logs tab; allow OSC for window titles | Boot output is colour, and replacing ESC turned systemd's into a wall of U+FFFD. Filtering on the server means `curl`, the UI and any future client get the same already-safe bytes, with no client trusted to filter for itself and no JS parser in the trust boundary. The allowlist is the whole defence: SGR only paints, while OSC 52 writes the clipboard and other CSI finals move the cursor, so anything not provably paint is destroyed rather than understood. |
| Browser terminals | Two `/v1` WebSocket routes carrying raw terminal bytes: the console bridges the existing single-client broker with its acknowledgement completed before the upgrade (`BUSY` → `409`), and the shell allocates a host pseudo-terminal and runs the same OpenSSH argv through `Cmd`, applying `TIOCSWINSZ` from a JSON resize control frame; a protocol upgrade additionally passes the same-origin rule | ship an SSH or terminal implementation into the browser; forward a TCP port to the guest's sshd; poll the console log over HTTP; make the browser the second console client and let it discover `BUSY` after upgrading | A browser terminal cannot speak the raw SSH wire protocol, so the SSH client stays on the host and the browser gets bytes. The console broker is single-client by design, so the contention answer must be an ordinary REST `409` a page can render, not a WebSocket that opens and dies. `SameSite=Strict` already fails closed cross-site, but an upgraded socket is a long-lived channel to a shell, so it earns a second lock. Cost: an attached terminal holds its connection until serve's drain timeout aborts it. |
| M6 interface contracts frozen up front | Snapshot, clone, resize, metrics, prune, and terminal WS routes plus spec additions are fixed before implementation (route shapes recorded in the M6 milestone's feature sections as they land); UI work proceeds against the contracts in parallel | design each surface inside its implementation PR; a single serialized workstream | Parallel agents need a stable seam; freezing the action names, REST paths and payload shapes first lets core and UI land independently while the drift gate keeps `docs/openapi.json` honest. |
| First-start pinned firmware | Before `vmconfig.json` publication, install only the effective built-in `auto`/`rhf`/`edk2` artifact through the shared locked, no-follow, exact hash/mode, fsynced, no-replace publisher; reverify it for VmConfig; never install or modify a custom firmware path | require a broad `doctor --fix` preflight; download every vendored artifact; trust an existing regular file; rewrite custom firmware | A direct first start from an empty home gets the one firmware it needs without unrelated host repair. The manifest identity and secure publisher keep VMM input inside Firestone's owned dependency boundary, while the custom path remains authoritative. |
| Passt runtime isolation diagnosis | Probe the exact foreground, one-off vhost-user mode with repair disabled; classify both `Couldn't create user namespace` and `Failed to detach isolating namespaces` as fatal; offer the existing AppArmor repair when host facts support it; runtime selects only the verified literal root-owned pinned copy after repair | treat later detach failure as available because the first userns succeeded; use generic `unshare`; disable AppArmor or a sysctl; accept a user-writable profiled path | Passt cannot serve Cloud Hypervisor after either fatal exit. Matching runtime argv closes the false-ok gap, and verified literal-path selection proves the repair authorizes only the pinned binary. |
| VMM | cloud‑hypervisor | QEMU | REST‑controlled, static Rust binary, userspace vsock, ~200 ms firmware boot, small device model; QEMU's breadth is legacy we do not need. Cost: KVM‑only, no graphics, narrower distro coverage. |
| Process model | no global daemon; one shim per machine; stateless `serve` | pure daemonless; libvirt‑style daemon | Exit codes, ordered start/stop and single‑writer state need a supervisor; a global daemon is bloat and a single point of failure. |
| State store | filesystem (TOML spec + JSON state, flock, atomic rename) | SQLite | Transparent, no dependency, trivially inspectable; scale is tens of machines, not thousands. |
| Liveness | socket connect + `vmm.ping` | pid files | Pids go stale; sockets in tmpfs self‑clean on reboot. |
| Rootless | yes by default; no capabilities. A user-confirmed `doctor --fix` may install only the exact embedded passt bytes and a literal-path AppArmor profile as root | wildcard profile over `<data>/bin/*`; root + bridge; silent sudo | A wildcard over a user-writable directory lets replaced code inherit `userns,` and defeats the host mitigation. The versioned root-owned attachment preserves the default rootless VM path while containing the one host-policy exception. |
| Default network | passt (vhost‑user) | slirp4netns/libslirp; bridge/tap | passt is fast, unprivileged, transparent addressing, forwards; slirp is slow and legacy; bridges need root and infrastructure. |
| VM‑to‑VM networking | not in v0.1; tap mode for users who need it | managed bridge | Owning an L2 story is a big surface; defer until demanded. |
| Shell transport | ssh over vsock | ssh over forwarded TCP port; serial login | Works with no network, full ssh feature set, no port allocation, keeps `shell` working when users break networking. |
| SSH keys | firestone generates its own; user keys are an appended list; multipart cloud‑init | `--ssh-key` as the only path; "pass the user's key" | The user's key needs `authorized_keys`, not custom networking; multipart keeps user‑data untouched. |
| Boot | firmware boot of stock cloud images; catalog entries select a tested firmware, while local/URL defaults remain RHF on x86_64 and edk2 on aarch64 | direct kernel boot; one firmware default for every image | Direct boot needs per-distro kernels and rootfs extraction. Firmware is image-specific: the Ubuntu 24.04 x86_64 observation requires edk2, without generalizing that result to untested releases or architectures. |
| Seed disk | vfat via `fatfs` | ISO via genisoimage | One fewer host dependency. |
| VMM configuration | JSON `VmConfig` via `vm.create` | argv flags | Data, not shell strings; enables `config_overlay`. |
| `create` behavior | TTY-guided configuration by default; the first prompt is a standard arrow-key selector over the merged catalog plus a final custom URL/local-path option, implemented with exactly pinned `dialoguer` 0.12.0 without default features; `--yes`/`--json` and non-TTY use deterministic arguments; never boots; always renders the effective spec and config path; `--edit` opens the editor | blind image text input; fuzzy search dependency; opt-in wizard; always prompt regardless of TTY | Beginners can see what Firestone supports before typing an image. A standard selector is predictable for the bounded catalog, while the custom option keeps every existing image source available. The wizard remains a CLI input adapter that produces the same `MachineSpecPatch` used by config and REST. |
| Catalog discovery surfaces | Project one `CatalogList` action through `firestone catalog` and `GET /v1/catalog`; return canonical references, aliases, and ordered architecture/effective-firmware pairs; render the human table from that typed payload | treat `images ls` as remote discovery; read only the built-in TOML; derive REST output independently; display one fallback firmware when an architecture overrides it | The owned image store and the remote catalog answer different questions. One shared payload keeps CLI, JSON, REST, built-in entries, and user catalog additions aligned without misreporting architecture-specific firmware. |
| `run` semantics | idempotent (create/start/shell) | Podman‑style "new instance every time" | "Instant context" every time, no name clutter. |
| Overlays | qcow2 with a qcow2 backing file via `qemu-img`; M1-06 records fio results without a pass threshold | raw per-machine copies; reflink | Fast creation and small machine disks. The exact x86_64 edk2 path booted under Cloud Hypervisor v53 with `backing_files: true`. |
| Console | Cloud Hypervisor PTY for virtio-console plus a shim-brokered `console.sock`; serial output remains a file | direct Cloud Hypervisor socket console; shim tees serial | Pinned v53 rejects `console.mode = "Socket"` for the virtio-console device but supports PTY. The shim is already the lifetime owner and can broker reconnects without racing `console.log`. |
| CID | fixed 3 | allocation table | CH's vsock is userspace; the CID is not host‑global. |
| REST transport | unix socket only in v0.1 | TCP with token | Auth by file permissions is simple and correct; TCP later with a token. |
| Language | Rust | Go | Same ecosystem as the VMM; one static binary. Go would also work. |
| Pre-pull image identity | `created` state stores the canonical image reference with null `id` and `sha256`; the first successful pull fills both before overlay creation | empty-string sentinels; download during `create`; omit `state.json` until start | `create` is specified as a local spec write and M0 must work on an empty home before M1 image pulling exists. Nulls represent unavailable identity without inventing one; image removal ignores machines until a real id is recorded. |
| CLI support crates | `jiff` for RFC 3339 timestamps; `shlex` for `VISUAL`/`EDITOR` argv; `unicode-width` for terminal table columns | hand-written timestamp formatting, shell-word parsing, or Unicode width tables; invoke the editor through a shell | These are bounded data-formatting/parsing concerns with mature implementations. Direct argv execution preserves the no-shell process invariant, while measured display width keeps deterministic tables aligned without truncating user data. |
| Editor selection | Use the first nonempty `$VISUAL`, then `$EDITOR`, otherwise `nano`, for both `edit` and `create --edit`; parse the selected command with `shlex` and launch its argv through `Cmd` | fail when both variables are unset; default to `vi`; invoke the command through a shell | `nano` gives hosts that provide it a usable edit command without setup. User configuration retains precedence and quoted arguments, while direct `Cmd` execution preserves the no-shell process invariant and existing dependency errors. |
| M5 terminal feedback | Keep `firestone-core` terminal-UI-free; the binary uses exactly pinned `indicatif` 0.18.6, `console` 0.16.4, and `owo-colors` 4.4.0. Live ordered rows are enabled only when stderr is a TTY and `TERM` is not `dumb`; each step occurrence keeps its own settled row. `NO_COLOR` and `--no-color` disable only SGR color, cursor hiding is forbidden, and non-TTY, JSON, quiet, `serve`, and dumb-terminal streams retain their static contracts. | Hand-written ANSI; a core progress abstraction; replacing settled rows by step id; disabling all TTY control under `NO_COLOR` | The binary already owns terminal policy, while the core owns events. Mature width/progress/color crates avoid a second terminal implementation. Occurrence rows preserve repeated `fs` events, and capability gates keep automation byte-stable without sacrificing an interactive no-color progress display. |
| M5 error diagnostic precedence | Preserve the primary operational error kind and hint when cleanup also fails. Supervised process exits retain their configured kind and report the program, numeric exit code or signal, and at most the last ten control-safe lines from a current-user mode-0600 regular process log. Once an HTTP status is parsed, VMM API failures retain that status and a bounded control-escaped body preview. Raw transport/read failures are distinct from checksum or content-length verification failures. | Let cleanup replace the root error; bespoke VMM/sidecar messages; discard malformed error bodies; classify every image read as checksum | Stable kinds drive both CLI exits and REST statuses, so secondary failures cannot change them. One bounded process diagnostic prevents divergent failure text and unbounded or secret-file reads. Status-aware previews make VMM failures actionable, while transport/integrity separation reserves checksum status for actual verification failures. |
| Dependency pins | cloud-hypervisor v53.0; Rust Hypervisor Firmware 0.5.0; cloud-hypervisor edk2 ch-1e1b96f126; Firestone virtiofsd v1.14.0 for both compile targets; Firestone static-helper release `helpers-v0.1.0-firestone.1` with passt `2025_02_17.a1e48a0` and qemu-img 8.2.2 for x86_64 plus corresponding source/build provenance | moving `latest` URLs; distro passt/qemu-img; mutable CI artifacts; unverified source-only builds | Exact release URLs and SHA-256 values make refreshes reproducible. The helper build verifies QEMU's detached signature, all source/APK hashes, static ELF facts, exact versions, and byte identity before publication; `scripts/pin-deps.sh` re-downloads every public asset. |
| Doctor passt minimum | passt `2025_02_17.a1e48a0` or newer; exact help tokens for foreground, one-off, vhost-user, socket-path, repair-path, and log-file; successful no-side-effect `--tcp-ports none --udp-ports none --help` parser probe | the first vhost-user release; presence alone; version alone; require truncated tail help tokens | M3 depends on grammar added after the first vhost-user release, including repair-path control. The pinned binary's fixed help buffer can truncate the TCP/UDP tail, so a parse-only help invocation verifies those long options without opening sockets. Checking the release date, visible tokens, and parser result rejects older or feature-stripped builds without claiming verify 14 runtime interoperability. |
| Embedded helper publication | Strict x86_64 release builds download the pinned Cloud Hypervisor, passt, and qemu-img assets before Cargo; `firestone-core/build.rs` checks target, manifest identity and SHA-256 before generating `include_bytes!` metadata; runtime publishes immutable versioned executable files under `Paths` | compile helper/VMM sources inside Cargo; commit opaque binaries; download executables on first use; trust PATH | Cargo remains network-free and the released Firestone byte identifies the exact VMM and helper bytes. First-use materialization avoids host packages while retaining auditable normal executable paths and process identity. |
| Embedded default VMM precedence | For the default VMM, materialize embedded Cloud Hypervisor v53.0 lazily and then apply the existing owned-file mode/hash validation; an explicit `vmm.binary` is imported first and never triggers embedded VMM publication; development/aarch64 retain the installed artifact path | ignore custom VMMs; materialize before checking the custom override; weaken post-publication validation | Custom VMM behavior is an existing escape hatch and must remain authoritative. Reusing the generic secure materializer gives Cloud Hypervisor the same no-follow, lock, checksum, fsync, mode, and no-clobber guarantees as passt/qemu-img without changing VMM argv or protocol. |
| AppArmor passt remediation | Diagnose passt's mandatory userns stage with host policy/seccomp/container/audit context; install a checksum-verified root-owned literal passt copy and matching unconfined `userns,` profile only after an interactive confirmation; noninteractive prints commands | wildcard user-owned profile; infer AppArmor from `unshare` alone; silently invoke sudo; disable the kernel restriction | AppArmor attaches policy by path, not manifest hash. Literal root-owned attachment prevents same-user replacement from inheriting userns. Correlated evidence avoids blaming AppArmor for container seccomp or disabled kernel user namespaces. |
| Standalone helper Linux runtime gate | On exact commit `285bcf8acfae0cbccd9c62e0859cbd7932a2c7ea`, build the x86_64 musl Firestone binary twice from the pinned helper release, compare bytes, verify static ELF facts, run `doctor --fix` as an unprivileged user, and complete the M3 real-KVM network/mount/tap coordinator using only materialized helper paths | infer embedding from binary size; run helper `--version` only; retain system passt/qemu-img in the harness; portable tests as KVM proof | Both native builds produced identical 14,092,624-byte Firestone binaries with SHA-256 `b1f1fd72989bfae0287a5ca851cf5ce9db2b99201e4b9ed1b7c90cb0a8c81564` and no PT_INTERP/DT_NEEDED. Unprivileged extraction produced mode-0755 passt/qemu-img with their exact pinned hashes and versions. The updated M3 gate passed E2E 3, 4, changed/unchanged 8, verify 7 normal/crash teardown, verify 8 tap, verify 10 merge, and recorded passt/qemu-img in evidence SHA-256 `26bf0f818fae403258281c0a24a57768bdf9218665007fd5c0afb55b3aed4407` with complete cleanup. |
| [verify 1] firmware mapping at Cloud Hypervisor v53.0 | RHF 0.5.0 uses `payload.kernel`; edk2 ch-1e1b96f126 uses `payload.firmware`; Ubuntu 24.04 x86_64 accepts edk2 as its default path | pass either firmware through the other payload field; treat RHF as the accepted Ubuntu default | The pinned v53 CLI, README, and `PayloadConfig` source define the mapping. Run `577116f86ef6c61a302a5fabccf775ae267ee6be` verified the pinned edk2 SHA-256 `9fb511fc0dd423d90a79615a90a8ace9b9e078b4a115ea2c459e0ac2f4e60218`, emitted `payload.firmware`, and reached `m1-graceful login:`. RHF remains source-mapped and separately observed, not the accepted default for this image. |
| [verify 2] API and VmConfig at Cloud Hypervisor v53.0 | Use `GET /api/v1/vmm.ping`, `PUT vm.create`, `PUT vm.boot`, `GET vm.info`, `PUT vm.power-button`, `PUT vm.shutdown`, and `PUT vmm.shutdown`. Ping and info return 200 JSON; create, boot, power-button, and VM shutdown return empty 204; VMM shutdown returns 200 with `Content-Length: 0`. Root disks use `image_type: "Qcow2", backing_files: true`; CIDATA uses `image_type: "Raw", readonly: true`; the accepted console uses `mode: "Pty"`. | infer fields, methods, or status codes from endpoint names; use image auto-detection; use the rejected console socket mode | Tag v53.0 is commit `9ed824d6d08df3e96f7d5f50795d9449ac99f431`. Its [OpenAPI](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/9ed824d6d08df3e96f7d5f50795d9449ac99f431/vmm/src/api/openapi/cloud-hypervisor.yaml) and handlers define the wire contract. M1-06 exercised create, boot, ping 200, info 200 `Running`, power-button, and process exit using the exact persisted VmConfig bytes. |
| VMM API transport and framing | `UnixStream` plus `nix`, one fresh connection and one absolute deadline per request; 51,200-byte create body, 16-KiB headers, 64-KiB ping/error bodies, 1-MiB info body, and zero-byte empty successes; strict non-chunked `Content-Length` framing | `hyper` + `hyperlocal`; the unbounded upstream client; reading to EOF; accepting chunked or ambiguous framing | Cloud Hypervisor v53.0 pins micro-http commit `5c2254d6cf4f32a668d0d8e57ba20bebad9d4fba`. Its 51,200-byte server limit is in `micro-http/src/server.rs` lines 21-24, and Cloud Hypervisor does not override it (`vmm/src/api/http/mod.rs` lines 440-489). The pinned response writer emits HTTP/1.1 keep-alive, non-chunked `Content-Length` framing in `micro-http/src/response.rs` lines 79-85, 160-194, 245-304, and 357-373. A small closed parser avoids new dependencies, bounds hostile or drifted responses, and lets liveness reuse the lifecycle transport instead of maintaining a second parser. |
| VMM pause/resume scope | Do not expose pause or resume as v0.1 Firestone actions or `VmmApi` methods; the v53 endpoints remain intentionally unused. | Add transport-only client methods without shared `Action` variants; expand the public action model in M1-03. | The M1-03 contract names an exact seven-endpoint set, and the normative §5.2 `Action` enum has no pause or resume variants. Exposing transport methods alone would create a fourth surface that the CLI, config, and REST action model cannot project, contrary to §1 and §5. Any future pause/resume feature must enter through the shared action model and update all surfaces together. |
| [verify 12] vsock host handshake at Cloud Hypervisor v53.0 | Write `CONNECT <guest-port>\n`, then wait for `OK <allocated-host-port>\n` after the guest accepts | treat Unix-socket connect alone as readiness; expect the guest port in the acknowledgement | The exact pinned `docs/vsock.md` specifies the request. Commit `9ed824d6d08df3e96f7d5f50795d9449ac99f431` in `virtio-devices/src/vsock/unix/muxer.rs` parses the command and its unit test proves the acknowledgement carries the allocated local port only after the virtio-vsock response. M1 does not add SSH readiness. |
| [verify 9] NoCloud vfat CIDATA runtime | Keep the deterministic 4 MiB vfat seed labeled `CIDATA`, attached read-only as the second raw disk | require an ISO tool; infer datasource selection from seed construction | On run `577116f86ef6c61a302a5fabccf775ae267ee6be`, Ubuntu 24.04.4 reported `status: done`, `DataSourceNoCloud [seed=/dev/vdb]`, `datasource=nocloud`, and `cidata_label=CIDATA`, with empty error lists. No seed or cloud-init content was logged. |
| Spec flag projection | clap-free field metadata in `firestone-core`; clap introspection in the CLI crate | derive `clap::Args` on the core patch type | Keeps the normative drift checks while preserving the crate boundary in §18.2. |
| Layer vector semantics | machine-file vectors replace lower values; CLI and REST PATCH vectors append | append every layer; replace every layer | Persisting and reloading a complete machine file remains idempotent, while repeatable command flags and PATCH requests can add values without resending the full list. |
| Portable patch clears | typed `clear` list shared by TOML, JSON and future `--clear FIELD` | JSON-only null; string sentinels per field | One closed enum keeps every optional and vector clear operation available and rejects unknown paths before dispatch. |
| Portable VMM merge patch | JSON object on JSON surfaces; canonical JSON text under the same TOML key | second TOML key; TOML null sentinel; omit RFC 7396 deletion | TOML has no null scalar. Canonical object text preserves nested RFC 7396 null deletion without changing the public key or object model. |
| XDG and HOME path inputs | absolute XDG config/data roots are honored; relative values are ignored; startup HOME is captured once as absolute | ignore XDG config/data; read environment for every expansion | Matches XDG rules and keeps path behavior stable after startup. |
| Runtime path trust | validate XDG base ownership/mode and explicit-root ancestry before creating the Firestone leaf | recursively create and validate only the leaf | Runtime sockets are authority-bearing files. A safe leaf inside renameable or symlinked ancestry is not safe. |
| Data path trust | validate ownership, mode, node type, and existing ancestry before accessing Firestone-owned data; create owned directories with mode 0700 | trust caller-supplied roots; validate only the final node; repair permissive directories automatically | Machine specs, state, binaries, and keys are authority-bearing. Refusing unsafe ancestry prevents another uid from replacing an owned path between validation and publication. |
| Interrupted machine creation | lock before writing `.creating`; reclaim only an unlocked, revalidated, incomplete publication | permanent tombstone; delete every incomplete directory; publish without a marker | A crash must not reserve a name forever, but recovery must not race an active creator or remove a complete machine. |
| User path resolution | retain `.` and `..` until complete kernel resolution; any canonicalization happens only for an existing complete path | lexical normalization before filesystem access | Lexical collapse changes meaning when a prefix is missing or a symlink participates in resolution. |
| Relative spec paths | resolve relative paths from `firestone.toml` against the machine directory and relative action patches against their supplied base directory inside `MachineSpec::load` | process working directory at validation time; adapter-side pre-resolution | Machine behavior remains stable across CLI and REST invocations, and callers cannot accidentally skip path provenance handling. |
| Image removal action payload | `ImageRemove` carries `force` | leave force in CLI/REST adapters | Sections 15.1, 15.4, and 16.2 expose forced image removal. The shared action must carry that choice so every interface dispatches the same operation. |
| M1 seed rendering crates | `fatfs` 0.3.6 with default features disabled and only `std` + `alloc`; `minijinja` 2.24.0 with `serde` | host ISO tools; hand-written templates; newer unbounded crate versions | Both exact versions resolve under the workspace's Rust 1.85 minimum. `fatfs` avoids a host process while MiniJinja keeps the Firestone cloud-config readable and golden-tested. |
| M1 cloud-init staging | render only Firestone's multipart part; reject configured `cloud_init.user_data` and `cloud_init.network_config` with `invalid_spec`; `provisioning = false` is the sole empty `user-data` exception | silently ignore deferred fields; accept user parts before M3; emit an empty multipart document | A successful M1 boot must not imply that requested user or network data reached the guest. Explicitly disabling provisioning is different: it intentionally requests zero user-data bytes and does not require the Firestone SSH key. |
| M1 multipart identity | fixed boundary `===============firestone==`; CRLF MIME framing; Firestone YAML remains LF; `instance-id` is `iid-<name>-<sha256(final user-data bytes)[0..12]>` | hash the template part only; random MIME boundaries; hash the seed image | Hashing the exact final bytes makes identity stable and makes any future multipart change trigger re-provisioning. With provisioning disabled, the suffix is the SHA-256 prefix for the empty byte string. |
| M1 network-config identity gap | keep `network-config` absent and reject the configured field until M3 defines whether and how its bytes affect `instance-id` | hash only multipart forever; include network bytes now without a stable formula | The normative text promises re-provisioning when cloud-init inputs change but does not yet define network-config composition. M3 owns that formula alongside user parts and network publication. |
| Deterministic CIDATA publication | 4 MiB vfat, label `CIDATA`, volume id `0x46530001`, 512-byte sectors and clusters, 1980-01-01 00:00:00 file times, and creation order `meta-data`, `user-data`, then optional `network-config`; stream directly into the atomic sibling file | variable image size; wall-clock timestamps; build a 4 MiB in-memory buffer; ISO | The complete image is byte-identical across rebuilds and still uses the existing fsync + rename publication boundary without a second full-size allocation. |
| M1 firmware path mapping | resolve RHF and edk2 install names from `DependencyManifest` for the selected architecture; map RHF to `payload.kernel`, edk2 to `payload.firmware`; an absolute validated custom firmware path also maps to `payload.firmware` | derive install names in the mapper; map every firmware through `kernel`; reject custom firmware | Manifest resolution keeps paths aligned with pinned artifacts. Cloud Hypervisor v53 treats a custom firmware image like edk2's firmware input and rejects simultaneous kernel and firmware payloads. |
| M1 canonical VmConfig overlay boundary | serialize a typed v53 base, apply the RFC 7396 object patch last, require every Firestone base field and managed device prefix to remain unchanged, preserve `net`/`fs` absence for disabled devices and payload exclusivity, recursively sort keys, and atomically persist the exact compact bytes sent | let overlays delete boot devices or disable shared memory; persist pretty JSON separately; compare only selected scalar fields | The overlay can add advanced Cloud Hypervisor fields but cannot disconnect sidecars or invalidate boot assumptions. One canonical byte sequence prevents inspection output from drifting from the API request. |
| NoCloud metadata scalar encoding | JSON-quote `instance-id` and `local-hostname`, which is valid YAML double-quoted scalar syntax | interpolate machine names as bare YAML; restrict otherwise valid machine names to a smaller character set | Firestone machine names may contain YAML metacharacters such as `:`. Quoting preserves every accepted name exactly and prevents a name from changing metadata structure. |
| Machine artifact publication trust | immediately before seed or VmConfig atomic writes, revalidate the data directory, machines directory, and final machine directory through `Paths`; seed publication also validates before and after creating its inspection directory | rely on validation performed during machine creation; canonicalize the final path; follow symlinks | Any of the owned ancestors can be replaced between actions. Revalidation rejects symlinks, wrong ownership, unsafe modes, and renameable ancestry before a write can reach an external path. |
| M1 overlay default invariants | treat required absent/default values as semantic invariants: the root disk stays writable, the seed has no backing chain, disks stay non-vhost-user, and mapped network backends retain their required absent/default fields | compare only keys serialized in the typed base; serialize every Cloud Hypervisor default explicitly | RFC 7396 can add a key that the base omitted. Checking the effective default semantics blocks changes such as root `readonly = true` without bloating the canonical request with server defaults. |
| M1 vsock CID | require exactly CID 3 in generated VmConfig | accept every Cloud Hypervisor CID of 3 or greater | Firestone's userspace-vsock design fixes CID 3 for every machine (§11.1); accepting other values would create state/config drift without any allocator or public configuration contract. |
| Owned key and firmware reads | revalidate the owned data directory plus final `ssh` or `bin` directory immediately before access; require the Firestone public key and installed firmware artifact to be regular non-symlink files | trust `doctor` having run earlier; follow final file symlinks; validate only the leaf directory | Seed and VmConfig inputs are authority-bearing. Refusing unsafe modes, ancestry, directory symlinks, and final file symlinks prevents external content from being consumed after an earlier check. User-supplied `cloud_init.ssh_keys` retain user-path semantics. |
| Image pull checksum action payload | `ImagePull` carries optional `sha256`; absent values remain omitted on serialization | keep `--sha256` only in the CLI adapter; create a second URL-pull action | URL verification is shared behavior in §§8.2, 15.1, and 16.2. One optional field lets CLI and REST invoke the same core operation without changing checksum-free catalog/local pulls. |
| Image sidecar v1 and identity | strict atomic v1 records full source/stored identities, architecture, formats, verifier, stored size and timestamp; stable id is `image-` plus a full SHA-256 over length-framed source identity | the six-field §6.1 sketch; use `<sha8>` as identity; hash only the converted qcow2 | Raw conversion makes source and stored bytes different, while `MachineState.image.sha256` must keep source identity. Full digests prevent prefix collisions and let remove/prune/pinned start validate the same immutable pair. |
| Image store publication and locking | one checked `images/.lock`; source/converted partials; recoverable `.removing` tombstones; base mode 0400, sidecar/lock mode 0600; sidecar atomic write; machine state pin before overlay | per-id locks; direct local backing; overwrite cache entries; two direct unlinks; unlocked prune | A single mutation lock keeps pull, pin, remove and prune ordering boring and race-free. Owned copies survive mutable/deleted local sources. Fsynced tombstone renames let the next mutation finish a removal after process death without adopting or stranding a half-pair. |
| Machine image cache and execution validation | canonicalize supported refs before cache lookup; persist canonical ref/id/source SHA together; require host architecture and canonical ref on every use; hash stored qcow2 before execution | compare raw alias text; trust id/source SHA or file size alone; hash only in `images inspect` | Moving catalog URLs must not bypass a warm canonical cache, and copied/stale state must not select a wrong-architecture or wrong-source base. The stored digest is meaningful only if execution paths verify the bytes it names. |
| Image HTTPS transport bounds | identity encoding; strict HTTPS at every redirect; five redirects; 30 s connect and 30 min request timeouts; manifests capped at 2 MiB; body progress coalesced per 1 MiB plus a final total | reqwest defaults; permit HTTPS→HTTP redirects; buffer manifests without a limit; emit per transport read | A bounded transport fails malformed or stalled sources predictably. Coalescing by accumulated bytes prevents attacker-controlled one-byte frames from amplifying events or memory while preserving a final exact total. Image bodies remain streaming and have no arbitrary product-size cap. |
| Catalog reference and firmware precedence | ASCII-safe non-path reference components; optional architecture firmware overrides the entry default; Ubuntu 24.04 x86_64 selects edk2 while its unverified aarch64 source retains RHF | permit arbitrary TOML strings; change the whole release to edk2 | Reference strings cross CLI, state, and filesystem-shaped resolution boundaries, so URL/path grammar is unsafe. The observed firmware result is architecture-specific and must not be generalized. |
| Image verifier provenance and generations | stable id excludes verifier and firmware; validated identical bytes atomically strengthen verifier/firmware and advance a positive per-source/architecture generation; unchecked pulls never downgrade; an unchecked direct start can reuse stronger same-locator provenance; cache selection uses verifier-aware tri-state matching and a unique highest generation | include verifier in id; choose by timestamp; require null provenance for an unchecked warm lookup; let current catalog URL or firmware invalidate every warm cache | Provenance may become stronger without changing bytes, wall clocks can repeat or move backward, verified direct URL caches must remain available offline, and moving release URLs must not force network access during start. |
| Local image descriptor trust | canonicalize the parent, reject a symlink/special final node, open once with no-follow/nonblocking flags, stream that descriptor, and reject metadata mutation | canonicalize the complete path then reopen; stat before a path copy; use local images directly as backing files | Holding one regular-file descriptor closes final-component substitution and FIFO blocking while preserving immutable owned bases after the source is changed or deleted. |
| Bounded owned JSON reads | image sidecars are at most 64 KiB and machine state at most 1 MiB, read from the validated open descriptor with limit plus one | trust pre-open length; unbounded `read_to_end`; deserialize directly from a pathname | An attacker or concurrent writer can grow a file after a metadata check; the read itself must enforce the allocation bound. |
| Interrupted image pair recovery | under the image lock, delete validated unreferenced base-only, sidecar-only, and sidecar-temp files; preserve and report any referenced half-pair | ignore half-pairs; delete all half-pairs; adopt a surviving file | Crash debris must not block a repull, but state references make automatic deletion data loss rather than cleanup. |
| qemu image inspection hardening | parse pinned 8.2.2 backing, dirty, corrupt, and format-specific data-file fields; require clean immutable bases; allow dirty but non-corrupt writable overlays; reject hidden external dependencies; require exact backing strings and qcow2 format; bound every qemu-img command | reject every dirty overlay; trust create exit status; filesystem-canonicalize a reported backing path; accept unknown dependency-shaped fields | Writable qcow2 overlays can legitimately remain dirty after a host crash and are recoverable, while a dirty immutable base indicates broken cache integrity. A successful command alone does not prove the image is standalone or points at the owned base. |
| Persisted image source classification | user input remains path-first; persisted state is strict HTTPS, then absolute local, then catalog without a relative existence probe; a complete pin remains loadable after local source deletion or catalog removal | rerun user-input heuristics for state; require the original local source or current catalog entry forever | State must not change meaning when a relative shadow file appears, and strict owned sidecar identity—not a mutable external source registry—is authoritative after pinning. |
| Descriptor-relative owned directory creation | walk from `/` with no-follow directory descriptors, use `mkdirat`, force each new descriptor to 0700 despite umask, and preserve insecure existing nodes unchanged | prevalidate then recursively create by path; pathname chmod; auto-repair existing permissions | Validation followed by path mutation leaves a substitution window. Descriptor-relative creation makes each checked parent the authority for the next component. |
| Interrupted image lock creation | recover a current-user regular lock whose mode is 000, 0200, or 0400 to 0600 before opening and validating it; reject every broader mode, wrong owner, symlink, or special node without chmod | reject all non-0600 locks; chmod any existing lock | SIGKILL can land between exclusive creation under a restrictive umask and descriptor chmod. Only an owner-only permission subset can be proven to be Firestone's safe interrupted state. |
| Strict sidecar publication preflight | serialize and validate the complete sidecar at a 64 KiB maximum before publishing the base; atomically write those exact prevalidated bytes; apply the same bound to provenance upgrades | publish the base then serialize; rely only on bounded reads; truncate metadata | A successful pull must never create a pair that its own list/start path cannot read. Preflight keeps over-limit references as a clean operation failure rather than crash-recovery debris. |
| Transient executable-busy spawn | `Cmd` retries only `ExecutableFileBusy` at 5 ms intervals, at most 20 times and never beyond the command's original absolute deadline; all other spawn errors return immediately | rerun CI; retry every spawn error; reset the timeout after spawn | A just-written executable can transiently return `ETXTBSY`. A narrow bounded retry fixes that filesystem race without hiding missing/permission failures or extending operation timeouts. |
| [verify 4] qemu-img 8.2.2 raw conversion | `convert -f raw -O qcow2 SOURCE TARGET`; the Ubuntu 24.04 x86_64 source was converted qcow2 to raw, then Firestone converted raw to an owned qcow2 and booted it through Cloud Hypervisor v53 with edk2 to `m1-convert login:` | retain raw bases; infer conversion flags; claim both architectures from one boot | Run `577116f86ef6c61a302a5fabccf775ae267ee6be` recorded raw source SHA-256 `8571b6b38c1d309d53f603e2d6c7de94287764acdff5a49fa82c8949d0ad187f` and stored qcow2 SHA-256 `1279c3ded1a79b7a3a6ce3896b6a7dde1055514e5694db806b179318f728d230`. This closes x86_64 edk2 only. |
| [verify 5] Cloud Hypervisor v53 qcow2 backing and measured fio | `qemu-img create -f qcow2 -F qcow2 -b ABS_BASE OVERLAY SIZE`; `info --output=json -f qcow2 PATH`; VmConfig `backing_files: true` | raw per-machine copy; reflink; declare performance acceptable without a product threshold | The converted machine booted from the exact overlay. Guest fio 3.36 ran 10 s, 64 MiB, 4 KiB randrw 70/30, psync, depth 1, direct I/O, seed 20260829. Overlay read/write measured 51,214,948/21,922,466 B/s and 12,503.650/5,352.165 IOPS; the raw auxiliary disk measured 45,577,368/19,493,781 B/s and 11,127.287/4,759.224 IOPS. `threshold_applied` is false because SPEC defines none. |
| Ubuntu 24.04 firmware release gate | the x86_64 catalog source overrides the release fallback to edk2; the unverified aarch64 source remains on RHF | change both architectures to edk2; keep x86_64 on RHF; generalize the result to other Ubuntu releases | On the observed x86_64 source RHF panicked during `LABEL=root` resolution while the same converted base and overlay booted through edk2. Only that architecture is corrected; verify 3 remains open for aarch64 and other releases. |
| Ubuntu 24.04.4 external-initrd RHF observation | Keep the durable image-sidecar firmware choice authoritative. Separately, Cloud Hypervisor reached both `ttyS0` and `hvc0` login when `--kernel hypervisor-fw` was combined with an externally extracted guest initrd and explicit `root=/dev/vda1` cmdline. This remains an RHF recipe, not a different firmware. | switch the current x86_64 edk2 catalog choice automatically; classify the recipe as non-RHF; add image extraction to M1 start | The observed path is a possible future boot optimization, but it requires image-specific initrd extraction, kernel-version and root-device discovery, and cmdline synthesis that SPEC does not define. It therefore does not change the current edk2/default contract or the persisted firmware sidecar in M1. |
| M1 shim lock ownership | The start caller keeps the machine flock through durable `starting` + shim pid publication, then the shim blocks on and owns that same flock from startup through child teardown, runtime cleanup, its one final atomic state write, and process exit. While supervised, `launch`, `status`, and `stop` serialize through `shim.sock`; a crashed shim releases the flock for verified unsupervised recovery. | let every CLI action reacquire while the shim runs; pass an inherited lock fd with `SCM_RIGHTS`; no lifetime lock | A separately reacquired stop lock deadlocks against the required lifetime owner. Safe Rust's owned ancillary-fd surface is not already in the dependency set, so the M1 handoff uses durable `starting` state plus the exact shim pid before the parent unlocks; any intervening action must observe that live identity and refuse mutation. The runtime tests prove lifetime contention and post-exit reacquisition. |
| M1 shim protocol and authority | Private mode-0700 machine runtime directory; mode-0600 `shim.sock`, `shim.pid`, `launch.json`, and `identity.json`; server and client verify same uid with Linux `SO_PEERCRED` or BSD/macOS `getpeereid`. One deny-unknown-fields NDJSON request per connection: 4 KiB request, 64 KiB response frame, 4 MiB response stream, one absolute read/write deadline, and stop timeout at most one hour. Status pids are exactly `{"shim":PID,"vmm":PID_OR_NULL,"sidecars":{}}`. | unbounded `read_line`; socket permissions alone; multi-request connections; pid-only status | Slow, oversized, malformed, extra, and disconnected clients cannot monopolize the supervisor or couple VM life to a terminal. The fixed pids shape leaves M3 sidecars representable without claiming that M1 launched any. |
| M1 process and recovery identity | `Cmd` owns non-waiting process spawn: the hidden shim calls safe `setsid` before serving, every VMM is a separate process-group leader, stdin is null, stdout/stderr append to a Paths-owned log, and environments are explicit/reduced. Linux records uid, pid, pgid, executable, and `/proc/<pid>/stat` start ticks, revalidates all fields before every recovery signal, acts through the API first, and refuses a reused identity. The shim is a Linux child subreaper and reaps its direct child/adopted descendants. | raw `Command`; pid files; signal a stored numeric pid; one shared process group | A live `Child` is authoritative during normal supervision. The stronger runtime identity makes crash recovery reject unrelated reused pids. Linux sockets are created atomically `SOCK_NONBLOCK|SOCK_CLOEXEC`; platforms without those creation flags set both before the descriptor can reach spawning code. |
| M1 exact launch bytes and scope | `prepare_start` composes `ImageStore::prepare_machine_image`, deterministic `publish_seed`, and canonical `publish_vm_config`; the runtime plan pins hashes and lengths. After bounded v53 readiness and ping-pid equality, `VmmApi::vm_create` sends the exact persisted `vmconfig.json` bytes without parsing or reserialization, then `vm.boot` transitions state to running. M1 requires `network.mode = none`, no forwards, and no mounts; passt, tap, virtiofsd, SSH readiness, and user/network cloud-init remain rejected rather than silently omitted. | serialize a second API body; start M3 sidecars partially; claim SSH readiness in M1 | One canonical byte string is both audit artifact and API authority. Refusing deferred devices/data prevents a successful M1 launch from claiming M2/M3 behavior that is not supervised yet. |
| [verify 6] Cloud Hypervisor v53.0 lifecycle evidence | `vm.power-button` injects the guest notification; with `no_shutdown = false`, guest exit invokes VMM shutdown and breaks the VMM loop; Firestone waits for child exit, retains `vmm.shutdown` if a stopped VM leaves the process alive, then verified TERM/KILL fallback | treat power-button 204 as completed shutdown; trust stored status; omit fallback and reap | Pinned commit `9ed824d6d08df3e96f7d5f50795d9449ac99f431` defines power-button handling in [`api/mod.rs`](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/9ed824d6d08df3e96f7d5f50795d9449ac99f431/vmm/src/api/mod.rs#L1409-L1427) and guest-exit shutdown in [`vmm/src/lib.rs`](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/9ed824d6d08df3e96f7d5f50795d9449ac99f431/vmm/src/lib.rs#L2095-L2104). M1-06 observed `reboot: Power down`, VMM exit code 0, `last_exit.reason = "guest shutdown"`, and a 30,294 ms stop without escalation. |
| M1 Cloud Hypervisor log truncation and console history | v53 opens `--log-file` with truncating `File::create` ([`main.rs` 569-574](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/9ed824d6d08df3e96f7d5f50795d9449ac99f431/cloud-hypervisor/src/main.rs#L569-L574)) and opens `serial.mode = File` the same way during `vm.create` ([`console_devices.rs` 229-256](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/9ed824d6d08df3e96f7d5f50795d9449ac99f431/vmm/src/console_devices.rs#L229-L256)). M1 keeps the approved serial-file VmConfig mapping: before create it safely rotates prior `console.log`, and after every stop/failure (or at the next recovery) atomically merges prior bytes before current-boot bytes and restores mode 0600. Shim logs append across runs; VMM logs are per run. | assume v53 appends; change the approved mapping to `serial.mode = Tty`; keep only the latest boot | The pinned source disproves append behavior. Rotation plus atomic recovery preserves deterministic cross-boot console history without changing the canonical VmConfig contract or buffering an unbounded log in memory. |
| M1 custom VMM import and wrapper identity | Open a validated custom executable once with no-follow/nonblocking flags, stream and hash at most 256 MiB into an atomically published machine-owned mode-0700 `vmm.bin`, and execute only that copy. Preserve scripts and wrappers: persist the actual post-`exec` executable path/device/inode, uid, start token, pgid, exact current argv, exact planned argv, staged artifact hash, and a launch-binding environment value. | Execute the mutable original after hashing; reject every non-ELF file; require non-renameable external ancestry without copying. | One owned descriptor closes final-node swaps, FIFOs, mid-read mutation, and hash/exec races. Rejecting wrappers would narrow §7.2; copying can change `$ORIGIN`/sibling lookup, so the staging behavior is explicit rather than silent. The actual interpreter/binary plus staged binding lets recovery support shebang and exec-style wrappers without trusting argv0 alone. |
| M1 owner guard, descendant cleanup, and recovery | Arm an owned VMM guard immediately after spawn; observe exit with non-reaping `waitid`; signal the pinned group only while its leader is unreaped; enumerate every Linux task-thread child, prove parent/start/uid after opening pidfds, signal escaped descendants through pidfds, and repeatedly drain adopted children to `ECHILD`. A process-local positive reap proof distinguishes safe launch rollback from retained recovery evidence. `recover_shim` deliberately starts a replacement supervisor, which adopts exactly one live candidate using API pid, full process identity, launch argv/artifact/hash/binding, or fails closed; unsupervised stop uses the same evidence. | reap then `killpg`; one `WNOHANG` sweep; numeric descendant pids; relaunch blindly after shim death; clear runtime on every error. | Reaping releases pid/pgid identity, task children can escape a process group or originate on nonleader threads, and pid numbers can be reused between enumeration and signalling. Positive exit and drain proof is required before state clears pid/runtime evidence. |
| M1 lifecycle deadlines, liveness, and control detachment | Launch and stop use shared client/server absolute deadlines with per-phase caps and reserved cleanup/response budgets; timeout zero remains bounded. Only socket absence/refusal proves negative VMM liveness; timeout, malformed protocol, reset, and unexpected status are ambiguous and preserve state. Every control response write failure is connection-local and logged once without request data; the lifetime machine lock remains held through the final response and shim return. Peer authentication is enabled only for audited Linux/Android `SO_PEERCRED` and BSD/macOS `getpeereid` targets; every other Unix target fails closed. | reset a timeout per phase; treat ping timeout as stale; let terminal disconnect abort supervision; release the lock before replying; trust socket mode on unknown Unix targets. | A hung or slow VMM is not absent, cumulative phases must not exceed the caller contract, and a client terminal cannot own VM lifetime. Keeping the lock through the response prevents a second lifecycle action from observing a partially finalized machine. |
| M1 executable and log descriptor hardening | Hash actual bytes with limit-plus-one and before/after descriptor identity checks. Command logs and VMM failure diagnostics open with no-follow, nonblocking, and close-on-exec behavior, then require a current-uid regular mode-0600 descriptor before clearing nonblocking; diagnostic failure falls back to the primary lifecycle error and never aborts cleanup. | trust pre-open length; follow final symlinks; pathname stat then blocking open; let log-tail errors replace the launch/exit result. | The final node can change after a pathname check and a FIFO can block forever. Diagnostics are secondary and cannot decide whether durable state/runtime cleanup runs. |
| M1 release acceptance platform | Linux x86_64 with the pinned Cloud Hypervisor v53.0 binary and Ubuntu 24.04 x86_64 edk2 boot path. Custom wrappers are supported when they `exec` the VMM and remain within the normal supervisor contract. | require aarch64 runtime parity, non-Linux shim authentication/recovery, or containment of adversarial wrapper forks/`setsid` helpers before M1 merge | The current release gate is the specified Linux boot/stop product path. aarch64 runtime validation, non-Linux authority backends, and hostile-wrapper containment are separate follow-on work and are not inferred from portable compilation or unit coverage. |
| M1 lifecycle and image action projection | `Remove` carries the complete ordered name list, `Show` carries `vmconfig`, `Logs` carries its typed source/line/follow request, and `ImageInspect` joins the shared action enum. Log bytes travel as `Output`; every successful invocation still ends in one `Result`. The standalone CLI polls the shared future with a safe thread-waker executor so blocking image transport is not nested inside a Tokio runtime. | loop over single-name actions and emit several results; implement logs or VmConfig directly in the adapter; retain a Tokio CLI runtime around `reqwest::blocking` | Multi-name `rm`, exact VmConfig output, logs, image inspection, CLI, and future REST adapters now invoke one dispatcher behavior. The standard-library executor avoids the blocking-client nested-runtime panic without a second action implementation. |
| M1 bounded log reads and follow | Accept 0 through 100,000 lines; reverse-scan at most 8 MiB; refuse a requested tail that crosses the bound; follow in no more than 256 KiB passes with a 100 ms sleep, safe rotation reopen, and `SIGINT` cancellation reported as `interrupted` | read the whole append log; silently truncate a long line; busy-poll; add platform-specific inotify behavior for M1 | The bounds cap memory and cancellation latency, keep output deterministic across supported Unix hosts, and preserve explicit failure instead of losing log bytes silently. |
| Pinned v53 console transport and first-boot autologin | Use `console.mode = "Pty"`; M2's shim brokers the PTY through `console.sock`. After writing the hvc0 getty override, cloud-init enables and restarts the unit on the first boot. | `console.mode = "Socket"`; require a second boot before the override applies; tee the serial file | Pinned v53 returns `NoSocketOptionSupportForConsoleDevice` for the virtio-console socket branch ([`console_devices.rs` 223-225](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/9ed824d6d08df3e96f7d5f50795d9449ac99f431/vmm/src/console_devices.rs#L223-L225)). The original `enable --now` did not restart Ubuntu's already-running hvc0 getty. The corrected first boot produced PTY autologin and preserved serial logging. |
| M1-06 Linux KVM acceptance evidence | Run the gated `scripts/m1-kvm-e2e.py` with `FIRESTONE_E2E=1`, a new mode-0700 `FIRESTONE_HOME`, and the exact pinned artifacts; keep `network.mode=none` | fake VMM claims; an ungated shared home; network, SSH readiness, passt, mounts, or aarch64 expansion | The recorded command was `FIRESTONE_E2E=1 FIRESTONE_HOME=/tmp/firestone-m1-577116f FIRESTONE_BIN=/tmp/firestone-25f7119 FIRESTONE_E2E_KEEP=1 FIRESTONE_E2E_EVIDENCE=$HOME/m1-evidence-577116f.json scripts/m1-kvm-e2e.py`. Run `577116f86ef6c61a302a5fabccf775ae267ee6be` on Linux 6.17.0-1022-azure x86_64 with read/write `/dev/kvm` passed E2E 1, 5, 6, and 7. CH SHA-256 was `448af3d4e59b22c2987f7df94c213ad40fb53a10d437e42b5ee6c4fce7c29ecc`; edk2 was `9fb511fc0dd423d90a79615a90a8ace9b9e078b4a115ea2c459e0ac2f4e60218`; Ubuntu source was `d0fe84bb5f80853425fa6be28e2c106f30104c3cfe8611933f2e65c9b63f0e30`. VMM SIGKILL reached `failed` in 140.309 ms and restarted to login. Shim SIGKILL reached `running (unsupervised)` in 7.099 ms and stopped with guest-shutdown reason. The evidence JSON SHA-256 is `a91a27d5921858e827515872e8f2d7ae53fff5056b5e0902b2efcf83ca3fe1d3`. No cloud-init content or secret was captured. |
| M2 SSH identity publication and machine host trust | Resolve all paths through `Paths`; serialize first use with a current-user mode-0600 `<data>/.ssh-identity.lock`; create `<data>/ssh` as mode 0700; mark an unpublished generation with mode-0600 `.generating`; run exactly `ssh-keygen -t ed25519 -N "" -C firestone@<gethostname> -f <data>/ssh/id_ed25519` through `Cmd`; require a mode-0600 private key and mode-0644 public key; fsync both and the directory before removing the marker. Recover only marked non-directory partial nodes created after an empty-directory preflight; never overwrite an unmarked incomplete pair. Validate per-machine `known_hosts` as a current-user protected regular file, preserve it when `instance-id` is unchanged, and unlink plus fsync before durably recording a changed id; `rm` continues to remove the whole machine directory. | generate at a temporary `-f` path and rename two files; publish with hard links; let concurrent callers race; repair or overwrite arbitrary incomplete keys; delete `known_hosts` on every byte-identical seed build | The exact normative argv names the final private-key path, while the lock and marker prevent any Firestone consumer from observing a partial pair and make interrupted first use recoverable. Standard OpenSSH modes protect private material without making the public half unnecessarily private. Instance-id comparison rotates trust exactly when cloud-init regenerates guest host keys, and retaining the old durable id makes a crash between seed publication and trust deletion self-healing on retry. |
| M2 bounded v53 vsock proxy | Validate the Paths-resolved mode-0700 machine runtime directory and a current-user, non-symlink Unix `vsock.sock` with no group/world write; use one nonblocking connect/handshake deadline of 5 s and a 64-byte response-line cap; send exact `CONNECT <nonzero-u32>\n`; accept only `OK <nonzero-u32>\n`; escape malformed bytes in deterministic errors. After acknowledgement, relay raw bytes in both directions with blocking worker copies, propagate stdin EOF as a socket write half-close, finish on socket/stdout closure, treat broken pipes as closure, and leave default process signals authoritative. The hidden command emits neither events nor a terminal `Result`. | unbounded `read_line`; buffered reads that can consume payload bytes after the acknowledgement; joined copies that hang forever on stdin; poll and mutate inherited stdio file flags; frame relay bytes as events | Exact one-byte handshake reads cannot steal payload bytes, while one absolute deadline bounds connect, partial frames, and guest acceptance. Independent blocking copies provide kernel backpressure without busy loops; returning when the download direction closes lets the short-lived proxy process cancel a stdin-blocked worker safely and preserves binary stdout byte-for-byte. |
| M2 guest SSH rendering and activation | Carry a validated catalog `sshd_path` into image metadata and the deterministic seed; give both root and the image default user the Firestone/user keys; make root key-only; preserve `/run/sshd` for per-connection OpenSSH; prefer systemd-256+'s generated socket by an inverse generator-path condition; require one socket active after daemon reload | hard-code `/usr/sbin/sshd`; let both sockets race; mask/disable the native unit; prefix `sshd -i` with `-`; ignore a failed activation | The Ubuntu KVM run exposed a real `/run/sshd` failure that the prior failure-prefixed `ExecStart` reported as success. `RuntimeDirectory=sshd`, `RuntimeDirectoryPreserve=yes`, and an unprefixed `ExecStart` made root/default SSH succeed while preserving failures. Typed path/key/user validation and exact multipart/seed goldens keep dynamic content deterministic and non-secret. |
| [verify 11] systemd-257 native-vsock coexistence | When `/run/systemd/generator/sshd-vsock.socket` exists, native `sshd-vsock.socket` owns `vsock::22` and `firestone-sshd.socket` is condition-skipped with a successful result | infer from systemd source; accept a bind race; claim Debian's unverified catalog firmware | On Linux 6.17.0-1022-azure x86_64 with pinned Cloud Hypervisor v53 + edk2, exact host commands `FIRESTONE_HOME=/tmp/firestone-m2-guest-20260829 /tmp/firestone-m2-guest-20260829/harness-bin/firestone --json create m2-native-edk2 debian:13 --net none --vmm-firmware edk2` and then `... --json start m2-native-edk2 --no-wait --timeout 600s` returned `running`. In the guest, `systemctl --version` began `systemd 257 (257.13-1~deb13u1)`; `test -e /run/systemd/generator/sshd-vsock.socket` succeeded; `systemctl show -p LoadState -p ActiveState -p SubState -p Result -p ConditionResult sshd-vsock.socket firestone-sshd.socket` returned native `active/listening, Result=success, ConditionResult=yes` and Firestone `inactive/dead, Result=success, ConditionResult=no`; `systemctl list-sockets --all --no-pager` listed `vsock::22 sshd-vsock.socket`; `systemd-analyze verify /run/systemd/generator/sshd-vsock.socket /usr/lib/systemd/system/sshd@.service /etc/systemd/system/firestone-sshd.socket /etc/systemd/system/firestone-sshd@.service /usr/lib/systemd/system/serial-getty@.service` exited 0. OpenSSH over the pinned CH `CONNECT 22` transport ran `id -un` as both `root` and `debian`. This resolves coexistence only; the explicit edk2 override does not resolve catalog verify 3. |
| [verify 17] Ubuntu 24.04.4 guest SSH and hvc0 units | Use the Firestone socket on the accepted Ubuntu systemd-255 image; keep first-boot hvc0 enable plus restart | infer unit support from package files; count an active socket without completing SSH; require a second boot for hvc0 | Exact host commands `FIRESTONE_HOME=/tmp/firestone-m2-guest-20260829 /tmp/firestone-m2-guest-20260829/harness-bin/firestone --json create m2-guest ubuntu:24.04 --net none` and then `... --json start m2-guest --no-wait --timeout 600s` returned `running`; the hardened seed changed the instance from `iid-m2-guest-4aa5984ea771` to `iid-m2-guest-45e487e61f18` and cloud-init reran. Guest `systemctl --version` began `systemd 255 (255.4-1ubuntu8.17)`; `cloud-init status --long` returned `status: done`, `DataSourceNoCloud [seed=/dev/vdb]`, and empty errors; the native generator path was absent; `systemctl show -p LoadState -p ActiveState -p SubState -p Result firestone-sshd.socket sshd-vsock.socket serial-getty@hvc0.service` returned Firestone `active/listening`, native `not-found`, and hvc0 `active/running`; `systemctl list-sockets --all --no-pager` and `ss -ln --vsock` showed `vsock::22` and `*:22`; `/usr/sbin/sshd -T` returned `permitrootlogin without-password` and `passwordauthentication no`; both authorized-key files existed with one line; SSH `id -un` returned `root` and `ubuntu`; the live hvc0 PTY returned `hvc0_login=root`. `systemd-analyze verify firestone-sshd.socket firestone-sshd@.service serial-getty@hvc0.service` exited 0. |
| M2 readiness ownership | Keep readiness in shared Action::Start and its ordered Event stream; preserve running as the only successful persisted status; terminal shell, console, and run orchestration use typed core plans/results while the CLI alone owns exec and raw-terminal control | add a persisted ready status; add terminal attachment Action variants; implement readiness only in the CLI parser | Readiness changes when Start may return, not machine liveness. Keeping terminal byte streams outside Dispatcher preserves the section 5.2 action boundary while shared plans prevent SSH and path-policy drift. |
| M2 console broker framing and exclusivity | A current-user mode-0600 console.sock sends one bounded server-first OK or BUSY acknowledgement, then carries unframed binary PTY bytes; one client attaches at a time, PTY output is staged privately while Cloud Hypervisor owns console.log and merged after VMM exit, and detach leaves the shim broker alive for reattach | expose the PTY pathname directly; queue concurrent interactive clients; frame every terminal packet; let two writers race on console.log | The private acknowledgement makes concurrent attach failure deterministic without contaminating guest bytes. One lifetime-owned broker keeps logging independent of CLI lifetime, while post-exit merging preserves both serial and virtio-console history without concurrent file-offset races. |
| Structured output for terminal byte streams | Reject --json for run, shell, and console before starting a terminal stream; ssh-config remains a single serializable Result | interleave NDJSON and remote or PTY bytes; silently ignore --json | Arbitrary SSH and console bytes are not NDJSON and can contain any byte sequence. Failing before attachment keeps stdout machine-readable and leaves ssh-config available to structured callers. |
| OpenSSH ProxyCommand execution | Prefix the path-preserving environment assignments with POSIX `env` before the owned `firestone _vsock-proxy` argv | bare leading assignments; an extra shell wrapper; discard the captured config/data/runtime roots | OpenSSH executes ProxyCommand as `exec <text>`. Bare `NAME=value` words after that `exec` are treated as a pathname, which the M2 KVM run reproduced as `$HOME/FIRESTONE_CONFIG_DIR=…: No such file or directory`. `exec env NAME=value …` preserves the exact roots and works under real OpenSSH; the regression executes the rendered command behind the same `exec` prefix. |
| Streaming vsock ProxyCommand output | Copy socket-to-stdout in fixed 64 KiB blocks and flush each completed block | `io::copy` plus one flush at EOF; raw descriptor writes | Real SSH reached `SSH2_MSG_NEWKEYS` and then stalled because the next binary packet contained no newline and remained in Rust's stdout buffer. Per-block flush preserves bounded allocation and duplex progress. A subprocess regression requires a server-first packet without a newline to reach stdout before socket EOF. |
| First-boot start deadline | Default `[start].timeout_first_boot` to 300 s; keep warm starts at 60 s and explicit overrides authoritative | 180 s; 600 s; define M3 network configuration early | The accepted no-network Ubuntu boot spends roughly two minutes behind guest network-online ordering before the cloud-init runcmd activates vsock SSH. Actual clean starts reached SSH at 150.1 s and 178.0 s, while one 180 s run timed out. 300 s is bounded and leaves margin without pulling M3 network configuration into M2. |
| [verify 13] pinned-v53 PTY attach lifecycle | Keep one shim-owned PTY broker across client lifetimes; Ctrl-] detaches and a later client can attach; terminal state is restored on detach and interruption | expose the Cloud Hypervisor PTY directly; make console lifetime equal client lifetime | On Linux 6.17.0-1022-azure x86_64 with Cloud Hypervisor v53.0, the first and second `firestone console ubuntu` attachments each reached the real `root@ubuntu` hvc0 shell, executed distinct markers, detached with Ctrl-], and exited 0. A third attachment interrupted with SIGTERM exited 130. The slave termios matched its complete pre-attach value after all three paths. While the VMM ran, the 88,928-byte serial `console.log` stayed byte-stable, PTY bytes remained in a mode-0600 staging log, and no concurrent writer corrupted the serial file. |
| M2-04 Linux KVM evidence set | Use the gated `scripts/m2-kvm-e2e.py` from a new mode-0700 home, plus one focused invocation for the corrected launch-observation bound | fake VMM evidence; a shared home; unbounded manual SSH; M3 networking | On Linux 6.17.0-1022-azure x86_64 with read/write `/dev/kvm`, pinned CH v53, edk2, and the accepted Ubuntu source SHA-256 `d0fe84bb5f80853425fa6be28e2c106f30104c3cfe8611933f2e65c9b63f0e30`, command `timeout --signal=TERM --kill-after=30s 45m env FIRESTONE_E2E=1 FIRESTONE_HOME=/tmp/firestone-m2-04-accepted5-20260829 FIRESTONE_BIN=/tmp/firestone-target-m2-cli-stable/debug/firestone FIRESTONE_E2E_EVIDENCE=$HOME/m2-04-accepted5-20260829-evidence.json scripts/m2-kvm-e2e.py` started from an empty home. Cold E2E 2 took 184,677.186 ms; the warm run took 379.181 ms; the PTY `run ubuntu` prompt/command/exit took 469.192 ms. Root and `ubuntu` users, argv order, exit 37, guest SIGTERM→OpenSSH 255, verify 11/17 Ubuntu facts, verify 13 interactions, and every E2E 10 framing/control assertion passed. The run then stopped on the harness's old 20 s pre-publication observation bound after all preceding scenarios; cleanup removed both machines and the home. After widening only that harness bound to 120 s, focused actual-KVM evidence observed `starting` at 9,218.314 ms then SIGINT→created/130/runtime removed, and `running` at 9,476.195 ms then SIGINT→running/130. Both evidence files are mode 0600: partial full-run SHA-256 `95ae1557db8a418de42e0dbbb777b0927c66868d4532593a955cbd36bd7477cd`; focused supplement SHA-256 `b215e01505ee4fa288b9551c82e058a83f0e2b21f0f66a799ad0e17b724cf58c`. |
| M2-04 Rust gates | Run the complete stable and Rust 1.85 gates locally and on the accepted Linux host | one toolchain; targeted tests only | Local commands `rustup run {stable,1.85.0} cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` passed. Wall times were stable 0.287/19.614/66.363 s and 1.85 0.345/13.413/70.533 s. Linux ran the same commands through `rustup run` with `CARGO_TARGET_DIR=/tmp/firestone-target-m2-cli-{stable,185-final}`; wall times were stable 3.740/9.071/46.413 s and 1.85 3.603/37.641/92.303 s. Linux's full suite passed 411 tests; local passed 405 portable tests, with zero failures on both toolchains. GitHub Actions run [33231170618](https://github.com/0xchasercat/firestone/actions/runs/33231170618) ran `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test`; its Rust checks job passed in 92 s. |
| M3 cloud-init input reads | Keep the public `user_data`, `network_config`, and `ssh_keys` forms path-only. Resolve relative and `~` paths through existing `MachineSpec` provenance and `Paths`; render from one internal owned byte input after one nonblocking descriptor read and regular-file check. Limit user-data and network-config to 1 MiB each, each public-key file to 64 KiB, and all public-key files to 256 KiB. User files may end in a regular-file symlink. The owned Firestone public key is no-follow, current-user mode 0644, and limited to 16 KiB. | add an inline config form; reopen after path validation; canonicalize complete user paths; reject user symlinks; read without a bound | The public model and `--user-data FILE` stay aligned with §7 and §15. One descriptor preserves the kernel's path resolution while avoiding FIFO blocking and final-node reopen races. Bounds leave deterministic room in the 4 MiB CIDATA image. Private-key bytes never enter this path. |
| M3 multipart and key layering | Use fixed boundary `===============firestone==`, CRLF MIME framing, and one delimiter-owning CRLF after each unchanged UTF-8 body. Put the user part first and Firestone's cloud-config second. Keep the exact `list(append)+dict(recurse_dict,recurse_list,no_replace)+str()` directive. Traverse user key files and lines in order and de-duplicate by parsed key material while preserving the first text; Firestone's identity key remains first. | random or content-derived boundaries; normalize source newlines; put Firestone first; replace lists; retain duplicate key material | Byte-exact input survives MIME parsing even without a final newline. The later Firestone part controls merge policy without replacing user scalars, while lists append in user-then-Firestone order. Stable de-duplication avoids changing identity for an ineffective duplicate key. |
| M3 seed instance identity | Preserve `SHA-256(user-data)` when `network-config` is absent. When present, hash the domain `b"firestone-instance-v1"`, one zero byte, big-endian 64-bit user-data length and bytes, then big-endian 64-bit network-config length and bytes; use the first 12 lowercase hex digits. | ignore network-config; concatenate two unframed inputs; hash source paths or the FAT image; change every M1 identity | Existing no-network identities keep their meaning. Domain and length framing distinguish absence, empty content, and every pair of input byte strings. Equal effective bytes remain stable across source path, CPU, memory, duplicate-key, and host-only mount changes. |
| [verify 10] cloud-init multipart merge | Keep the specified user-first order and exact Firestone `merge_how`; both parts pass the target schema | rely on documentation or a reimplemented merger; reverse the parts; move policy to an unverified MIME header | On 2026-08-29 the exact catalog URL's `noble-server-cloudimg-amd64.manifest` named `cloud-init 26.1-0ubuntu1~24.04.1`; the Linux validation host reported the same `/usr/bin/cloud-init` version. `cloud-init schema -c` returned `Valid schema` for both decoded parts of SHA-256 fixture `331c6c041fc07bf15ed152b63b057cf344b3b9b8baa8209b7077c943b4cc6dc6`. That package's `CloudConfigPartHandler` processed the exact MIME order and returned user `hostname`/`disable_root`, with appended lengths `ssh_authorized_keys=3`, `write_files=4`, `mounts=3`, and `runcmd=6`. |
| M3-03 Rust gates | Run the complete stable and Rust 1.85.0 gates locally and on the accepted Linux x86_64 host | one toolchain; targeted cloud-init tests only | Local and Linux commands `rustup run {stable,1.85.0} cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` all passed. The first Linux 1.85 clippy attempt exhausted `/tmp` after the stable target completed; removing only this run's stable and partial 1.85 target directories, then rerunning from a fresh 1.85 target, passed clippy and the full suite. GitHub Actions runs [33233385525](https://github.com/0xchasercat/firestone/actions/runs/33233385525) and [33233387001](https://github.com/0xchasercat/firestone/actions/runs/33233387001) each passed the Rust checks job in 101 seconds on the same tested head. No KVM result is claimed by M3-03. |
| [verify 15] passt `2025_02_17.a1e48a0` forwarding grammar | Emit repeated `-t`/`-u` values as `[address/]host[-end]:guest[-end]`; bracket IPv6; require equal-length ranges; allow ports 1 through 65535; reject same-protocol host-range overlap; put `--repair-path none` last | infer from Firestone syntax; colon-delimit the bind; aggregate conflicting mappings; expand ranges into thousands of argv entries; place repair-path among ordinary options | Pinned commit `a1e48a02ff3550eb7875a7df6726086e9b3a1213` documents address and port separation by `/`, translation after `:`, ranges by `-`, repeatable protocol options, and examples in `passt.1` lines 458-535. Its `conf.c` lines 212-247 parse the address prefix, lines 321-347 require equal spans and store one delta per host port, and lines 1869-1932 end the second option pass at repair-path. Exact mapping and boundary tests execute a fake pinned passt argv contract. |
| M3 passt process and readiness plan | Build a typed plan only; use `Cmd` with foreground, one-off, vhost-user, Paths-owned `net.sock` and `passt.log`, TCP then UDP mappings, final `--repair-path none`, reduced environment, null stdin, `/` cwd, and both process streams appended to the protected log; wait on no-follow socket metadata with a bounded 10 s deadline, 10 ms poll, and abort-launch cancellation | spawn during preparation; connect-probe the socket; accept passt's default migration repair socket; inherit the full environment; let an overall launch deadline extend sidecar readiness | Connecting would consume the only vhost-user client before Cloud Hypervisor. The pinned release otherwise creates `<net.sock>.repair`, but migration is outside v0.1. The shim's mode-0077 umask yields a current-user mode-0700 passt socket; passt creates its log mode 0600. Plans reject stale nodes, unsafe modes, symlinks, insecure ancestry, overlong Unix paths, and invalid timing before M3-04 performs spawning. |
| M3 network plan and Cloud Hypervisor boundary | `none` emits no sidecar and no `net`; `passt` maps to one v53 vhost-user client entry; `tap` validates a Linux interface name, existing TAP sysfs type, and `/dev/net/tun` access, records the existing-user-owned assumption, emits no sidecar, and maps MAC with `ip`/`mask` absent | create/configure TAP or bridge; add NAT/firewall rules; silently ignore forwards outside passt; map directly from unvalidated spec | Firestone never escalates privileges or configures host networking. Typed plans let M3-04 compose lifecycle work without rebuilding argv or device JSON. Forwards outside passt and duplicate/conflicting same-protocol host ranges fail with stable hints. Verify 8 and verify 14 remain runtime gates. |
| M3-01 pinned passt Linux flag evidence | Build commit `a1e48a02ff3550eb7875a7df6726086e9b3a1213` as version `2025_02_17.a1e48a0`; require the six visible help tokens; run the TCP/UDP parse-only probe; launch the exact planned option order with IPv4 bind, translated ranges, UDP, and repair disabled | rely only on source; test the host's older distro binary; count a socket bind as verify 14 | On the Linux x86_64 validation host, the built binary accepted the grammar probe and planned argv, created current-user `net.sock` mode 0700 and `passt.log` mode 0600, and created no repair socket. The host has no usable IPv6 route, but a bracketed IPv6 mapping reached bind rather than syntax rejection. Passt then exited 1 because that host denies its namespace detach, so this is flag/path/mode evidence only; verify 14 remains open for M3-05. Evidence directory: `/tmp/firestone-passt-20250217-sjLH7X`. |
| M3-01 Rust gates | Run `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`, and the full test suite under stable and Rust 1.85.0 locally and on Linux x86_64 | one toolchain; targeted network tests only; infer Linux from portable compilation | Both local toolchains passed 84, 4, 1, 12, 9, and 414-test binaries. Both Linux toolchains passed 84, 4, 1, 17, 9, and 420-test binaries. GitHub Actions runs [33233817977](https://github.com/0xchasercat/firestone/actions/runs/33233817977) and [33233816132](https://github.com/0xchasercat/firestone/actions/runs/33233816132) passed their Rust checks. No KVM scenario belongs to the pure M3-01 plan row; M3-04 owns spawning and M3-05 owns verify 8 and 14 runtime gates. |

| M3 virtio-fs mount boundary | At most 16 mounts; generated tags are `share<i>`; every effective tag is unique, 1 through 36 ASCII bytes, and limited to letters, digits, dot, underscore, and hyphen. Host sources are absolute canonical UTF-8 current-user directories with no symlink or alias components, no group/world write, and protected current-user/root ancestry with root sticky directories allowed. Canonical host sources and absolute non-root guest paths must be pairwise disjoint. Linux path limits are 4,095 bytes total and 255 bytes per component. | trust virtiofsd or Cloud Hypervisor to reject late; allow aliases, nested devices, arbitrary tags, or unbounded sidecars; recursively create guest state on the host | One declaration now has one stable tag, one sidecar, one vhost-user socket, and one guest mount. Canonical disjoint paths prevent duplicate or order-dependent mounts. Protected ancestry prevents another uid from redirecting the source between validation and launch. The count bounds processes and stays below the default v53 PCI-device budget after Firestone's required devices. Firestone never creates the guest path on the host. |
| M3 pinned virtiofsd v1.14.0 plan and source evidence | Exact argv is `--socket-path PATH --shared-dir HOST --sandbox namespace|none --cache auto --announce-submounts [--readonly] --log-level warn`; do not pass `--tag`, mmap, writeback, xattr, thread-pool, or migration options. Use the pinned mode-0755 Paths binary, cwd `/`, null stdin, an environment limited to PATH, HOME, XDG_CONFIG_HOME, XDG_DATA_HOME, XDG_RUNTIME_DIR, and FIRESTONE_* variables, and one Paths-owned append log. Require absent socket and `.pid` nodes before launch, then metadata-only readiness for a current-user mode-0700 socket plus mode-0600 pid file with a 10 s timeout, 10 ms poll, and abort-launch cancellation. | inherit the shim environment; connect to probe readiness; let virtiofsd unlink existing nodes; rely on defaults or add tuning flags | The Firestone x86_64 asset SHA-256 `9ad3e33c45dd816b24ad483b60ca469974ba54c3b37ef93be3da2a623986646f` printed `--sandbox <SANDBOX>` with default `namespace`, `--readonly`, `--cache`, `--announce-submounts`, and stderr logging in `--help`. Pinned source commit `c2540f8db14caba81c1e37fba23fc7bf2cd7f0dd` defines those clap flags in [`main.rs`](https://gitlab.com/virtio-fs/virtiofsd/-/blob/c2540f8db14caba81c1e37fba23fc7bf2cd7f0dd/src/main.rs#L124-244), enters a user namespace for non-root callers in [`sandbox.rs`](https://gitlab.com/virtio-fs/virtiofsd/-/blob/c2540f8db14caba81c1e37fba23fc7bf2cd7f0dd/src/sandbox.rs#L418-528), and selects `PassthroughFsRo` for `--readonly` in [`main.rs`](https://gitlab.com/virtio-fs/virtiofsd/-/blob/c2540f8db14caba81c1e37fba23fc7bf2cd7f0dd/src/main.rs#L887-899). This is source/help evidence only. Verify 16 remains open for the M3-05 guest read/write and namespace runtime checks. |
| M3 Cloud Hypervisor v53 virtio-fs mapping | Map each validated plan to exactly `{tag, socket, num_queues: 1, queue_size: 1024}` in declaration order. Read-only is enforced by virtiofsd and the guest mount, not an `FsConfig` field. Do not emit DAX or optional PCI/id fields. | serialize extra tuning defaults; infer DAX; map directly from unchecked `MountSpec` values | The exact v53 OpenAPI [`FsConfig`](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/9ed824d6d08df3e96f7d5f50795d9449ac99f431/vmm/src/api/openapi/cloud-hypervisor.yaml#L1159-L1182) requires tag, socket, num_queues, and queue_size, defaults queues to 1/1024, and has no DAX or read-only property. Keeping the typed base to those four keys makes config-overlay additions explicit instead of silently changing the product mapping. |
| M3-02 no-KVM gate evidence | Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` with both stable Rust and 1.85.0 locally and on Linux x86_64 at code commit `4749771`. Use clean Linux target directories with incremental compilation disabled. | one toolchain; targeted tests only; infer Linux from macOS | Both local toolchains passed 421 portable core tests plus every CLI integration test. Both Linux toolchains passed 427 core tests plus every CLI, shim, and vsock-proxy integration test. These are no-KVM plan/validation gates; verify 16's guest read/write and rootless namespace runtime checks remain M3-05 work. |
| M3 exact prepared-plan authority | Under the machine lock, call `prepare_network` once and `prepare_virtiofs_plans_with_readiness` once; derive the persisted launch plan, `VmConfig`, effective forward/mount result payloads, and shim sidecar commands from those same typed plans. The private deny-unknown-fields launch plan snapshots the selected executable, exact argv, readiness artifacts, tap mapping, and sandbox choice; it never remaps devices from `MachineSpec`. Passt argv keeps final `--repair-path none`. | Re-run preparation in the shim; rebuild commands or `VmConfig` from the spec; connect-probe a one-off vhost-user socket; let displayed results diverge from launched devices. | A start has one validated device truth across the CLI/shim boundary. Metadata-only readiness leaves each one-off socket for Cloud Hypervisor, while the durable exact plan prevents PATH, host-state, or mapping drift between preparation and spawn. |
| M3 sidecar supervision and recovery | Spawn passt first, then virtiofsd in declaration order, then the VMM; each sidecar is a direct child and process-group leader with reduced environment, protected append log, launch binding, staged executable hash, exact argv, uid, pid, pgid, start token, and actual executable identity. Roll back and reap every launched group on readiness failure, timeout, interruption, VMM launch failure, or VMM exit. A sidecar exit while the VMM lives leaves the machine `running` with exact degraded reason and log evidence. Stop the VMM before sidecars; VMM descendant scans exclude exact sidecar pid/start identities and reap only specific non-sidecar children. Linux crash recovery adopts or stops a sidecar only after exact full-identity matching and refuses missing, ambiguous, or reused candidates. | daemonize sidecars; store pid alone; stop helpers before the VMM; broad-wait across every shim child; mark the whole VM failed when one helper exits; scan by executable basename; clear unverifiable recovery evidence. | The shim remains the sole lifetime authority and never signals or reaps an unrelated or separately-owned process. Degraded state truthfully distinguishes a live guest with a failed device backend, and VMM-first teardown prevents a live client from racing disappearing vhost-user endpoints. |
| M3-04 Rust and Linux gates | Run complete stable and Rust 1.85.0 format, Clippy, and test gates locally, then require final-implementation-head Ubuntu 24.04 x86_64 GitHub Actions before review. Record an unreachable dedicated host as an infrastructure limit rather than inferring pinned-binary or KVM evidence. | one toolchain; targeted tests only; portable compilation as Linux proof; block the no-KVM integration row on a deallocated host; claim M3-05 runtime verification. | At implementation head `1ef4efd1feaa`, both local toolchains passed format, `cargo clippy --all-targets -- -D warnings`, and all 551 tests. GitHub Actions runs [33264028360](https://github.com/0xchasercat/firestone/actions/runs/33264028360) and [33264026699](https://github.com/0xchasercat/firestone/actions/runs/33264026699) each passed the Ubuntu 24.04 x86_64 Rust checks and all 562 Linux tests. On 2026-08-29, SSH to the dedicated Ubuntu x86_64 validation host timed out after 10 seconds and no alternate host or wake control was available; pinned passt/virtiofsd/Cloud Hypervisor KVM reruns and verify 7, 8, 14, and 16 remain M3-05. |
| M3-05 bounded KVM acceptance harness and infrastructure boundary | Use gated `scripts/m3-kvm-e2e.py` from a new current-user mode-0700 empty `FIRESTONE_HOME`, with every host command, boot, guest action, teardown, and cleanup absolutely bounded. The harness stages an optional `FIRESTONE_BIN`, verifies exact pins and installed hashes, uses default passt with a recorded free TCP/UDP host port, exercises HTTP and UDP forwarding while SSH stays on vsock, tests two namespace-sandboxed virtio-fs tags including read-only denial, recomputes the exact seed instance-id across changed and unchanged restarts, replaces stale host trust, inventories exact process identities/argv/capabilities/hashes, proves normal-stop and VMM-crash reaping, and creates/removes one user-owned TAP only through explicit host setup. An AppArmor user-namespace sysctl adjustment is permitted only with `FIRESTONE_E2E_ALLOW_USERNS_SETUP=1`; the harness records before/after state and restores it in cleanup. | unbounded manual commands; reuse a populated home; accept fake-VMM or portable tests as KVM evidence; silently fall back to `--sandbox none`; mutate host policy without opt-in/restoration; wait indefinitely for a deallocated host | Static harness SHA-256 is `21d6c465e0dbe989f8fc3daf199ed4a7738d61e36b64234919d66b3e05367a3b`. It pins CH v53.0 SHA-256 `448af3d4e59b22c2987f7df94c213ad40fb53a10d437e42b5ee6c4fce7c29ecc`, edk2 `ch-1e1b96f126` SHA-256 `9fb511fc0dd423d90a79615a90a8ace9b9e078b4a115ea2c459e0ac2f4e60218`, virtiofsd v1.14.0 commit `c2540f8db14caba81c1e37fba23fc7bf2cd7f0dd` SHA-256 `9ad3e33c45dd816b24ad483b60ca469974ba54c3b37ef93be3da2a623986646f`, and passt `2025_02_17.a1e48a0` commit `a1e48a02ff3550eb7875a7df6726086e9b3a1213`; passt's runtime binary SHA is deliberately required from the eventual host rather than invented. On 2026-08-30 the documented `firestone@172.203.242.136` endpoint timed out at TCP/22 three times: exact BatchMode/one-attempt SSH probes with `ConnectTimeout=8` took 8.05 s, then two probes with `ConnectTimeout=6` took 6.05 s and 6.06 s; all exited 255 with `Operation timed out`. Therefore no KVM process inventory or evidence JSON/SHA exists, E2E 3/4/8 and verify 7/8/14/16 remain open, and verify 10/15 are not reopened or re-claimed. Full local gates passed with `rustup run {stable,1.85.0} cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --locked` using separate non-incremental target directories; rebased-head total wall times were 138.65 s for stable and 137.61 s for 1.85.0, with the complete suite green (including 451 firestone-core tests). Rebased-head static proof was `python3 -m py_compile scripts/m3-kvm-e2e.py` (0.14 s) and the gated skip path (0.59 s); the unchanged harness's expected non-Linux failure/cleanup/mode-0600 evidence smoke had already passed in 0.19 s. |
| M4 REST adapter framing and cancellation | Route every operation through one shared Action and Dispatcher. Use a 16-event bounded channel; hold the sole Result until dispatch succeeds, then send it last. A disconnected mutation sink becomes a no-op while the action reaches its safe point; open-ended Logs observes sink cancellation. NDJSON sends one compact Event per line; Accept application/json sends {events, result} with the full Result event. Log Output becomes text/plain after the same control-byte sanitization as CLI output. | invoke LocalDispatcher methods from handlers; unbounded channels; cancel mutations with the client; synthesize REST-only results; send raw log controls | One action remains authoritative across CLI and REST. Holding Result prevents duplicate or non-terminal success records. Backpressure bounds memory, and transport loss cannot roll back a mutation after its safe point. Shared log sanitization keeps HTTP clients from receiving terminal controls. |
| M4 REST bounds, metadata, and HTTP stack | Pin axum 0.8.4, hyper 1.6.0, and hyperlocal 0.9.1 for Rust 1.85. JSON request bodies are limited to 1 MiB; timeout_s is 1 through 3,600; Logs defaults to console, false, and 200 lines with the shared 100,000-line ceiling. The version payload uses a dependency-name-to-version map and paths keys config, data, and runtime. Unsupported methods use the shared 404 route error. | floating HTTP versions; framework rejection bodies; unbounded requests and timeouts; dependency arrays with transport-only fields; framework 405 pages | Exact versions keep the server stack inside the project MSRV. Fixed limits prevent request-controlled memory or deadline growth. Stable shared errors avoid leaking parser internals or request bytes, and the compact version shape exposes only the contract named in section 16. |
| M4-01 Rust and Linux gates | Run the complete format, Clippy, and test gates under local stable and Rust 1.85.0, then require final-head GitHub Ubuntu x86_64 CI before review. | one toolchain; handler tests only; infer Linux from macOS | Both local toolchains passed format, `cargo clippy --all-targets -- -D warnings`, and all 569 tests across seven suites. GitHub Actions runs [33267875392](https://github.com/0xchasercat/firestone/actions/runs/33267875392) and [33267873910](https://github.com/0xchasercat/firestone/actions/runs/33267873910) passed the Ubuntu x86_64 Rust checks at review head `144877c`. M4-01 has no KVM-dependent path; socket binding and E2E 9 remain M4-02 and M4-03. |
| M4 Unix serve ownership and shutdown | Expose `serve [--listen unix:PATH]` only; resolve listener names under `Paths` and require the socket directly inside the current-user mode-0700 Firestone runtime directory. Register SIGINT/SIGTERM before publication; use umask 0077 for runtime creation and 0177 for listener publication; hold a persistent current-user mode-0600 `.serve.lock` flock; validate socket type, uid, mode, device, and inode before stale takeover or cleanup. Run the merged `api::router` with `LocalDispatcher` on Tokio's multi-thread runtime, accept connections concurrently, stop accepting on either signal, drain HTTP for 5 s, abort remaining follow streams, then wait bounded action workers to preserve mutation safe points. Signals are clean exit 0; `--yes` is usage-invalid before binding. | TCP or token flags; sockets outside the owned runtime directory; PID files; unlink-before-bind; current-thread or serial serving; unbounded graceful shutdown; canceling mutations with their clients | The flock makes simultaneous starts choose one owner and releases on crash without daemon state. Descriptor-relative checks reject hostile nodes and stored socket identity prevents normal cleanup from deleting a replacement. Forced connection drops close router receivers, so follow workers cancel while disconnected mutations retain the shared dispatcher safe-point behavior. A restarted stateless server reuses live machine shims. |
| M4-03 real-process REST equivalence and E2E 9 gate | Launch the built `firestone` CLI and `firestone serve`, send HTTP over its private Unix socket, and compare parsed terminal Result payload bytes and key order. Exact same-event adapter tests cover serialization. Real repeated lifecycle actions compare every byte except their independently measured `elapsed_ms`, which remains a checked `u64`; repeated doctor runs likewise isolate only the live free-space byte count. Keep E2E 9 in a separate opt-in Linux x86_64 KVM harness with atomic mode-0600 evidence, exact pin hashes, absolute bounds, and cleanup. | handler calls without a real process; router-only tests; fake KVM evidence; production clock hooks; leave the socket publication umask active for action workers | Three real-binary Unix-socket tests cover create/show/list/PUT/PATCH, lifecycle, running and 204 deletes, image pull/list/remove/prune, doctor, version, VmConfig, shared errors, delayed NDJSON, JSON aggregation, log text, CLI/API lock serialization, disconnect safe point, and serve restart with a live shim. They reproduced and fixed the missing CLI `version` command and a leaked 0177 listener umask that made REST-created seed directories mode 0600. Local rustc 1.98.0 and 1.85.0 format, Clippy, and all 585 tests pass. The first stable full-test attempt hit one concurrent doctor fixture failure; that exact test and a complete unchanged rerun passed. GitHub Actions runs [33273602481](https://github.com/0xchasercat/firestone/actions/runs/33273602481) and [33273573353](https://github.com/0xchasercat/firestone/actions/runs/33273573353) passed Ubuntu x86_64 Rust checks at implementation head `e56cea8`. The gated `scripts/m4-kvm-e2e.py` records E2E 9 without claiming a run. At 2026-08-29T19:38:22Z and 2026-08-29T19:38:38Z, `ssh -o BatchMode=yes -o ConnectTimeout=10 -o ConnectionAttempts=1 firestone@172.203.242.136 true` timed out on TCP/22 after 10.06 seconds with exit 255, so E2E 9 remains open. |
| M5-03 Linux x86_64 catalog boundary | Keep exactly Ubuntu 24.04 and 22.04, Debian 12 and 13, and current Fedora 44. Retain dated x86_64 and aarch64 locators so both build targets can parse and compile the catalog, but make doctor reject aarch64 runtime in the Linux x86_64 MVP. Select builds from Canonical's released-stream JSON, Debian's official genericcloud Build records, and Fedora releases.json; use matching dated checksum documents, edk2, explicit `/usr/sbin/sshd`, and the normative root user. Limit M5 network retrieval and E2E 11 to x86_64, and leave verify 3 open until that matrix reaches SSH for every row. | keep moving `current` and `latest` image URLs; remove aarch64 metadata and break portable no-KVM paths; treat compile-only metadata as runtime evidence; add more distributions; infer RHF or boot support from source availability | The user selected a Linux x86_64 MVP on pinned Cloud Hypervisor v53 and edk2 while allowing aarch64 compile-only verification. Dated sources and independent release metadata make each locator reproducible while checksum bytes remain the image identity. Package ownership checks confirm `/usr/sbin/sshd`; the root user comes from `MachineSpec`, not invented catalog metadata. Source HTTP 200, successful aarch64 catalog parsing, and matching digests are not boot claims, so aarch64 runtime and verify 3 remain explicitly deferred. |
| M5-03 fresh-host doctor matrix | Run the real Linux x86_64 binary as uid 10001 in current Ubuntu 24.04, Fedora 44, and Arch containers. Query each native package database and executable owner; use a root-owned group-kvm regular file only to test the permission hint, never as KVM; use a one-GiB tmpfs for the space warning; compare `unshare -U true` with doctor; trap privileged and package-manager commands during `--fix`; and require package state plus the KVM fixture to remain unchanged. Give Ubuntu 24.04 the exact pinned passt source build because its package is too old, while Fedora and Arch use their current package commands. | fixture-only unit tests; claim containers provide KVM; assume package names or user-namespace behavior; suggest Ubuntu's ineffective passt package; let `doctor --fix` run displayed privileged fixes | The local matrix observed Ubuntu passt `2024_02_20`, Fedora's RPM version spelling `0^20260728.gf8df3f1-2.fc44.x86_64`, and Arch `2026_07_28.f8df3f1`; Fedora's spelling required a bounded parser addition. All rows produced exact KVM, qemu-img, OpenSSH, runtime, space, dependency, and namespace results from their actual environment. `doctor --fix` only installed checksum-pinned owned artifacts, directories, and the SSH key. Containers supplied no boot evidence. |
| M5-03 doctor artifact-install errors | Classify raw transport, temporary-file, read/hash, mode, publish, directory-sync, and readback I/O as a host dependency failure; preserve the phase, dependency, and destination path; and give a read/write-permissions, free-space, and `doctor --fix` hint. Reserve checksum failure for a digest successfully computed over all downloaded bytes that does not match the pin. | classify SHA-256 reader I/O as checksum failure; expose only a generic I/O source; omit the install path and recovery hint | An interrupted or denied local read does not prove that downloaded bytes are corrupt. Dependency classification and install-specific context direct users to the actionable host repair, while a completed mismatching digest remains an integrity failure. |
| M5-03 gates and unavailable KVM evidence | Require local stable and Rust 1.85.0 format, Clippy with warnings denied, and the complete suite; live x86_64 release-metadata/checksum retrieval; Python validator tests; generated-help command and local-link checks; real unprivileged Ubuntu, Fedora, and Arch container rows; final-head GitHub Ubuntu Rust CI plus the distro workflow; and only a gated skip when KVM is unavailable. | targeted Rust tests; one toolchain; source HTTP checks as boot evidence; fake `/dev/kvm` as KVM; omit a failed duplicate run | Both local toolchains passed format, Clippy, and all 587 tests. Fifteen Python tests passed; five x86_64 catalog metadata/checksum pairs passed; 52 documented Firestone commands and six local links passed; and all final-head GitHub jobs passed. The host has no `/dev/kvm`, so E2E 11 only proved its disabled skip and fail-closed preflight; the earlier same-code push-only CI shim timing failure remains recorded rather than concealed. |
| M5 completion contract | Pin `clap_complete` 4.6.9 and accept every stable `Shell` value it defines: bash, elvish, fish, powershell, and zsh. Generate one deterministic script directly on stdout. Keep `_shim` and `_vsock-proxy` in private parsers outside the completion command tree. Reject `--json`, `--quiet`, and `--verbose` for `completions`; allow the other global flags because they do not change the script. | emit completion text as `Output` and `Result` events; maintain a second hand-written public command tree; trust each generator to erase hidden commands; silently let quiet suppress the script | Completion files must contain shell source and no event envelope or stderr text. One Clap command remains authoritative for every public command, option, and visible alias. The private parsers remove hidden command names before generation instead of filtering generated shell syntax. Exact snapshots and public-grammar traversal cover all five generators. |
| M5 version identity | `--version` prints exactly `firestone <package-version>`. The shared `version` Result carries package version, release name `v<version>`, optional full lowercase git commit supplied by a clean release build, target architecture, an ordered dependency-name-to-`{version, sha256}` map selected for that architecture, and resolved config/data/runtime `Paths`. Human output projects the same Result. A non-release build uses JSON null and `not embedded` for the absent commit. | retain M4's version-only dependency map; run git at startup; embed build time, source directory, builder host, or toolchain paths; make CLI and REST metadata shapes differ | The payload identifies the executable and every vendored binary byte without non-reproducible build-host data. Runtime Paths remain explicit as required by section 15.1. This supersedes the M4 version-only dependency map while keeping one Result for CLI and REST. |
| M5 Linux release gate | Build `firestone-v0.1.0-x86_64-unknown-linux-musl` natively in the multi-architecture Rust 1.85.0 Alpine image pinned at `sha256:bea885d2711087e67a9f7a7cd1a164976f4c35389478512af170730014d2452a`; require exact rustc, Cargo, GCC, binutils, `Cargo.lock`, and `deps.toml` identities; verify architecture-specific Alpine `musl-dev` 1.2.5-r11 package hashes and use only its headers while host proc macros link to the image musl and target code uses rustc's self-contained CRT and libc; remap build paths; disable the ELF build id; use one LTO codegen unit and strip symbols; reject `PT_INTERP`, dynamic `NEEDED`, and `.symtab`; produce `SHA256SUMS`; build x86_64 twice and compare bytes; re-download both `deps.toml` architecture pin sets; smoke `--version`, `version`, all completions, and help. Build `aarch64-unknown-linux-musl` once on the documented `ubuntu-24.04-arm` native runner as a compile and static-ELF gate only. Upload only the x86_64 candidate as a 14-day CI artifact. | unpinned host packages or actions; cross-emulated release output; publish a GitHub Release; execute or advertise the aarch64 binary before its deferred runtime gate | Exact container, header, and source inputs make x86_64 reproduction testable across two clean builds. Native runners avoid an undeclared emulator input. The workflow publishes no release and makes no aarch64 runtime claim. Stable and Rust 1.85 CI remain separate compatibility gates. |
| M5-04 final Linux x86_64 acceptance coordinator | Require an exact clean `origin/main` commit, materialize that commit into a private mode-0700 git-archive snapshot, recheck every pinned input and harness there, and compare the exact static x86_64 release artifact with both an independently supplied expected SHA-256 and its exact adjacent `SHA256SUMS`. Require a bounded mode-0600 final-head doctor workflow attestation generated and prevalidated by the orchestrator, plus its independently supplied SHA-256; revalidate its schema, canonical repository and workflow, accepted main SHA, exact run/job URLs, completed-success status, build job, and Ubuntu/Fedora/Arch jobs on the target without transferring credentials or calling anonymous output authenticated. Also require `FIRESTONE_E2E=1` and a read/write character-device `/dev/kvm` that answers KVM API version 12. Hold one current-user mode-0600 host lock; run M1, M2, M3, M4, then the five-row catalog harness sequentially from separate owned mode-0700 homes under per-harness and six-hour aggregate deadlines. Preserve only bounded, valid mode-0600 JSON evidence and checksums outside those homes; delete invalid or unexpected child output. Stop at the first failure, drain the harness process group even if its leader exited, remove named machines and exact home-referencing processes, restore M3 TAP and user-namespace setup after processes are gone, and remove the aggregate root. Resume only a checksum-verified contiguous passed prefix whose commit, release, pins, source hashes, complete scenario evidence, and complete cleanup record all revalidate; remove every stale suffix artifact before rerunning it. Block handled signals through cleanup and the durable terminal checkpoint, recording a late interruption as failure before unblocking. The aggregate manifest enumerates E2E 1 through 11, verify 1 through 17, every harness/evidence hash, the release SHA-256, dependency hashes, the prevalidated doctor rows, and the user-approved aarch64-runtime, non-Linux-authority, and hostile-wrapper deferrals without embedding child evidence, cloud-init, secret, credential, or key bytes. | parallel harnesses or shared homes; manual unbounded chaining; self-authenticating artifacts; mutable source execution; writable or fake KVM fixtures; transferring a GitHub token to the target; calling anonymous API output authenticated; capturing stdout, disks, cloud-init, or key material; trusting an arbitrary workflow ID, skip, filename, stale suffix, or minimal JSON on resume; retaining invalid evidence or a failed host mutation for diagnosis | `scripts/m5-linux-mvp-e2e.py` and its no-KVM tests enforce this boundary. The only host attempt began at 2026-08-29T23:43:33.049Z and ended at 23:43:43.100Z with SSH exit 255 after TCP/22 timed out in 10.05 seconds. Consequently E2E 3, 4, 8, 9, and 11; verify 3, 7, 8, 14, and 16; M3-05; M4-03; and M5-04 remain open. No KVM or final-release completion is claimed. |
| M5-04 final Linux x86_64 acceptance evidence | Accept exact clean main `47daf52ac7df8195cd852647e67dae06b275b689` as the Linux x86_64 MVP after the secure sequential coordinator passes against static release SHA-256 `411d26c63e3ea4b0e1aa807165f0f00f08d09ef0fa75726edbb906217bf06505` and prevalidated doctor attestation SHA-256 `0f3149748cfd2470b1edf569f26fc4d21422e4a1d99f17b2ce3425d3fa25d741` | Reuse focused or prior-main evidence with different repository/release identities; infer KVM behavior from CI or source checks; extend acceptance to aarch64 runtime, non-Linux supervisor authority, or hostile wrappers | On Linux 6.8.0-137-generic x86_64 with read/write KVM, the run from 2026-08-30T21:27:16.616986Z through 21:59:24.612209Z passed M1, M2, M3, M4, and the Ubuntu 24.04/22.04, Debian 12/13, Fedora 44 catalog matrix. E2E 1–11 passed and verify 1–17 resolved. The private manifest bundle recomputed against `SHA256SUMS` with mode 0600 files: acceptance `e29a3fc7d189e29ea7a789783ce585ee4247032aa5be000931964107fd7faff4`, M1 `c6e4aa477bbd1e467d9d907a292f1dadee2d6ee3480f875e0e4afcc8f9f239e0`, M2 `e1c391a92746000ae04a11bd0a3c682e964226c5da9659cd0e5a442fb3fd933f`, M3 `761766b1ef0a7c34eb2416d5e7cc0cdf7e04c5f6e95be61796e249dcc9580dab`, M4 `364223d2be540372ace6cc942a76ff9158baf2965b7b7f03b1cb05756042a322`, and catalog `26d989ddaeb17628bd2815862b1b9ea0dbf3c9ed8881d1a130132f5909311c68`. Exact-main CI run `33335967366`, doctor run `33335967251`, and release run `33335967353` passed; the harness left no processes, TAP, network, or AppArmor-policy drift. The prior TCP/22 timeout is superseded by this observed KVM run, while aarch64 runtime, non-Linux authority, and hostile wrappers remain deferred. |
| M5-12 browser-reachable listener | Implement SPEC 16.1's reserved `--listen tcp:HOST:PORT --token FILE` rather than inventing a second transport, and constrain it so it cannot be weaker than the Unix socket: loopback-only bind refused with `usage` before the listener exists; TCP without a token and a token without TCP both usage errors before binding; mode-0600 current-user token file created `O_CREAT|O_EXCL`; a per-invocation in-memory token for `firestone ui`. Gate every TCP request on a `Host` allowlist, then a constant-time token compare over the raw 32 bytes accepted from a cookie or a bearer header, then a `Sec-Fetch-Site`/`Origin` check on mutations. Bootstrap `?token=` with a 303 that strips the query and sets an `HttpOnly; SameSite=Strict` cookie. | a routable or wildcard bind behind any flag; an unauthenticated TCP mode; a token in a process argument, a log, or an error body; trusting `SameSite` alone; `WWW-Authenticate`; hex-string `==` comparison; reusing the Unix gate on TCP or vice versa | A browser cannot open a Unix socket, so a web UI needs a second listener or it needs a proxy — and a proxy would be a second process for a product whose whole claim is one file. The constraints exist so the new listener is not a downgrade: loopback plus a 256-bit token plus a `Host` allowlist is a stronger boundary than loopback alone, and the DNS-rebinding check has to precede authentication or a rebound page could spend the cookie the browser attaches for it. The token leaves the address bar on the first navigation because browser history is not a secret store. |
| M5-12 web UI contract | Serve the UI from the same axum application, with `/v1` reaching the REST router and every other path the UI router, so both keep their own 404 contract. Render only shared action results; route every lifecycle mutation from the browser to the documented `/v1` endpoint and render its NDJSON as it arrives. Allow exactly one UI-side write, `POST /ui/machines`, which dispatches the same `Action::Create` so a rejected field can answer beside that field. Serve a Content-Security-Policy with neither `'unsafe-inline'` nor `'unsafe-eval'`, and match static assets against a closed table by exact name. | a second lifecycle implementation; client-invented terminal state; a UI-only endpoint that adds capability the REST contract lacks; inline styles or scripts; joining a request path onto an asset directory; a CDN | Two implementations of "start a machine" is two behaviours, and the one users would compare against real infrastructure products is whichever is wrong. Keeping the UI a read surface plus the real endpoints means the NDJSON stream, the safe-point-on-disconnect rule and the error envelope are exercised by the UI rather than re-described by it. The strict CSP is affordable precisely because everything is embedded, and it rules out the htmx constructs (`new Function()` trigger filters, `hx-on:*`, `js:` values) that would otherwise creep in. |
| M5-13 release version has one source of truth | Treat `[workspace.package] version` in `Cargo.toml` as the only declaration of the release version. Derive every consequence — `identity.release`, the `--version` line, the published artifact name — rather than restating it, and check the files the release workflow rewrites (`versions.env`'s `FIRESTONE_VERSION` and `CARGO_LOCK_SHA256`) for agreement with `Cargo.toml` and `Cargo.lock` instead of pinning their bytes. Enforce that agreement in `cargo test`. | a literal version in a test, a harness, a doc, or a comment; a `EXPECTED_FILE_HASHES` entry for a file the release workflow rewrites; discovering the drift in a release build | The workflow bumps `Cargo.toml`, `Cargo.lock` and `versions.env` together on a tagged run, so anything that restates the version is wrong the moment a release is cut and anything that pins those bytes is stale. That is not hypothetical: after `release: v0.1.3`, `store.rs` still asserted `v0.1.0`, the M5-04 harness still pinned the pre-bump `Cargo.lock`, and its `FIRESTONE_VERSION` pin still read `0.1.0` — a red `cargo test` and a red doctor-matrix CI job. Checking `versions.env` against the real `Cargo.lock` is also strictly stronger than a literal hash, because it catches a lock edited without updating its pin, which is what a `deps.toml` or dependency change does. |
| M5-12 vendored web assets | Pin every third-party web asset by SHA-256 in `web-assets.toml`, not `deps.toml`, and enforce it with a Rust test that recomputes each hash and compares the pinned set against the files actually present. | append to `deps.toml`; trust a vendored file because it is committed; hash only at vendoring time | `scripts/pin-deps.sh` regenerates `deps.toml` byte-for-byte from a heredoc and verifies with `cmp`, so an appended table breaks `verify` and is deleted by the next `refresh`. A separate manifest keeps that generator canonical, and moving the check into `cargo test` makes it part of the required gate instead of an optional script. |

| Static Firestone REST contract | Keep `docs/openapi.json` as OpenAPI 3.1 JSON for the 19 explicitly authored §16.2 operations. Build production routing from one private `REST_ROUTES` factory table, parse the static document with existing `serde_json`, and probe each configured axum `MethodRouter` through `Allow`; remove only axum's synthesized `HEAD` when its `GET` is present. | generate or serve OpenAPI at runtime; add schema/router dependencies or annotations; scrape Rust source text; advertise incidental `HEAD` operations as first-class Firestone routes | The checked-in artifact remains usable without Firestone or a parser dependency and adds no endpoint or behavior. One executable route source plus behavior-level method probing makes additions, removals, and method changes fail the contract test in both directions while preserving §16.2 as the authored public surface. |

---

## 22. Milestones

Each milestone ends with its acceptance criteria green in CI and this document updated.

**M0 — skeleton (no VM yet)**
`Paths`, `MachineSpec`/`Patch`/validation, `Action`, `Event`, `Dispatcher`, lock, state + reconcile, catalog parsing, CLI with renderer (TTY, non‑TTY, `--json`), `create`, `ls`, `show`, `edit`, `doctor` (all checks), `deps.toml` with real checksums, `doctor --fix`. Acceptance: unit suite (§19.1) except cloud‑init/VmConfig; drift test passes; `firestone create ubuntu --cpus 4 && firestone ls` works with no KVM.

**M1 — boots**
Image pull/verify/storage, overlay, seed disk (firestone part only), VmConfig mapping, VMM API client, shim (launch/stop/status), `start`/`stop`/`restart`/`rm`, `logs`. Acceptance: e2e 1, 5, 6, 7; `console.log` shows a login prompt; verify items 1, 2, 4, 5, 6, 9, 12 resolved.

**M2 — shell**
SSH key, vsock proxy, guest units, readiness loop, `shell`, `ssh-config`, `console`, `run`. Acceptance: e2e 2, 10; `firestone run ubuntu` on an empty home ends at a root prompt; verify 11, 13, 17 resolved.

**M3 — network and folders**
passt spawn + forwards, tap mode, virtiofsd + mounts, user cloud‑init parts and keys, instance‑id semantics. Acceptance: e2e 3, 4, 8; verify 7, 8, 10, 14, 15, 16 resolved.

**M4 — serve**
axum server, NDJSON streaming, all routes, error mapping. Acceptance: e2e 9; REST `Result` payloads byte‑equal to CLI `--json` `Result` payloads for the same action.

**M5 — polish and release**
Progress bars, timing details, hints on every error kind, `completions`, `version`, aarch64 build and edk2 path, catalog matrix (e2e 11), user guide. Acceptance: full e2e nightly green on both architectures; `doctor` on a fresh Ubuntu/Fedora/Arch host gives a correct fix for every failure.

**M6 — daily driver (v0.2.0)**
Snapshots (cold guaranteed; warm gated on verified vhost-user restore), `clone`, disk grow and live cpu/memory resize with opt-in headroom, per-machine metrics sampling, `cp`, system prune, pending-forward surfacing, cloud-init password/inline-key/inline-user-data authoring, OCI image boot (registry pull, ext4 rootfs packing, pinned direct-boot kernel, embedded `firestone-init` PID 1), and the web UI grown to parity: ANSI-color logs, WebSocket console and SSH shell terminal, machine editing, create-form image picker and structured inputs, metrics sparklines, snapshot/clone/prune surfaces. Each feature lands with its own normative sections and decision-log entries. Acceptance: required local gate plus `scripts/m6-kvm-e2e.py` and `scripts/m6-oci-kvm-e2e.py` green on a real x86_64 KVM host, existing e2e regression green, docs validated, release `v0.2.0` published.

## 25. Metrics (normative)

### 25.1 Design

Firestone runs no metrics daemon, stores no time series, and keeps no ring buffer on the host. `Action::Metrics { name }` reads one sample on demand from sources that already exist — the machine's Cloud Hypervisor API socket and the host `/proc` entries for its VMM process — and returns it as a single JSON object. Every device figure is cumulative since the VMM process started, so a client that wants a rate takes two samples and divides by the elapsed wall clock between their `sampled_at` values.

The sample is read-only. It never writes state, never restarts a sidecar, and never mutates the spec. It takes the machine lock only for the ordinary reconciled state read; it holds no lock while talking to the VMM.

A machine that is not active fails with the shared machine-not-running error: kind `conflict`, message ``machine `NAME` is not running``, hint `run firestone start NAME`. The REST adapter maps `conflict` to 409 and the CLI exits 4 (§15.5).

### 25.2 Payload (normative)

`MetricsResult` is emitted as one `Event::Result` with action `metrics` and rendered as a plain JSON object by `GET /v1/machines/{name}/metrics` — not as an NDJSON stream.

```json
{
  "sampled_at": "2026-09-02T12:00:00Z",
  "cpu": { "vcpus": 2, "cpu_time_ns": 9500000000 },
  "memory": {
    "rss_bytes": 67108864,
    "allocated_bytes": 2147483648,
    "guest_actual_bytes": 2147483648
  },
  "block": [
    {
      "device": "_disk0",
      "read_bytes": 4096,
      "written_bytes": 8192,
      "read_ops": 2,
      "write_ops": 3
    }
  ],
  "net": null
}
```

| Field | Type | Meaning |
|---|---|---|
| `sampled_at` | RFC 3339 instant | when the host and VMM sources were read |
| `cpu.vcpus` | integer | guest vCPU count from the effective spec |
| `cpu.cpu_time_ns` | integer or null | cumulative VMM user + system time in nanoseconds |
| `memory.rss_bytes` | integer or null | VMM resident set size in bytes |
| `memory.allocated_bytes` | integer | guest memory requested by the spec, in bytes |
| `memory.guest_actual_bytes` | integer or null | Cloud Hypervisor `memory_actual_size` |
| `block[]` | array | one entry per block device the VMM reports, in device-id order |
| `block[].device` | string | the VMM's device id, such as `_disk0` |
| `block[].read_bytes`, `written_bytes`, `read_ops`, `write_ops` | integer or null | cumulative virtio-block counters |
| `net` | array or null | network devices, or null when the VMM reports none |
| `net[].device` | string | the VMM's device id, such as `_net0` |
| `net[].counters` | object | counter name to cumulative value, exactly as the VMM names them |

Every field is always present; an unavailable figure is `null` and is never replaced by a zero, a guess, or a saturating number. Field order is fixed and identical across the CLI `--json` `Result` payload and the REST body (§16.2 equivalence).

Network counter key names are passed through rather than typed, because Firestone has not verified them on a real device: the default vhost-user `passt` path reports no network entries at all, so `net` is `null` for a default machine. Typed network fields are a later amendment, gated on a bare-metal `tap`-mode probe.

### 25.3 Sampling sources (normative)

One sample reads, in order:

1. the reconciled machine state and effective spec, for `status`, `vmm_pid`, `cpus`, and `memory`;
2. `/proc/<vmm_pid>/stat` fields 14 and 15 (`utime` + `stime`), converted to nanoseconds with `sysconf(_SC_CLK_TCK)`, and `/proc/<vmm_pid>/status` `VmRSS`, converted from kibibytes to bytes;
3. `GET /api/v1/vm.counters` on the machine's `api.sock`, for the per-device counter map;
4. `GET /api/v1/vm.info` on the same socket, for `memory_actual_size`.

Field 14 and 15 are located relative to the closing parenthesis of `/proc/<pid>/stat` field 2, because a process name may itself contain spaces and parentheses. `/proc` sampling is best effort and Linux-only: a host without `/proc`, or a VMM that exited between the state read and the sample, yields `null` for `cpu_time_ns` and `rss_bytes` rather than failing the action. A failing `vm.counters` or `vm.info` call fails the action with the VMM API client's error.

`vm.counters` is added to the bounded v53 client as a `GET` with expected status 200, no request body, and a 64 KiB response limit.

**Counter projection.** A device whose counter map carries both `read_bytes` and `write_bytes` is a block device; every other device is a network device. `write_bytes` is published as `written_bytes`. Cloud Hypervisor v53 reports a counter a device has never exercised as a `u64::MAX`-family sentinel — `write_latency_min` and `write_latency_max` are `u64::MAX` on a disk with no writes, and the matching average is derived from those saturating values. Any counter at or above 2^63 is therefore treated as absent: block counters become `null` and network counters are dropped from the map. A sentinel is never surfaced, and no counter Firestone does not publish, such as latency, appears in the payload.

`memory_actual_size` of 0 means the VMM has no actual-size figure and is projected as `null`.

### 25.4 Surfaces

- CLI: `firestone metrics NAME` prints a one-shot human table — sample instant, CPU line, memory line, a block-device table, and one network line — with `-` for every absent figure and no derived rate. `--json` emits the `Result` event unchanged.
- REST: `GET /v1/machines/{name}/metrics` returns the payload as `application/json` with status 200.
- Both project the same `Action::Metrics` through the shared dispatcher; neither adds a field the other lacks.

---

## 24. Clone (normative)

`clone` copies an existing machine definition, and by default its writable disk, into a new machine that has never run.

### 24.1 Surfaces

| Surface | Form |
|---|---|
| CLI | `firestone clone SRC DEST [--fresh-disk]` |
| REST | `POST /v1/machines/{name}/clone` with body `{"name": "<dest>", "fresh_disk": false}`, streaming NDJSON events (§16.3) |
| Action | `Action::Clone { source, dest, fresh_disk }` |
| Result | `CloneResult { source, dest, disk_bytes }` under result action `clone` |

`fresh_disk` is optional in the REST body and defaults to `false`. `disk_bytes` is the virtual size of the destination overlay in bytes, or `0` when the source had no overlay and none was created. Both surfaces construct the same action and therefore produce byte-identical `Result` payloads.

### 24.2 Requirements

- `DEST` must be a valid machine name (one non-empty path component, §6.1) and must not already name a machine; a taken name fails with `already_exists`.
- `DEST` must differ from `SRC`; cloning a machine onto itself is a `usage` error.
- The source must be `created` or `stopped`. Any other status fails with `conflict`, message `machine '<src>' is <status> and cannot be cloned`, and hint `run 'firestone stop <src>' and clone the stopped machine`. This is checked before the source machine lock is taken, so a running machine reports the refusal instead of waiting on its shim's lock.
- Locks are taken in one stable order: the source machine lock first, then the destination through the same creation path `create` uses (`.creating` marker, destination machine lock, publication). A failed clone leaves no published destination.
- The destination's `firestone.toml` is the source document **byte for byte**. It must resolve against the destination machine directory; a spec whose relative `cloud_init.user_data`, `cloud_init.network_config`, or `mount.host` path only exists beside the source is rejected before anything is written.
- The destination `state.json` is written fresh in `created` status with the source's pinned `image` identity (`ref`, `id`, `sha256`) so the clone reuses the exact verified base, and with `mac`, `instance_id`, `shim_pid`, `vmm_pid`, `sidecar_pids`, `started_at`, `degraded` and `last_exit` empty.
- Events: `StepStart`/`StepDone` for `spec`, then for `disk`. The `disk` step reports `copied overlay` or `fresh overlay`, or is a `StepSkip` with reason `no disk yet` when the source has never started and therefore owns no overlay.

### 24.3 What is and is not copied

| Artifact | Default clone | `--fresh-disk` |
|---|---|---|
| `firestone.toml` | copied verbatim | copied verbatim |
| `disk.qcow2` | copied with `qemu-img convert`, keeping the shared base | new empty overlay on the same base |
| pinned image identity in `state.json` | copied | copied |
| `state.json` runtime fields | never copied | never copied |
| `known_hosts` | never copied | never copied |
| `seed.img`, `seed/`, `vmconfig.json` | never copied | never copied |
| `console.log` and other logs | never copied | never copied |
| snapshots | never copied | never copied |
| base image in the image store | shared, never duplicated | shared, never duplicated |

The default copy is a full qcow2 overlay copy, so packages installed in the source guest are present in the clone; only the immutable base stays shared. The copy runs through `Cmd` with the pinned qemu-img 8.2.2 argv

```
qemu-img convert -f qcow2 -O qcow2 -o backing_fmt=qcow2 -B <absolute base path> <source overlay> <dest partial>
```

with the 30-minute conversion timeout, under the image-store lock, followed by the same discipline `create` uses for a fresh overlay (§8.4): mode `0600`, `fsync`, `qemu-img info` validation of the copy's backing chain and virtual size, publication with no replace, and a directory `fsync`. The source overlay's backing chain is validated to be exactly the pinned base before the copy starts, so a clone can never silently re-point at a different image.

### 24.4 Identity

| Identity | Derivation | Clone result |
|---|---|---|
| MAC address | `sha256("firestone-machine-mac-v1\0" ‖ data dir ‖ "\0" ‖ machine name)` at first start, unless `network.mac` is set in the spec | fresh by construction |
| cloud-init instance id | `iid-<machine name>-<12 hex of the rendered seed digest>` at first start | fresh by construction |
| vsock CID | fixed 3, per-machine socket namespace | unchanged |
| guest `/etc/machine-id` | written inside the guest filesystem | **duplicated** (known limitation) |

Because the MAC and the instance id are derived from the machine name and are only materialized at start, the clone allocates its own on its first boot; nothing is carried over in `state.json`. A source spec that pins `network.mac` explicitly is the one exception: the clone inherits that address verbatim with the rest of the document, and Firestone emits a warning naming both machines. Change `network.mac` in the clone before running both machines on the same L2 segment.

Known limitation: a copied overlay carries the guest's `/etc/machine-id`, `/etc/hostname` contents written by the source's first boot, and any other in-guest state. Two clones of the same source therefore present the same `/etc/machine-id` to DHCP servers and to systemd-journal upload. Firestone does not rewrite guest filesystems; run `systemd-firstboot --setup-machine-id` (or delete `/etc/machine-id` and reboot) inside the clone when a unique guest identity matters. `--fresh-disk` avoids the issue entirely by re-provisioning from the base image.

---

## Appendix A. Example session

```
$ firestone run ubuntu
  ✓ image    ubuntu:24.04 · x86_64 · 613 MB · 12.4s
  ✓ disk     20G overlay
  ✓ seed     instance iid-ubuntu-5f3a9c1e2b7d
  ✓ shim     pid 41200
  ✓ net      passt
  ✓ vmm      cloud-hypervisor v48.0
  ✓ boot     firmware+kernel 1.3s
  ✓ ssh      ready · 6.8s
root@ubuntu:~# exit

$ firestone ls
NAME    STATUS   IMAGE          CPUS  MEM  UPTIME  FORWARDS
ubuntu  running  ubuntu:24.04   2     2G   41s     -

$ firestone create dev debian:12 --cpus 4 --memory 8G -p 8080:80 --mount ~/code:/code
created dev · edit: firestone edit dev · boot: firestone run dev

$ firestone run dev
  ✓ image    debian:12 · x86_64 · cached
  ✓ disk     20G overlay
  ✓ seed     instance iid-dev-91c0aa72e4f3
  ✓ shim     pid 41388
  ✓ net      passt · 8080→80
  ✓ fs       ~/code → /code
  ✓ vmm      cloud-hypervisor v48.0
  ✓ boot     firmware+kernel 1.1s
  ✓ ssh      ready · 7.2s
root@dev:~# ls /code

$ curl --unix-socket $XDG_RUNTIME_DIR/firestone/serve.sock -X POST http://firestone/v1/machines/ubuntu/stop
{"type":"StepStart","id":"stop","label":"acpi power button"}
{"type":"StepDone","id":"stop","detail":"guest shutdown","elapsed_ms":2140}
{"type":"Result","action":"stop","payload":{"name":"ubuntu","status":"stopped","elapsed_ms":2201}}

$ firestone stop dev && firestone rm dev --yes
  ✓ stop     guest shutdown · 2.3s
removed dev
```

## Appendix B. Suggested `CLAUDE.md`

```markdown
# Firestone

Read SPEC.md before doing anything. It is the source of truth; sections marked normative are not negotiable
without a decision-log entry (SPEC.md §21) in the same change.

## Working rules
- Never guess a third-party flag or JSON field. Each is tagged [verify N] in SPEC.md §20; check the pinned
  binary's --help / man page / OpenAPI file, then record the result in §21.
- `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test` before every commit.
- No unwrap/expect outside tests. Every user-facing error has a kind and, where actionable, a hint.
- firestone-core must not depend on clap, indicatif or axum.
- All paths via `Paths`; all subprocesses via `Cmd`.
- Do not add features, flags or config keys that are not in SPEC.md. Propose them in §21 first.
- Keep diffs small and milestone-ordered (SPEC.md §22). Current milestone: M0.

## Commands
- unit tests: `cargo test`
- e2e (needs /dev/kvm): `FIRESTONE_E2E=1 FIRESTONE_HOME=$(mktemp -d) cargo test --test e2e -- --test-threads=1`
- refresh dependency pins: `scripts/pin-deps.sh` (downloads, hashes, writes deps.toml; never edit checksums by hand)
```
