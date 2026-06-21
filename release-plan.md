# Release Plan — 0.4.0

Publication plan for the 16→3 crate consolidation to crates.io.

This is the first publication of `rustyfarian-network` to the Rust package registry.
Three publishable crates are released simultaneously at version 0.4.0, with coordinated lockstep versioning.

## Versioning

- **Scheme:** SemVer (pre-1.0 — minor bumps signal breaking changes)
- **Lockstep release:** all three crates (`juggler`, `rustyfarian-esp-idf-network`, `rustyfarian-esp-hal-network`) move together at version 0.4.0
- **Single source of truth:** `[workspace.package].version` in the root `Cargo.toml` (all member crates inherit via `version.workspace = true`)
- **Pre-1.0 stability:** breaking API changes are acceptable; semver communicates intent, not stability promise

## Branch and Tag Convention

- **Release branch:** `prepare-crates-publishing` (branched from `main`, code review → fast-forward merge to `main`)
- **Tag format:** `v0.4.0` (annotated, on the release commit)
- **Tagging:** manual step after the `just release-publish*` recipes succeed for all three crates

## Pre-flight Checklist

Before any publication attempt:

- [ ] Working tree clean on `prepare-crates-publishing` (untracked `review-queue/` and `tmp/` are OK)
- [ ] `just fmt` clean (all code formatted)
- [ ] `just verify` passes (`fmt-check` + `cargo deny` + `cargo check` + `cargo clippy`)
- [ ] `just test` passes (all pure-crate host tests on `juggler`)
- [ ] At least one hardware example builds per tier via `just build-example <name>` (sanity check, not exhaustive):
  - ESP-IDF example: `just build-example idf_c3_connect`
  - Bare-metal example: `just build-example hal_c3_connect_async`
- [ ] `cargo audit` shows no new advisories beyond those allowlisted in `deny.toml`
- [x] `CHANGELOG.md` `[0.4.0]` section is cut with entries documenting all changes in this release (already done — `## [0.4.0] - 2026-06-20` with an empty `[Unreleased]` above it; see Changelog Update below)
- [ ] Per-crate `README.md` files exist and are accurate:
  - `crates/juggler/README.md` ✓
  - `crates/rustyfarian-esp-idf-network/README.md` ✓
  - `crates/rustyfarian-esp-hal-network/README.md` ✓
- [ ] `Cargo.toml` metadata is complete for all three crates:
  - `[package]` `description`, `keywords`, `categories` fields set
  - `[package]` `readme = "README.md"` field set (enables per-crate README on crates.io)
  - `version = "0.4.0"` (or `version.workspace = true` if using workspace inheritance)

## Dry-run Checklist

Before publishing for real, run the one-command pre-flight (it also runs version-lockstep, `just verify`, package-content, and audit checks):

```sh
just release-publish-validate
```

For just the pre-juggler packaging dry-run portion (also run in CI by `.github/workflows/publish-dry-run.yml`):

```sh
just release-dry-run
```

What it validates and a fundamental ordering constraint:

- **`juggler`** gets a full `cargo publish --dry-run` (it is host-buildable): this verify-builds the crate, packages the tarball (confirming README + dual licenses + all source files are included), and exercises the metadata — with no actual upload.
- **`rustyfarian-esp-idf-network`** and **`rustyfarian-esp-hal-network`** get `cargo package --list` only at this stage. A full `cargo publish --dry-run` for them resolves `juggler ^0.4` **against the crates.io index** (the published manifest drops the local `path`), so it fails with `no matching package named 'juggler' found` until juggler is actually published. This is the same staged constraint the sibling `rustyfarian-ws2812` workspace handles. Once juggler is live, their real cross-target dry-run can be run via `just release-dry-run-idf` / `just release-dry-run-hal` (Stage 2 below) before the actual publish.

The two `-network` crates are **verify-built against their real cross-compilation target** (not the host, and **not** `--no-verify`): the IDF crate against `riscv32imac-esp-espidf` via the `esp` toolchain, the HAL crate against `riscv32imac-unknown-none-elf`. This matches the `rustyfarian-ws2812` convention and means the verify-build actually runs where `esp-idf-sys` / bare-metal code compiles, rather than being skipped.

**Expected outcome:** juggler's dry-run succeeds; both `-network` crates package cleanly with README + `LICENSE-MIT` + `LICENSE-APACHE` included. If juggler's dry-run fails, fix the issue (missing `version`, forbidden path-dep, excluded file) and re-run before publishing.

