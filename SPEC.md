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
2. **Sane defaults, zero paternalism.** The default path just works. Every other window stays open. Firestone never enforces opinions on the user's machine, never runs privileged commands on the user's behalf, and never silently "fixes" things. It tells the user exactly what to run and why.
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
- x86_64 and aarch64 hosts. Guests run the host architecture only.

### 2.2 Non-goals (v0.1)

Snapshots and restore; live migration; PCI/GPU passthrough; graphics/VNC/SPICE; non‑Linux guests; cross‑architecture emulation; VM‑to‑VM L2 networking beyond user‑managed tap; remote hosts (`FIRESTONE_HOST`); TCP REST listener; hotplug of CPU/memory/devices (cloud‑hypervisor supports it; not exposed yet); image building or customization; a provisioning DSL beyond cloud‑init; Windows/macOS hosts.

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
 │   cloud-hypervisor ── api.sock (REST control) ── vsock.sock ── console.sock  │
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
4. either spawns the shim (start) or talks directly to the VMM's `api.sock` (pause, resume, info) or to the shim's `shim.sock` (stop).

`firestone serve` is axum in front of the same `Action` dispatcher the CLI uses. It holds no state, can be killed and restarted at any time, and may run concurrently with CLI invocations; the machine lock serializes them.

### 4.2 Why a shim and not a daemon

"Daemonless" means no global service, not no processes. A VM is a long‑running process by definition. The shim is the per‑machine supervisor that Podman calls `conmon`:

- It is the parent of `cloud-hypervisor`, `passt` and `virtiofsd`, so it gets their exit statuses and reaps them.
- It enforces startup order (sidecar sockets must exist before the VMM connects to them) and teardown order (ACPI power button → wait → escalate → stop sidecars → final state write).
- It owns `state.json` while the machine runs, so there is exactly one writer at a time (§4.3).
- It survives the CLI exiting, terminal closing, and `serve` restarting.

Without it, the CLI would have to double‑fork the VMM and lose exit codes, crash reasons and deterministic teardown. A few hundred lines and ~2 MB of RSS per machine is the right price.

The shim does not proxy the console or the API. `firestone console` connects to the VMM's `console.sock` directly; `firestone shell` connects to `vsock.sock` directly; pause/resume/info hit `api.sock` directly.

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
    pub memory: ByteSize,            // default 2G
    pub disk: ByteSize,              // default 20G (virtual size of the overlay)
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
    pub network_config: Option<PathBuf>,
    pub ssh_keys: Vec<PathBuf>,          // public key files; contents appended to authorized keys
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
    Remove { name: String, force: bool },
    List,
    Show { name: String },
    SetSpec { name: String, spec: MachineSpec },        // PUT
    PatchSpec { name: String, patch: MachineSpecPatch }, // PATCH
    ImageList, ImagePull { r#ref: ImageRef }, ImageRemove { id: String, force: bool }, ImagePrune,
    Doctor { fix: bool },
    Version,
}
```

CLI subcommands and REST routes are thin adapters that construct an `Action` and hand it to `Dispatcher::run(action, &mut EventSink)`. Actions that require a terminal (`shell`, `console`, `logs -f`, `edit`) are CLI‑only by nature and are implemented in the CLI crate on top of core primitives; they are documented as such in §16 (REST exposes `logs` as a stream, and nothing else from that set).

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
    Result     { action: String, payload: serde_json::Value }, // exactly one, last, on success
}
```

- The CLI renders events as spinners, bars and colored lines (§15.3).
- `serve` streams them as NDJSON (§16.3).
- `firestone --json` prints them as NDJSON to stdout, unchanged.

Every action emits `Result` exactly once on success, or returns an error (which the CLI/REST layer turns into the terminal failure output). Step ids for `start` are fixed and ordered: `image`, `disk`, `seed`, `shim`, `net`, `fs`, `vmm`, `boot`, `ssh`.

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

Firestone-owned data directories use the same ancestry trust model as runtime paths. Existing ancestors must be real directories owned by the current uid or root and must not be renameable by another uid; root-owned sticky shared directories are allowed. The final data, `machines`, machine, `bin`, and `ssh` directories must be owned by the current uid and must not be group- or world-writable. Firestone creates owned directories with mode 0700 and refuses unsafe existing paths before reading, writing, fixing, or publishing machine data.

