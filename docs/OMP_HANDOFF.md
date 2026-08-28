# OMP handoff

This document transfers Firestone development from the Codex orchestration session to OMP. `SPEC.md` remains the product and architecture source of truth. `AGENTS.md` remains the working contract.

## Start here

1. Read `AGENTS.md`, `SPEC.md`, and `docs/PROJECT_STATUS.md` in full.
2. Start from `origin/main` after the handoff commit containing this file.
3. Preserve the two unfinished worktrees described below. Do not reset or delete them.
4. Keep implementation work on `agent/<task>` branches and open bounded pull requests. The user owns the repository and expects autonomous progress, review, Linux verification, and regular commits.

## Scope remaining

The project is not close to the full MVP. Most of the M0 foundation is complete. M0 still needs dependency publication pins, doctor integration, the CLI, and no-KVM integration. Milestones M1 through M5 remain largely unimplemented:

- M1: image pull and storage, qcow2 overlays, seed disk, VmConfig mapping, VMM API client, shim, start/stop/restart/remove, and logs.
- M2: SSH key installation, vsock proxy, guest units, readiness, shell, ssh-config, console, and run.
- M3: passt, tap, port forwards, virtiofsd mounts, user cloud-init parts, and instance-id behavior.
- M4: Unix-socket REST server, routes, streaming, and byte-equivalent CLI/REST result payloads.
- M5: UX polish, completions, versioning, aarch64 native verification, catalog boot matrix, documentation, and release gates.

A long-running autonomous agent is appropriate for the remaining work.

## Main branch

Before this handoff document, `main` was clean at:

```text
0f9734b6c0f14c4f5176dc34a2faf57879c67132
Merge pull request #7 from 0xchasercat/agent/m0-virtiofsd-dist
```

Merged pull requests:

| PR | Area | Result |
|---|---|---|
| #1 | workspace and shared contracts | Rust workspace, `Action`, `Event`, typed errors, dispatcher seam, CI |
| #2 | catalog data | five releases, ten architecture sources, live URL and checksum validation |
| #3 | dependency evidence | Cloud Hypervisor v53.0, RHF 0.5.0, edk2 ch-1e1b96f126, virtiofsd source pins |
| #4 | state | atomic files, machine locks, state schema, process identity, liveness reconciliation |
| #5 | catalog runtime | override merge, aliases/defaults, architecture selection, deterministic suggestions |
| #6 | spec and paths | shared spec/patch, typed clears, persistence, validation, runtime ancestry, scalar types, port-forward grammar |
| #7 | virtiofsd distribution recipe | reproducible static builds, safe publication, deterministic mode-preserving packages, CI |

The final PR #6 gates passed locally, on GitHub, and on Azure with Rust 1.85.0. The suite reported 224 tests on macOS and 225 on Linux. The final PR #7 workflow rebuilt both targets twice and passed publication tests. No pull requests are currently open.

## Public-asset blocker

The orchestrator created this private-repository prerelease:

```text
https://github.com/0xchasercat/firestone/releases/tag/virtiofsd-v1.14.0-firestone.1
target: 0f9734b6c0f14c4f5176dc34a2faf57879c67132
draft: false
prerelease: true
```

It contains deterministic tar packages and raw installer binaries:

| Asset | SHA-256 |
|---|---|
| `virtiofsd-v1.14.0-x86_64-unknown-linux-musl` | `9ad3e33c45dd816b24ad483b60ca469974ba54c3b37ef93be3da2a623986646f` |
| `virtiofsd-v1.14.0-aarch64-unknown-linux-musl` | `e45bd62e346eca87857279d5680782e80148379fbca524a648089f642ac001d2` |
| `virtiofsd-v1.14.0-x86_64-unknown-linux-musl.tar` | `014d575701c5ecce57b31ff30ddcea684bc8db2952d82c9f22983ecf8816aa2a` |
| `virtiofsd-v1.14.0-aarch64-unknown-linux-musl.tar` | `cdf5e8015286e5700b019dcecb1059e7eb9f15b93377506c91b9190923a94382` |

Authenticated `gh release download` readback matched every byte. The repository is private, so unauthenticated release downloads return HTTP 404. `doctor --fix` cannot consume these URLs.

Do not make the repository public without explicit user approval. Resolve one of these product decisions first:

- make the Firestone repository public;
- create a separate public release-assets repository;
- publish the same reviewed bytes to user-approved public object storage.

After public hosting exists, finish `agent/m0-virtiofsd-pins` by downloading the public raw binaries, deriving their hashes from the downloaded bytes, generating `deps.toml`, updating verification evidence, and opening a PR.

## Unfinished virtiofsd pin worktree

Worktree:

```text
/Users/chaser/firestone-worktrees/m0-virtiofsd-pins
branch: agent/m0-virtiofsd-pins
base: 0f9734b6c0f14c4f5176dc34a2faf57879c67132
remote branch: not pushed
```

The worktree has one uncommitted file:

```text
M scripts/pin-deps.sh
```

