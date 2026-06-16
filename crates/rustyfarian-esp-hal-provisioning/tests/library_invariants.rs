//! Integration tests that verify library-level invariants that cannot be
//! easily encoded in the type system.
//!
//! These tests run on the host (no chip feature required) via:
//!
//! ```sh
//! cargo test --test library_invariants -p rustyfarian-esp-hal-provisioning
//! ```
//!
//! # Security-checklist item 7
//!
//! The library must never call `esp_hal::reset`, `software_reset`, or
//! `store.erase_all()` from any portal handler — those are integrator
//! decisions.  The `library_does_not_call_esp_hal_reset` test enforces this
//! by grepping the source tree and asserting zero matches.

use std::process::Command;

/// Security-checklist item 7 lock: the library source must never call
/// `esp_hal::reset`, `software_reset`, or `esp_hal_reset`.
///
/// Rebooting is the integrator's responsibility after receiving a
/// [`ProvisioningOutcome::Committed`] or `FactoryResetRequested` from the
/// session.  The library signalling the outcome without acting on it is the
/// architectural boundary; calling `reset` from inside the library would break
/// that contract silently.
///
/// This test spawns `rg` (ripgrep) to search the provisioning crate source.
/// If `rg` is not installed, the test is skipped with a warning rather than
/// failing — CI must have `rg` installed.
#[test]
fn library_does_not_call_esp_hal_reset() {
    let crate_src = concat!(env!("CARGO_MANIFEST_DIR"), "/src");

    let output = Command::new("rg")
        .args([
            "--type",
            "rust",
            "-n",
            r"esp_hal::reset|software_reset|esp_hal_reset",
            crate_src,
        ])
        .output();

    match output {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!(
                "WARN: `rg` (ripgrep) not found — skipping library_does_not_call_esp_hal_reset. \
                 Install ripgrep to enable this check on CI."
            );
        }
        Err(e) => {
            panic!("failed to spawn rg: {e}");
        }
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                stdout.is_empty(),
                "library source must not call esp_hal::reset / software_reset / esp_hal_reset.\n\
                 Found:\n{stdout}\n\
                 Rebooting is the integrator's responsibility after a ProvisioningOutcome signal."
            );
        }
    }
}

/// Security-checklist item 7 supplementary: confirm `store.erase_all()` is
/// not called from any portal handler (only from integrator code paths).
///
/// `erase_all` is an integrator-only operation — the portal signals
/// `FactoryResetRequested` and the integrator decides whether and when to
/// erase.  Calling it from inside the library would be a destructive
/// side-effect the caller did not consent to.
#[test]
fn portal_handlers_do_not_call_erase_all() {
    // Only check portal.rs — erase_all is legitimately present in store.rs
    // (where it is defined) and in tests (where it is tested).  The
    // invariant is that the PORTAL ROUTER never calls it.
    let portal_src = concat!(env!("CARGO_MANIFEST_DIR"), "/src/portal.rs");

    let output = Command::new("rg")
        .args(["--type", "rust", "-n", r"erase_all", portal_src])
        .output();

    match output {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("WARN: `rg` not found — skipping portal_handlers_do_not_call_erase_all.");
        }
        Err(e) => {
            panic!("failed to spawn rg: {e}");
        }
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                stdout.is_empty(),
                "portal.rs must not call store.erase_all() — that is an integrator decision.\n\
                 Found:\n{stdout}"
            );
        }
    }
}