```
~/.config/firestone/
  config.toml                  global defaults (§7.3)
  catalog.toml                 optional catalog additions/overrides (§8.1)

~/.local/share/firestone/
  bin/                         vendored binaries: cloud-hypervisor-<ver>, hypervisor-fw-<ver>, CLOUDHV-<ver>.fd, virtiofsd-<ver>
  ssh/id_ed25519, id_ed25519.pub   firestone's own key (0600), generated on first use
  images/
    ubuntu-24.04-x86_64-<sha8>.qcow2
    ubuntu-24.04-x86_64-<sha8>.json   {ref, url, sha256, size, pulled_at, format}
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
memory = "2G"              # "512M", "4G", "4096M", or integer MiB
disk   = "20G"             # virtual size of the overlay; must be >= base image virtual size
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
# user_data      = "user-data.yaml"     # relative to the machine dir or absolute; #cloud-config or #!script
# network_config = "network-config.yaml"
ssh_keys     = []                        # ["~/.ssh/id_ed25519.pub"]; contents appended to authorized keys of `user`
provisioning = true                      # false: firestone injects nothing; shell/console will not work

[vmm]
# binary       = "/usr/local/bin/cloud-hypervisor"   # default: vendored pinned binary
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
- `network.mode = "tap"` requires `network.tap`; the device must exist (`/sys/class/net/<tap>`), and `/dev/net/tun` must be openable. Firestone never creates it.
- `forward` entries parse per §12.4; guest ports 1–65535; host ports 1–65535 (ports < 1024 are allowed and will fail at passt start without privileges; passt's error is surfaced, not pre‑empted).
- `mount.host` must exist; `mount.guest` absolute; tags unique.
- `cloud_init.user_data`: a symlink to a regular file is allowed; the opened target must be a readable regular file. The first line must be `#cloud-config` or start with `#!`; otherwise error with hint (`provisioning = false` plus a raw user‑data file is the escape hatch, see §10.2).
- `cloud_init.network_config`: a symlink to a regular file is allowed; the opened target must be a readable regular file.
- `cloud_init.ssh_keys`: each opened target must be a readable regular file and parse as one or more OpenSSH public keys.
- `user`: `[a-z_][a-z0-9_-]*`.
- `vmm.firmware = "edk2"` on x86_64 uses `CLOUDHV.fd`; on aarch64 `CLOUDHV_EFI.fd`; a custom path must identify a readable regular file.
- `vmm.binary`, when set, must identify a regular file executable by the current user.
- Spec changes while running are accepted and saved with a `Log` warning "takes effect on next start". Nothing is applied live in v0.1.

### 7.3 Global config `~/.config/firestone/config.toml`

```toml
[defaults]              # any MachineSpec key; layered under every machine's spec
cpus   = 2
memory = "2G"
disk   = "20G"

[start]
timeout_first_boot = "180s"
timeout            = "60s"

[stop]
timeout = "30s"

[ui]
color = "auto"          # "auto" | "always" | "never"   (NO_COLOR env also respected)

[images]
catalog = []            # extra catalog files, merged over the built-in one
```

### 7.4 CLI flag ↔ field mapping

Generated from `MachineSpecPatch`. Rule: `a.b` → `--a-b`; vectors are repeatable flags. Documented exceptions:

| Field | Flag |
|---|---|
| `image` | positional on `run`/`create`, or `--image` |
| `network.mode` | `--net passt\|tap\|none` |
| `network.forward[]` | `-p, --forward SPEC` (repeatable) |
| `network.tap` | `--tap DEV` |
| `mount[]` | `--mount HOST:GUEST[:ro]` (repeatable) |
| `cloud_init.user_data` | `--user-data FILE` |
| `cloud_init.ssh_keys[]` | `--ssh-key FILE` (repeatable) |
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

### 8.2 Resolution rules

`ImageRef` parsing, in order:

1. Absolute or relative path that exists → local file (raw or qcow2; detected by header).
2. `https://…` → download to the images dir keyed by URL sha; no checksum unless `--sha256` given (warn once).
3. `distro:version` or `distro:alias` → catalog entry.
4. `distro` → the catalog entry marked `default` for that distro.
5. Otherwise error `unknown image` listing the closest catalog names.

The host architecture selects the `[image.arch.<arch>]` table; a missing table is an error naming the architectures that exist.

### 8.3 Pull and verify

- Download streams to `images/<id>.partial` while hashing; on completion the hash is compared with the catalog `sha256` or the entry for the file name in the fetched `checksum_url` (SHA256SUMS/SHA512SUMS format). Mismatch → delete partial, error kind `checksum`.
- Success → rename to `images/<distro>-<version>-<arch>-<sha8>.<format>` and write the sidecar `.json`.
- `raw` images are converted to qcow2 at pull time (`qemu-img convert -O qcow2`) so every base is qcow2 **[verify 4]**.
- Resume of interrupted downloads: not in v0.1 (delete partial, restart).
- Events: `StepStart image` → `Progress` (bytes, total from `Content-Length`) → `StepDone image "ubuntu:24.04 · x86_64 · 613 MB"`, or `StepSkip image "cached"`.
- "Current" URLs (Ubuntu `current/`, Debian `latest/`) change over time. Firestone treats the checksum as identity: re-pulling when a newer file exists is explicit (`images pull ubuntu:24.04`), never automatic. A machine fixes its base when the first successful `start` resolves and records immutable `image.id` and `image.sha256` before creating the overlay; later pulls do not change that base.

### 8.4 Overlays

- `create` records the canonical image reference in `state.json`; `image.id` and `image.sha256` are null until the first pull resolves immutable content identity. Pull fills both fields atomically before the overlay is created at first `start`. The overlay is created lazily:
  `qemu-img create -f qcow2 -F qcow2 -b <abs base path> <machine>/disk.qcow2 <disk>`
- cloud‑hypervisor reads qcow2 with backing files natively **[verify 5]**. Fallback if that proves unreliable for the pinned version: `qemu-img convert` to a raw per‑machine copy (slow, large) or `cp --reflink=auto` on reflink‑capable filesystems.
- Base images are never opened read‑write. `images rm` refuses (without `--force`) while any `state.json` references the id; `images prune` removes unreferenced ones and reports bytes freed.