## Publication Order and Rationale

All publishing is driven through `just` recipes (never raw `cargo publish`), mirroring the `rustyfarian-ws2812` convention. Each crate is published against its real target; the publish recipes carry a `[confirm]` prompt. The three crates **must** be published in this staged order.

### Stage 1 — Publish `juggler` first

```sh
just release-publish juggler
```

(Recipe: `cargo publish -p juggler --target {{ host_target }}`.)
Rationale: `juggler` has no internal crate dependencies; it is self-contained and host-buildable. Once published and indexed, the two `-network` crates can resolve `juggler ^0.4` from crates.io.

Expected time on crates.io: ~2–5 minutes after the command succeeds.

### Stage 2 — Dry-run the dependent crates (now that juggler is live)

These resolve `juggler ^0.4` from the crates.io index and verify-build against the real cross-target, so they only work after Stage 1 is indexed:

```sh
just release-dry-run-idf   # cargo +esp publish --dry-run ... --target riscv32imac-esp-espidf
just release-dry-run-hal   # cargo publish --dry-run ... -Zbuild-std=core,alloc --target riscv32imac-unknown-none-elf
```

If either fails on something other than transient indexing, fix before Stage 3.

### Stage 3 — Publish the dependent crates

```sh
just release-publish-idf   # cargo +esp publish -p rustyfarian-esp-idf-network --target riscv32imac-esp-espidf --target-dir {{ idf_dir }}
just release-publish-hal   # cargo publish -p rustyfarian-esp-hal-network -Zbuild-std=core,alloc --target riscv32imac-unknown-none-elf --target-dir {{ hal_dir }}
```

Both depend on `juggler = "0.4"` (published in Stage 1). They are verify-built against their **real cross-compilation target**, not the host:

- **IDF crate:** `esp-idf-svc` / `esp-idf-hal` are always-on deps and the crate ships a `build.rs`, so a host verify-build would fail (`esp-idf-sys` rejects `aarch64-apple-darwin` / CI x86). Publishing with `cargo +esp publish --target riscv32imac-esp-espidf` runs the verify-build under the ESP toolchain where it compiles. (This is why the host-target default fails — the fix is the correct target, **not** `--no-verify`.)
- **HAL crate:** a bare-metal `no_std` crate built for `riscv32imac-unknown-none-elf`. The `-Zbuild-std=core,alloc` override is required because the workspace `.cargo/config.toml` default `build-std = ["std", "panic_abort"]` cannot build `std` for a bare-metal target (same override the `check-hal*` recipes use).

Expected time on crates.io: ~2–5 minutes after each command succeeds.

**Why not parallel?** crates.io requires all transitive dependencies to be publicly available before a crate can reference them.
Publishing in dependency order (pure tier first, then HAL tiers) ensures each crate resolves cleanly at publish time.

## Changelog Update

**Status: done.** `CHANGELOG.md` has already been cut for this release — it contains a populated `## [0.4.0] - 2026-06-20` section with an empty `## [Unreleased]` header above it.
No further changelog action is needed before tagging.
`release-validate.sh` assumes this state (it prints "CHANGELOG.md was already moved [Unreleased] -> [0.4.0]" on success).

For reference, the move that was performed (and the pattern for future releases) is:

**Before:**
```markdown
## [Unreleased]

### Added
- ...
```

**After:**
```markdown
## [0.4.0] - 2026-06-20

### Added
- ...

## [Unreleased]

### Added
```

