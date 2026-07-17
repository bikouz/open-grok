---
name: release-open-grok
description: Build, publish, and verify an Open Grok macOS arm64 release end to end. Use when the user says push the build, publish the release, update an existing release, bump `OPEN_GROK_VERSION`, upload release assets, verify latest, or smoke-test the installer or managed installation.
---

# Release Open Grok

Treat source push, tag creation, local artifact build, GitHub publication, and installed-binary verification as distinct gates. “The release” means all gates pass.

## Prepare an exact clean source

1. Read `docs/agents/development.md`, the matching `docs/releases/` note, `OPEN_GROK_VERSION`, and `scripts/build-macos-release.sh`.
2. Run the focused/full tests required by the changes and commit them before building.
3. Require a clean worktree and record the full/short HEAD. Recheck status after the long link in case concurrent edits appeared.
4. Verify a trusted arm64 `ripgrep 15.0.0`; set `GROK_TOOLS_BUNDLE_RG_PATH` to its explicit path. Never substitute a newer Homebrew `rg`.

## Build and verify local assets

Run `./scripts/build-macos-release.sh`. It must produce the canonical five assets:

- `dist/open-grok-macos-aarch64`
- `dist/open-grok-macos-aarch64.sha256`
- `dist/install.sh`
- `dist/LICENSE`
- `dist/THIRD-PARTY-NOTICES`

Independently verify arm64 Mach-O type, strict ad-hoc signature, embedded version and commit, SHA-256, and the bundled `rg`. Exercise `dist/install.sh` against `OPEN_GROK_RELEASE_BASE_URL=file://<absolute-dist-dir>` with explicit temporary `OPENGROK_HOME` and `OPEN_GROK_BIN_DIR` paths.

## Publish exact bytes

1. Push the exact source commit and tag.
2. Check GitHub CLI auth without inherited overrides: `env -u GH_TOKEN -u GITHUB_TOKEN gh auth status`.
3. Create or update the full release with the five verified local assets; mark the intended release Latest.
4. Do not use browser upload as the primary path when local file access is blocked.

If another publisher races this release, compare tag peel, asset size, and digest before replacing anything. Never assume an existing same-version asset was built from the current head.

## Verify public and managed paths

- Re-download all five public assets to a fresh directory and compare GitHub/local digests and sizes.
- Run both tag-specific and `/releases/latest/download/install.sh` smokes in isolated homes.
- Verify the downloaded/installed binary reports the expected version and commit and passes signature/checksum checks.
- Upgrade the managed install only when requested or already part of the release task, then verify `open-grok --version` and updater latest-state behavior.

Report the release URL, tag/commit, artifact digest, tests, public installer result, managed-install result, and any missing attestation.
