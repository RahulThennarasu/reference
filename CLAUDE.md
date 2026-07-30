# claude.md

context for working on this project. read readme.md first for the product pitch, this file is about how to build it.

## what this is

reference (symbol: &) is a tauri desktop app: local file indexing + semantic search, gpu-accelerated, fully offline. see readme.md for the full feature scope and rationale.

the name reflects the core mechanism: the index doesn't store or duplicate your files, it stores a reference back to where they actually live (file path + embedding). a reference is only ever valid within memory you actually own, which mirrors the local-only design.

## build philosophy

- **one rust core, not per-platform code.** the watch, embed, store, search pipeline is a single rust codebase. gpu backend (cuda vs metal) should be a config/feature flag via `candle`, not a fork.
- **no python dependency.** embeddings run through `candle`, not a bundled python runtime. if a task seems to need python, stop and find the rust equivalent first.
- **local-only, always.** no code path should make a network call for indexing, embedding, or search. if a feature seems to require network access, it's probably out of scope or needs to be explicitly opt-in and clearly surfaced.
- **writes go through `merge_insert` only.** never write to the lancedb table via `add()` directly in the indexing pipeline, primary keys aren't enforced as a uniqueness constraint on plain writes, so bypassing `merge_insert` risks duplicate rows for the same chunk. every upsert (new/changed file) deletes existing rows for that path first, then goes through `table::merge_insert` keyed on `path + start_line`: a file is chunked into multiple rows now (see `docs/code-aware-chunking.md`), so a plain path-only key can't express "this chunk no longer exists" when a function is deleted.
- **build in vertical slices, not layers.** get watch, embed, store, search fully working end-to-end for one folder via cli before adding: gpu backend selection, the tauri ui, hybrid fuzzy+semantic ranking, or answer synthesis.

## current architecture decisions

| decision        | choice                                                                                          | notes                                                                                                 |
| --------------- | ----------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------- |
| app shell       | tauri                                                                                           | cross-platform, lightweight vs electron                                                               |
| file watching   | `notify` crate                                                                                  |                                                                                                       |
| embedding model | all-minilm-l6-v2 via `candle`                                                                   | 384-dim output. cpu first, gpu backend later                                                          |
| vector db       | lancedb (rust crate, embedded)                                                                  | local filesystem storage, no server process                                                           |
| upsert          | delete rows for `path`, then `table::merge_insert(&["path", "start_line"])` with `when_matched_update_all` + `when_not_matched_insert_all` | one file -> N chunk rows now, see `docs/code-aware-chunking.md` |
| index type      | `index::auto`                                                                                   | lancedb auto-selects ivf-pq for vector columns, btree otherwise, no manual index tuning needed for v1 |
| release profile | `[profile.release]` with `strip = true`, `lto = true`, `codegen-units = 1`                      | default release profile produced a ~220mb macOS bundle (candle + arrow/datafusion/lance + 4 tree-sitter grammars statically linked); this cut it to ~102mb. lto forces a slow full rebuild (~15min), dev profile is untouched |
| agent-facing interface | MCP server (`mcp/`, crate `rmcp` 2.2.0) over stdio, replacing `reference-cli` | verified `rmcp` 2.2.0 (not the `3.0.0-beta.2` cargo surfaces by default) as current stable via docs.rs + github tags, per this file's own rule below on verifying crate claims; four read-only tools, `search`, `explain`, `find_similar`, and `check_doc_drift`, no folder-management tool by design — both tools' optional `folder` param scopes to one already-watched folder, a different thing from managing *which* folders are watched, which stays app-only |

## known gaps / things to verify before relying on them

- ~~`merge_insert` has a reported (possibly fixed) issue when called on a table that already has a vector index built.~~ verified fixed on lancedb 0.31.0: `core/src/store.rs`'s `merge_insert_after_vector_index_is_built` test builds a real IVF-PQ index (300+ rows) then runs the same delete+merge_insert upsert `replace_chunks` does against it — upsert succeeds, stale rows are gone, and the updated row stays searchable.
- lancedb's rust crate is younger and less battle-tested than its python bindings. if something about table operations behaves unexpectedly, check `docs.rs/lancedb/latest` directly rather than assuming parity with python docs/behavior.
- the `embeddings` module in the lancedb crate leans on openai/bedrock integrations, not used here. embeddings are computed manually via `candle` and inserted as plain vectors.

## how to verify a crate/library claim before building on it

go straight to `docs.rs/<crate-name>/latest` and check the actual struct/trait method list and doc comments, that's generated directly from the crate source, so it's the canonical current api, not commentary about it. don't rely on blog posts, stack overflow, or general knowledge for fast-moving rust crates.

## semantic code search (dogfooding)

this repo is itself indexed by `reference`, chunked at function/class granularity (see `docs/code-aware-chunking.md`).

use it first, before grep, whenever a question names no literal string/identifier to search for and instead describes *behavior* or *intent*: "why don't impl methods get their own chunk", "what stops a wip file with a syntax error from disappearing from the index", "how does a citation's line number stay correct when the chunk isn't the whole file". grep is still the right tool once you already know the identifier/string you're looking for (a constant name, an error message, a literal number), this is for the case where you'd otherwise have to guess identifier names to even start grepping.

`reference-cli` (the old shell-out-and-parse-json interface) has been removed. its replacement is an MCP server (`mcp/`), exposing four read-only tools over the same index — no folder-management tool by design, see the opt-in principle above. `mcp__reference-mcp__search` finds chunks matching a natural-language query; prefer it directly over grep or shelling out whenever the question describes behavior/intent rather than naming a literal string to search for. its optional `exclude_folder` param is the inverse of `folder` — omits one watched project instead of scoping to it, for "how did I solve this in a different project" recall. `mcp__reference-mcp__explain` runs the same extractive citation synthesis `search` uses for question-shaped queries, but unconditionally regardless of phrasing — use it for a bare identifier or short phrase (e.g. "parse_config") where `search` alone would return raw results with no citations. `mcp__reference-mcp__find_similar` takes a chunk already found (path + start_line, usually from a prior `search` hit) and finds other chunks with the closest embedding — no query text, useful for catching duplicated/near-duplicate logic elsewhere in the index before writing new code. `mcp__reference-mcp__check_doc_drift` takes a doc chunk the same way and checks whether it still scores close to any actual code construct in the index, flagging `likely_stale` when nothing does — useful for catching documentation that no longer reflects the current implementation. full agent usage reference (setup, tool schemas, caveats): `docs/mcp-agent-usage.md`.

tool selection is a model decision, not something CLAUDE.md can force — if grep gets reached for anyway on a behavior/intent question, that's expected, not a bug to fix here. `/refsearch <query>` (`.claude/commands/refsearch.md`) is the explicit, reliable way to invoke the tool directly instead of relying on this instruction being followed; it also passes `folder: ${CLAUDE_PROJECT_DIR}` automatically, scoping the search to the current project by default (see `search`'s `folder` param in `docs/mcp-agent-usage.md`) — an unrelated watched folder that happens to share some vocabulary with the query has outranked the right file before, this is the guard against that.

## explicit non-goals (don't build these without discussing first)

- no cloud sync of the index
- no general chatbot mode
- no auto-indexing by default
- no persistent "network activity: none" indicator in the main search ui, this was deliberately cut as unnecessary clutter, it can live in settings if surfaced at all
