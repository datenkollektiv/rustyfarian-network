#!/usr/bin/env bash
set -euo pipefail
# release-semver-check.sh — semver compatibility check against v0.5.0 tag
# Usage: scripts/release-semver-check.sh
#
# Compares current tree with v0.5.0 tag for all three published crates.
# 0.6.0 is a pre-1.0 minor release — breaking changes are allowed but must be
# documented. Tool verdict is informational; interpretation requires the CHANGELOG.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
host_target="$("$SCRIPT_DIR/host-target.sh")"

set +e  # Don't exit on tool failure; report and continue

echo "════════════════════════════════════════════════════════════════════════"
echo "SEMVER CHECK: current tree vs. v0.5.0 tag (pre-1.0 minor release — breaking changes allowed)"
echo "════════════════════════════════════════════════════════════════════════"
echo ""

# ── juggler (pure crate, host-buildable) ──────────────────────────────────

echo "[1/3] juggler (host target: $host_target, all-features)"
echo "────────────────────────────────────────────────────────────────────────"
if cargo semver-checks -p juggler --baseline-rev v0.5.0 --target "$host_target" --all-features; then
    juggler_pass=1
    echo "✓ juggler: no semver violations"
else
    juggler_pass=0
    echo "✗ juggler: semver violations detected (see above)"
fi
echo ""

# ── rustyfarian-esp-idf-network (IDF target requires custom rustdoc build) ──

echo "[2/3] rustyfarian-esp-idf-network (target: riscv32imac-esp-espidf)"
echo "────────────────────────────────────────────────────────────────────────"
echo "Not checked by the tool: cargo-semver-checks builds rustdoc in a placeholder"
echo "crate outside the workspace, so rust-toolchain.toml (esp), .cargo/config.toml"
echo "and build-std do not apply; on 2026-10-09 rustdoc failed with"
echo "'output of --print=file-names missing' (also with RUSTUP_TOOLCHAIN=esp)."
echo "Check by hand: git diff v0.5.0..HEAD -- crates/<crate>/src | grep -E '^-\s*pub '"
echo ""
echo "Workaround: manual review of CHANGELOG.md [Unreleased] section instead."
echo "This crate's semver compatibility is tracked through:"
echo "  1. CHANGELOG.md [Unreleased] → [0.6.0] entry documents breaking changes"
echo "  2. Downstream integrations verify compatibility at integration time"
echo ""
idf_pass=0  # Cannot check with this tool; defer to manual review
echo "⊘ rustyfarian-esp-idf-network: tool cannot verify on cross-target"
echo ""

# ── rustyfarian-esp-hal-network (bare-metal target has same limitation) ──

echo "[3/3] rustyfarian-esp-hal-network (target: riscv32imac-unknown-none-elf)"
echo "────────────────────────────────────────────────────────────────────────"
echo "Not checked by the tool: cargo-semver-checks builds rustdoc in a placeholder"
echo "crate outside the workspace, so rust-toolchain.toml (esp), .cargo/config.toml"
echo "and build-std do not apply; on 2026-10-09 rustdoc failed with"
echo "'output of --print=file-names missing' (also with RUSTUP_TOOLCHAIN=esp)."
echo "Check by hand: git diff v0.5.0..HEAD -- crates/<crate>/src | grep -E '^-\s*pub '"
echo ""
echo "Workaround: manual review of CHANGELOG.md [Unreleased] section instead."
echo "This crate's semver compatibility is tracked through:"
echo "  1. CHANGELOG.md [Unreleased] → [0.6.0] entry documents breaking changes"
echo "  2. Downstream integrations verify compatibility at integration time"
echo ""
hal_pass=0  # Cannot check with this tool; defer to manual review
echo "⊘ rustyfarian-esp-hal-network: tool cannot verify on cross-target"
echo ""

# ─ Summary ───────────────────────────────────────────────────────────────

echo "════════════════════════════════════════════════════════════════════════"
echo "SUMMARY"
echo "════════════════════════════════════════════════════════════════════════"
echo "  juggler (host):                  $([ $juggler_pass -eq 1 ] && echo "✓ PASS" || echo "✗ FAIL")"
echo "  rustyfarian-esp-idf-network:     ⊘ Tool limitation (manual review required)"
echo "  rustyfarian-esp-hal-network:     ⊘ Tool limitation (manual review required)"
echo ""
echo "Manual verification checklist:"
echo "  1. Review CHANGELOG.md [Unreleased] for documented breaking changes"
echo "  2. Confirm CHANGELOG entries match semver requirements for 0.6.0 (minor)"
echo "  3. For tool limitations above: verify integration tests pass"
echo ""

if [ $juggler_pass -eq 0 ]; then
    echo "FYI: juggler has semver violations — see output above for details."
    exit 1
fi

echo "✓ Tool-checkable crates verified. Manual verification of the two"
echo "  cross-target crates is deferred to CHANGELOG review and integration tests."
exit 0
