# Release Plan

Release process for `rustyfarian-network`.
Covers versioning, pre-flight checks, publish targets, and post-release steps.

## Versioning

- **Scheme:** SemVer (pre-1.0 — minor bumps signal breaking changes)
- **Lockstep:** all 13 workspace crates move together at the same version
- **Snapshot convention:** none (we tag the release commit, no `-SNAPSHOT` between releases)
- **Who decides version:** maintainer, based on `CHANGELOG.md` `[Unreleased]` content

## Branch and Tag Convention

- **Release branch:** `prepare-release` → merged to `main` after publish
- **Tag format:** `vX.Y.Z` (annotated)
- **Tagging:** manual on the release commit on `main`

## Pre-flight Checklist

Before any release:
- [ ] Working tree clean on `prepare-release` (untracked `review-queue/` is OK)
- [ ] `just fmt` clean
- [ ] `just verify` passes (fmt-check + cargo deny + check + clippy)
- [ ] `just test` passes (all pure-crate host tests)
- [ ] At least one hardware example builds via `just build-example <name>` for each tier touched (sanity check; not exhaustive)
- [ ] `cargo audit` shows no new advisories beyond those allow-listed in `deny.toml`
- [ ] `CHANGELOG.md` `[Unreleased]` has entries for the target version
- [ ] `[workspace.package].version` in root `Cargo.toml` matches the target version; no member crate overrides it (each declares `version.workspace = true`)

## Version Bump

Single source of truth: `[workspace.package].version` in the root `Cargo.toml`.
Each member crate inherits via `version.workspace = true`, so a release bump is one edit.

Post-release bump target: none (next-version bump happens when the next `[Unreleased]` block is finalised)

## Publish

**Target registry:** none — tag-only on GitHub
**Tag command (on the release commit):**

```sh
git tag -a vX.Y.Z -m "vX.Y.Z"
git push origin vX.Y.Z
```

**Branch push:** `git push origin main` (or open a PR from `prepare-release` → `main` and merge)
**Credentials:** standard GitHub push credentials (no registry tokens needed)
**Signing:** not required (commit / tag GPG signing optional, follow whatever git is already configured to do)

## Changelog

**Location:** `CHANGELOG.md`
**Format:** Keep a Changelog 1.1.0
**Process:** rename `## [Unreleased]` to `## [X.Y.Z] - YYYY-MM-DD`; create a fresh empty `## [Unreleased]` block above it for next cycle

## GitHub Release

- [ ] Create release page at: `https://github.com/datenkollektiv/rustyfarian-network/releases/new`
- [ ] Use tag `vX.Y.Z`
- [ ] Title: `vX.Y.Z`
- [ ] Body: paste the `## [X.Y.Z]` section from `CHANGELOG.md`
- [ ] No artifact attachments (workspace is library crates; users consume via git dep)

## Post-release Steps

- [ ] Merge `prepare-release` → `main` (fast-forward)
- [ ] Update `docs/ROADMAP.md` if any items shipped in this release are still listed as in-progress
- [ ] Verify the GitHub release page is publicly visible and the tag resolves

## Rollback Procedure

If a release must be retracted after tagging:
1. Delete remote tag: `git push --delete origin vX.Y.Z`
2. Delete local tag: `git tag -d vX.Y.Z`
3. Delete the GitHub release page (via web UI or `gh release delete vX.Y.Z`)
4. Revert the release commit on `main`: `git revert <release-commit-sha>` and push

Note: because nothing is published to a registry, rollback is fully reversible.

## Release Record Location

Each release produces files in `release/`:
1. `YYYY-MM-DD-<version>-preflight.md` — pre-flight assessment
2. `YYYY-MM-DD-<version>-plan.md` — ordered execution plan
3. `YYYY-MM-DD-<version>-record.md` — what was published and what remains
