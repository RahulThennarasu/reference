# manual update notifications

status: superseded. `tauri-plugin-updater` is now wired up end-to-end — see `docs/distribution.md` section 5. this doc is kept for the historical reasoning below.

## the problem (resolved)

`docs/distribution.md` used to scope auto-update as a stretch goal, deliberately not required for a first release, since there was no hosted update manifest for `tauri-plugin-updater` to point at yet. that gap is closed: `.github/workflows/release.yml` publishes a signed `latest.json` alongside every `.dmg` to the public `RahulThennarasu/reference-macos` repo, and the app polls it on startup.

before this, every new build was a fully manual process on both sides: build it, upload the new `.dmg` to the download page, and separately tell people it exists, with nothing in the app itself checking for or surfacing a new version.

## what to do until auto-update exists

whenever a new build goes up on the download page, say so somewhere people already look, a changelog on the page itself, a README note, whatever the actual announcement channel ends up being once there's a real audience. the point isn't the channel, it's remembering the step exists at all.

## why this is easy to forget

it's not something that breaks visibly, an old build just keeps working fine and looking current to the person running it, there's no error, no warning, nothing pointing back at this gap. it only becomes a real cost the first time a meaningful fix or feature ships and nobody who already installed the app finds out. flagging it here so it's not silently skipped once builds start going out semi-regularly.
