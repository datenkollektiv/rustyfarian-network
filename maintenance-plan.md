# Maintenance Plan

Regular maintenance workbook for `rustyfarian-network`.
Covers: build verification, security scanning, dependency updates, CI/CD health,
and cross-repo (`rustyfarian-ws2812`) coordination.

## Build & Test

### Build command

```sh
just fmt && just verify
```

`just verify` runs `just fmt-check`, `just deny`, `just check` (all ESP-IDF RISC-V
targets), and `just clippy`. It does **not** cover Xtensa IDF or bare-metal targets.

For hardware examples, also run:

```sh
just build-example <name>
```

A clean `just verify` with no warnings is the acceptance bar for each maintenance cycle.

### Test command

```sh
just test
```

Runs all pure-crate host tests: backoff, MQTT, Wi-Fi, LoRa, ESP-NOW, OTA, OTA-HAL.
No ESP toolchain is required for these.

### What to verify

- `just verify` exits 0 with no new warnings.
- `just test` exits 0 with all tests passing.
- No new `cargo audit` advisories since last cycle.

---

## Dependency Updates

### Version update strategy

- **Monthly**: patch-level bumps only (x.y.**Z**) for non-pinned crates; security fixes
  regardless of level.
- **Quarterly**: minor version bumps (x.**Y**.z); esp-hal wave upgrades; major version
  evaluations.

### Non-pinned crates (monthly candidates)

Located in `Cargo.toml` `[workspace.dependencies]` — crates declared with a semver
range (e.g. `"1.0"`, `"0.4"`) rather than `=x.y.z`.

Key crates to check each month:

| Crate               | Version style | Notes                                      |
|:--------------------|:--------------|:-------------------------------------------|
| `anyhow`            | range         | patch-safe                                 |
| `log`               | range         | patch-safe                                 |
| `nb`                | range         | patch-safe                                 |
| `rgb`               | range         | patch-safe                                 |
| `embuild`           | range         | patch-safe                                 |
| `esp-idf-svc`       | range         | minor can be breaking; monthly: patch only |
| `esp-idf-hal`       | range         | minor can be breaking; monthly: patch only |
| `embedded-hal`      | range         | patch-safe                                 |
| `embedded-svc`      | range         | patch-safe                                 |
| `heapless`          | range         | patch-safe                                 |
| `sha2`              | range         | patch-safe                                 |
| `lorawan-device`    | range         | patch-safe                                 |
| `lora-modulation`   | range         | patch-safe                                 |
| `sx126x`            | range         | patch-safe                                 |
| `static_cell`       | range         | patch-safe                                 |
| `embedded-io-async` | range         | patch-safe                                 |

Check with:

```sh
cargo outdated --depth 1
```

(Install with `cargo install cargo-outdated` if needed.)

### Exact-pinned crates (quarterly only)

These crates are pinned with `=x.y.z` and must be upgraded as a coordinated wave.
Do **not** bump them individually — they share a compatibility graph.

| Group                   | Crates                                                                                                  |
|:------------------------|:--------------------------------------------------------------------------------------------------------|
| esp-hal April 2026 wave | `esp-hal`, `esp-radio`, `esp-rtos`, `esp-alloc`, `esp-bootloader-esp-idf`, `esp-storage`, `esp-println` |
| Embassy wave            | `embassy-executor`, `embassy-net`, `embassy-time`, `embassy-sync`                                       |
| Backtrace               | `esp-backtrace`                                                                                         |

See `docs/features/archive/esp-hal-stack-upgrade-april-2026-v1.md` for wave upgrade
history and compatibility notes.

### Cross-repo: `rustyfarian-ws2812` (monthly check)

Check whether a new release is available on crates.io each cycle:

```sh
cargo search pennant
cargo search rustyfarian-esp-hal-ws2812
cargo search rustyfarian-esp-idf-ws2812
```

Current: `0.6.0`. These crates now resolve from crates.io (switched from git deps
when all three HAL driver crates were published). A ws2812 minor or major bump that
changes esp-hal version pins belongs to the quarterly wave cycle, not monthly.

### Security scanning

```sh
cargo audit          # local advisory check
just deny            # license + bans + advisories via cargo-deny
```

CI runs `cargo audit` weekly (Monday 06:00 UTC via `audit.yml`). The monthly cycle
re-runs it locally to catch anything that landed since last Monday's run.

Current advisory exceptions in `deny.toml`:

| Advisory          | Reason                                                                                                       | Tracking                                             |
|:------------------|:-------------------------------------------------------------------------------------------------------------|:-----------------------------------------------------|
| RUSTSEC-2023-0089 | `atomic-polyfill` unmaintained, transitive via `lorawan-device 0.12 → heapless 0.7.x` — no safe upgrade path | Revisit when `lorawan-device` drops `heapless 0.7.x` |

---

## CI/CD

Five workflows in `.github/workflows/`:

| Workflow     | Trigger                 | What it checks                           |
|:-------------|:------------------------|:-----------------------------------------|
| `rust.yml`   | push/PR to main         | `cargo deny`, `cargo check`, `just test` |
| `fmt.yml`    | push/PR to main         | `just fmt-check`                         |
| `clippy.yml` | push/PR to main         | `just clippy`                            |
| `audit.yml`  | push/PR + weekly Monday | `cargo audit`                            |
| `codeql.yml` | push/PR + scheduled     | GitHub Advanced Security scan            |

Monthly check: confirm all five workflows are green on `main`.

```sh
gh run list --branch main --limit 20
```

Watch for `rust/hard-coded-cryptographic-value` CodeQL alerts in test code — use
`TEST_PSK` constants and helper functions (not inline string literals flowing to
`password` parameters). See `docs/project-lore.md` "CodeQL / GitHub Advanced Security".

---

## Scheduled Maintenance Cadence

### Monthly checklist

- [ ] `cargo audit` — no new advisories
- [ ] `just deny` — clean
- [ ] `just verify` — exits 0, no new warnings
- [ ] `just test` — all pass
- [ ] Check patch-level updates for non-pinned crates (`cargo outdated --depth 1`)
- [ ] Apply safe patch bumps; re-run `just verify` + `just test`
- [ ] Check `pennant` / `rustyfarian-esp-*-ws2812` for new crates.io releases
- [ ] Verify all five CI workflows are green on `main`
- [ ] Check RUSTSEC-2023-0089 exception — is an upgrade path now available?

### Quarterly checklist

- [ ] Everything in the monthly checklist
- [ ] Evaluate minor version bumps for all non-pinned crates
- [ ] Evaluate esp-hal / embassy wave upgrade (check `esp-hal` release notes for new wave)
- [ ] Evaluate `ws2812` minor/major bump alongside esp-hal wave
- [ ] Review `deny.toml` advisory exceptions — resolve any that now have a fix
- [ ] Review `esp-idf-svc` + `esp-idf-hal` minor bumps (can be breaking)
- [ ] Review `lorawan-device` minor for `heapless` 0.8.x adoption (unblocks RUSTSEC-2023-0089)
- [ ] Architecture / roadmap review: does `docs/ROADMAP.md` reflect current priorities?

---

## Maintenance Protocol

Each cycle produces three files in `audit/`:

1. `YYYY-MM-DD-<cadence>-audit.md` — read-only assessment (findings, versions, CI status)
2. `YYYY-MM-DD-<cadence>-plan.md` — executable plan, reviewed before any changes
3. `YYYY-MM-DD-<cadence>-maintenance.md` — record of what was applied and what was deferred
