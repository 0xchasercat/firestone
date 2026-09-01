# Static `mkfs.ext4` helper: recipe evidence and release runbook

This record covers the reproducible `mkfs.ext4` recipe added to [`build/helpers`](../../build/helpers) and the exact publish-then-pin steps that turn it into a pinned Firestone dependency. The recipe is complete and tested. Publication and pinning are a follow-up owned by the orchestrator, because both need artifact hashes that exist only after the helpers workflow has run and the release has been created.

## What the recipe builds

| Item | Value |
|---|---|
| Upstream | e2fsprogs 1.47.3, `misc/mke2fs`, published under the name `mkfs.ext4` |
| Source | `https://cdn.kernel.org/pub/linux/kernel/people/tytso/e2fsprogs/v1.47.3/e2fsprogs-1.47.3.tar.xz` |
| Source SHA-256 | `857e6ef800feaa2bb4578fbc810214be5d3c88b072ea53c5384733a965737329` |
| Tar input library | Alpine libarchive 3.8.3 (`libarchive-3.8.3.tar.xz`, `90e21f2b89f19391ce7b90f6e48ed9fde5394d23ad30ae256fb8236b38b99788`) |
| Builder | the existing pinned `alpine@sha256:7c8cb692ae09657cbc4a3f3cbd0e8d5a2690ba38386aaaf252dbb060bf5eb2e6`, no network |
| Configure | `--with-libarchive=direct --disable-nls --disable-uuidd --disable-fuse2fs`, `LDFLAGS="-static -Wl,--build-id=none -Wl,-z,relro -Wl,-z,now"` |
| Make targets | `libs`, then `misc/mke2fs` with `LIBARCHIVE` overridden, then `debugfs` (dynamic, verification only, never published) |
| Output | stripped static ELF64 `EXEC`, no `PT_INTERP`, no `DT_NEEDED`, no build id, about 5.7 MB |
| Licenses | e2fsprogs GPL-2.0 (libext2fs and libe2p LGPL-2.0-or-later, libuuid BSD, libet and libss MIT); libarchive BSD-2-Clause; libacl LGPL-2.1-or-later; expat MIT; liblzma 0BSD; zstd BSD-3-Clause; lz4 BSD-2-Clause; bzip2 BSD-like; OpenSSL Apache-2.0 |

Three upstream properties are load-bearing and are asserted by the recipe and its tests, because each of them silently breaks the build when it drifts:

- The configure flag is `--with-libarchive=direct`. The plain `--with-libarchive` form selects the dlopen path, which cannot be linked statically.
- `misc/Makefile` hardcodes `LIBARCHIVE=-larchive`, which never closes a static link. The recipe passes the full `pkg-config --static --libs libarchive` closure at make time and requires `-larchive -lacl -lexpat -lzstd -llz4 -lbz2 -lz -llzma -lcrypto` to be present in it.
- `make all` fails: `debugfs` cannot link statically. The recipe builds only `libs` and `misc/mke2fs`, and builds `debugfs` separately with an emptied `LDFLAGS` purely to inspect the smoke image.

## Functional evidence

The container build creates a tar holding a `0741` regular file owned by uid/gid 12345, a symlink, a hard link to that file, and a character device node, runs `mkfs.ext4 -F -t ext4 -d layer.tar -b 4096 out.img 64M`, and then requires the freshly built `debugfs` to report, in `out.img`:

- `Type: regular    Mode:  0741` and `User: 12345   Group: 12345` for the file,
- `Links: 2` proving the hard link survived as one inode,
- `Fast link dest: "dir/file.txt"` for the symlink,
- `Device major/minor number: 01:03` for the device node.

`.github/workflows/helpers.yml` repeats the same smoke against the published artifact using the runner's own `debugfs`, and requires `mke2fs 1.47.3 (8-Jul-2025)` from `mkfs.ext4 -V`.

The recipe was exercised end to end before it was written down. The locked package closure was installed offline in the pinned builder image with `apk add --no-network --repositories-file /dev/null`, `pkg-config --static --libs libarchive` returned `-larchive -lacl -lexpat -lzstd -llz4 -lbz2 -lz -llzma -lcrypto -ldl -pthread`, and the exact configure, make, strip and ELF checks in `build-in-container.sh` produced a 5,688,712-byte static `EXEC` with no `PT_INTERP`, no `DT_NEEDED` and no build id, which then passed every smoke assertion above. That run used an `linux/amd64` container on an arm64 development host, so it establishes the recipe and its assertions, not a publishable hash; the authoritative bytes come from the native x86_64 workflow build. `build/helpers/tests/test-build-scripts.sh` guards the lock contents, lock counts, the three upstream invariants above, the smoke assertions, and the double-build byte-identity path.

## Release runbook (orchestrator)

Nothing below can be done from this pull request: every step needs bytes that only the workflow produces.

