# code-aware chunking

status: **phases 1–2 implemented** (Rust, Python, TypeScript/JavaScript). This is a scoping doc, not an implementation plan to follow blindly — expect it to change once real code proves parts of it wrong. Go/others (phase 3) not started, no known need yet.

## the problem

`Embedder::embed()` (`core/src/embedding.rs`) takes a whole file's text and produces a single 384-dim vector for the entire file. A 2000-line file with 40 functions gets one embedding representing the "average topic" of everything in it. A query like *"function that validates JWT tokens"* has to compete against that file's overall vibe — if the same file also handles logging, routing, and error handling, the JWT-specific signal gets diluted into noise from unrelated code.

This is why search today finds the right *file* but not the right *part of the file*. The click-to-open-at-best-line feature (`synthesize::best_matching_line`) is a workaround for this — it re-scores the file line-by-line at click time to find the relevant spot — but that's compensating for a coarse index, not fixing it. With per-function embeddings, the index itself would already know which function is relevant, and citations/results could point at exact functions instead of "somewhere in this file, we're not sure exactly where."

## goal

Index code files at function/class granularity instead of whole-file, so:
- search results can be "function `validate_jwt` in `auth.rs`", not just "`auth.rs`"
- answer synthesis can cite an exact function instead of a best-guess line found post-hoc
- ranking isn't diluted by unrelated code living in the same file

## non-goals (for the first version)

