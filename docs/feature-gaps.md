# feature gaps before this is public-ready

status: not exhaustive, this is the list that came out of a single review pass, feature-level only. distribution/signing/testing infra is a separate, larger gap not covered here.

the core mechanism works and is genuinely differentiated: local opt-in watching, hybrid search, code-aware chunking, answer synthesis with citations, and a working mcp path for agents. these are the gaps worth closing before treating it as feature-complete for a broad public audience, not signs the foundation is weak.

## 1. language coverage

function-level chunking covers rust, python, typescript, javascript, go, java, c, and c++. ruby and everything else still falls back to whole-file chunking, which is exactly the dilution problem chunking exists to solve in the first place (see `docs/code-aware-chunking.md`). most developers touch at least one language outside the original four. of everything on this page, this was the single gap most likely to make a new user bounce off immediately; the languages added this pass cover the large majority of that traffic, the rest are lower-traffic languages to pick up only as real need shows up.

## 2. no folder scoping in the app itself

the mcp `search` tool has an optional `folder` param that restricts a query to one watched folder (see `docs/mcp-agent-usage.md`), added after a real observed failure where an unrelated watched project outranked the correct file. the desktop app's own search palette has no equivalent. a human with five projects watched has every query search all five, with no filter ui. that's an inconsistency between the two surfaces, not just a missing nice-to-have.

## 3. no exact symbol or identifier lookup

search is always fuzzy filename plus semantic plus content overlap. there is no fast, guaranteed-precise "find this exact function by name" path. this was a deliberate choice for the agent-facing mcp use case (the guidance was explicitly not to replace grep), and it was the right call there. a public human user coming from ide go-to-definition expectations might still miss it.

## 4. silent truncation with no visibility

a chunk longer than the embedding model's token limit gets truncated with just a log line, nothing surfaced to the user. search quality can quietly degrade on unusually large functions and nobody watching the ui would know it happened.

## 5. zero configurability

model choice, ranking weights, gpu backend selection, all compile time constants. defensible as "it just works" philosophy and consistent with the project's opt-in-only, no-config-sprawl instincts elsewhere, but public users sometimes expect at least minimal knobs, especially around which embedding model runs on their hardware.
