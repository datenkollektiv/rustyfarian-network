# Feature: Isolated Build Targets for HAL and IDF with Optional RAM Disk

Isolate `esp-hal` (no\_std / bare-metal) and `esp-idf` (std) build artefacts into
separate target directories, with an optional macOS RAM disk used as the backing
store for faster ephemeral builds.
The justfile auto-detects whether the RAM disk is attached and routes each recipe
to the correct target dir — no `.envrc` or `direnv` required.

## Decisions

| Decision                                                        | Reason                                                                                                                                                                                                                                                                                      | Rejected Alternative                                                                             |
|:----------------------------------------------------------------|:--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|:-------------------------------------------------------------------------------------------------|
| Separate target dirs per runtime (`target/hal` vs `target/idf`) | IDF (std) and HAL (no\_std) produce incompatible artefacts; sharing a single `target/` causes full rebuilds on every switch                                                                                                                                                                 | Single shared `target/` — switching runtimes triggers complete recompilation                     |
| Isolation always active, RAM disk is optional backing store     | Separation is the core invariant; the RAM disk is a speed optimisation; removing it should not collapse both environments back into the same dir                                                                                                                                            | Isolation only when RAM disk is attached — fallback collapses environments, losing the guarantee |
| RAM disk for `target/` backing                                  | Fast I/O eliminates SSD wear from Rust's heavy write load; `target/` is ephemeral and safe to lose on reboot                                                                                                                                                                                | SSD-backed `target/` — slow on large workspaces, accelerates SSD degradation                     |
| Host/pure crates and IDE tooling use `target/ide`               | `target-dir = "target/ide"` in `.cargo/config.toml` redirects all cargo invocations that don't override `--target-dir` (IDE analysis, plain `cargo` calls, pure-crate `just` recipes) to a single directory; HAL and IDF recipes are unaffected because they pass `--target-dir` explicitly | Third RAM-disk slot for host — extra complexity with no measurable benefit                       |
| justfile decides target dirs — no `.envrc` needed               | Single source of truth; works identically locally and in CI without any shell setup; `path_exists` auto-detects the RAM disk                                                                                                                                                                | `.envrc` + direnv — adds a dependency, requires `direnv allow`, breaks CI without extra config   |
| `sccache` shared between both runtimes (optional)               | A shared sccache may improve repeated builds where compiler inputs match, while HAL and IDF target outputs remain isolated                                                                                                                                                                  | Per-runtime caches — miss cross-runtime hits; no caching — cold starts after every reboot        |
| RAM disk managed via `just ramdisk attach / detach`             | Self-documenting, idempotent, discoverable via `just --list`                                                                                                                                                                                                                                | Shell script or launch agent — opaque, easy to forget                                            |

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

## How It Works

Two justfile variables resolve at parse time using `path_exists`:

```
ramdisk := "/Volumes/RustBuilds"
hal_dir  := if path_exists(ramdisk + "/targets/hal") == "true" { ramdisk + "/targets/hal/" + file_name(justfile_directory()) } else { "target/hal" }
idf_dir  := if path_exists(ramdisk + "/targets/idf") == "true" { ramdisk + "/targets/idf/" + file_name(justfile_directory()) } else { "target/idf" }
```

Every `cargo` invocation in a HAL recipe gets `--target-dir {{ hal_dir }}`.
Every `cargo` invocation in an IDF recipe gets `--target-dir {{ idf_dir }}`.
Pure/host recipes (`verify`, `test`, `clippy`, etc.) and IDE tooling (RustRover, rust-analyzer) use
`target/ide`, set via `target-dir = "target/ide"` in `.cargo/config.toml` — no `--target-dir` override needed.
When the RAM disk is not attached, `hal_dir` resolves to `target/hal` and `idf_dir` to `target/idf`
— the environments remain isolated, just on SSD instead of RAM.

### Environment Map

