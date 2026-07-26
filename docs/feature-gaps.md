# feature gaps before this is public-ready

status: not exhaustive, this is the list that came out of a single review pass, feature-level only. distribution/signing/testing infra is a separate, larger gap not covered here.

the core mechanism works and is genuinely differentiated: local opt-in watching, hybrid search, code-aware chunking, answer synthesis with citations, and a working mcp path for agents. these are the gaps worth closing before treating it as feature-complete for a broad public audience, not signs the foundation is weak.

## 1. language coverage

function-level chunking covers rust, python, typescript, javascript, go, java, c, and c++. ruby and everything else still falls back to whole-file chunking, which is exactly the dilution problem chunking exists to solve in the first place (see `docs/code-aware-chunking.md`). most developers touch at least one language outside the original four. of everything on this page, this was the single gap most likely to make a new user bounce off immediately; the languages added this pass cover the large majority of that traffic, the rest are lower-traffic languages to pick up only as real need shows up.

## 2. no folder scoping in the app itself — closed

the mcp `search` tool has an optional `folder` param that restricts a query to one watched folder (see `docs/mcp-agent-usage.md`), added after a real observed failure where an unrelated watched project outranked the correct file. the desktop app's own search palette had no equivalent — a human with five projects watched had every query search all five, with no filter ui.

closed: the `search` tauri command (`app/src-tauri/src/lib.rs`) now takes the same optional `folder` param and threads it straight into `hybrid_search`. the app surfaces it as a scope picker in the search bar (`app/src/main.ts`, `#scope-btn`) — hidden entirely when zero or one folder is watched, since scoping is meaningless with nothing to choose between. picking a folder there renders inline in the results list rather than as a native `<select>` or a floating popover: this app dynamically resizes the actual OS window to fit `#results`' content (see `resize()`), so anything positioned outside that flow risks getting clipped by the window bounds — rendering the choices as ordinary result rows sidesteps that entirely and reuses the app's existing `.result`/`.selected` styling for free.

## 3. no exact symbol or identifier lookup — closed

search is always fuzzy filename plus semantic plus content overlap. there was no fast, guaranteed-precise "find this exact function by name" path. this was a deliberate choice for the agent-facing mcp use case (the guidance was explicitly not to replace grep), and it was the right call there — this gap was specifically about the human-facing desktop app, where ide go-to-definition expectations don't map onto "search is always fuzzy/semantic."

closed, app-only (the mcp `search` tool is deliberately untouched, per the reasoning above): `chunk::extract_name` (`core/src/chunk.rs`) pulls each chunk's own identifier straight from the already-parsed tree-sitter node — a direct `name` field for the common case, one level down for wrapper nodes (js/ts `const foo = () => {}`, go's `type_declaration` wrapping a `type_spec`), or by following a `declarator` field chain for c/c++, where a function's identifier isn't exposed via a `name` field at all (`int *foo(...)` nests it inside `function_declarator { declarator: pointer_declarator { declarator: identifier } }`). `impl_item` is explicitly excluded rather than left to the generic fallback: its named children include the trait being implemented (`impl Debug for Thing`), which itself exposes a `name` field ("Debug") that the fallback would otherwise mistake for the impl block's own name. the name is stored as a new `name` column (`core/src/store.rs`'s schema — see the note below on what this means for an existing index) and matched via `Store::find_by_name`, an equality filter pushed down through `only_if` rather than a table scan, so it stays fast regardless of index size, no embedding or per-row scoring involved. the app's `search` command (`app/src-tauri/src/lib.rs`) only attempts this when the query is shaped like a bare identifier (`looks_like_identifier`: single token, valid identifier characters, deliberately conservative) — natural-language queries never touch it — and exact hits lead the results list, badged `EXACT` in the ui, ahead of whatever the fuzzy/semantic/content-overlap blend separately found.

**breaking schema change, no migration path.** adding the `name` column means an index built before this change is missing it entirely — `hybrid_search`/`find_by_name` will error (`missing name column`) against an old on-disk table, the same "wipe and reindex" situation `docs/code-aware-chunking.md` already documents hitting twice before. there's still no migration system in this codebase; this is the third time the fix was `rm -rf ~/.reference/index` and letting it reindex from a clean schema.

## 4. silent truncation with no visibility

a chunk longer than the embedding model's token limit gets truncated with just a log line, nothing surfaced to the user. search quality can quietly degrade on unusually large functions and nobody watching the ui would know it happened.

## 5. zero configurability

model choice, ranking weights, gpu backend selection, all compile time constants. defensible as "it just works" philosophy and consistent with the project's opt-in-only, no-config-sprawl instincts elsewhere, but public users sometimes expect at least minimal knobs, especially around which embedding model runs on their hardware.
