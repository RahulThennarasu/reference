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

## idea 3: cross-repo recall

`search`'s `folder` param currently narrows a query to one watched folder. add the inverse: a tool for "how did i solve x in a different project i've indexed."

value here has no substitute inside a single repo, grep structurally cannot do this. stays useful for the whole session since the need for prior-project recall doesn't front-load the way "where is this file" does.

## idea 4: expose synthesize as its own tool

`core/src/synthesize.rs` already builds full answers with citations, the mcp server only exposes bare `search` today. split it out as an `explain` tool: "how does x work end to end," not "find snippet of x."

different job from search. narrative explanation keeps getting needed deep into a session (onboarding-shaped questions, "why is this built this way"), unlike point lookups which get answered once and stay answered.

## idea 5: push instead of pull

`watcher.rs` already runs continuously per watched folder. add a subscription: notify when a chunk semantically related to what's being worked on changes on disk elsewhere, mid-session.

turns the tool from a stateless query an agent has to remember to call into something with standing value for the life of the session.

## why these and not a second search tool

all five give an agent a reason to call the mcp server again after orientation is done: catching duplication while writing, catching drift while editing docs, recalling prior solutions across projects, explaining behavior instead of just locating it, reacting to changes instead of waiting to be asked. none of them compete with grep, which is the actual reason `search` alone stops getting used once file paths are known.