---

## 9. Boot: firmware, VMM config, start/stop sequences

### 9.1 Firmware

cloud‑hypervisor boots stock cloud images through a firmware; it supports Rust Hypervisor Firmware (RHF, a small PVH payload) and an edk2 UEFI build (`CLOUDHV.fd` on x86_64, `CLOUDHV_EFI.fd` on aarch64). Which works best depends on the guest OS, and RHF's EFI support is minimal: enough for the shim + GRUB2 path used by Ubuntu and similar, not universal.

Policy:

- `firmware = "auto"` (default) uses the catalog entry's `firmware` field; local/URL images default to `rhf` on x86_64 and `edk2` on aarch64.
- RHF is passed as the VMM `payload.kernel`; edk2 as `payload.firmware` **[verify 1]**.
- Both firmwares are vendored and pinned (§17.2). `doctor` verifies their checksums.

### 9.2 VmConfig mapping (normative)

The VMM is started with only `--api-socket` (plus `--log-file` and `vmm.extra_args`). The machine itself is created through the API with a JSON `VmConfig` (`PUT /api/v1/vm.create`, then `PUT /api/v1/vm.boot`). This keeps the spec → VMM mapping as data (serde struct → JSON), makes `config_overlay` a clean escape hatch, and avoids building shell argv.

Target mapping for the default machine (field names to be validated against the pinned version's OpenAPI document, `vmm/src/api/openapi/cloud-hypervisor.yaml` **[verify 2]**):

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
  "console": { "mode": "Socket", "socket": "/run/user/1000/firestone/ubuntu/console.sock" },
  "rng":     { "src": "/dev/urandom" }
}
```

Rules:

- `memory.shared = true` is mandatory whenever any vhost‑user device (passt, virtiofsd) is attached. Always set it; the cost is negligible and it removes a class of "works without mounts, breaks with mounts" bugs.
- `network.mode = "tap"` → `"net": [{ "tap": "<dev>", "mac": "…" }]` with no `ip`/`mask` so the VMM does not try to configure the device **[verify 8]**.
- `network.mode = "none"` → no `net` entry.
- Each mount → one `fs` entry with its own virtiofsd socket.
- `serial` captures kernel and systemd output because cloud images put `console=ttyS0` on the kernel command line. `console` (virtio‑console, `hvc0`) is the interactive rescue console (§11.6).
- `config_overlay`, if set, is applied last as an RFC 7396 merge‑patch. The resulting JSON is written to `machines/<name>/vmconfig.json` on every start for inspection, and `firestone show --vmconfig` prints it.

The API client is a small hand-written HTTP/1.1-over-unix-socket client built on `std::os::unix::net::UnixStream` and the existing `nix` poll/socket dependency; it does not add `hyper` or `hyperlocal`. It opens one fresh stream per request and applies one absolute deadline across connect, every partial write, and every read. Endpoints used in v0.1 are `GET /api/v1/vmm.ping`, `PUT /api/v1/vm.create`, `PUT /api/v1/vm.boot`, `GET /api/v1/vm.info`, `PUT /api/v1/vm.power-button`, `PUT /api/v1/vm.shutdown`, and `PUT /api/v1/vmm.shutdown` **[verify 2]**.

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
3. Shim: write `stopping`; `PUT vm.power-button` (ACPI; systemd guests shut down cleanly); wait for the VMM process to exit or `vm.info` to report the VM stopped, then `PUT vmm.shutdown` if the process is still alive **[verify 6]**. On timeout: SIGTERM the VMM, wait 5 s, SIGKILL. `force: true` skips ACPI and goes straight to SIGKILL.
4. Shim: sidecars exit on their own when the VMM disconnects (`passt --one-off`; virtiofsd exits on client disconnect **[verify 7]**); any still alive after 5 s get SIGTERM then SIGKILL.
5. Shim: write final `state.json` (`stopped`, `last_exit` with code/signal/reason/time), remove its sockets and pid file, exit 0.
6. CLI: `Result { name, status: stopped, elapsed_ms }`.

`restart` = `stop` then `start` under one lock acquisition.

`rm` = `stop` (prompting if running and interactive; refusing without `--force` if non‑interactive and running) then delete the machine directory and runtime dir.

---

## 10. Cloud-init

### 10.1 Seed disk

The NoCloud datasource is fed from a small vfat image labeled `CIDATA` containing `meta-data`, `user-data` and optionally `network-config`. It is generated in Rust with the `fatfs` crate (no `genisoimage`/`xorriso` dependency), attached read‑only as the second disk **[verify 9]**. Rendered inputs are also written to `machines/<name>/seed/` for inspection.

`meta-data`:

```yaml
instance-id: iid-<name>-<sha256(user-data)[0..12]>
local-hostname: <name>
```

### 10.2 Multipart user-data and merge rules (normative)

The user‑data written to the seed is always MIME multipart (`multipart/mixed`, `MIME-Version: 1.0`). Parts, in order:

1. **The user's part**, if `cloud_init.user_data` is set: `text/cloud-config` if the file starts with `#cloud-config`, `text/x-shellscript` if it starts with `#!`. Content is passed byte‑for‑byte; firestone never edits it.
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
ssh_pwauth: false
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
      ExecStart=-/usr/sbin/sshd -i
      StandardInput=socket
      StandardError=journal
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
  - systemctl daemon-reload
  - systemctl enable --now firestone-sshd.socket
  - systemctl enable --now serial-getty@hvc0.service
