# Feature: Isolated HAL and IDF Build Targets

Isolate `esp-hal` (no\_std / bare-metal) and `esp-idf` (std) build artefacts into
separate target directories, with an optional macOS RAM disk as the backing store for
faster ephemeral builds.
Isolation is mandatory in all environments; the RAM disk only changes the storage
location of the isolated target directories, not the directory model itself.
The justfile auto-detects whether the RAM disk is attached and routes each recipe
to the correct target dir — no `.envrc` or `direnv` required.

## Design Decisions

| Decision                                                        | Reason                                                                                                                                                                                                                                                                                                                                                                                                      | Rejected Alternative                                                                             |
|:----------------------------------------------------------------|:------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|:-------------------------------------------------------------------------------------------------|
| Separate target dirs per runtime (`target/hal` vs `target/idf`) | IDF (std) and HAL (no\_std) produce incompatible artefacts; sharing a single `target/` causes full rebuilds on every switch                                                                                                                                                                                                                                                                                 | Single shared `target/` — switching runtimes triggers complete recompilation                     |
| Isolation always active, RAM disk is optional backing store     | Separation is the core invariant; the RAM disk is a speed optimisation; removing it should not collapse both environments back into the same dir                                                                                                                                                                                                                                                            | Isolation only when RAM disk is attached — fallback collapses environments, losing the guarantee |
| RAM disk for `target/` backing                                  | Fast I/O eliminates SSD wear from Rust's heavy write load; `target/` is ephemeral and safe to lose on reboot                                                                                                                                                                                                                                                                                                | SSD-backed `target/` — slow on large workspaces, accelerates SSD degradation                     |
| Host/pure crates and IDE tooling use `target/ide`               | `target-dir = "target/ide"` in `.cargo/config.toml` redirects all cargo invocations that don't override `--target-dir` to a single shared directory. Host artifacts cannot overwrite embedded outputs because HAL and IDF recipes always pass an explicit `--target-dir`; any invocation that does not is by definition a host/default build and belongs in `target/ide`.                                   | Third RAM-disk slot for host — extra complexity with no measurable benefit                       |
| justfile decides target dirs — no `.envrc` needed               | Single source of truth; works identically locally and in CI without any shell setup; `path_exists` auto-detects the RAM disk                                                                                                                                                                                                                                                                                | `.envrc` + direnv — adds a dependency, requires `direnv allow`, breaks CI without extra config   |
| `sccache` shared between both runtimes (optional)               | A shared sccache may improve repeated builds where compiler inputs match, while HAL and IDF target outputs remain isolated                                                                                                                                                                                                                                                                                  | Per-runtime caches — miss cross-runtime hits; no caching — cold starts after every reboot        |
| RAM disk managed via `just ramdisk attach / detach`             | Self-documenting, idempotent, discoverable via `just --list`                                                                                                                                                                                                                                                                                                                                                | Shell script or launch agent — opaque, easy to forget                                            |

## Constraints

- macOS only — uses `hdiutil attach` and `diskutil erasevolume` for RAM disk creation
- RAM disk is lost on reboot; a cold start rebuilds from scratch (sccache warms subsequent builds)
- Build isolation (`target/hal` vs `target/idf`) is always active — no RAM disk required
- These paths must stay **persistent** (never on the RAM disk):
  - `~/.cargo` — registry and git sources
  - `~/.rustup` — toolchains
  - `~/.cache/sccache` — sccache store
  - `~/.espressif` — Espressif toolchain and ESP-IDF (`ESP_IDF_TOOLS_INSTALL_DIR = "global"`, shared across projects)
- `sccache` is optional; set `RUSTC_WRAPPER=sccache` in your shell profile to enable it
- No `direnv` or `.envrc` required
- Linux support is deferred; `hdiutil` / `diskutil` are macOS-only
- `file_name(justfile_directory())` is used as the per-project key under the RAM disk for v1;
  two checkouts with identical directory basenames (e.g. both named `firmware`) would collide on the RAM disk —
  this is accepted as a known limitation and can be resolved with a path-derived key in a later version

## How It Works

Two justfile variables resolve at **invocation time** using `path_exists`:

