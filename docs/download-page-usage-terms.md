# download page usage terms

status: not started. note-to-self doc, not a scoping doc like the others in this folder.

## the problem

this repo is private and has no LICENSE file, deliberately (see the "current status" discussion around distribution: the repo should stay private for now, and redistribution shouldn't be permitted). but a private repo with no LICENSE only protects the source code that lives in this repo. it says nothing to the person who downloads the built `.dmg` from wherever it ends up getting hosted, since they will never see this repo at all, private or not.

concretely: the moment there's a separate public download page for the `.dmg` (see `docs/distribution.md`), that page is the only thing an actual user ever sees. if it doesn't say anything about usage terms, nobody downloading the app has any way to know redistribution isn't permitted, license or no license.

## what to do when the download page gets built

put a short, plain line on the download page itself, near the download button, something like:

> for personal use. redistribution not permitted.

that's it, no need for a full EULA or terms-of-service page for a v1. the point is just that the terms exist somewhere a real user will actually read, instead of only existing as an absence (no LICENSE file) in a repo they'll never have access to.

## why this is easy to forget

the repo being private already handles source-code protection today, so this doesn't feel urgent while there's no download page yet. it becomes relevant the moment `docs/distribution.md`'s distribution plan actually ships a public download link, not before. flagging it here specifically so it doesn't get missed at that point.
