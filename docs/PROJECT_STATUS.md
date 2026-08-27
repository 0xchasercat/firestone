# Project status

This file is the durable handoff for Firestone development. The orchestrator updates it on `main` after every merge wave. Agents update only their assigned row when a pull request changes its scope or status.

## Current position

- Milestone: M0, skeleton
- Baseline: specification only
- Linux validation host: `firestone@172.203.242.136`, Ubuntu 24.04 x86_64, `/dev/kvm` present
- Required local gate: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`
- Required integration gate: the same commands on the Linux host
- M0 acceptance: all section 19.1 tests except cloud-init and VmConfig pass; `firestone create ubuntu --cpus 4 && firestone ls` works without KVM

## Work queue

| ID | Work | Branch | PR | Status | Depends on | Acceptance |
|---|---|---|---|---|---|---|
| M0-00 | Workspace, crate boundaries, typed errors, events, actions, dispatcher seam, CI | `agent/m0-foundation` | [#1](https://github.com/0xchasercat/firestone/pull/1) | complete | baseline | Workspace builds; architecture boundaries compile; CI runs the required gate |
| M0-01 | Paths, spec and patch model, layering, validation, drift coverage | `agent/m0-spec` | pending | in progress | M0-00 | SPEC sections 5 through 7 unit coverage passes |
| M0-02 | Atomic files, machine state, locking, liveness reconciliation | `agent/m0-state` | pending | in progress | M0-00 | Reconcile matrix and lock contention tests pass |
| M0-03a | Built-in catalog data and authoritative source validation | `agent/m0-catalog-data` | pending | in progress | baseline | Initial entries have current per-architecture URLs and checksum sources; unbooted entries are marked as release gates |
| M0-03b | Image reference parsing, catalog merge, architecture selection, resolution | `agent/m0-catalog` | pending | blocked | M0-00, M0-03a | Catalog rules from sections 8.1 and 8.2 pass without network |
| M0-04 | CLI parser, renderers, create/list/show/edit, dispatcher adapters | `agent/m0-cli` | pending | blocked | M0-01, M0-02, M0-03b | M0 no-KVM command acceptance and renderer snapshots pass |
| M0-05a | Dependency manifest, real pins, download/hash tooling, third-party evidence | `agent/m0-deps` | pending | in progress | baseline | Real per-architecture URLs and checksums; verified behavior recorded against exact versions |
| M0-05b | Host checks, doctor report, unprivileged fixes, deterministic tests | `agent/m0-doctor` | pending | blocked | M0-00, M0-05a | Section 17.3 checks and safe fixes pass without KVM |
| M0-06 | M0 integration, docs, Linux verification | `agent/m0-integration` | pending | blocked | M0-01 through M0-05b | M0 acceptance is green locally, in CI, and on Linux |

## Merge order

1. M0-00 foundation
2. M0-03a and M0-05a when their evidence is complete
3. M0-01, M0-02, M0-03b, and M0-05b
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

## Known risks

- The third-party version pins and every SPEC section 20 assumption remain unverified.
- The Azure VM has only 29 GB total storage. Image-matrix tests must prune artifacts between runs.
- aarch64 runtime testing needs a separate native host or CI runner. Cross-compilation alone cannot close M5 acceptance.
