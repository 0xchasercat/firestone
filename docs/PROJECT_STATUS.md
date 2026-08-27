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
| M0-00 | Workspace, crate boundaries, typed errors, events, actions, dispatcher seam, CI | `agent/m0-foundation` | pending | ready | baseline | Workspace builds; architecture boundaries compile; CI runs the required gate |
| M0-01 | Paths, spec and patch model, layering, validation, drift coverage | `agent/m0-spec` | pending | blocked | M0-00 | SPEC sections 5 through 7 unit coverage passes |
| M0-02 | Atomic files, machine state, locking, liveness reconciliation | `agent/m0-state` | pending | blocked | M0-00 | Reconcile matrix and lock contention tests pass |
| M0-03 | Built-in catalog, image reference parsing, merge and resolution | `agent/m0-catalog` | pending | ready | baseline | Catalog rules from section 8.1 and 8.2 pass without network |
| M0-04 | CLI parser, renderers, create/list/show/edit, dispatcher adapters | `agent/m0-cli` | pending | blocked | M0-01, M0-02, M0-03 | M0 no-KVM command acceptance and renderer snapshots pass |
| M0-05 | Host checks, dependency manifest, verified pins, download/install support | `agent/m0-deps` | pending | ready | baseline | Real per-architecture URLs and checksums; doctor checks have deterministic tests |
| M0-06 | M0 integration, doctor command, docs, Linux verification | `agent/m0-integration` | pending | blocked | M0-01 through M0-05 | M0 acceptance is green locally, in CI, and on Linux |

## Merge order

1. M0-00 foundation
2. M0-01, M0-02, M0-03, and the non-overlapping pin work from M0-05
3. M0-04 plus remaining doctor work from M0-05
4. M0-06 integration

The orchestrator reviews each pull request for spec alignment, public contract drift, unsafe filesystem or process behavior, test quality, and dependency direction. A passing branch gate is necessary but does not replace review.

## Infrastructure state

| Resource | State | Next action |
|---|---|---|
| GitHub remote | `git@github.com:0xchasercat/firestone.git` configured; baseline push pending | Push `main`, then require pull-request integration |
| Local macOS toolchain | Rust 1.97.1, Cargo 1.97.0 | Use for fast unit feedback; Linux behavior requires remote validation |
| Azure Linux host | Reachable; KVM device exists; user not yet in `kvm` group | Add `firestone` to `kvm`, install Rust and M0 host tools |
| Bare-metal host `w` | Available | Use only for behavior that nested Azure KVM cannot validate |

## Completed work

No implementation work has merged yet.

## Known risks

- The third-party version pins and every SPEC section 20 assumption remain unverified.
- The Azure VM has only 29 GB total storage. Image-matrix tests must prune artifacts between runs.
- aarch64 runtime testing needs a separate native host or CI runner. Cross-compilation alone cannot close M5 acceptance.
