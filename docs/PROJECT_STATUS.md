# Project status

This file is the durable handoff for Firestone development. The orchestrator updates it on `main` after every merge wave. Agents update only their assigned row when a pull request changes its scope or status.

## Current position

- Milestone: M5 Linux MVP implementation; M0 through M2 are complete, M3/M4 code is merged, and their KVM acceptance is externally blocked
- Baseline: all shared lifecycle, shell, sidecar, REST, Unix serve, and real-process equivalence behavior through M4 is merged and green in Ubuntu CI
- Linux validation host: `firestone@172.203.242.136`, Ubuntu 24.04 x86_64, `/dev/kvm` present
- Required local gate: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`
- Required integration gate: the same commands on the Linux host
- KVM blocker: the only validation host times out on TCP/22; M3 E2E 3/4/8, M4 E2E 9, and final KVM acceptance remain open with secure gated harnesses merged or staged

## Work queue

| ID | Work | Branch | PR | Status | Depends on | Acceptance |
|---|---|---|---|---|---|---|
| M0-00 | Workspace, crate boundaries, typed errors, events, actions, dispatcher seam, CI | `agent/m0-foundation` | [#1](https://github.com/0xchasercat/firestone/pull/1) | complete | baseline | Workspace builds; architecture boundaries compile; CI runs the required gate |
| M0-01 | Paths, spec and patch model, layering, validation, drift coverage | `agent/m0-spec` | [#6](https://github.com/0xchasercat/firestone/pull/6) | complete | M0-00 | SPEC sections 5 through 7 unit coverage passes |
| M0-02 | Atomic files, machine state, locking, liveness reconciliation | `agent/m0-state` | [#4](https://github.com/0xchasercat/firestone/pull/4) | complete | M0-00 | Reconcile matrix and lock contention tests pass |
| M0-03a | Built-in catalog data and authoritative source validation | `agent/m0-catalog-data` | [#2](https://github.com/0xchasercat/firestone/pull/2) | complete | baseline | Initial entries have current per-architecture URLs and checksum sources; unbooted entries are marked as release gates |
| M0-03b | Image reference parsing, catalog merge, architecture selection, resolution | `agent/m0-catalog` | [#5](https://github.com/0xchasercat/firestone/pull/5) | complete | M0-00, M0-03a | Catalog rules from sections 8.1 and 8.2 pass without network |
| M0-04 | CLI parser, renderers, create/list/show/edit/doctor, dispatcher adapters | `agent/m0-cli` | [#9](https://github.com/0xchasercat/firestone/pull/9) | complete | M0-01, M0-02, M0-03b, M0-05a, M0-05b | M0 no-KVM command acceptance and renderer snapshots pass |
| M0-05a | Dependency manifest, real pins, download/hash tooling, third-party evidence | `agent/m0-deps` | [#3](https://github.com/0xchasercat/firestone/pull/3) | complete | baseline | Real per-architecture URLs and checksums; verified behavior recorded against exact versions |
| M0-05b | Host checks, doctor report, unprivileged fixes, deterministic tests | `agent/m0-doctor` | [#8](https://github.com/0xchasercat/firestone/pull/8) | complete | M0-01, M0-02, M0-05a | Section 17.3 checks and safe fixes pass without KVM |
| M0-05c | Reproducible virtiofsd builds and Firestone-owned release assets | `agent/m0-virtiofsd-pins` | [#7](https://github.com/0xchasercat/firestone/pull/7) + [#10](https://github.com/0xchasercat/firestone/pull/10) | complete | M0-05a | Both musl targets build from pinned inputs; published assets have verified immutable URLs and checksums in `deps.toml` |
| M0-06 | M0 integration, docs, Linux verification | `main` | [#8](https://github.com/0xchasercat/firestone/pull/8) + [#9](https://github.com/0xchasercat/firestone/pull/9) + [#10](https://github.com/0xchasercat/firestone/pull/10) | complete | M0-01 through M0-05c | M0 acceptance is green locally, in CI, and on Linux |
| M1-01 | Image store, pull/verify, raw conversion, overlay, remove, prune | `agent/m1-images` | [#13](https://github.com/0xchasercat/firestone/pull/13) | complete | M0 | Immutable source identity, owned qcow2 storage, overlay and image lifecycle tests pass |
| M1-02 | Deterministic cloud-init seed and canonical typed VmConfig | `agent/m1-seed-vmconfig` | [#12](https://github.com/0xchasercat/firestone/pull/12) | complete | M0 | CIDATA golden and every VmConfig mapping/overlay contract pass without KVM |
| M1-03 | Bounded Cloud Hypervisor Unix-socket API client | `agent/m1-vmm-api` | [#11](https://github.com/0xchasercat/firestone/pull/11) | complete | M0 | Exact v53 methods, status codes, framing limits, and error bodies are covered against a fake server |
| M1-04 | Shim process supervision, launch, status, stop, and cleanup | `agent/m1-shim` | [#14](https://github.com/0xchasercat/firestone/pull/14) | complete | M1-01, M1-02, M1-03 | Linux x86_64 acceptance uses pinned Cloud Hypervisor v53 + edk2; ordered launch/stop, ownership cleanup, deadlines, lock lifetime, client detachment, child reaping, and verified crash recovery pass. aarch64 runtime, non-Linux authority, and hostile wrapper containment are deferred |
| M1-05 | Lifecycle and image CLI integration | `agent/m1-lifecycle` | [#15](https://github.com/0xchasercat/firestone/pull/15) | complete | M1-01 through M1-04 | Linux x86_64 fake-VMM lifecycle covers shared start/stop/restart/rm/images/logs/show-vmconfig actions, deterministic rendering, bounded secure logs, safe deletion, and supervised/unsupervised stop; real KVM evidence remains M1-06 |
| M1-06 | M1 KVM integration and verify evidence | `agent/m1-integration` | [#16](https://github.com/0xchasercat/firestone/pull/16) | complete | M1-01 through M1-05 | Linux x86_64 E2E 1, 5, 6, and 7 pass under pinned Cloud Hypervisor v53 + edk2; serial login and verify 1, 2, 4, 5, 6, 9, and 12 are resolved |
| M2-01 | SSH identity, host-key trust, and vsock proxy transport | `agent/m2-transport` | [#17](https://github.com/0xchasercat/firestone/pull/17) | complete | M1 | Exact SSH/vsock transport contracts, key permissions, trust rotation, bounded handshake, cancellation, and binary relay pass without KVM |
| M2-02 | Guest SSH units and cloud-init integration | `agent/m2-guest` | [#18](https://github.com/0xchasercat/firestone/pull/18) | complete | M1 | Typed deterministic guest units activate key-only root/default-user SSH, coexist with native systemd vsock, preserve first-boot hvc0 rescue, and KVM verify 11 and 17 pass |
| M2-03 | Readiness, shell, ssh-config, console, and run CLI | `agent/m2-cli` | [#19](https://github.com/0xchasercat/firestone/pull/19) | complete | M2-01, M2-02 | Shared actions, deterministic output, interactive exec, no-wait behavior, and readiness transitions pass |
| M2-04 | M2 Linux KVM integration | `agent/m2-integration` | [#20](https://github.com/0xchasercat/firestone/pull/20) | complete | M2-01 through M2-03 | E2E 2 and 10 pass; empty-home run reaches a root prompt; verify 11, 13, and 17 close |
| M3-01 | Passt, forward grammar, and tap network plans | `agent/m3-network` | [#23](https://github.com/0xchasercat/firestone/pull/23) | complete | M2 | Exact pinned passt/tap commands, socket/config plans, forward validation, and bounded errors pass without KVM |
| M3-02 | Virtiofsd mount plans and VmConfig mapping | `agent/m3-virtiofs` | [#22](https://github.com/0xchasercat/firestone/pull/22) | complete | M2 | Exact pinned virtiofsd commands, tags, read-only mapping, ownership, and bounded errors pass without KVM |
| M3-03 | User cloud-init parts, SSH keys, and instance identity | `agent/m3-cloudinit` | [#21](https://github.com/0xchasercat/firestone/pull/21) | complete | M2 | Canonical user-first MIME, bounded path inputs, key de-duplication/order, network-config publication, exact byte identities, target cloud-init merge evidence, and deterministic seed goldens pass |
| M3-04 | Network/filesystem sidecars and CLI lifecycle integration | `agent/m3-sidecars` | [#24](https://github.com/0xchasercat/firestone/pull/24) | complete | M3-01 through M3-03 | Exact prepared plans drive VmConfig/results; shim launch, rollback, degradation, VMM-first stop, identity-safe reaping, and Linux recovery cover passt plus ordered virtiofsd sidecars; local stable/1.85 and final-head Ubuntu x86_64 CI pass, while pinned KVM reruns remain M3-05 |
| M3-05 | M3 Linux KVM integration | `agent/m3-integration` | [draft #25](https://github.com/0xchasercat/firestone/pull/25) | blocked | M3-01 through M3-04 | Harness/static gates pass; E2E 3, 4, 8 and verify 7, 8, 14, 16 await a reachable KVM host; verify 10 and 15 remain resolved |
| M4-01 | Axum REST router, routes, streaming, and error mapping | `agent/m4-api` | [#26](https://github.com/0xchasercat/firestone/pull/26) | complete | M3-04 | Every SPEC route projects shared actions/results, NDJSON framing and limits are exact, handler tests pass with a mocked Dispatcher, and local stable/1.85 plus final-head Ubuntu x86_64 CI are green |
| M4-02 | Serve CLI and HTTP runtime integration | `agent/m4-serve` | [#27](https://github.com/0xchasercat/firestone/pull/27) | complete | M4-01 | Unix-only mode-0600 listener, secure stale/conflict handling, identity-safe cleanup, concurrent requests, bounded signal drain, and real CLI/curl smoke pass; final Ubuntu CI is green |
| M4-03 | M4 REST equivalence and E2E integration | `agent/m4-integration` | [#28](https://github.com/0xchasercat/firestone/pull/28) | blocked | M4-01, M4-02 | Real CLI/Unix-serve equivalence, streaming, 204, locking, disconnect, and restart pass; secure E2E 9 harness is merged, but E2E 9 awaits the unreachable KVM host |
| M5-01 | CLI progress, timing, output polish, and error hints | `agent/m5-cli-polish` | pending | ready | M4 implementation | TTY progress is exact and responsive; non-TTY/JSON stay deterministic; every reachable error kind has actionable stable context and hint |
| M5-02 | Completions, version, and Linux release artifacts | `agent/m5-release` | pending | ready | M4 implementation | Shell completions, version metadata, x86_64 musl release, checksums, and aarch64 compile-only path pass; aarch64 runtime remains user-deferred |
| M5-03 | Catalog gate, fresh-host doctor matrix, and user guide | `agent/m5-docs-catalog` | pending | ready | M4 implementation | Catalog sources are authoritative, doctor fixes validate on Ubuntu/Fedora/Arch containers, and the user guide matches the Linux MVP |
| M5-04 | Final Linux x86_64 MVP acceptance | `agent/m5-integration` | pending | blocked | M3-05, M4-03, M5-01 through M5-03 | Full Linux x86_64 E2E and release gates pass; aarch64 runtime, non-Linux authority, and hostile wrappers remain explicitly deferred |

## Merge order

1. M0-00 foundation
2. M0-03a and M0-05a when their evidence is complete
3. M0-01, M0-02, M0-03b, M0-05b, and M0-05c
4. M0-04
5. M0-06 integration
6. M1-01, M1-02, and M1-03 in parallel
7. M1-04 shim integration
8. M1-05 lifecycle projection
9. M1-06 KVM integration

The orchestrator reviews each pull request for spec alignment, public contract drift, unsafe filesystem or process behavior, test quality, and dependency direction. A passing branch gate is necessary but does not replace review.

## Infrastructure state

| Resource | State | Next action |
|---|---|---|
| GitHub remote | `git@github.com:0xchasercat/firestone.git`; `main` baseline pushed | All implementation enters through reviewed pull requests |
| Local macOS toolchain | Rust 1.97.1, Cargo 1.97.0 | Use for fast unit feedback; Linux behavior requires remote validation |
| Azure Linux host | Reachable; fresh SSH sessions have KVM read/write access; Rust 1.98.0 and M1 build/runtime tools installed | Run every integration candidate and M1 KVM acceptance here before merge |
| Bare-metal host `w` | Available | Use only for behavior that nested Azure KVM cannot validate |

## Completed work

- M0-00 merged in PR [#1](https://github.com/0xchasercat/firestone/pull/1). It established the Rust workspace, closed shared action and event contracts, typed errors, the dispatcher seam, and the required CI gate. The merge passed local, GitHub-hosted Ubuntu, and Azure Linux verification.
- M0-03a merged in PR [#2](https://github.com/0xchasercat/firestone/pull/2). It added ten source-validated Ubuntu, Debian, and Fedora catalog entries plus a bounded HTTPS validator. All entries remain behind the explicit boot-to-SSH release gate.
- M0-05a merged in PR [#3](https://github.com/0xchasercat/firestone/pull/3). It pinned Cloud Hypervisor v53.0, RHF 0.5.0, the VMM-tested edk2 build, and virtiofsd 1.14.0 source. It also resolved the source-level portions of verify items 1, 2, and 12 against exact versions.
- M0-02 merged in PR [#4](https://github.com/0xchasercat/firestone/pull/4). It added durable atomic state writes, per-machine locking, verified shim identity, and exhaustive liveness reconciliation. Linux subprocess contention and `/proc` checks passed on the Azure host.
- M0-03b merged in PR [#5](https://github.com/0xchasercat/firestone/pull/5). It added deterministic built-in and override catalog parsing, strict HTTPS/source validation, and default, alias, version, and architecture resolution.
- M0-01 merged in PR [#6](https://github.com/0xchasercat/firestone/pull/6). It added the shared machine spec and patch model, typed clears, deterministic layering and persistence, startup path resolution, strict validation, scalar and port-forward types, schema/metadata drift gates, and secure runtime-directory ancestry checks.
- The reproducible virtiofsd recipe merged in PR [#7](https://github.com/0xchasercat/firestone/pull/7). Both required musl targets reproduce byte-for-byte in GitHub and on bare metal, and deterministic tar packages preserve modes and provenance.
- Host diagnostics merged in PR [#8](https://github.com/0xchasercat/firestone/pull/8). It added the deterministic 13-check doctor report, safe unprivileged repairs, process-group timeouts, and live state reconciliation.
- Public virtiofsd pins merged in PR [#10](https://github.com/0xchasercat/firestone/pull/10). Anonymous versioned assets for x86_64 and aarch64 match the immutable SHA-256 values in `deps.toml` and the reproducible source recipe.
- The M0 CLI merged in PR [#9](https://github.com/0xchasercat/firestone/pull/9). It projects create, list, show, edit, and doctor through shared contracts; emits deterministic human and NDJSON output; preserves crash-safe publications; validates owned data paths; and retains live supervision state.
- M0 integration completed on main at merge `de97ad5`. The 340-test gate, public dependency verification, create/list smoke, interrupted-publication recovery, and unsafe-path refusal passed on macOS, GitHub CI, and the Azure Linux host.

- M1 image storage merged in PR [#13](https://github.com/0xchasercat/firestone/pull/13). It added bounded immutable qcow2 storage, strict sidecars, cache generations, crash recovery, descriptor-relative path creation, architecture-specific firmware, and overlay validation.
- M1 seed and VmConfig mapping merged in PR [#12](https://github.com/0xchasercat/firestone/pull/12). It added deterministic CIDATA publication, Firestone cloud-init, typed canonical Cloud Hypervisor v53 JSON, and invariant-preserving merge patches.
- M1 VMM transport merged in PR [#11](https://github.com/0xchasercat/firestone/pull/11). It added the bounded Unix-socket HTTP client, exact endpoint/status contracts, and status-only liveness probing. The integrated foundation has 431 passing tests on macOS and GitHub Linux CI.
## Known risks

- Runtime portions of the SPEC section 20 checks remain open unless the decision log explicitly records exact-version evidence.
- The Azure VM has only 29 GB total storage. Image-matrix tests must prune artifacts between runs.
- aarch64 runtime testing needs a separate native host or CI runner. Cross-compilation alone cannot close M5 acceptance.