```

Notes:

- Root login works because `disable_root: false` plus a key; stock sshd allows key‑only root (`PermitRootLogin prohibit-password`). If the user sets `user = "ubuntu"`, the default user already has the keys.
- On guests with systemd ≥ 256, `systemd-ssh-generator` may already bind sshd to vsock port 22 as `sshd-vsock.socket`. Either unit serving port 22 is fine; if firestone's unit fails to bind because the native one won, the failure is cosmetic **[verify 11]**.
- The `sshd` path differs between distros only rarely (`/usr/sbin/sshd` on Debian/Ubuntu/Fedora); the catalog entry may override `sshd_path` if a distro needs it.
- Templates are rendered with `minijinja`; the rendered output is unit‑tested against golden files per template input.

### 10.4 Instance id and re-provisioning

`instance-id` is derived from the rendered user‑data. Editing anything under `[cloud_init]`, adding a mount, or changing `user` changes the id, so cloud‑init re‑runs its per‑instance modules on the next boot — that is how config changes reach the guest. Because a new instance id regenerates the guest's SSH host keys (`ssh_deletekeys` default), `start` deletes `machines/<name>/known_hosts` whenever it rewrites the seed.

---

## 11. Shell, console, logs

### 11.1 SSH over vsock (normative)

Shell access does not depend on guest networking. cloud‑hypervisor implements virtio‑vsock in userspace and exposes it as a unix socket on the host (`vsock.sock`); no `/dev/vhost-vsock`, no kernel module, no CID coordination — every machine uses CID 3. The guest runs `sshd` on vsock port 22 via the socket unit in §10.3.

### 11.2 Host proxy

`firestone _vsock-proxy <name> <port>` (hidden subcommand):

1. Connect to `$RUNTIME/<name>/vsock.sock` (error kind `not_running` with a hint if absent).
2. Write `CONNECT <port>\n`; read one line; expect `OK <n>\n` **[verify 12]**. Anything else → exit 1 with the line on stderr.
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

`firestone console <name>` connects to `$RUNTIME/<name>/console.sock` (virtio‑console, `hvc0`), puts the terminal in raw mode, and relays bytes. Escape sequence `Ctrl-]` detaches. On connect it prints to stderr: `connected to <name> console · escape: Ctrl-]`. The guest side has an autologin getty on `hvc0` (§10.3), so the console is a rescue path that works when SSH does not. cloud‑hypervisor's socket console must accept a new client after one disconnects **[verify 13]**; if it does not, the fallback is `console.mode = "Pty"` with the shim holding the pty master and brokering attach/detach over `shim.sock`.

### 11.7 `logs`

`firestone logs <name> [-f] [--source console|vmm|shim|passt|virtiofsd-N] [-n LINES]` prints the chosen file (default `console.log`, last 200 lines) and follows with `-f` (inotify/poll). Over REST: `GET /v1/machines/{name}/logs?source=&follow=`.

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
```

- `--foreground` keeps passt a child of the shim (passt daemonizes by default).
- `--one-off` makes passt exit when the VMM disconnects, so teardown is automatic.
- The VMM connects as a vhost‑user client (`vhost_mode: "Client"`) **[verify 14]**.
- The guest reaches host loopback services through its gateway address (passt default). Documented for users; not configurable in v0.1.
- Forwards are fixed for the life of the process; changing `network.forward` takes effect on the next start. `ssh -L` over vsock covers ad‑hoc needs (§11.5).
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
| `create [NAME] IMAGE [spec flags] [-f SPEC.toml] [--edit]` | write a spec; never boots, never prompts. One positional = image (name derived as in `run`); two = name then image |
| `start NAME [--no-wait] [--timeout D]` | boot and wait for ssh |
| `stop NAME [--timeout D] [--force]` | graceful ACPI stop, escalate on timeout |
| `restart NAME` | stop + start |
| `rm NAME… [--force]` | stop if needed, delete everything |
| `ls` (alias `list`) | table of machines (§15.3) |
| `show NAME [--vmconfig]` | spec + state (+ generated VmConfig) |
| `edit NAME` | open `firestone.toml` in `$VISUAL`/`$EDITOR`; validate on save, re‑open on error |
| `shell NAME [--user U] [-- CMD…]` (alias `ssh`) | ssh over vsock |
| `ssh-config NAME` | print an OpenSSH Host block |
| `console NAME` | attach to hvc0 |
| `logs NAME [-f] [--source S] [-n N]` | view logs |
| `images ls` / `images pull REF` / `images rm ID [--force]` / `images prune` | image management |
| `doctor [--fix]` | diagnose host; `--fix` downloads vendorable binaries and prints the rest |
| `serve [--listen unix:PATH]` | REST listener |
| `completions SHELL` | shell completions |
| `version` | version, pinned dependency versions, paths |
| `_shim NAME`, `_vsock-proxy NAME PORT` | hidden internals |

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
- All feedback goes to stderr; stdout carries only data (`ls` table, `show`, `ssh-config`, `--json` streams), so pipes work.
- Non‑TTY: no control characters or spinner frames; each step prints `[image] ubuntu:24.04 · x86_64 · cached` on start/done; NO_COLOR respected.
- `--json`: NDJSON `Event`s on stdout, one per line, and nothing else on stdout.
- `ls` table: `NAME  STATUS  IMAGE  CPUS  MEM  UPTIME  FORWARDS`; statuses `running`, `running!` (degraded), `stopped`, `failed`, `starting`, `stopping`, `created`. Never truncates names.
- Time, sizes, rates use short human units (`1.3s`, `613 MB`, `48 MB/s`).
- Success ends with a single result line where useful (`start`: `ubuntu is running · shell: firestone shell ubuntu`). No decorative banners.

