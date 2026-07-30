# mcp tool ideas beyond search

status: proposal, nothing here is built yet. written after testing `search` directly against this repo (see `docs/mcp-agent-usage.md`) to confirm the gap is real before writing ideas around it.

## the actual problem

`search` retrieval quality is fine. tested against this repo's own code (chunking test suite, tauri command surface) and results were precise, not padded with irrelevant hits.

the complaint from agent sessions ("useful at the start, useless after") isn't a ranking problem. it's a tool-shape problem: `search` only answers "where is x." an agent calls that once or twice during orientation, has file paths and identifiers in hand, and falls back to grep for the rest of the session because there's nothing new to search for. one tool that only finds things will always get front-loaded and forgotten.

fixing this means shipping tools that answer questions grep and one-shot search can't, and that stay relevant deep into a session, not a better `search`.

## idea 1: find_similar — implemented

given a chunk (path + line range), return other chunks with high embedding similarity elsewhere in the index. the embeddings already exist per chunk in lancedb, this is a lookup against vectors already stored, not new infrastructure.

useful mid-edit, not just at orientation: "you're about to write logic that already exists in store.rs." catches duplication an agent writing fresh code would otherwise never think to search for, because it doesn't know the duplicate exists.

shipped as `mcp__reference-mcp__find_similar` (`Store::find_similar` in `core/src/store.rs`, tool wiring in `mcp/src/main.rs`), plus a `/findsimilar <path> <start_line>` command mirroring `/refsearch`. see `docs/mcp-agent-usage.md` for the full tool reference.

## idea 2: stale doc check — implemented

docs and code are both already chunked and embedded. given a doc chunk, compare its embedding against the code it describes and flag drift past some distance threshold.

answers "is this doc still true," a question that needs re-asking every time code changes, not once per session.

shipped as `mcp__reference-mcp__check_doc_drift` (`Store::check_doc_drift` in `core/src/store.rs`, tool wiring in `mcp/src/main.rs`), plus a `/checkdocdrift <path> <start_line>` command mirroring `/findsimilar`. built on the same embedding-scan mechanism as `find_similar`, filtered to exclude other doc chunks (`chunk_kind = "file"`) so a prose doc only gets compared against actual code constructs. see `docs/mcp-agent-usage.md` for the full tool reference.

## idea 3: cross-repo recall — implemented

`search`'s `folder` param currently narrows a query to one watched folder. add the inverse: a tool for "how did i solve x in a different project i've indexed."

value here has no substitute inside a single repo, grep structurally cannot do this. stays useful for the whole session since the need for prior-project recall doesn't front-load the way "where is this file" does.

shipped as an `exclude_folder` param on the existing `search` tool rather than a new tool — same ranking mechanism, just an inverted `path NOT LIKE` predicate ANDed alongside the existing `folder` scope-in (`Store::hybrid_search` in `core/src/store.rs`, param wiring in `mcp/src/main.rs`), plus a `/recall <query>` command that passes `exclude_folder: ${CLAUDE_PROJECT_DIR}` (the inverse of `/refsearch`'s `folder: ${CLAUDE_PROJECT_DIR}`). see `docs/mcp-agent-usage.md` for the full tool reference.

## idea 4: expose synthesize as its own tool — implemented, narrower than originally framed

correction made while building this: `core/src/synthesize.rs` is not a narrative answer generator, there's no LLM call anywhere in this codebase (only `candle` for embeddings, per this project's own non-goals). `synthesize()` is purely extractive — it cites a whole code chunk verbatim, or picks the single closest-matching sentence for prose. "how does x work end to end" as generated prose isn't something this codebase can produce without adding a generative model, which conflicts with the explicit non-goal of no general chatbot mode.

the real, narrower gap: `search` already calls `synthesize()`, but only for grammatically question-shaped queries (`synthesize::is_question`). a bare identifier or short phrase query — exactly what an agent types after finding a name via `search` or `find_similar` — gets raw results with zero citations, even though that's clearly "explain this," not "just list matches." this is a real, already-tested boundary case (`search_returns_citations_only_for_question_shaped_queries` in `mcp/src/main.rs` proves `"parse_config"` gets no citations today).

shipped as `mcp__reference-mcp__explain` (tool wiring in `mcp/src/main.rs`, no core changes needed — reuses `hybrid_search` and `synthesize` as-is): same extractive citation mechanism as `search`, but unconditional, no phrasing gate. plus a `/explain <query>` command mirroring `/refsearch`. see `docs/mcp-agent-usage.md` for the full tool reference.

## idea 5: push instead of pull

`watcher.rs` already runs continuously per watched folder. add a subscription: notify when a chunk semantically related to what's being worked on changes on disk elsewhere, mid-session.

turns the tool from a stateless query an agent has to remember to call into something with standing value for the life of the session.

## why these and not a second search tool

all five give an agent a reason to call the mcp server again after orientation is done: catching duplication while writing, catching drift while editing docs, recalling prior solutions across projects, explaining behavior instead of just locating it, reacting to changes instead of waiting to be asked. none of them compete with grep, which is the actual reason `search` alone stops getting used once file paths are known.
