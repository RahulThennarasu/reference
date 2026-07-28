# manual update notifications

status: not started. note-to-self doc, not a scoping doc like the others in this folder.

## the problem

`docs/distribution.md` already scopes auto-update as a stretch goal, deliberately not required for a first release, since there's no hosted update manifest for `tauri-plugin-updater` to point at yet (that needs the download site from `docs/distribution.md` to exist first, and a signed update manifest hosted somewhere on it).

until that's built, every new build is a fully manual process on both sides: build it, upload the new `.dmg` to the download page, and separately tell people it exists. nothing in the app itself checks for or surfaces a new version. if that last step doesn't happen, a user who installed once has no way of finding out there's anything newer, no notification, no badge, nothing.

## what to do until auto-update exists

whenever a new build goes up on the download page, say so somewhere people already look, a changelog on the page itself, a README note, whatever the actual announcement channel ends up being once there's a real audience. the point isn't the channel, it's remembering the step exists at all.

## why this is easy to forget

it's not something that breaks visibly, an old build just keeps working fine and looking current to the person running it, there's no error, no warning, nothing pointing back at this gap. it only becomes a real cost the first time a meaningful fix or feature ships and nobody who already installed the app finds out. flagging it here so it's not silently skipped once builds start going out semi-regularly.
