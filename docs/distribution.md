# distribution

status: manual local build works. sections 1-3 (signing identity, notarization, entitlements) verified end-to-end via `pnpm tauri build` — produces a signed, notarized, stapled `.app` + `.dmg` given a "Developer ID Application" cert in the keychain and `APPLE_ID`/`APPLE_PASSWORD`/`APPLE_TEAM_ID`/`APPLE_SIGNING_IDENTITY` exported locally. section 4 (`.github/workflows/release.yml`, automated on tag push) not started. section 5 (auto-update) not started, per its own note not required yet. mac only for now (this project only targets mac right now, no cuda/windows code exists yet, see readme.md).

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

none of the above should be a manual step run from someone's laptop, that doesn't scale past one person and isn't reproducible. the standard shape for this is a github actions workflow (`.github/workflows/release.yml`, doesn't exist yet) that:

- triggers on a version tag push
- runs `tauri-apps/tauri-action` (or an equivalent manual `pnpm tauri build` + upload) with the signing cert (base64-encoded `.p12`, stored as a repo secret) and the notarization credentials above also as secrets
- uploads the resulting `.dmg`/`.app` as a github release artifact

this also forces the version bump + changelog discipline that a manual build process doesn't.

### 5. auto-update (stretch, not required for a first release)

tauri has an official updater plugin (`tauri-plugin-updater`) that checks a hosted json manifest for new versions and can apply them in place. worth adding once there's an actual release cadence to update *to*, not before, a first version doesn't need to update itself.

## explicitly deferred

- **windows/cuda packaging.** this project only targets mac right now (metal/cpu backend, no cuda code written yet, per the project's current stated scope). windows signing (authenticode, a separate cert from apple's) and a cuda-enabled build are a distinct, later effort, not part of this doc.
- **linux packaging.** not discussed anywhere in this project's scope so far, not assumed in-scope here either.
- **homebrew cask / other package manager distribution.** worth considering once a signed, notarized `.dmg` exists as the underlying artifact to point a cask at, doesn't make sense before that.

## effort estimate

the apple developer account and first-time signing/notarization setup is mostly one-time friction (getting certs generated, entitlements right, github secrets configured), probably the slowest part elapsed-time-wise since it involves apple's own account/cert issuance, not code. the github actions workflow itself is small, comparable to a single language addition in `docs/code-aware-chunking.md`'s phased rollout. auto-update is the piece worth explicitly *not* doing yet.
