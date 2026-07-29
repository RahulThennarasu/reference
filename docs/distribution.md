# distribution

status: manual local build works. sections 1-3 (signing identity, notarization, entitlements) verified end-to-end via `pnpm tauri build` — produces a signed, notarized, stapled `.app` + `.dmg` given a "Developer ID Application" cert in the keychain and `APPLE_ID`/`APPLE_PASSWORD`/`APPLE_TEAM_ID`/`APPLE_SIGNING_IDENTITY` exported locally. section 4 (`.github/workflows/release.yml`, automated on tag push) built, not yet run against a real tag — needs the github secrets listed in that section added before the first tag push. section 5 (auto-update) built: `tauri-plugin-updater` checks `https://github.com/RahulThennarasu/reference-macos/releases/latest/download/latest.json` on app startup. mac only for now (this project only targets mac right now, no cuda/windows code exists yet, see readme.md).

## the problem

right now "using" this app means cloning the repo and running `pnpm tauri build` yourself. that's fine for a contributor, it's a hard wall for anyone else. a real developer audience (the mcp-integration use case this app is actually strongest at, see the "can this be used by devs" discussion) won't clone and build a rust/tauri project just to try a search tool.

concretely, what's missing:

- no code signing, so gatekeeper blocks the built `.app` on any mac that isn't the one that built it
- no notarization, so even a signed build gets a scary warning on first launch
- no release workflow, so there's no repeatable, automated way to produce a distributable build at all, every build today is a manual local `pnpm tauri build`
- no auto-update path, so even an installed user has no way to get a new version without repeating the manual build

this is a separate gap from the five in `docs/feature-gaps.md`, that doc's intro line already calls it out as "a separate, larger gap not covered here."

## what mac distribution actually needs

### 1. an apple developer account + signing identity

code signing on mac requires an apple developer program membership ($99/year) and a "developer id application" certificate, this is a hard prerequisite, not something the build tooling can work around. tauri's bundler picks up a signing identity via `tauri.conf.json`'s `bundle.macOS.signingIdentity`, currently unset (the bundle config in `app/src-tauri/tauri.conf.json` has no `macOS` key at all).

### 2. notarization

once signed, the built app has to be submitted to apple's notary service (`xcrun notarytool submit`) and the notarization ticket stapled to it (`xcrun stapler staple`). tauri's cli has built-in support for this when the right environment variables are set (`APPLE_ID`, `APPLE_PASSWORD` as an app-specific password, `APPLE_TEAM_ID`), it's not a separate custom script needed, just config + secrets.

### 3. entitlements