1. Merge the recipe. Run the `Static helper reproducibility` workflow (`workflow_dispatch`) on the merge commit. The `build` job fetches the locked inputs, builds twice with no network, requires byte-identical outputs, and prints `helpers.build-info`.
2. Collect the output directory: `passt`, `qemu-img`, `mkfs.ext4`, `LICENSES/`, `firestone-static-helpers-corresponding-source.tar`, `helpers.build-info`, and `SHA256SUMS`. Re-run `sha256sum -c SHA256SUMS` locally before uploading anything.
3. Create the release tag `helpers-v0.2.0-firestone.1`. It supersedes `helpers-v0.1.0-firestone.1` because `packages.lock` and `sources.lock` both grew: the same run republishes `passt` and `qemu-img` alongside the new helper. Upload, with these exact asset names:
   - `passt-2025_02_17.a1e48a0-x86_64-unknown-linux-musl`
   - `qemu-img-8.2.2-x86_64-unknown-linux-musl`
   - `mkfs.ext4-1.47.3-x86_64-unknown-linux-musl`
   - `firestone-static-helpers-v0.2.0-corresponding-source.tar`
   - `firestone-static-helpers-v0.2.0-build-info.txt`
4. Re-upload the input mirrors that `sources.lock` resolves from a Firestone release, so the new tag is self-contained: `passt-a1e48a02ff3550eb7875a7df6726086e9b3a1213.tar.xz` and the six `aports-*.tar.gz` snapshots currently hosted on `helpers-v0.1.0-firestone.1`, byte for byte. Then add aports packaging snapshots for the libraries this recipe newly links statically (`libarchive`, `acl`, `expat`, `xz`, `zstd`, `lz4`, `bzip2`, `openssl`) and add them to `sources.lock` in the same follow-up. Until that lands, the corresponding-source bundle carries each upstream tarball and each installed `.apk` for those libraries, but not their Alpine `APKBUILD` files.
5. Compare the republished `passt` and `qemu-img` hashes with the values already in `deps.toml`. They should be unchanged, since no input they link against moved; if either differs, re-pin it in the same change and say so in the release notes.
6. Extend `scripts/pin-deps.sh`:
   - `HELPERS_RELEASE_TAG="helpers-v0.2.0-firestone.1"`, and the corresponding-source and build-info asset names to their `v0.2.0` spellings.
   - New constants `MKFS_EXT4_VERSION="1.47.3"`, `MKFS_EXT4_X86_64_URL="$HELPERS_ASSET_BASE/mkfs.ext4-1.47.3-x86_64-unknown-linux-musl"`, `E2FSPROGS_SOURCE_URL`, and `LIBARCHIVE_SOURCE_URL`.
   - New `write_manifest` sections `[dependency.mkfs-ext4]` with `architectures = ["x86_64"]` and `[dependency.mkfs-ext4.x86_64]` (`asset = "mkfs.ext4-1.47.3-x86_64-unknown-linux-musl"`, `install_name = "mkfs.ext4-1.47.3"`), plus `[helper.e2fsprogs]` / `[helper.e2fsprogs.source]` and `[helper.libarchive]` / `[helper.libarchive.source]` carrying the two SHA-256 values in the table above.
   - Matching `require_manifest_value`, `require_manifest_sha`, and x86_64 `verify_artifact` entries, and the extra positional arguments in `write_manifest` and `verify_manifest_shape`.
7. Regenerate and verify the manifest with `scripts/pin-deps.sh refresh --arch all`, then `scripts/pin-deps.sh verify --arch all`. Do not hand-edit checksums.
8. Update the `mkfs.ext4` row in SPEC section 17.2 to name the published release, add the resulting hashes to this document, and move the M6-11 row in `docs/PROJECT_STATUS.md` to `complete`.

`deps.toml` is deliberately untouched by the recipe pull request so that it does not collide with the M6-10 regeneration of the same file.

## Published release (orchestrator record)

`helpers-v0.2.0-firestone.1` was published from workflow run `33557495344` (twice-built, byte-identical) and independently reproduced byte-for-byte on the bare-metal x86_64 host `w` with the same locked inputs before upload:

| Asset | SHA-256 |
|---|---|
| `mkfs.ext4-1.47.3-x86_64-unknown-linux-musl` | `f1ed0b2b8b14a29e4edccf2bb44e2fb81e63a9bf74286746057915655795b987` |
| `passt-2025_02_17.a1e48a0-x86_64-unknown-linux-musl` | `a60b0b5e54e6f48caa5984b0a6b21938a9e57ba2222cddb9c0ca021f10e9b10e` |
| `qemu-img-8.2.2-x86_64-unknown-linux-musl` | `7d7f32b1f6861140a95c4daa31c013a888dcc02c04551136f05e7da519d7e0ed` |
| `firestone-static-helpers-v0.2.0-corresponding-source.tar` | `7f13fd891d15d8a4f6a7780be64927a05f18b1b9ace1ab2495f983cef442ae28` |
| `firestone-static-helpers-v0.2.0-build-info.txt` | `addcdf903e19f785ca038887cfdbe46f7240c3507d2be506b622efe4b8a427af` |

`passt` and `qemu-img` changed bytes relative to `helpers-v0.1.0-firestone.1` because the shared locked package closure grew for this recipe; both were re-pinned in the same `deps.toml` regeneration, as anticipated by step 5 above. The v0.1.0 input mirrors were re-uploaded byte-for-byte, and aports packaging snapshots for the newly linked static libraries remain the recorded follow-up.