### 15.4 Prompts

Prompts appear only when stdin and stderr are TTYs and `--yes` is absent. They exist for exactly two situations: `rm` of a running machine, and `images rm` of an image in use. Non‑interactive invocations of those fail with a hint to pass `--force`/`--yes`. Nothing else prompts.

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

`firestone serve` listens on `unix:$XDG_RUNTIME_DIR/firestone/serve.sock` (mode 0600). Authentication is the socket's file permissions. No TCP listener in v0.1; the option is reserved (`--listen tcp:127.0.0.1:PORT --token FILE`, token required, never unauthenticated).

The server holds no state and takes the same machine locks as the CLI. `curl --unix-socket … http://firestone/v1/machines` is the smoke test.

### 16.2 Routes

| Method | Path | Body | Response |
|---|---|---|---|
| GET | `/v1/version` | | `{version, deps, paths}` |
| GET | `/v1/doctor` | | doctor report |
| GET | `/v1/machines` | | `[MachineSummary]` (same rows as `ls`) |
| POST | `/v1/machines` | `{name, spec}` (spec = `MachineSpecPatch` layered on defaults) | 201 `{name, spec, state}` |
| GET | `/v1/machines/{name}` | | `{spec, state}` |
| PUT | `/v1/machines/{name}` | `MachineSpec` | 200 `{spec}` (+ `warnings`) |
| PATCH | `/v1/machines/{name}` | `MachineSpecPatch` | 200 `{spec}` |
| DELETE | `/v1/machines/{name}?force=` | | 204 (stream if a stop is needed) |
| POST | `/v1/machines/{name}/start` | `{wait?, timeout_s?}` | event stream → `Result` |
| POST | `/v1/machines/{name}/stop` | `{timeout_s?, force?}` | event stream → `Result` |
| POST | `/v1/machines/{name}/restart` | | event stream → `Result` |
| GET | `/v1/machines/{name}/logs?source=&follow=&lines=` | | `text/plain`, chunked |
| GET | `/v1/machines/{name}/vmconfig` | | generated VmConfig JSON |
| GET | `/v1/images` | | `[Image]` |
| POST | `/v1/images/pull` | `{ref}` | event stream → `Result` |
| DELETE | `/v1/images/{id}?force=` | | 204 |
| POST | `/v1/images/prune` | | `{removed, bytes_freed}` |

The user's example holds: `POST /v1/machines/ubuntu/start` returns the same events the CLI shows, ending in `{"type":"Result",…}`, and `GET /v1/machines` then reports `running`.

### 16.3 Streaming

Action routes respond with `Content-Type: application/x-ndjson` and stream `Event`s as they happen, one JSON object per line, the last being `Result` (or an error object). Clients that send `Accept: application/json` get a single JSON response after completion containing `{events: [...], result}` or an error. Requests are canceled by closing the connection; the underlying action continues to a safe point (a started VM stays started).

### 16.4 Errors

```json
{ "error": { "kind": "not_found", "message": "no machine named 'ubunut'", "hint": "firestone ls" } }
```

HTTP status by kind: `usage`/`invalid_spec` 400, `not_found` 404, `conflict`/`already_running`/`busy` 409, `timeout` 504, `dependency` 503, `checksum` 502, everything else 500.

---

## 17. Dependencies and `doctor`

### 17.1 Host requirements

- Linux, x86_64 or aarch64, KVM available (`/dev/kvm` readable and writable by the user).
- `$XDG_RUNTIME_DIR` (or the `/tmp` fallback) writable.
- Unprivileged user namespaces for virtiofsd's default sandbox (optional; degrades to `--sandbox none`).
- No root, no capabilities, no kernel modules beyond KVM.

### 17.2 Binaries

| Binary | Role | Source | How obtained |
|---|---|---|---|
| `cloud-hypervisor` | VMM | GitHub releases, static (`cloud-hypervisor-static`, `-aarch64`) | vendored, pinned, sha256‑checked |
| `hypervisor-fw` | RHF firmware | rust-hypervisor-firmware releases | vendored, pinned |
| `CLOUDHV.fd` / `CLOUDHV_EFI.fd` | edk2 firmware | cloud-hypervisor edk2 releases | vendored, pinned |
| `virtiofsd` | shared folders | virtio-fs/virtiofsd releases (static) | vendored, pinned |
| `passt` | networking | distro package (`passt`) | system; version must support `--vhost-user` |
| `qemu-img` | overlays, raw→qcow2 | distro package (`qemu-utils` / `qemu-img`) | system |
| `ssh`, `ssh-keygen` | shell | distro package (`openssh-client`) | system |