- Full call-graph / cross-reference understanding (that's a much bigger project — this is chunking, not a code intelligence engine)
- Every language on day one — see [phased language rollout](#phased-language-rollout)
- Replacing whole-file indexing for prose (`.md`, `.txt`) — chunking only makes sense for code; prose files keep working exactly as they do today
- Solving semantic-versioning of chunks across refactors (e.g. "this function got renamed, treat it as the same logical unit") — a moved/renamed function is just a delete + insert, not tracked as continuity

## how chunking actually works

[tree-sitter](https://tree-sitter.github.io/tree-sitter/) parses source text into a concrete syntax tree you can walk to find nodes like `function_item`, `class_declaration`, `impl_item`, each with an exact byte range in the source. That's what makes it possible to slice out "just this function's text" cleanly, instead of guessing at line-count boundaries (which would cut a function in half arbitrarily).

Verified against docs.rs (per `CLAUDE.md`'s own rule — don't trust memory for fast-moving crates):

| crate | version (verified via `cargo info`) |
| --- | --- |
| `tree-sitter` | 0.26.11 |
| `tree-sitter-rust` | 0.24.2 |
| `tree-sitter-python` | 0.25.0 |
| `tree-sitter-typescript` | 0.23.2 |
| `tree-sitter-javascript` | 0.25.0 |
| `tree-sitter-go` | 0.25.0 |

Core API surface confirmed present on docs.rs for `tree-sitter` 0.26.11: `Parser`, `Language`, `Tree`, `Node`, `Query`, `QueryCursor`. The actual chunk-extraction shape:

```rust
let mut parser = Parser::new();
parser.set_language(&tree_sitter_rust::LANGUAGE.into())?;
let tree = parser.parse(source, None).ok_or(...)?;

// a .scm query file, one per language, e.g. "function boundaries":
//   (function_item name: (identifier) @name) @function
//   (impl_item) @impl
let query = Query::new(&language, QUERY_SRC)?;
let mut cursor = QueryCursor::new();
for m in cursor.matches(&query, tree.root_node(), source.as_bytes()) {
    // m gives you the matched node's byte_range() -> slice `source` directly
}
```

Each language needs its own `.scm` query file defining what counts as a chunk boundary for that language (Rust: `function_item`, `impl_item`; Python: `function_definition`, `class_definition`; etc.) — these aren't shared across languages, they're hand-written per grammar.

## what actually has to change

This touches nearly every layer built so far, not just embedding. In rough dependency order:

### 1. schema (`core/src/store.rs`)

Today: one row per file, keyed by `path` (the `merge_insert` key in `Store::upsert`).

Proposed: one row per chunk. Key becomes `path + start_line` (a single file can now produce 1 to 50+ rows). New columns needed:

| column | type | notes |
| --- | --- | --- |
| `path` | Utf8 | unchanged |
| `start_line` | Int32 | 1-indexed, becomes part of the merge key |
| `end_line` | Int32 | for citing a range, not just a point |
| `chunk_kind` | Utf8 | `"function"`, `"class"`, `"impl"` (Rust), `"interface"` (TS), `"file"` (whole-file fallback) — lets the UI show what kind of unit matched, not just plain file |
| `content` | Utf8 | now the chunk's text, not the whole file |
| `embedding` | FixedSizeList<Float32, 384> | unchanged shape, now per-chunk |

Non-code files (`.md`, `.txt`, etc.) still get exactly one row with `chunk_kind = "file"`, `start_line = 1`, `end_line = <last line>` — this keeps prose handling identical to today, just expressed in the new schema instead of a special case.

### 2. indexing pipeline (`core/src/watcher.rs`)

`index_file()` currently does: read file → embed whole content → one `store.upsert()` call.

Becomes: read file → detect language from extension → if a chunker exists for that language, parse and extract chunks; otherwise (unsupported language, or parse failure) fall back to the current whole-file behavior → embed each chunk → one `upsert` per chunk.

**Fallback is not optional.** A parse failure (syntax error in a WIP file, an unsupported language, a binary misdetected as text) must never mean the file silently drops out of the index — it must fall back to whole-file indexing, same as today. This mirrors the existing principle in `read_text()`: skip gracefully, never hard-fail the whole watch loop over one bad file.

### 3. stale chunk cleanup

This is the sharp edge. `merge_insert` only adds/updates rows — it has no concept of "these 5 rows that used to exist for this file no longer should." We've already hit this exact problem twice this session (unwatched folders leaving orphaned rows, `.git` content indexed before gitignore support existed) and both times the fix was a manual `rm -rf` of the whole index.

With whole-file indexing, an `upsert` on file save was a clean 1-in-1-out replacement. With chunking, if a function gets deleted or a file shrinks from 10 functions to 6, the old rows for the removed functions need to be explicitly deleted — an upsert alone can't detect "this chunk no longer exists."

Concrete plan: on every file re-index (create/modify event), first delete all existing rows for that `path` (`Store::delete_under`-style, but exact-path rather than folder-prefix match), then insert the freshly-parsed chunks. This makes re-indexing a file idempotent (delete-then-insert) instead of relying on `merge_insert`'s upsert semantics, which don't fit a one-to-many relationship. Cost: a delete + N inserts per file save instead of one upsert — fine at personal-index scale, worth revisiting only if it becomes a measured bottleneck.

### 4. search (`core/src/store.rs::hybrid_search`)

The full-table brute-force scan (`hybrid_search` fetches every row, scores in Rust, no LanceDB ANN index — see `CLAUDE.md`'s note on `index::auto`) now runs over chunks instead of files. For a real codebase, expect 5–20x more rows than the current file-count. This is the point where "just scan everything" — chosen deliberately for simplicity at personal-file-index scale — starts to matter and should be re-measured, not assumed fine.

**Measured** (`core/examples/bench_hybrid_search_scale.rs`, M-series CPU, release build, synthetic rows so the numbers isolate scan cost from disk/embedding variance):

| rows | avg search time |
| --- | --- |
| 1,000 | 0.9ms |
| 5,000 | 2.7ms |
| 20,000 | 9.7ms |
| 50,000 | 24.7ms |

Scales linearly at roughly 0.5ms per 1,000 rows, as expected for a full scan. This project's own index (Rust+Python+TS/JS chunked) currently sits at 114 rows and searches in ~2-4ms (`core/examples/bench_hybrid_search.rs`) — consistent with the synthetic numbers and nowhere near a problem.

Extrapolating: a codebase with ~5,000 files at ~8 chunks/file (≈40,000 rows) lands around ~20ms — still comfortably invisible next to the ~5-9ms embedding step and the UI's 150ms debounce. The scan only becomes noticeable (~100ms+) somewhere past ~200,000 rows, i.e. a very large monorepo. Verdict: the brute-force scan doesn't need replacing with a real ANN index for the personal/project-scale use case this app targets — revisit only if someone actually points it at something monorepo-sized.

Fuzzy filename matching (the other half of hybrid ranking) stays file-level — matching "auth.rs" fuzzily against a query still makes sense per-file, not per-chunk.

### 5. answer synthesis (`core/src/synthesize.rs`)

Currently: pick the best-matching *sentence* within a whole file's content, for prose files only (`is_prose_file` filters to `.md`/`.txt`/etc — see the comment there about why code produces garbage "sentences" like `pub mod embedding;`).

With chunking, code chunks become legitimate citation candidates too — a chunk is already a coherent, complete unit (a whole function), so citing "this function, verbatim" doesn't have the incoherence problem that citing a naively-split code "sentence" did. This actually removes a limitation rather than adding complexity: `PROSE_EXTENSIONS` filtering could relax for chunk-shaped code once chunks are known to be syntactically complete.

### 6. UI (`app/src/main.ts`, `app/src-tauri/src/lib.rs`)

Results need a way to show chunk identity, not just file identity — e.g. `validate_jwt()` under `auth.rs`, not just `auth.rs` twice for two different functions. `SearchResult`/`Citation` gain `start_line`/`end_line`/`chunk_kind` fields, which flow straight into the click-to-open logic already built (`openInEditor`) — this actually *simplifies* that code, since the line number becomes a known fact from the index instead of something computed on click via `find_line`/`best_matching_line` for every plain search result.

## phased language rollout

Don't build all five languages at once. Vertical-slice this the same way the rest of the project was built (per `CLAUDE.md`'s stated philosophy):

1. **Rust only**, end-to-end: schema migration, watcher changes, delete-then-reinsert, search, UI — proven completely on this project's own codebase (`core/`, `cli/`, `app/src-tauri/`) before adding a second language. This project indexing itself is the natural first test case.
2. Add Python and TypeScript/JavaScript next (covers `Orbis`'s stack, which has been the other real-world test corpus this whole session).
3. Go, or others, only if there's an actual need.

Every language added is: one new grammar dependency + one new `.scm` query file + a branch in the extension→language dispatch. The schema, watcher restructuring, stale-cleanup logic, and UI changes are all one-time costs paid once, in step 1.

## open questions

- **Nested functions/closures**: does a closure inside a function get its own chunk, or is it swallowed into the parent function's chunk? (Leaning: swallow — a chunk should be independently meaningful, and most closures aren't.)
- **Very large functions** (a 500-line match statement, say): still one chunk, even if it blows past what's comfortable for a single embedding? Or split further? (Leaning: still one chunk for v1 — splitting mid-function reintroduces the exact incoherence problem chunking is meant to solve.)
- **Module-level code** (imports, top-level `const`s, doc comments not attached to a function): does this become its own "file header" chunk, or get dropped? (Leaning: one small chunk per file for this, so a query like "what does this file import" still resolves to something.)
- **Chunk size limits for the embedding model**: `all-MiniLM-L6-v2` has a token limit; an unusually large function could exceed it and get silently truncated by the tokenizer. Worth an explicit check/warning rather than silent truncation.

## effort estimate

Touches: schema, watcher, store, hybrid search, synthesize, both Tauri commands and the frontend rendering/click-to-open path. Comparable in scope to everything built this session combined, concentrated mostly in step 1 (Rust-only) of the phased rollout above — later languages are much cheaper once that foundation exists.