| Recipes                                                                                                                                      | Target dir variable                         | Toolchain                          |
|:---------------------------------------------------------------------------------------------------------------------------------------------|:--------------------------------------------|:-----------------------------------|
| `check-wifi-hal`, `check-lora-hal`, `check-wifi-hal-embassy`, `check-ota-hal`, `check-ota-hal-embassy`, `build-example hal_*`, `flash hal_*` | `hal_dir`                                   | stable (RISC-V) / `+esp` (Xtensa) |
| `check-wifi`, `check-mqtt`, `check-lora`, `check-ota-idf`, `check-espnow`, `build-example idf_*`, `flash idf_*`                             | `idf_dir`                                   | stable (RISC-V) / `+esp` (Xtensa) |
| `verify`, `test`, `test-*`, `check`, `clippy`, `doc` + IDE tooling (anything that doesn't pass `--target-dir` explicitly)                   | `target/ide` (`.cargo/config.toml` default) | stable                             |

## Just Recipes

```sh
just doctor           # show RAM disk status, resolved target dirs, sccache
just ramdisk attach   # create and mount the RAM disk (idempotent, 6 GB default)
just ramdisk detach   # eject the RAM disk and free memory
```

`just doctor` is for human diagnostics only — it always exits 0 and is not intended
as a CI validation command.

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

## Failure Modes / Recovery

| Situation                                  | `just doctor` report                          | Recovery                                            |
|:-------------------------------------------|:----------------------------------------------|:----------------------------------------------------|
| RAM disk not attached                      | `ramdisk MISSING`                             | `just ramdisk attach`                               |
| Volume mounted, subdirs absent             | `ramdisk PARTIAL`                             | `just ramdisk attach` (mkdir -p is idempotent)      |
| RAM disk fully ready                       | `ramdisk ok`                                  | —                                                   |
| sccache not installed                      | `sccache MISSING`                             | `brew install sccache` (optional)                   |
| sccache installed, `RUSTC_WRAPPER` not set | `sccache installed but RUSTC_WRAPPER not set` | Add `export RUSTC_WRAPPER=sccache` to shell profile |

Without the RAM disk, builds fall back to `target/hal` / `target/idf` on SSD —
isolation is preserved, builds are slower.

## RustRover Integration

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

## State

- [x] Design approved
- [x] Option B fallback adopted (always-separate dirs; RAM disk = optional acceleration)
- [ ] `hal_dir` and `idf_dir` variables added to justfile
- [ ] `just doctor`, `just ramdisk attach`, `just ramdisk detach` added to justfile
- [ ] `target-dir = "target/ide"` set in `.cargo/config.toml`
- [ ] HAL per-crate recipes (`check-wifi-hal`, `check-lora-hal`, `check-wifi-hal-embassy`, `check-ota-hal`, `check-ota-hal-embassy`) route to `hal_dir`
- [ ] IDF per-crate recipes (`check-wifi`, `check-mqtt`, `check-lora`, `check-ota-idf`, `check-espnow`) route to `idf_dir`
- [ ] `build-example.sh` and `flash.sh` accept and thread `idf_dir` / `hal_dir` for artifact paths and bootloader lookup
- [ ] `ensure-bootloader.sh` accepts `idf_dir` for bootloader cache lookup
- [ ] `clean-idf` recipe updated to use `idf_dir`
- [ ] `clean` recipe updated to also clean `hal_dir` and `idf_dir`
- [ ] `scripts/doctor.sh` created
- [ ] `scripts/ramdisk.sh` created
- [ ] RustRover: target directory field cleared; `target/` excluded from indexing (per-developer, one-time)
- [ ] Tested end-to-end with RAM disk attached and detached
- [ ] Documentation updated (README / AGENTS.md prerequisites)

### Acceptance criteria

- [ ] Switching from HAL to IDF does not trigger a full rebuild of the HAL environment
- [ ] Switching from IDF to HAL does not overwrite prior HAL artefacts
- [ ] Host-only workflows (`just verify`, `just test`) remain unchanged
- [ ] Builds without RAM disk still land in `target/hal` and `target/idf` (not `target/`)
- [ ] `clean-idf` only removes IDF-specific generated build artefacts

## Session Log

- 2026-05-27 — Feature doc created; design decisions agreed
- 2026-05-27 — Adapted to rustyfarian-network: removed AVR row (no AVR target in this workspace); updated Environment Map to match actual justfile recipes; corrected State checkboxes (implementation not yet started); added `ensure-bootloader.sh` threading to the State checklist
