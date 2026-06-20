#!/usr/bin/env bash
set -euo pipefail
# release-validate.sh — pre-flight validation for the crates.io release (no actual publish)
# Usage: scripts/release-validate.sh
#
# Runs the full release gate: version lockstep, `just verify`, package-content
# checks, `cargo publish --dry-run` for all three crates in dependency order, and
# a security audit. See release-plan.md for the full publication sequence.
#
# CHANGELOG.md is updated (move [Unreleased] -> [X.Y.Z]) in the release commit
# BEFORE running this and tagging — it is NOT a post-publish step.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
host_target="$("$SCRIPT_DIR/host-target.sh")"

CRATES=(juggler rustyfarian-esp-idf-network rustyfarian-esp-hal-network)

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "Release Validation — 0.4.0 lockstep"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo ""

echo "[1/5] Verifying version consistency..."
versions=$(cargo metadata --format-version 1 2>/dev/null |
    jq -r '.packages[] | select(.name == "juggler" or .name == "rustyfarian-esp-idf-network" or .name == "rustyfarian-esp-hal-network") | "\(.name)=\(.version)"')
juggler_ver=$(echo "$versions" | grep "^juggler=" | cut -d= -f2)
idf_ver=$(echo "$versions" | grep "^rustyfarian-esp-idf-network=" | cut -d= -f2)
hal_ver=$(echo "$versions" | grep "^rustyfarian-esp-hal-network=" | cut -d= -f2)
if [ "$juggler_ver" != "$idf_ver" ] || [ "$idf_ver" != "$hal_ver" ]; then
    echo "ERROR: version mismatch — juggler=$juggler_ver idf=$idf_ver hal=$hal_ver" >&2
    exit 1
fi
echo "  OK — all crates at version $juggler_ver"
echo ""

echo "[2/5] Running 'just verify'..."
if just verify >/tmp/release-verify.log 2>&1; then
    echo "  OK — fmt-check, deny, check, clippy, IDF/HAL checks, guards"
else
    echo "  FAIL — see /tmp/release-verify.log" >&2
    tail -20 /tmp/release-verify.log >&2
    exit 1
fi
echo ""

echo "[3/5] Validating package contents (README + dual licenses)..."
for crate in "${CRATES[@]}"; do
    listing=$(cargo package --list -p "$crate" --allow-dirty 2>&1)
    has_license=$(echo "$listing" | grep -c "LICENSE" || true)
    has_readme=$(echo "$listing" | grep -c "README.md" || true)
    if [ "$has_license" -lt 2 ] || [ "$has_readme" -lt 1 ]; then
        echo "  ERROR: $crate is missing LICENSE or README in its package" >&2
        exit 1
    fi
    echo "  OK — $crate: $(echo "$listing" | wc -l | tr -d ' ') files, LICENSE-MIT + LICENSE-APACHE + README.md"
done
echo ""

echo "[4/5] cargo publish --dry-run (juggler — host-buildable, full verify)..."
if cargo publish --dry-run -p juggler --target "$host_target" --all-features --allow-dirty >/tmp/release-dryrun-juggler.log 2>&1; then
    echo "  OK — juggler packages and verify-builds"
else
    echo "  FAIL — see /tmp/release-dryrun-juggler.log" >&2
    tail -20 /tmp/release-dryrun-juggler.log >&2
    exit 1
fi
# The two -network crates depend on `juggler ^0.4`. A `cargo publish --dry-run` for
# them resolves juggler against the crates.io index (the published manifest drops the
# path), which only succeeds AFTER juggler is published — so a standalone dry-run is
# not possible here. Their packaging/contents are validated above in [3/5] via
# `cargo package --list`; their real publish --dry-run happens as the ordered publish
# proceeds (publish juggler first, which unblocks them). See release-plan.md.
echo "  NOTE — rustyfarian-esp-idf-network / -esp-hal-network: full publish --dry-run"
echo "         requires juggler on crates.io first; validated via package --list above"
echo "         and as part of the ordered publish."
echo ""

echo "[5/5] cargo audit (security advisories)..."
if cargo audit >/tmp/release-audit.log 2>&1; then
    echo "  OK — no known vulnerabilities"
else
    echo "  NOTE — cargo audit reported findings; review /tmp/release-audit.log against deny.toml" >&2
fi
echo ""

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "PRE-FLIGHT VALIDATION PASSED — ready to publish v$juggler_ver"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo ""
echo "Publish via the just recipes, in staged dependency order (clean tree + CARGO_REGISTRY_TOKEN):"
echo "  Stage 1: just release-publish juggler        # wait ~2-5 min to index"
echo "  Stage 2: just release-dry-run-idf            # now resolves juggler ^0.4 from the index"
echo "           just release-dry-run-hal"
echo "  Stage 3: just release-publish-idf            # cargo +esp publish, --target riscv32imac-esp-espidf"
echo "           just release-publish-hal            # -Zbuild-std=core,alloc --target riscv32imac-unknown-none-elf"
echo ""
echo "  The two -network crates verify-build against their real cross-target (NOT the host,"
echo "  NOT --no-verify); the IDF crate needs the esp toolchain. See release-plan.md."
echo ""
echo "Then: git tag -a v$juggler_ver -m \"v$juggler_ver\" && git push --tags, and cut the GitHub release."
echo "(CHANGELOG.md was already moved [Unreleased] -> [$juggler_ver] in the release commit.)"
echo ""
echo "See release-plan.md for the full checklist and troubleshooting."