```
ramdisk := "/Volumes/RustBuilds"
hal_dir  := if path_exists(ramdisk + "/targets/hal") == "true" { ramdisk + "/targets/hal/" + file_name(justfile_directory()) } else { "target/hal" }
idf_dir  := if path_exists(ramdisk + "/targets/idf") == "true" { ramdisk + "/targets/idf/" + file_name(justfile_directory()) } else { "target/idf" }
```

Variables are evaluated once per `just` invocation, not mid-run.
Running `just ramdisk attach` and then immediately running `just build-example hal_c6_connect` in a new
invocation will see the RAM disk; a single invocation that calls `ramdisk attach` and then a build recipe
will not re-resolve `hal_dir` mid-run.

Neither path has a trailing slash; scripts and recipes that concatenate these values must not
assume one — use `path/to/file` concatenation, not `path/to/dir/` + `file`.

Every `cargo` invocation in a HAL recipe gets `--target-dir {{ hal_dir }}`.
Every `cargo` invocation in an IDF recipe gets `--target-dir {{ idf_dir }}`.
Host/IDE recipes and plain `cargo` calls use `target/ide`, set via `target-dir = "target/ide"` in
`.cargo/config.toml` — no `--target-dir` override needed.
When the RAM disk is not attached, `hal_dir` resolves to `target/hal` and `idf_dir` to `target/idf`
— the environments remain isolated, just on SSD instead of RAM.

### Environment Map

The table below is normative: any recipe not listed that does not pass an explicit `--target-dir`
is expected to use the default Cargo target dir (`target/ide`).

