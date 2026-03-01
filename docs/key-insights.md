# Key Insights

This file records non-obvious technical discoveries: facts that caused surprising
failures, took significant time to debug, or would save a future developer 30+
minutes if known upfront.

Refer to `CLAUDE.md` and the `/key-insights` skill for recording guidelines.

---

## Toolchain & Build

**`rust-toolchain.toml` is required to activate the `esp` toolchain — `espup` alone is not enough.**
Without this file, `cargo check` silently falls back to the host `stable` toolchain, which has no
knowledge of `riscv32imac-esp-espidf`, producing the misleading error
`can't find crate for 'core' / target may not be installed` even when `espup` is fully installed.
Fix: add `rust-toolchain.toml` with `channel = "esp"` to the repo root;
rustup then selects the correct toolchain automatically in any shell session without requiring
`source ~/export-esp.sh`.

**When `rust-toolchain.toml` pins `channel = "esp"`, every CI job must install the `esp` toolchain — not just the build job.**
`rustup` reads `rust-toolchain.toml` for every `cargo` invocation, including `cargo fmt`.
A CI job that installs only `stable` (e.g. via `dtolnay/rust-toolchain@stable`) will fail with
`error: custom toolchain 'esp' specified in override file '...rust-toolchain.toml' is not installed`
even though `cargo fmt` itself does not compile ESP-IDF code.
Fix: replace `dtolnay/rust-toolchain@stable` in the `format` job with `esp-rs/xtensa-toolchain@v1.6`
(`ldproxy: false` suffices — the linker proxy is not needed for a format check).
The `esp` toolchain ships `rustfmt`, so no separate stable step is required.

**`just fmt` must be run before `just verify` (and before every commit) — skipping it causes CI to fail.**
`just verify` calls `just fmt-check` which only *detects* formatting drift; it does not fix it.
Any code change that was not passed through `cargo fmt` first will cause `fmt-check` to fail in CI
with no compiler error to aid diagnosis.
Fix: always run `just fmt` then `just verify` in that order; see the `## Completion Gate` section
in `CLAUDE.md`.

---

## ESP-IDF Event Loop (`esp-idf-svc`)

**`EspEventLoop::subscribe` requires the callback to accept the event type by value, not by reference.**
The bound is `F: for<'a> FnMut(D::Data<'a>)`, so the callback signature must be
`|event: WifiEvent<'_>|`, not `|event: &WifiEvent|`.
Using a reference produces `E0631: type mismatch in closure arguments` with the note
`expected closure signature 'for<'a> fn(WifiEvent<'a>) -> _' / found 'fn(&WifiEvent<'_>) -> _'`.
The compiler's `help` suggestion (remove `&`) is correct and sufficient.

**An `EspSubscription` must be stored for as long as events are needed — dropping it unregisters the handler.**
`EspSystemEventLoop::subscribe` returns an `EspSystemSubscription<'static>` whose `Drop` impl
automatically deregisters the callback.
If the subscription is bound to a local variable that goes out of scope (e.g. inside an `if` branch),
the handler fires zero times.
Fix: store the subscription in the owning struct (e.g. as `Option<EspSystemSubscription<'static>>`).
