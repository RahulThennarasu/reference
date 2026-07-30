# reference-mcp for coding agents

`reference-mcp` is the agent-facing side of `reference` — the successor to the removed `reference-cli`. It's an MCP server exposing three read-only tools, `search`, `find_similar`, and `check_doc_drift`, over the same index the desktop app watches and writes (`~/.reference/index`).

## why this exists (and why not the old CLI)

`reference-cli` worked by an agent shelling out to a fresh process per query, reloading the embedding model from scratch every single call — fine occasionally, expensive if an agent is calling it repeatedly mid-session, which is exactly the intended usage pattern. An MCP server is a long-running process for the life of a session: the model loads once, stays warm, and every subsequent `search` call skips straight to embedding the query.

MCP also fits how Claude Code (and other MCP-aware agents) actually discover and call tools — a typed schema the agent calls directly — instead of `CLAUDE.md` prose telling an agent to shell out to a specific binary and parse its stdout.

## what it deliberately doesn't do

There's no tool to add or remove watched folders. Folder opt-in is a human action taken through the app's picker (⌘7) — see `readme.md`'s "opt-in, never automatic" principle and the non-goal "no auto-indexing by default." An agent calling `search` only ever sees folders a person already chose to index; it cannot expand that set itself.

## install / build

```
cargo build -p reference-mcp
```

No `--release` needed for correctness — this project's `[profile.release]` uses `lto = true` + `codegen-units = 1` for a smaller shipped binary, which makes release builds slow (~15 min, see `CLAUDE.md`). The dev binary at `target/debug/reference-mcp` is functionally identical for this purpose; only our own crates run unoptimized in dev, the heavy math (candle) is still built with `opt-level = 2` per the workspace's `[profile.dev.package."*"]` override.

## registering with Claude Code

Two different audiences, two different paths:

**Working in this repo (contributor/dev):** no checked-in `.mcp.json` — deliberately removed so nothing auto-registers a server for anyone who clones this repo. Build it once (`cargo build -p reference-mcp`), then register it yourself:
```
claude mcp add --scope local reference-mcp -- ${CLAUDE_PROJECT_DIR:-.}/target/debug/reference-mcp
```
`--scope local` is the one that does *not* write to `.mcp.json` — it's stored outside the repo entirely (per-user, per-project), so it's never shared via version control. `--scope project` would recreate the same checked-in `.mcp.json` this setup deliberately removed — don't use it here.

**Installed the app, not the repo (end user):** `reference-mcp` ships inside the app bundle already — Tauri's `externalBin` sidecar mechanism (`app/src-tauri/scripts/prepare-mcp-sidecar.sh`, wired into `beforeBundleCommand`) builds it in release mode and copies it in before every `tauri build`. Verified (by actually building and inspecting the bundle, not just assuming — Tauri's own docs don't document this path) to land at `Contents/MacOS/reference-mcp` on macOS, plain filename, right alongside the main app binary. Register it user-scoped, since there's no project/repo in this case:
```
claude mcp add --scope user reference-mcp -- /Applications/reference.app/Contents/MacOS/reference-mcp
```
See `README.md`'s "using the mcp server with claude code" section for the end-user-facing version of this.

## the `search` tool

Input:
```json
{ "query": "how does the hybrid search combine fuzzy and semantic scores", "top_k": 5, "folder": "/Users/you/Documents/GitHub/reference" }
```
- `query`: natural language, not a grep pattern — describe behavior or intent, not a literal string.
- `top_k`: optional, defaults to 5.
- `folder`: optional, scopes the search to one watched folder (absolute path). Set this whenever the agent knows which project the query is about — without it, an unrelated watched folder that merely shares some vocabulary with the query can outrank the file that's actually relevant. This was an actual observed failure: a query about this repo's own MCP server returned a file from a completely different watched project ahead of the correct one, purely on coincidental word overlap. Filtering happens at the query level (`only_if`), not after fetching results, so out-of-scope rows are never scored at all.

Output (JSON text content):
```json
{
  "results": [
    { "path": "/abs/path/to/file.rs", "start_line": 154, "end_line": 251, "chunk_kind": "function", "score": 0.61, "content": "pub async fn hybrid_search(...) { ... }" }
  ],
  "citations": [
    { "path": "/abs/path/to/file.rs", "snippet": "pub async fn hybrid_search(...) { ... }", "start_line": 154, "end_line": 251, "chunk_kind": "function" }
  ]
}
```
Broadly the same shape the old CLI's `--json` mode produced, with one addition: every `results` entry now includes the chunk's full `content` too, not just a file:line pointer — `hybrid_search` already has it in hand, and an MCP caller (an agent) almost always needs the actual text next, so returning it here saves a follow-up file read on every search, question-shaped or not. `citations` is still only populated when the query reads as a question (`synthesize::is_question`) — see `docs/code-aware-chunking.md` for what `chunk_kind` values mean and how citation snippets are chosen.