| Recipes                                                                                                                                      | Target dir variable                         | Toolchain                         |
|:---------------------------------------------------------------------------------------------------------------------------------------------|:--------------------------------------------|:----------------------------------|
| `check-wifi-hal`, `check-lora-hal`, `check-wifi-hal-embassy`, `check-ota-hal`, `check-ota-hal-embassy`, `build-example hal_*`, `flash hal_*` | `hal_dir`                                   | stable (RISC-V) / `+esp` (Xtensa) |
| `check-wifi`, `check-mqtt`, `check-lora`, `check-ota-idf`, `check-espnow`, `build-example idf_*`, `flash idf_*`                              | `idf_dir`                                   | stable (RISC-V) / `+esp` (Xtensa) |
| `verify`, `test`, `test-*`, `check`, `clippy`, `doc` + IDE tooling (anything that doesn't pass `--target-dir` explicitly)                    | `target/ide` (`.cargo/config.toml` default) | stable                            |

## Just Recipes

```sh
just doctor           # show RAM disk status, resolved target dirs, sccache
just ramdisk attach   # create and mount the RAM disk, then create targets/hal and targets/idf subdirs (idempotent, 6 GB default)
just ramdisk detach   # eject the RAM disk and free memory
```

`just doctor` is for human diagnostics only — it always exits 0, and its output format is not
API-stable. CI must not parse or depend on its output.

`just doctor` output with RAM disk attached:

```
  ramdisk    ok       /Volumes/RustBuilds
  hal target ok       /Volumes/RustBuilds/targets/hal/rustyfarian-network
  idf target ok       /Volumes/RustBuilds/targets/idf/rustyfarian-network
  sccache    ok       sccache 0.8.1
```

`just doctor` output without RAM disk:

```
  ramdisk    MISSING  run: just ramdisk attach
  hal target fallback target/hal
  idf target fallback target/idf
  sccache    MISSING  run: brew install sccache  (optional, speeds up cold builds)
```

`just doctor` output with RAM disk mounted but subdirectories missing:

```
  ramdisk    PARTIAL  /Volumes/RustBuilds (subdirs missing — run: just ramdisk attach)
  hal target fallback target/hal
  idf target fallback target/idf
```

## Cleaning Semantics

Cleaning commands must respect the same isolation boundaries as builds:

- **`clean`** — removes `target/` (workspace default), `target/hal`, `target/idf`, and `target/ide`.
  Does not touch RAM disk paths; those must be cleaned manually or via `just ramdisk detach`.
- **`clean-idf`** — removes only the active `idf_dir` contents, including `esp-idf-sys` build
  subdirectories inside it. Does not touch `target/hal` or `target/ide`.
- **`clean-hal`** (future) — would remove only `hal_dir` contents, symmetric with `clean-idf`.

## Failure Modes / Recovery

| Situation                                        | `just doctor` report                          | Recovery                                                                  |
|:-------------------------------------------------|:----------------------------------------------|:--------------------------------------------------------------------------|
| RAM disk not attached                            | `ramdisk MISSING`                             | `just ramdisk attach`                                                     |
| Volume mounted, subdirs absent                   | `ramdisk PARTIAL`                             | `just ramdisk attach` (mkdir -p is idempotent)                            |
| RAM disk fully ready                             | `ramdisk ok`                                  | —                                                                         |
| Volume mounts under unexpected path / name clash | `ramdisk MISSING` (path doesn't match)        | `diskutil list` to find actual mountpoint; update `ramdisk` var if needed |
| sccache not installed                            | `sccache MISSING`                             | `brew install sccache` (optional)                                         |
| sccache installed, `RUSTC_WRAPPER` not set       | `sccache installed but RUSTC_WRAPPER not set` | Add `export RUSTC_WRAPPER=sccache` to shell profile                       |

Without the RAM disk, builds fall back to `target/hal` / `target/idf` on SSD —
isolation is preserved, builds are slower.

## Concurrency

HAL and IDF builds can run concurrently without a conflict because they write to distinct target
directories; there is no Cargo lock contention between a `hal_*` and an `idf_*` build.
Two simultaneous HAL builds into the same `hal_dir` are subject to normal Cargo file-level locking
— one will wait — which is standard Cargo behaviour and not specific to this design.

## Developer Setup

`.idea/` is gitignored, so RustRover configuration is per-developer.
Once `target-dir = "target/ide"` is set in `.cargo/config.toml`, RustRover picks it up automatically
because it invokes `cargo` and `cargo` respects the config file.
Two one-time manual steps are still needed:

1. **Clear RustRover's explicit target directory** — Settings → Languages & Frameworks → Rust → Cargo → "Target directory".
   Leave this field empty; a non-empty value overrides `.cargo/config.toml` and defeats the isolation.
2. **Exclude `target/` from indexing** — Project Structure → select `target/` → Mark as Excluded.
   This covers `target/hal`, `target/idf`, and `target/ide`.
   The RAM disk path (`/Volumes/RustBuilds/`) is outside the project root and is ignored by RustRover automatically.

## Open Questions

- **Linux support**: `hdiutil` / `diskutil` are macOS-only; a Linux RAM disk path
  (tmpfs mount) could be added later, but is deferred for now.
- **RAM disk naming**: basename collision is a known limitation for v1; a path-derived key is the
  obvious fix but deferred until an actual collision is encountered.

## Divergences from rustyfarian-ws2812

This project follows ws2812's implementation pattern. The following intentional divergences apply:

| Divergence                          | rustyfarian-ws2812                            | rustyfarian-network                                 | Rationale                                                                                                       |
|:------------------------------------|:----------------------------------------------|:----------------------------------------------------|:----------------------------------------------------------------------------------------------------------------|
| IDF build mode                      | `cargo +esp build` (debug)                    | `cargo +esp build --release`                        | Firmware builds should be optimised; bootloader path uses `release/build` accordingly                           |
| `build-example` script signature    | `<crate_alias> <example> [hal_dir [idf_dir]]` | `<example> [hal_dir [idf_dir]]`                     | `crate_alias` is redundant (ws2812's own script notes this); auto-detection from example name prefix is cleaner |
| `flash.sh` structure                | Thin wrapper → `run-example.sh` (build+flash) | Combined build+flash in `flash.sh`                  | Without `crate_alias` dispatch the extra indirection adds no value                                              |
| `ensure-bootloader.sh` chip support | c3, c6, esp32                                 | c3, c6, esp32, esp32s3                              | This workspace has an `idf_esp32s3_join` example                                                                |
| HAL/IDF check recipes               | `check-hal`, `check-idf` (one each)           | Per-crate: `check-wifi-hal`, `check-lora-hal`, etc. | This workspace has multiple HAL and IDF crates                                                                  |

Anything not listed follows ws2812 exactly. If a divergence needs revisiting, bring it to the group.

## Implementation Checklist

- [x] Design approved
- [x] Option B fallback adopted (always-separate dirs; RAM disk = optional acceleration)
- [x] `hal_dir` and `idf_dir` variables added to justfile
- [x] `just doctor`, `just ramdisk attach`, `just ramdisk detach` added to justfile
- [x] `target-dir = "target/ide"` set in `.cargo/config.toml.dist` (and local `.cargo/config.toml`)
- [x] HAL per-crate recipes (`check-wifi-hal`, `check-lora-hal`, `check-wifi-hal-embassy`, `check-ota-hal`, `check-ota-hal-embassy`) route to `hal_dir`
- [x] IDF per-crate recipes (`check-wifi`, `check-mqtt`, `check-lora`, `check-ota-idf`, `check-espnow`) route to `idf_dir`
- [x] `build-example.sh` and `flash.sh` accept and thread `idf_dir` / `hal_dir` for artifact paths and bootloader lookup
- [x] `ensure-bootloader.sh` accepts and threads `hal_dir` / `idf_dir`; uses `lib.sh` for bootloader lookup
- [x] `clean-idf` recipe updated to use `idf_dir`; `clean` recipe updated to also clean `hal_dir` and `idf_dir`
- [x] `scripts/doctor.sh` created
- [x] `scripts/ramdisk.sh` created
- [x] `scripts/lib.sh` created with `find_idf_bootloader` helper
- [x] `ensure-bootloader` recipe added to justfile
- [x] `fresh-run` updated to use `just clean` (cleans all dirs, not just `target/ide`)
- [x] Tested end-to-end with RAM disk attached and detached
- [x] Documentation updated (README / AGENTS.md prerequisites)

### Acceptance Criteria

- [x] Switching from HAL to IDF does not trigger a full rebuild of the HAL environment
- [x] Switching from IDF to HAL does not overwrite prior HAL artefacts
- [x] Concurrent HAL and IDF builds complete without conflict
- [x] Host-only workflows (`just verify`, `just test`) remain unchanged and use `target/ide`
- [x] Builds without RAM disk still land in `target/hal` and `target/idf` (not `target/`)
- [x] `clean-idf` removes only IDF-specific build artefacts; does not touch `target/hal`
- [x] `just ramdisk attach` is idempotent; re-running when already attached is safe

## Session Log

- 2026-05-27 — Feature doc created; design decisions agreed
- 2026-05-27 — Adapted to rustyfarian-network: removed AVR row; updated Environment Map to actual justfile recipes; corrected State checkboxes
- 2026-05-27 — Review pass: title shortened; invariant sentence added; "Decisions" → "Design Decisions"; target/ide safety justification expanded; basename collision constraint added; parse-time evaluation and trailing-slash convention noted; Environment Map marked normative; ramdisk attach contract made explicit; doctor API-stability note added; permission/path failure row added to Failure Modes; Cleaning Semantics section added; Concurrency section added; "RustRover Integration" → "Developer Setup"; "State" → "Implementation Checklist" with not-yet-implemented framing; Acceptance Criteria expanded
- 2026-05-27 — Implementation complete (streamlined with rustyfarian-ws2812): `scripts/lib.sh`, `scripts/doctor.sh`, `scripts/ramdisk.sh` created; `build-example.sh`, `flash.sh`, `ensure-bootloader.sh` updated with dir params; `.cargo/config.toml.dist` gains `target-dir = "target/ide"`; justfile updated with `ramdisk`/`hal_dir`/`idf_dir` variables, `doctor`/`ramdisk`/`ensure-bootloader` recipes, per-crate `--target-dir` routing, and cleaned-up `clean`/`clean-idf`/`fresh-run`; Divergences from ws2812 documented
- 2026-06-18 — Feature wrapped up: acceptance criteria and end-to-end RAM-disk test verified manually and ticked; per-developer RustRover bullet removed from the checklist. README "Build target isolation (optional RAM disk)" subsection and AGENTS.md Development Workflow note added documenting `target/hal` / `target/idf` / `target/ide` routing and the `just doctor` / `just ramdisk` recipes. All checklist items complete — doc archive-eligible.