Pins live in `deps.toml` in the repository (name, version, per‑arch URL, sha256). Checksums are computed from the real downloads at pin time, never typed from memory. Vendored binaries install to `<data>/bin/<name>-<version>` and are selected by exact version; `vmm.binary` overrides the VMM only.

### 17.3 `doctor` checks

Each check prints `ok`, `warn` or `fail`, a one‑line reason, and for failures the exact command to run. `--fix` performs only the actions firestone can do unprivileged (download vendored binaries, generate the SSH key, create directories).

1. host architecture supported
2. `/dev/kvm` exists and opens O_RDWR → fix: `sudo usermod -aG kvm $USER` (group name detected from the device's owner group) + re‑login
3. KVM nested/virtualization enabled (informational if the device is missing: BIOS/`kvm_intel`/`kvm_amd`)
4. `XDG_RUNTIME_DIR` set and writable → warn and name the fallback
5. vendored `cloud-hypervisor`, `hypervisor-fw`, edk2 present with matching checksums → fix: `doctor --fix`
6. vendored `virtiofsd` present → fix: `doctor --fix`
7. `passt` on PATH and `passt --version` ≥ the minimum with vhost‑user → fix: distro install command (apt/dnf/pacman/zypper detected)
8. `qemu-img` on PATH → fix: distro install command
9. `ssh`, `ssh-keygen` on PATH → fix: distro install command
10. user namespaces available (`/proc/sys/user/max_user_namespaces` > 0 and `unshare -U true` works) → warn: virtiofsd will run with `--sandbox none`
11. firestone SSH key present → fix: generated by `--fix`
12. free space in the data dir ≥ 5 GB → warn
13. stale machine states (runtime dir missing while `state.json` says running) → info, reconciled

---

## 18. Implementation notes

### 18.1 Language and crates

Rust, edition 2024, stable toolchain, single static binary (`x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` release targets; glibc for dev).

| Concern | Crate |
|---|---|
| CLI | `clap` (derive), `clap_complete` |
| serialization | `serde`, `serde_json`, `toml` (with `toml_edit` for comment‑preserving writes), `schemars` |
| async runtime | `tokio` (multi‑thread for serve/shim; current‑thread is fine for CLI) |
| HTTP server | `axum`, `hyper`, `hyperlocal` (unix sockets) |
| HTTP client | `reqwest` (downloads, streaming), `hyper` + `hyperlocal` (VMM API) |
| hashing | `sha2` |
| terminal UI | `indicatif`, `console`, `owo-colors`, `crossterm` (raw mode for `console`), `unicode-width` (table layout) |
| processes / OS | `nix` (flock, setsid, signals, pidfd), `libc` |
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
- Cloud‑init: golden renders for {no user‑data, cloud‑config user‑data, shellscript user‑data, keys, mounts, provisioning=false}; instance id stability.
- VmConfig mapping: golden JSON for {default, tap, none, mounts, edk2, config_overlay}.
- State: reconcile matrix (status × socket alive × shim pid alive × runtime dir exists) → expected status/rewrite.
- Lock: contention and timeout behavior with two processes.
- vsock proxy: handshake against a fake unix server (`OK`, error line, EOF).
- Renderer: `insta` snapshots for TTY and non‑TTY output of a canned event stream; `--json` is byte‑exact NDJSON.
- REST: axum handlers with a mocked `Dispatcher`; NDJSON framing; error mapping (§16.4).

### 19.2 End-to-end (KVM required)

Gated by `FIRESTONE_E2E=1`; uses `FIRESTONE_HOME=$(mktemp -d)` and the vendored binaries. Scenarios, each independent:

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

| # | Assumption | How to check |
|---|---|---|
| 1 | RHF is passed as `payload.kernel`; edk2 as `payload.firmware` | `cloud-hypervisor --help`; docs/README "Firmware" section; boot both once |
| 2 | VmConfig field names and the `/api/v1/…` endpoint set used in §9.2 | pinned `vmm/src/api/openapi/cloud-hypervisor.yaml`; `ch-remote --help` |
| 3 | Each catalog image boots to ssh on its declared firmware | e2e scenario 11 |
| 4 | `qemu-img convert -O qcow2` output boots under CH; raw bases not needed | boot a converted image |
| 5 | CH opens qcow2 overlays with a qcow2 backing file and performance is acceptable | e2e scenario 2 + `fio` inside the guest vs a raw copy |
| 6 | After `vm.power-button`, a systemd guest powers off; whether the CH process exits by itself or needs `vmm.shutdown` | observe process state in e2e scenario 5 |
| 7 | `passt --one-off` and `virtiofsd` exit when the VMM disconnects | `ps` after stop |
| 8 | CH opens a user‑owned tap without CAP_NET_ADMIN when `ip`/`mask` are unset | tap e2e on a dev box |
| 9 | cloud‑init NoCloud accepts a vfat `CIDATA` volume built by `fatfs`; no ISO needed | first boot; `cloud-init status --long` |
| 10 | `merge_how` grammar and part ordering yield "user scalars win, lists append" | render both parts; run `cloud-init devel schema`/a merge test inside a guest; golden test |
| 11 | Coexistence of `firestone-sshd.socket` with systemd‑256's `sshd-vsock.socket` | boot a systemd ≥ 256 image; `systemctl list-sockets` |
| 12 | vsock host protocol is `CONNECT <port>\n` → `OK <n>\n` | CH `docs/vsock.md`; raw `socat` test |
| 13 | CH `console.mode = "Socket"` accepts reconnects; else use `Pty` + shim brokering | attach, detach, attach again |
| 14 | passt `--vhost-user` + CH `vhost_mode: "Client"` interoperate at the pinned versions | e2e scenario 3 |
| 15 | passt `-t`/`-u` grammar for bind addresses and ranges | `man passt`; unit tests against examples from the man page |
| 16 | virtiofsd supports `--readonly` (or the equivalent) and `--sandbox namespace` rootless | `virtiofsd --help`; mount e2e |
| 17 | `systemd` in target images supports `ListenStream=vsock::22` and `serial-getty@hvc0` | boot; `systemctl status firestone-sshd.socket serial-getty@hvc0` |
| 18 | The CI runner exposes `/dev/kvm` | `ls -l /dev/kvm` in a workflow |

---

## 21. Decision log

| Decision | Chosen | Alternatives considered | Why |
|---|---|---|---|
| VMM | cloud‑hypervisor | QEMU | REST‑controlled, static Rust binary, userspace vsock, ~200 ms firmware boot, small device model; QEMU's breadth is legacy we do not need. Cost: KVM‑only, no graphics, narrower distro coverage. |
| Process model | no global daemon; one shim per machine; stateless `serve` | pure daemonless; libvirt‑style daemon | Exit codes, ordered start/stop and single‑writer state need a supervisor; a global daemon is bloat and a single point of failure. |
| State store | filesystem (TOML spec + JSON state, flock, atomic rename) | SQLite | Transparent, no dependency, trivially inspectable; scale is tens of machines, not thousands. |
| Liveness | socket connect + `vmm.ping` | pid files | Pids go stale; sockets in tmpfs self‑clean on reboot. |
| Rootless | yes, by default; nothing needs capabilities | root + bridge | passt + userspace vsock + user‑owned tap make root unnecessary; matches non‑paternalism. |
| Default network | passt (vhost‑user) | slirp4netns/libslirp; bridge/tap | passt is fast, unprivileged, transparent addressing, forwards; slirp is slow and legacy; bridges need root and infrastructure. |
| VM‑to‑VM networking | not in v0.1; tap mode for users who need it | managed bridge | Owning an L2 story is a big surface; defer until demanded. |
| Shell transport | ssh over vsock | ssh over forwarded TCP port; serial login | Works with no network, full ssh feature set, no port allocation, keeps `shell` working when users break networking. |
| SSH keys | firestone generates its own; user keys are an appended list; multipart cloud‑init | `--ssh-key` as the only path; "pass the user's key" | The user's key needs `authorized_keys`, not custom networking; multipart keeps user‑data untouched. |
| Boot | firmware boot of stock cloud images (RHF default, edk2 fallback) | direct kernel boot | Direct boot needs per‑distro kernels and rootfs extraction; firmware boot is ~100–200 ms and maintenance‑free. |
| Seed disk | vfat via `fatfs` | ISO via genisoimage | One fewer host dependency. |
| VMM configuration | JSON `VmConfig` via `vm.create` | argv flags | Data, not shell strings; enables `config_overlay`. |
| `create` behavior | silent, never boots, `--edit` opens editor | prompt "boot with defaults?" | Prompts break scripting; `run` is the verb that boots. |
| `run` semantics | idempotent (create/start/shell) | Podman‑style "new instance every time" | "Instant context" every time, no name clutter. |
| Overlays | qcow2 with backing file via `qemu-img` | raw copies; reflink | Fast creation, small, standard; verify CH support (verify 5). |
| Console | virtio‑console socket + autologin getty; serial → file | shim tees serial | Both logging and interactive attach without a tee or a race. |
| CID | fixed 3 | allocation table | CH's vsock is userspace; the CID is not host‑global. |
| REST transport | unix socket only in v0.1 | TCP with token | Auth by file permissions is simple and correct; TCP later with a token. |
| Language | Rust | Go | Same ecosystem as the VMM; one static binary. Go would also work. |
| Pre-pull image identity | `created` state stores the canonical image reference with null `id` and `sha256`; the first successful pull fills both before overlay creation | empty-string sentinels; download during `create`; omit `state.json` until start | `create` is specified as a local spec write and M0 must work on an empty home before M1 image pulling exists. Nulls represent unavailable identity without inventing one; image removal ignores machines until a real id is recorded. |
| CLI support crates | `jiff` for RFC 3339 timestamps; `shlex` for `VISUAL`/`EDITOR` argv; `unicode-width` for terminal table columns | hand-written timestamp formatting, shell-word parsing, or Unicode width tables; invoke the editor through a shell | These are bounded data-formatting/parsing concerns with mature implementations. Direct argv execution preserves the no-shell process invariant, while measured display width keeps deterministic tables aligned without truncating user data. |
| Dependency pins | cloud-hypervisor v53.0; Rust Hypervisor Firmware 0.5.0; cloud-hypervisor edk2 ch-1e1b96f126; Firestone virtiofsd v1.14.0 release `virtiofsd-v1.14.0-firestone.1` for both musl targets plus upstream source | moving `latest` URLs; edk2 newer than the VMM-tested tag; mutable virtiofsd CI artifact; distro virtiofsd | Exact release URLs and SHA-256 values make refreshes reproducible. Cloud Hypervisor v53.0 pins the edk2 build in its integration assets. Firestone's public release reproduces upstream virtiofsd v1.14.0 for x86_64 and aarch64 from the pinned source and exposes immutable anonymous-download assets verified by `scripts/pin-deps.sh verify --arch all`. |
| Doctor passt minimum | passt 2024_12_11.09478d5 or newer, with the exact `--vhost-user` help token present | presence alone; capability token alone; semantic-version parsing | Upstream added `--vhost-user` in commit `28997fcb29b560fc0dcfd91bad5eece3ded5eb72`; tag 2024_11_27.c0fbc7e does not contain it and 2024_12_11.09478d5 is the first release tag that does. Passt uses date-and-commit release names rather than semantic versions. Checking both the release date and the pinned capability token rejects old builds and unversioned distro builds without claiming verify 14 runtime interoperability. |
| [verify 1] firmware mapping at cloud-hypervisor v53.0 | RHF 0.5.0 uses `payload.kernel`; edk2 ch-1e1b96f126 uses `payload.firmware` | pass RHF through `payload.firmware`; pass edk2 through `payload.kernel` | The v53.0 CLI exposes distinct `--kernel` and `--firmware` inputs, `PayloadConfig` exposes the matching JSON fields, and the v53.0 README documents RHF's Xen PVH entry as valid through the kernel input and edk2 through firmware. Source and CLI checks resolve the mapping. Boot behavior remains an M1 runtime check. |
| [verify 2] API and VmConfig at cloud-hypervisor v53.0 | Firestone uses `GET /api/v1/vmm.ping`, `PUT vm.create`, `PUT vm.boot`, `GET vm.info`, `PUT vm.power-button`, `PUT vm.shutdown`, and `PUT vmm.shutdown`. Ping and info return 200 JSON; create, boot, power-button, and VM shutdown return empty 204; VMM shutdown returns 200 with `Content-Length: 0`, despite OpenAPI advertising 204. Use the §9.2 field names and enum casing; overlay disks use `image_type: "Qcow2", backing_files: true` and the vfat seed uses `image_type: "Raw"`. | rely on image auto-detection; omit `backing_files`; infer methods, response framing, or success codes from endpoint names | Tag `v53.0` resolves to commit `9ed824d6d08df3e96f7d5f50795d9449ac99f431`. The pinned OpenAPI methods and schemas are in `vmm/src/api/openapi/cloud-hypervisor.yaml` lines 10-146; route registration is in `vmm/src/api/http/mod.rs` lines 95-176 and 233-307; create's required body/204 is in `http_endpoint.rs` lines 281-347; bodyless action 204 handling is in `http_endpoint.rs` lines 393-451 and `http/mod.rs` lines 138-153; and info, ping, and the dedicated VMM-shutdown 200 handler are in `http_endpoint.rs` lines 612-693. The 200/empty VMM-shutdown response was also observed against v53.0 on the Linux validation host. Source resolves the wire contract; boot behavior remains an M1 KVM check. |
| VMM API transport and framing | `UnixStream` plus `nix`, one fresh connection and one absolute deadline per request; 51,200-byte create body, 16-KiB headers, 64-KiB ping/error bodies, 1-MiB info body, and zero-byte empty successes; strict non-chunked `Content-Length` framing | `hyper` + `hyperlocal`; the unbounded upstream client; reading to EOF; accepting chunked or ambiguous framing | Cloud Hypervisor v53.0 pins micro-http commit `5c2254d6cf4f32a668d0d8e57ba20bebad9d4fba`. Its 51,200-byte server limit is in `micro-http/src/server.rs` lines 21-24, and Cloud Hypervisor does not override it (`vmm/src/api/http/mod.rs` lines 440-489). The pinned response writer emits HTTP/1.1 keep-alive, non-chunked `Content-Length` framing in `micro-http/src/response.rs` lines 79-85, 160-194, 245-304, and 357-373. A small closed parser avoids new dependencies, bounds hostile or drifted responses, and lets liveness reuse the lifecycle transport instead of maintaining a second parser. |
| [verify 12] vsock host handshake at cloud-hypervisor v53.0 | write `CONNECT <guest-port>\n`, then wait for `OK <allocated-host-port>\n` after the guest accepts | treat a successful Unix socket connect as guest readiness; expect the guest port in the acknowledgement | The pinned `docs/vsock.md` specifies the request. The v53.0 muxer source and unit test show that the acknowledgement contains the allocated local port and is sent only after the virtio-vsock response establishes the connection. A raw host-to-guest test remains an M1 runtime check. |
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
