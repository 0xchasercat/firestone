# Project status

This file is the durable handoff for Firestone development. The orchestrator updates it on `main` after every merge wave. Agents update only their assigned row when a pull request changes its scope or status.

## Current position

- Milestone: M0, skeleton
- Baseline: M0 core contracts, state, catalog, dependency evidence, and reproducible virtiofsd build recipe
- Linux validation host: `firestone@172.203.242.136`, Ubuntu 24.04 x86_64, `/dev/kvm` present
- Required local gate: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`
- Required integration gate: the same commands on the Linux host
- M0 acceptance: all section 19.1 tests except cloud-init and VmConfig pass; `firestone create ubuntu --cpus 4 && firestone ls` works without KVM

## Work queue

| ID | Work | Branch | PR | Status | Depends on | Acceptance |
|---|---|---|---|---|---|---|
| M0-00 | Workspace, crate boundaries, typed errors, events, actions, dispatcher seam, CI | `agent/m0-foundation` | [#1](https://github.com/0xchasercat/firestone/pull/1) | complete | baseline | Workspace builds; architecture boundaries compile; CI runs the required gate |
| M0-01 | Paths, spec and patch model, layering, validation, drift coverage | `agent/m0-spec` | [#6](https://github.com/0xchasercat/firestone/pull/6) | complete | M0-00 | SPEC sections 5 through 7 unit coverage passes |
| M0-02 | Atomic files, machine state, locking, liveness reconciliation | `agent/m0-state` | [#4](https://github.com/0xchasercat/firestone/pull/4) | complete | M0-00 | Reconcile matrix and lock contention tests pass |
| M0-03a | Built-in catalog data and authoritative source validation | `agent/m0-catalog-data` | [#2](https://github.com/0xchasercat/firestone/pull/2) | complete | baseline | Initial entries have current per-architecture URLs and checksum sources; unbooted entries are marked as release gates |
| M0-03b | Image reference parsing, catalog merge, architecture selection, resolution | `agent/m0-catalog` | [#5](https://github.com/0xchasercat/firestone/pull/5) | complete | M0-00, M0-03a | Catalog rules from sections 8.1 and 8.2 pass without network |
| M0-04 | CLI parser, renderers, create/list/show/edit, dispatcher adapters | `agent/m0-cli` | pending | ready | M0-01, M0-02, M0-03b | M0 no-KVM command acceptance and renderer snapshots pass |
| M0-05a | Dependency manifest, real pins, download/hash tooling, third-party evidence | `agent/m0-deps` | [#3](https://github.com/0xchasercat/firestone/pull/3) | complete | baseline | Real per-architecture URLs and checksums; verified behavior recorded against exact versions |
| M0-05b | Host checks, doctor report, unprivileged fixes, deterministic tests | `agent/m0-doctor` | pending | in progress | M0-01, M0-02, M0-05a | Section 17.3 checks and safe fixes pass without KVM |
| M0-05c | Reproducible virtiofsd builds and Firestone-owned release assets | `agent/m0-virtiofsd-pins` | [#7](https://github.com/0xchasercat/firestone/pull/7) + follow-up pending | ready | M0-05a | Both musl targets build from pinned inputs; published assets have verified immutable URLs and checksums in `deps.toml` |
| M0-06 | M0 integration, docs, Linux verification | `agent/m0-integration` | pending | blocked | M0-01 through M0-05c | M0 acceptance is green locally, in CI, and on Linux |

## Merge order

1. M0-00 foundation
2. M0-03a and M0-05a when their evidence is complete
3. M0-01, M0-02, M0-03b, M0-05b, and M0-05c
4. M0-04
5. M0-06 integration

The orchestrator reviews each pull request for spec alignment, public contract drift, unsafe filesystem or process behavior, test quality, and dependency direction. A passing branch gate is necessary but does not replace review.

## Infrastructure state

| Resource | State | Next action |
|---|---|---|
| GitHub remote | `git@github.com:0xchasercat/firestone.git`; `main` baseline pushed | All implementation enters through reviewed pull requests |
| Local macOS toolchain | Rust 1.97.1, Cargo 1.97.0 | Use for fast unit feedback; Linux behavior requires remote validation |
| Azure Linux host | Reachable; fresh SSH sessions have KVM read/write access; Rust 1.98.0 and M0 build/runtime tools installed | Run every integration candidate here before merge |
| Bare-metal host `w` | Available | Use only for behavior that nested Azure KVM cannot validate |

## Completed work

- M0-00 merged in PR [#1](https://github.com/0xchasercat/firestone/pull/1). It established the Rust workspace, closed shared action and event contracts, typed errors, the dispatcher seam, and the required CI gate. The merge passed local, GitHub-hosted Ubuntu, and Azure Linux verification.
- M0-03a merged in PR [#2](https://github.com/0xchasercat/firestone/pull/2). It added ten source-validated Ubuntu, Debian, and Fedora catalog entries plus a bounded HTTPS validator. All entries remain behind the explicit boot-to-SSH release gate.
- M0-05a merged in PR [#3](https://github.com/0xchasercat/firestone/pull/3). It pinned Cloud Hypervisor v53.0, RHF 0.5.0, the VMM-tested edk2 build, and virtiofsd 1.14.0 source. It also resolved the source-level portions of verify items 1, 2, and 12 against exact versions.
- M0-02 merged in PR [#4](https://github.com/0xchasercat/firestone/pull/4). It added durable atomic state writes, per-machine locking, verified shim identity, and exhaustive liveness reconciliation. Linux subprocess contention and `/proc` checks passed on the Azure host.
- M0-03b merged in PR [#5](https://github.com/0xchasercat/firestone/pull/5). It added deterministic built-in and override catalog parsing, strict HTTPS/source validation, and default, alias, version, and architecture resolution.
- M0-01 merged in PR [#6](https://github.com/0xchasercat/firestone/pull/6). It added the shared machine spec and patch model, typed clears, deterministic layering and persistence, startup path resolution, strict validation, scalar and port-forward types, schema/metadata drift gates, and secure runtime-directory ancestry checks.
- The reproducible virtiofsd recipe merged in PR [#7](https://github.com/0xchasercat/firestone/pull/7). Both required musl targets reproduce byte-for-byte in GitHub and on bare metal, and deterministic tar packages preserve modes and provenance. The private-repository prerelease exists, but unauthenticated downloads return 404, so it cannot become a supported dependency pin yet.

## Known risks

- Runtime portions of the SPEC section 20 checks remain open unless the decision log explicitly records exact-version evidence.
- The Azure VM has only 29 GB total storage. Image-matrix tests must prune artifacts between runs.
- aarch64 runtime testing needs a separate native host or CI runner. Cross-compilation alone cannot close M5 acceptance.
