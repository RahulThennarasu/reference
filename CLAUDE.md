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

## known gaps / things to verify before relying on them

- `merge_insert` has a reported (possibly fixed) issue when called on a table that already has a vector index built. test this specifically once indexing is running past the prototype stage, not just on a fresh unindexed table.
- lancedb's rust crate is younger and less battle-tested than its python bindings. if something about table operations behaves unexpectedly, check `docs.rs/lancedb/latest` directly rather than assuming parity with python docs/behavior.
- the `embeddings` module in the lancedb crate leans on openai/bedrock integrations, not used here. embeddings are computed manually via `candle` and inserted as plain vectors.

## how to verify a crate/library claim before building on it

go straight to `docs.rs/<crate-name>/latest` and check the actual struct/trait method list and doc comments, that's generated directly from the crate source, so it's the canonical current api, not commentary about it. don't rely on blog posts, stack overflow, or general knowledge for fast-moving rust crates.

## semantic code search (dogfooding)

this repo is itself indexed by `reference`, chunked at function/class granularity (see `docs/code-aware-chunking.md`).

use it first, before grep, whenever a question names no literal string/identifier to search for and instead describes *behavior* or *intent*: "why don't impl methods get their own chunk", "what stops a wip file with a syntax error from disappearing from the index", "how does a citation's line number stay correct when the chunk isn't the whole file". grep is still the right tool once you already know the identifier/string you're looking for (a constant name, an error message, a literal number), this is for the case where you'd otherwise have to guess identifier names to even start grepping.

full agent usage reference (json output shape, examples, caveats): `docs/cli-agent-usage.md`. quick version:

```
reference-cli search "<query>" --json
```

## explicit non-goals (don't build these without discussing first)

- no cloud sync of the index
- no general chatbot mode
- no auto-indexing by default
- no persistent "network activity: none" indicator in the main search ui, this was deliberately cut as unnecessary clutter, it can live in settings if surfaced at all