Use the actual release date (the date of the commit or publication, maintainer's choice).
If the publication date slips past 2026-06-20, update the `## [0.4.0]` date line to the real publish date before tagging.

## Git Tag and Push

After all three crates are published (Stages 1–3 via the `just release-publish*` recipes) and crates.io confirms availability:

```sh
git tag -a v0.4.0 -m "v0.4.0: First publication to crates.io — 16→3 crate consolidation"
git push origin v0.4.0
```

Then merge or fast-forward `prepare-crates-publishing` to `main`:

```sh
git checkout main
git pull origin main
git merge --ff-only prepare-crates-publishing  # or: git rebase prepare-crates-publishing
git push origin main
```

## GitHub Release

Create a release page at `https://github.com/datenkollektiv/rustyfarian-network/releases/new`:

- **Tag:** `v0.4.0`
- **Title:** `v0.4.0 — First Publication to Crates.io`
- **Body:** Keep it short — a one-line summary, the three crates, the breaking change with a migration link, and condensed fixes. Do **not** paste the full `## [0.4.0]` CHANGELOG section. Template:

```markdown
First publication to crates.io. Consolidates 16 workspace crates into 3 publishable crates, one per HAL tier ([ADR 016](docs/adr/016-crate-consolidation-for-publishing.md)).

## Crates
- **`juggler`** — pure/`no_std` shared types (Wi-Fi, MQTT, LoRa, ESP-NOW, OTA, provisioning), per-domain features, `default = []`
- **`rustyfarian-esp-idf-network`** — ESP-IDF/`std` drivers
- **`rustyfarian-esp-hal-network`** — bare-metal/`no_std` async drivers (ESP32-C3/C6/S3/ESP32)

## Breaking
Every import path changes (`wifi_pure::X` → `juggler::wifi::X`, `rustyfarian_esp_idf_wifi::X` → `rustyfarian_esp_idf_network::wifi::X`, etc.). No in-place upgrade — see the [migration guide](https://github.com/datenkollektiv/rustyfarian-network/blob/main/docs/features/archive/crate-consolidation-3-crates-v1.md#migration-guide--old-paths-to-new-paths).

## Fixed
- Provisioning store rejected valid stores at non-zero flash offsets (`OffsetOutOfBounds`).
- HAL provisioning examples panicked (`time_driver NoneError`) on the already-provisioned boot path.
- HAL async examples failed to compile (missing `embassy-executor`/`embassy-time` dev-deps).
```

- **Pre-release:** Uncheck (this is a stable release)
- **Create a discussion:** Uncheck (optional; useful for major releases)
- **Attachments:** None (library crates, no binary artifacts)

## Post-Publication Verification

Once all three crates are on crates.io (verified by visiting their crates.io pages):

- [ ] Verify `juggler 0.4.0` is published: https://crates.io/crates/juggler
- [ ] Verify `rustyfarian-esp-idf-network 0.4.0` is published: https://crates.io/crates/rustyfarian-esp-idf-network
- [ ] Verify `rustyfarian-esp-hal-network 0.4.0` is published: https://crates.io/crates/rustyfarian-esp-hal-network
- [ ] Check that each crate's documentation page builds (or documents the known limitation for the HAL/IDF crates):
  - `juggler` docs should build on docs.rs (pure crate, any platform)
  - `rustyfarian-esp-idf-network` docs will most likely **fail to build on docs.rs**: `esp-idf-sys` requires network access and the full ESP-IDF C toolchain at build time, neither of which the docs.rs sandbox provides. The `[package.metadata.docs.rs]` `default-target = "riscv32imc-esp-espidf"` is set as a best effort, but treat a failed docs.rs build as expected, not a regression — the README on the crates.io page carries the primary documentation.
  - `rustyfarian-esp-hal-network` docs will not build on docs.rs (bare-metal-only); the page should show feature documentation. If the build fails, the README on the crates.io page is the primary documentation.
- [ ] Spot-check a GitHub Actions workflow or local build that depends on the published crates via `Cargo.toml` (not path deps) to confirm external resolution works

## Credentials and Registry Authentication

- **crates.io token:** Required; obtain from https://crates.io/settings/tokens (login required)
- **Access scope:** Because all three crate names are **new** on crates.io, the token must include the **"publish new crates"** scope and must not be allowlisted to other crate names. (A token scoped to "publish updates" only, or restricted to a different crate allowlist, fails on the first publish.) Token scopes are visible only in the crates.io web UI, not via the API.

**Authentication method (this project): `CARGO_REGISTRY_TOKEN` environment variable.**

The token is provided to `cargo publish` via the `CARGO_REGISTRY_TOKEN` environment variable, exported from `.envrc` (loaded by direnv in the interactive shell). `cargo publish` reads it automatically — no `cargo login` and no `~/.cargo/credentials.toml` file are required.

```sh
# .envrc (loaded by direnv; the value lives in the developer's local environment, not in git)
export CARGO_REGISTRY_TOKEN="<crates.io token>"
```

Verify the token is present and valid before publishing (does not print the secret):

```sh
test -n "$CARGO_REGISTRY_TOKEN" && echo "token set" || echo "token NOT set — check .envrc / direnv"
curl -s -H "Authorization: $CARGO_REGISTRY_TOKEN" https://crates.io/api/v1/me | python3 -m json.tool
```

A JSON body containing your `user` confirms the token is valid. (The "publish new crates" scope still needs a one-time visual check in the web UI — it is not reported by the API.)

The same `CARGO_REGISTRY_TOKEN` is used by all three `cargo publish` invocations.

**Alternative (`cargo login`):** if you prefer not to use the environment variable, run `cargo login` once to store the token in `~/.cargo/credentials.toml`. Do not use both methods at once. `.envrc` must never commit the real token to git.

## Rollback Procedure

If a crate must be yanked or the release retracted after publication:

1. **Yank the crate (remove from dependency resolution, keep history):**
   ```sh
   cargo yank --version 0.4.0 <crate>
   ```
   Yank in reverse dependency order (the two `-network` crates first, then `juggler`). Requires the same `CARGO_REGISTRY_TOKEN` / credentials that published it.

2. **Or: delete the release on GitHub** (if not yet heavily used):
   ```sh
   git push --delete origin v0.4.0
   git tag -d v0.4.0
   ```
   Then use the GitHub web UI to delete the release page.

**Note:** On crates.io, versions cannot be truly deleted — only yanked (marked as unavailable for new dependency resolution, but remain visible in version history).
Once a version is yanked, downstream projects with explicit `= 0.4.0` pins will fail to resolve; projects using `^0.4` will skip to the next available version.

## Rollback Decision Tree

- **If a critical bug is discovered <1 hour after publish:** Contact crates.io admins (rare) or yank the version and re-publish as 0.4.1.
- **If a dependency resolution issue emerges:** Yank, fix the root cause, bump to 0.4.1 (patch), and re-publish.
- **If the consolidation itself is wrong:** Too late to fix with a patch; 0.5.0 (minor, pre-1.0) will be the next release, with a migration guide in release notes.

## Post-Release Follow-ups

After a successful publication:

- [ ] Update the root `README.md` to reference the published crates (remove git-dep examples, add crates.io version constraints)
- [ ] Update `docs/ROADMAP.md` to reflect that Phase 5 (publication) is complete
- [ ] Create a workspace `release/0.4.0/` subdirectory and document:
  - Date and time of publication
  - Which crates were published, in what order
  - Any dry-run issues encountered and how they were resolved
  - Post-publication verification checklist and results
- [ ] Announce the release on project channels (if applicable)

## Troubleshooting

### Error: `failed to select a version for the requirement 'juggler = "^0.4"'`

**Cause:** The ESP-IDF or HAL crate is being published before `juggler` reaches crates.io.

**Fix:** Ensure `juggler` is published successfully (check crates.io) before publishing the HAL crates.
Wait 2–5 minutes between publishes to allow crates.io to index the new crate.

### Error: `failed to publish: metadata.description is missing`

**Cause:** A `[package]` field is missing in `Cargo.toml`.

**Fix:** Add the missing field (typically `description`, `keywords`, or `categories`) and re-run the dry-run.

### Warning: `Compiling ... from git=` in the published tarball

**Cause:** An optional dependency is declared with a git URL instead of a version on crates.io.

**Fix:** Ensure all dependencies use explicit versions (e.g., `version = "0.3"`), not git refs, except for truly unpublished internal crates (which should be avoided at publication time).

### docs.rs build fails with `no method named 'foo' found for ... impl Future`

**Cause:** `embassy-executor` is missing from `[dev-dependencies]` for a bare-metal example.

**Fix:** Add `embassy-executor` and `embassy-time` to `[dev-dependencies]` on the HAL crate so examples compile during the docs.rs build.
(This is already done in `rustyfarian-esp-hal-network`; if the error persists, check the example's `required-features`.)

## Resources

- [ADR 016: Crate Consolidation for Publishing](docs/adr/016-crate-consolidation-for-publishing.md)
- [Feature doc: 3-Crate Consolidation v1](docs/features/archive/crate-consolidation-3-crates-v1.md) — includes migration table and feature descriptions
- [Cargo Book — Publishing](https://doc.rust-lang.org/cargo/reference/publishing.html)
- [crates.io — Publishing Crates](https://doc.rust-lang.org/cargo/reference/publishing.html#publishing-to-cratesio)