The patch adds Firestone release URLs, binary availability, architecture records, verification, canonical manifest generation, and live refresh downloads. `bash -n` passes. `deps.toml` is unchanged because `refresh --arch all` reached the private raw asset URL and received HTTP 404 before writing the manifest.

Treat this patch as a useful draft. Replace the release URLs only after public hosting is approved. Then run:

```sh
scripts/pin-deps.sh refresh --arch all
scripts/pin-deps.sh verify --arch all
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Run the same exact-tree gate on `firestone@172.203.242.136` and confirm `/dev/kvm` remains readable and writable. Update `docs/verification/dependencies.md`, `docs/verification/virtiofsd-distribution.md`, and only the M0-05c status row. Open a PR. Do not mark M0-05c complete until the public URLs work without authentication and the pin PR merges.

## Unfinished doctor branch

Worktree:

```text
/Users/chaser/firestone-worktrees/m0-doctor
branch: agent/m0-doctor
HEAD: 56f3129faa03935177b2e79d5884c53c160dfce8
remote: origin/agent/m0-doctor at the same commit
base: c49f39b233609e6131336d4a7829861c5a402561
PR: none
worktree: clean
```

Commits:

```text
0c0c72c doctor: implement host diagnostics and safe fixes
c77f766 doctor: harden process and filesystem probes
91116d8 doctor: bound probes and unsafe inputs
56f3129 doctor: quote key permission fixes safely
```

The branch implements 13 deterministic doctor checks, bounded HTTPS downloads, safe dependency installs, `Cmd` process timeouts and bounded output, passt capability/version checks, KVM and nested-virtualization checks, SSH key handling, and stale-state report inputs.

Before opening a PR:

1. Rebase onto current `origin/main`.
2. Build `DoctorContext` from the merged `Paths` instance instead of raw paths.
3. Use `Paths::validate_runtime_dir()` for read-only checks and `Paths::ensure_runtime_dir()` only for `--fix`.
4. Integrate real stale-state enumeration through sorted machine names, `MachineLock`, `StateStore`, `observe_liveness`, `reconcile`, and `write_reconciliation`.
5. Convert per-machine failures into report entries without aborting the other checks.
6. Preserve the rule that Ubuntu 24.04's old passt package is not advertised as a fix for the required vhost-user capability.
7. Re-run local, Rust 1.85, Azure Linux, and real-host probe/fix tests. Open a bounded PR and do not merge from the branch.

The earlier independent review findings were addressed in `91116d8` and `56f3129` except for the merged-Paths and real-state integration described above.

## CLI and M0 integration

M0-04 is unblocked but has not started. Create `agent/m0-cli` from current main. Implement only the M0 commands and adapters required by SPEC sections 7.4 and 15:

- parser and shared `MachineSpecPatch` projection, including repeatable typed `--clear`;
- deterministic TTY, non-TTY, and JSON renderers;
- `create`, `ls`, `show`, and `edit`;
- dispatcher adapters using the shared `Action`, `Event`, result payloads, errors, `Paths`, catalog, locks, state, and spec loader;
- no-KVM acceptance: `firestone create ubuntu --cpus 4 && firestone ls`.

Keep `firestone-core` independent of clap, indicatif, and axum. Do not add commands, flags, keys, or behavior absent from the SPEC.

After pins, doctor, and CLI merge, run M0-06 integration locally, in GitHub CI, and on Azure. Update the status document after each merge.

## Infrastructure

```text
Azure Linux/KVM: ssh firestone@172.203.242.136
Bare metal:      ssh w
Git remote:      git@github.com:0xchasercat/firestone.git
```

Azure is Ubuntu 24.04 x86_64 with `/dev/kvm` readable and writable. Rust stable 1.98.0 and Rust 1.85.0 are installed. The VM has limited disk space, so prune image artifacts between matrix tests. Use `w` only for Docker/bare-metal behavior or checks Azure cannot support.

## Review and security history

PR #6 and PR #7 received independent adversarial reviews, exact-head graph/source checks, local and Linux gates, and scoped security diff scans. No reportable security finding remained. A RustSec scan flagged locked `rsa 0.9.10` through `ssh-key`, but the `rsa` feature is inactive in the all-target resolved graph and Firestone only parses public OpenSSH keys.

The completed Codex session hit a known app-server MCP lifecycle leak. Do not recreate the large subagent tree in the same Codex host. Codebase Memory was useful for structural call and impact queries but not enough to justify a globally inherited MCP process. Its standalone CLI remains available:

```sh
codebase-memory-mcp cli <tool> --project Users-chaser-firestone ...
```

Native `rg`, direct source reads, compiler checks, and tests are the default for this handoff.

## Repository rules that must remain true

- Every path comes from `Paths`.
- Every external process starts through `Cmd`.
- CLI, config, and REST project the same shared contracts.
- Never log cloud-init contents.
- No `unwrap` or `expect` outside tests.
- JSON and non-TTY output remain deterministic.
- Do not guess third-party flags, schemas, protocols, or behavior.
- Record every resolved SPEC section 20 verification item in the decision log.
- Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` before every commit.
- Verify integration candidates on Linux and use KVM/bare metal in proportion to the changed behavior.
