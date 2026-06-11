#!/usr/bin/env bash
set -euo pipefail
# lint-docs.sh — validate Mermaid diagrams in markdown via mermaid-cli (mmdc).
# Usage: scripts/lint-docs.sh [root-dir]
# Requires: Node.js (npx); fetches @mermaid-js/mermaid-cli on first run.
#
# Exits 0 if every fenced ```mermaid block in every *.md file under root-dir
# parses cleanly. Exits 1 on the first failure, dumping mmdc's stderr so the
# offending line is visible. Exits 2 if prerequisites are missing.

root_dir="${1:-.}"

if ! command -v npx >/dev/null 2>&1; then
    printf 'error: npx is required (install Node.js, or set MMDC to a custom invocation)\n' >&2
    exit 2
fi

mmdc=(${MMDC:-npx --yes @mermaid-js/mermaid-cli})

# mmdc bundles puppeteer-core which does not auto-discover installed Chromes.
# Locate any compatible Chrome (or chrome-headless-shell) under the puppeteer cache
# and pass it through a config file so the user does not need to set anything.
puppeteer_cache="${PUPPETEER_CACHE_DIR:-$HOME/.cache/puppeteer}"
chrome_exe=""
if [ -d "$puppeteer_cache" ]; then
    chrome_exe="$(find "$puppeteer_cache" -type f \( -name 'Google Chrome for Testing' -o -name 'chrome-headless-shell' \) 2>/dev/null | sort -r | head -1)"
fi

if [ -z "$chrome_exe" ]; then
    printf 'error: no Chrome found under %s\n' "$puppeteer_cache" >&2
    printf '       install one with: npx puppeteer browsers install chrome\n' >&2
    printf '       (or chrome-headless-shell for a smaller download)\n' >&2
    exit 2
fi

mapfile -t files < <(
    grep -rl --include='*.md' '```mermaid' "$root_dir" 2>/dev/null \
        | grep -vE '(^|/)(node_modules|target|tmp|review-queue|vendor|\.claude)/' \
        || true
)

if [ "${#files[@]}" -eq 0 ]; then
    printf 'lint-docs: no markdown files with mermaid blocks under %s\n' "$root_dir"
    exit 0
fi

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

# Tell mmdc which Chrome to use, and silence sandbox warnings on headless macOS/Linux.
puppeteer_config="$tmpdir/puppeteer-config.json"
cat >"$puppeteer_config" <<JSON
{ "executablePath": "$chrome_exe", "args": ["--no-sandbox"] }
JSON

failed=0
for f in "${files[@]}"; do
    printf 'checking %s\n' "$f"
    out="$tmpdir/$(basename "$f" .md).out.md"
    if ! "${mmdc[@]}" -p "$puppeteer_config" -i "$f" -o "$out" --quiet >"$tmpdir/stdout.log" 2>"$tmpdir/stderr.log"; then
        printf 'FAIL: %s\n' "$f" >&2
        cat "$tmpdir/stderr.log" >&2
        failed=1
    fi
done

if [ "$failed" -ne 0 ]; then
    exit 1
fi

printf 'lint-docs: all mermaid diagrams parse cleanly (%d file(s))\n' "${#files[@]}"
