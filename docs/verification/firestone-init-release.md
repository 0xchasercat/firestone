# `firestone-init` payload: build recipe and release runbook

This record covers the static guest PID 1 added as `crates/firestone-init` (SPEC §10.5) and the exact build-then-publish-then-pin steps that turn it into an embedded Firestone payload (SPEC §17.2). The crate, the shared frame crate, the config-disk writer, the `disks[1]` branch and the embed seam are complete and tested. Publication and pinning are a follow-up owned by the orchestrator, exactly like the `mkfs.ext4` helper, because both need artifact hashes that exist only after a reproducible build has run and a release has been created.

## What has to be built

| Item | Value |
|---|---|
| Crate | `crates/firestone-init` (binary `firestone-init`), plus `crates/firestone-initproto` |
| Target | `x86_64-unknown-linux-musl` |
| Guest path | `/sbin/firestone-init`, mode 0755, injected into the merged OCI rootfs (§8.5) |
| Kernel entry | `init=/sbin/firestone-init` on the fixed direct-boot command line (§9.5) |
| Runtime dependencies | none; the binary is statically linked and the guest has no loader guarantees |
| Size tuning | `opt-level = "z"` (per package), `lto = "fat"`, `panic = "abort"`, `strip = "symbols"` |

Two Cargo facts shape the recipe and are worth stating once, because they look like oversights otherwise:

- Cargo accepts `opt-level` in `[profile.release.package.<name>]` but rejects `lto` and `panic` there: both are profile-wide settings. The workspace therefore carries only `[profile.release.package.firestone-init] opt-level = "z"`, and the release build supplies the other two itself.
- `panic = "abort"` cannot be applied to the workspace `release` profile without changing how `firestone` itself unwinds. The payload build therefore uses its own profile (or `RUSTFLAGS="-C panic=abort"` with `-Z build-std` avoided) rather than mutating the shared one.

The recommended invocation, run inside the existing pinned `build/firestone` container so the toolchain, the `Cargo.lock` and the musl headers are the ones that image already verifies:

```sh
cargo build --locked --release \
  --target x86_64-unknown-linux-musl \
  --package firestone-init \
  --config 'profile.release.panic="abort"' \
  --config 'profile.release.lto="fat"'
```

Build it twice into separate target directories and require the two outputs to be byte-identical, the way `build/helpers` already does. Record the resulting ELF facts: `ELF64`, `EXEC` or `DYN` with no `PT_INTERP`, no `DT_NEEDED`, no build id, and no dynamic symbol table.

## What is already proven here

- `firestone-init` compiles for `x86_64-unknown-linux-musl` and for the host. Every Linux-only path is behind `cfg(target_os = "linux")`, so the workspace gate — `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` — passes on macOS with the pure modules fully covered, exactly as `shim.rs` does.
- Unsafe is confined to `crates/firestone-init/src/ffi.rs`, which denies `unsafe_op_in_unsafe_fn` and documents every block. `firestone-initproto` forbids `unsafe` outright.
- `crates/firestone-core/build.rs` already verifies and embeds a `firestone-init` asset when — and only when — `deps.toml` carries a `[dependency.firestone-init]` entry **and** `FIRESTONE_EMBEDDED_HELPERS_DIR` holds the named asset. Half a pin is a build failure; neither half is an ordinary development build.
- Until then, `firestone_core::firestone_init_payload()` returns kind `dependency` with a hint naming the missing release, so the OCI pull pipeline cannot inject nothing by accident.

## Release runbook (orchestrator)

Nothing below can be done from this pull request: every step needs bytes that only a release build produces.

1. Merge this change. Add a `firestone-init` job to the reproducible build workflow that runs the two-pass command above inside the pinned `build/firestone` container with no network beyond the vendored registry, requires the two passes to be byte-identical, and prints the ELF facts and the SHA-256.
2. Collect `firestone-init-<version>-x86_64-unknown-linux-musl` and its `SHA256SUMS` line. Re-run `sha256sum -c` locally before uploading anything.
3. Upload the asset to the Firestone release for that version, under exactly that name. It is Firestone's own source, so it needs no corresponding-source bundle beyond the repository tag it was built from; record the commit in the release notes.
4. Extend `scripts/pin-deps.sh` with `FIRESTONE_INIT_VERSION`, `FIRESTONE_INIT_X86_64_URL`, and a `write_manifest` section:

   ```toml
   [dependency.firestone-init]
   version = "<workspace version>"
   release_url = "https://github.com/<owner>/firestone/releases/tag/v<version>"
   availability = "binary"

   [dependency.firestone-init.x86_64]
   asset = "firestone-init-<version>-x86_64-unknown-linux-musl"
   install_name = "firestone-init-<version>"
   url = "<asset url>"
   sha256 = "<sha256>"
   ```

   with the matching `require_manifest_value`, `require_manifest_sha` and `verify_artifact` entries. Do not hand-edit checksums; regenerate with `scripts/pin-deps.sh refresh --arch all`, then `scripts/pin-deps.sh verify --arch all`.
5. Stage the built asset into `FIRESTONE_EMBEDDED_HELPERS_DIR` in the x86_64 standalone release build, beside `cloud-hypervisor-static`, `passt` and `qemu-img`. `build.rs` will then hash-verify and embed it; `FIRESTONE_REQUIRE_EMBEDDED_HELPERS=1` keeps a strict release honest.
6. Update the `firestone-init` row in SPEC §17.2 to name the published release, add the resulting hash to this document, and move the M6-17 row in `docs/PROJECT_STATUS.md` accordingly.

`deps.toml` is deliberately untouched by this pull request, so it cannot collide with the other pins in flight.

## Guest-side verification (once a payload exists)

Neither the injection nor the boot can be exercised without KVM and a real registry, so both belong to the end-to-end suite (§19.2). The checks that matter, in order:

1. The packed rootfs holds `/sbin/firestone-init`, mode 0755, uid/gid 0, with bytes equal to the embedded payload.
2. `console.log` shows `firestone-init: started ...` and, on a machine whose `disk` exceeds the image's ext4 size, `firestone-init: root filesystem grown to N blocks of 4096 bytes`.
3. On a `network.mode = "passt"` machine, `firestone-init: eth0 configured with <address>` appears within the 5 s budget, and `/etc/resolv.conf` exists.
4. On a `network.mode = "none"` machine, no DHCP warning appears at all and boot reaches the entrypoint measurably sooner.
5. `firestone stop` reaches the entrypoint as `SIGTERM` on its own process group, and the machine's `last_exit` records a clean exit rather than a timeout — that is the `reboot(RB_POWER_OFF)` path working.
