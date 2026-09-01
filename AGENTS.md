# Firestone working rules

Read `SPEC.md` before changing code. It is the source of truth. Normative sections require a decision-log entry in SPEC.md section 21 when changed.

Read `docs/PROJECT_STATUS.md` before starting work. Update the row for your task in the same pull request if its scope, dependencies, acceptance criteria, or status changed.

## Architecture rules

- Keep `firestone-core` independent of clap, indicatif, and axum.
- Project the same `MachineSpec`, `MachineSpecPatch`, `Action`, `Event`, dispatcher, errors, and result payloads through the CLI, config, and REST API.
- Resolve every path through `Paths`.
- Start every external process through the shared `Cmd` wrapper.
- Do not guess third-party flags, JSON fields, socket protocols, or behavior. Check the pinned binary, its man page, source, or OpenAPI document. Record every resolved `[verify N]` item from SPEC.md section 20 in the decision log.
- Do not add features, flags, config keys, or public behavior absent from SPEC.md.

## Code rules

- Use stable Rust, edition 2024.
- Do not use `unwrap` or `expect` outside tests.
- Every actionable error has a stable kind, concrete context, and a useful hint.
- Keep user-visible output deterministic under `--json` and when stderr is not a TTY.
- Never log cloud-init contents.
- Add unit or integration coverage for every behavior changed.
- Test names use `subject_condition_expected`.
- In `crates/firestone/assets/ui/app.js`, keep braces and quotes out of regular-expression literals: use a character class (`[{]`) or `new RegExp("…")`. The `the_embedded_runtime_script_closes_every_block` guard counts braces outside comments and strings, and a literal brace or quote inside a pattern makes it fail on correct code.

Before committing, run:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Commits use `area: what`. Explain the reason in the body when the title cannot.

## Branch and pull request rules

- The orchestrator owns `main`. Agents work in isolated worktrees on branches named `agent/<task>`.
- Rebase or merge the current `origin/main` before final verification.
- Push the branch and open a pull request. Do not merge it.
- The pull request body lists SPEC sections implemented, files owned, tests run, Linux/KVM verification performed, and remaining limits.
- Keep pull requests bounded to one row in `docs/PROJECT_STATUS.md` unless the orchestrator changes the assignment.

## Codebase Memory

Use the indexed codebase graph before filesystem search for structural discovery:

1. `search_graph`
2. `trace_path`
3. `get_code_snippet`
4. `check_index_coverage`
5. `query_graph`
6. `get_architecture`

Use source search for literals, configuration, and graph coverage gaps. Check index coverage for every code path relied on before making exhaustive claims.