Ranking itself blends three signals, not two: semantic similarity, fuzzy filename match, and literal term overlap against the chunk's own content (a lightweight complement to embeddings — catches a chunk that verbatim contains the words searched for, even if its embedding similarity is only middling because it also covers other things). This match is whole-word, not a raw substring check — a naive `content.contains("build")` matches inside "rebuild"/"builds"/"building" too, which turned a query like "how does X build Y" into noise, rewarding any file with unrelated build-system comments (`Cargo.toml`'s "release build"/"rebuild" comments were an actual observed case) over the file that was really relevant. Fuzzy filename matching still drops to zero weight for question-shaped queries (see `core/src/store.rs`'s `hybrid_search` for the exact weights); content-term overlap applies to both query shapes.

## the `find_similar` tool

Input:
```json
{ "path": "/Users/you/Documents/GitHub/reference/core/src/chunk.rs", "start_line": 678, "top_k": 5, "folder": "/Users/you/Documents/GitHub/reference" }
```
- `path` / `start_line`: identify the chunk to compare against — usually taken straight from a prior `search` hit's `path` and `start_line`, not typed by hand. A file is chunked into multiple rows (see `docs/code-aware-chunking.md`), so `start_line` picks which chunk in the file, not just which file.
- `top_k`: optional, defaults to 5.
- `folder`: optional, scopes candidates to one watched folder, same reasoning as `search`'s `folder` param.

Output (JSON text content):
```json
{
  "results": [
    { "path": "/abs/path/to/other_file.rs", "start_line": 578, "end_line": 607, "chunk_kind": "function", "score": 0.92, "content": "fn some_similar_chunk(...) { ... }" }
  ]
}
```
No `query` field and no `citations` — there's no query text to embed or synthesize an answer from. The source chunk's own stored embedding is looked up first, then reused as the comparison vector against every other row's embedding (plain dot product, both sides already L2-normalized). The source chunk itself is always excluded from its own results by path + start_line, not by score.

Use this after `search` has already located a chunk, to answer a different question than `search` answers: not "where is x" but "what else in the index looks like this." The intended use is catching duplicated or near-duplicate logic elsewhere in the index before writing new code that unknowingly repeats it — not a general similarity browser.

## `/findsimilar` — explicit invocation

Same reasoning as `/refsearch` below: `.claude/commands/findsimilar.md` calls `mcp__reference-mcp__find_similar` directly, bypassing tool-choice uncertainty. Usage: `/findsimilar <path> <start_line>`, e.g. `/findsimilar core/src/chunk.rs 678`. Passes `folder: ${CLAUDE_PROJECT_DIR}` automatically, same auto-scoping as `/refsearch`.

## the `check_doc_drift` tool

Input:
```json
{ "path": "/Users/you/Documents/GitHub/reference/docs/mcp-agent-usage.md", "start_line": 1, "top_k": 5, "stale_threshold": 0.35, "folder": "/Users/you/Documents/GitHub/reference" }
```
- `path` / `start_line`: identify the doc chunk to check — usually taken from a prior `search` hit. Markdown has no code-aware chunker (see `docs/code-aware-chunking.md`), so a doc file's chunks always fall back to one whole-file chunk at `start_line: 1`.
- `top_k`: optional, defaults to 5.
- `stale_threshold`: optional, defaults to 0.35. Score below which the top matching code chunk is treated as evidence the doc has drifted — same dot-product scale as `find_similar`'s scores, not a probability.
- `folder`: optional, same reasoning as `search`'s `folder` param.

Output (JSON text content):
```json
{
  "results": [
    { "path": "/abs/path/to/file.rs", "start_line": 154, "end_line": 251, "chunk_kind": "function", "score": 0.61, "content": "pub async fn hybrid_search(...) { ... }" }
  ],
  "likely_stale": false
}
```
Built on the same mechanism as `find_similar` (the doc chunk's own stored embedding, scored by dot product against the rest of the index), with one filter added: candidates with `chunk_kind = "file"` are excluded, so a prose doc chunk is compared only against actual code constructs (functions, types, ...) and never against other doc files that happen to share vocabulary. `likely_stale` reads the top result's score against `stale_threshold` — it's a heuristic signal ("nothing in the index still reads as close to this doc"), not a guarantee the doc is wrong.

## `/checkdocdrift` — explicit invocation

Same reasoning as `/refsearch` below: `.claude/commands/checkdocdrift.md` calls `mcp__reference-mcp__check_doc_drift` directly. Usage: `/checkdocdrift <path> <start_line>`, e.g. `/checkdocdrift docs/mcp-agent-usage.md 1`. Passes `folder: ${CLAUDE_PROJECT_DIR}` automatically, same auto-scoping as `/refsearch`.

## `/refsearch` — explicit invocation

Tool selection is a model decision; `CLAUDE.md` telling an agent to "prefer this over grep" is guidance, not an enforced rule, and in practice grep still sometimes wins when it's the faster-looking path for a given question. `.claude/commands/refsearch.md` is the reliable alternative: typing `/refsearch <query>` in a Claude Code session calls `mcp__reference-mcp__search` directly with the typed text, bypassing tool-choice uncertainty entirely — and passes `folder: ${CLAUDE_PROJECT_DIR}` automatically, so it's scoped to whatever project the session is actually in by default, same reasoning as the `folder` param above. A personal, cross-project copy also lives at `~/.claude/commands/refsearch.md`, for using the command outside this repo — same auto-scoping behavior, since `${CLAUDE_PROJECT_DIR}` resolves per-session, not to this repo specifically.

## when to use this vs grep

Same rule as the old CLI: reach for `search` first, before grep, whenever the question describes behavior or intent rather than naming a literal string/identifier — "why does search skip fuzzy filename matching for some queries" has no single identifier to grep for. Once you already know the identifier, error message, or literal string, grep is still faster and more precise than semantic search.

## caveats

- **First run needs network.** `Embedder::load()` fetches the MiniLM model weights from Hugging Face Hub on first load, cached under `~/.cache/huggingface` afterward. This isn't new to the MCP server — the app and old CLI had the same one-time fetch — but "fully offline" starts after that first launch, not on it.
- **Startup takes a few seconds** (model load) — comfortably inside Claude Code's default 30s MCP startup timeout based on measurements so far, but `MCP_TIMEOUT=60000 claude` raises it if a slower machine ever needs it.
- Results only reflect folders a human has opted into watching via the app. Empty results usually mean nothing's indexed yet, not a broken query.