the app does two things that macOS increasingly gates behind explicit entitlements: it reads arbitrary user-chosen folders (file access) and it fetches model weights from hugging face hub on first run / on model switch (network access, see `core/src/embedding.rs`'s `Embedder::load`). an entitlements plist needs to declare both, otherwise a sandboxed or hardened-runtime build can silently fail at exactly those operations, in a way that's hard to debug from the outside since the failure looks like "search just doesn't work" rather than a clear permissions error.

### 4. release workflow

`.github/workflows/release.yml` triggers on a `v*.*.*` tag push and runs `tauri-apps/tauri-action` against `app/` (the actual tauri project root — see the workflow's `projectPath: app`), which builds, signs, and notarizes exactly like a local `pnpm tauri build`, then uploads the `.dmg` + a signed `latest.json` (see section 5) as release assets on a *separate* public repo, `RahulThennarasu/reference-macos` — not this repo, which stays private (see `docs/download-page-usage-terms.md` on why that separation matters: a public release repo is the thing an actual user's updater talks to, this source repo never needs to be).

this needs the following as repo secrets on **this** repo (`reference`), since that's where the workflow runs:

- `APPLE_CERTIFICATE` / `APPLE_CERTIFICATE_PASSWORD` — base64-encoded "Developer ID Application" `.p12` + its password, imported into a throwaway keychain by `apple-actions/import-codesign-certs`
- `APPLE_SIGNING_IDENTITY` / `APPLE_ID` / `APPLE_PASSWORD` / `APPLE_TEAM_ID` — same four env vars section 2 already uses for local notarization
- `TAURI_SIGNING_PRIVATE_KEY` / `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` — the updater signing keypair (see section 5)
- `RELEASE_MACOS_TOKEN` — a fine-grained personal access token with `contents: write` on `RahulThennarasu/reference-macos` specifically (the workflow's own default `GITHUB_TOKEN` only has permissions scoped to `reference`, it can't create a release on a different repo)

version bumps are still manual: `app/src-tauri/tauri.conf.json`'s `version` field has to be bumped before tagging, since that's what the updater compares against `latest.json`'s version, tagging alone doesn't change it.

**hardening against supply-chain/worm-style attacks**, since this workflow is the one place real signing secrets and a cross-repo write token ever exist at once:

- every third-party action in both `ci.yml` and `release.yml` is pinned to a full commit SHA, not a mutable tag
- `step-security/harden-runner` runs first in every job (audit mode — logs egress, doesn't block, since macOS runner network enforcement is less mature than linux's), giving a concrete log to check if a release ever looks wrong
- frontend deps install with `--ignore-scripts`, then only this repo's own `postinstall` script runs explicitly — blocks arbitrary preinstall/postinstall code from any of the ~85 transitive npm dependencies, the exact mechanism self-propagating worms like Shai-Hulud use
- the `release` job requires a GitHub Environment named `release` (Settings -> Environments, not provisionable from the yaml) with a required reviewer — a tag push alone can no longer silently trigger a signed release, someone has to look at which tag and approve it
- `permissions: contents: read` at the workflow level in both files — the only write capability release.yml uses is `RELEASE_MACOS_TOKEN` (a PAT scoped to `contents: write` on `reference-macos` alone), never the ambient `GITHUB_TOKEN`

### 5. auto-update

`tauri-plugin-updater` (+ `tauri-plugin-process` for `relaunch()` after install) is wired up in `app/src-tauri` (`Cargo.toml`, `lib.rs`'s `.plugin(...)` registration, `capabilities/default.json`'s `updater:default`/`process:allow-restart`). `tauri.conf.json`'s `plugins.updater` points at `https://github.com/RahulThennarasu/reference-macos/releases/latest/download/latest.json` and carries the public half of a dedicated minisign keypair (private half lives only at `~/.tauri/reference-updater.key` on the machine that runs releases, password-protected, never committed — needed as the `TAURI_SIGNING_PRIVATE_KEY`/`_PASSWORD` secrets in section 4). the app calls `check()` once on startup (`app/src/main.ts`'s `checkForAppUpdate`); if a newer signed release is found it lights a small dot on the settings button rather than interrupting search, and the settings panel (⌘,) gets an "install update and restart" row that calls `downloadAndInstall()` then `relaunch()`.

## explicitly deferred

- **windows/cuda packaging.** this project only targets mac right now (metal/cpu backend, no cuda code written yet, per the project's current stated scope). windows signing (authenticode, a separate cert from apple's) and a cuda-enabled build are a distinct, later effort, not part of this doc.
- **linux packaging.** not discussed anywhere in this project's scope so far, not assumed in-scope here either.
- **homebrew cask / other package manager distribution.** worth considering once a signed, notarized `.dmg` exists as the underlying artifact to point a cask at, doesn't make sense before that.

## effort estimate

the apple developer account and first-time signing/notarization setup is mostly one-time friction (getting certs generated, entitlements right, github secrets configured), probably the slowest part elapsed-time-wise since it involves apple's own account/cert issuance, not code. the github actions workflow itself is small, comparable to a single language addition in `docs/code-aware-chunking.md`'s phased rollout. auto-update turned out to be similarly small once the release workflow existed to publish a manifest for it to point at — see section 5.
