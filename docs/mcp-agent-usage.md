# reference-mcp for coding agents

`reference-mcp` is the agent-facing side of `reference` — the successor to the removed `reference-cli`. It's an MCP server exposing one read-only tool, `search`, over the same index the desktop app watches and writes (`~/.reference/index`).

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

**Working in this repo (contributor/dev):** the repo ships a project-scoped `.mcp.json` (checked into version control) pointing at `${CLAUDE_PROJECT_DIR:-.}/target/debug/reference-mcp`, so it works regardless of where the repo is cloned. Anyone opening this project still sees Claude Code's one-time approval prompt for project-scoped MCP servers (a Claude Code security requirement, not something a checked-in file can bypass) — approve it once via `claude` run interactively, or `claude mcp list` to check status.

**Installed the app, not the repo (end user):** `reference-mcp` ships inside the app bundle already — Tauri's `externalBin` sidecar mechanism (`app/src-tauri/scripts/prepare-mcp-sidecar.sh`, wired into `beforeBundleCommand`) builds it in release mode and copies it in before every `tauri build`. Verified (by actually building and inspecting the bundle, not just assuming — Tauri's own docs don't document this path) to land at `Contents/MacOS/reference-mcp` on macOS, plain filename, right alongside the main app binary. Register it user-scoped, since there's no project/repo in this case:
```
claude mcp add --scope user reference-mcp -- /Applications/reference.app/Contents/MacOS/reference-mcp
```
See `README.md`'s "using the mcp server with claude code" section for the end-user-facing version of this.

## the `search` tool

Input:
```json
{ "query": "how does the hybrid search combine fuzzy and semantic scores", "top_k": 5 }
```
- `query`: natural language, not a grep pattern — describe behavior or intent, not a literal string.
- `top_k`: optional, defaults to 5.

Output (JSON text content):
```json
{
  "results": [
    { "path": "/abs/path/to/file.rs", "start_line": 154, "end_line": 251, "chunk_kind": "function", "score": 0.61 }
  ],
  "citations": [
    { "path": "/abs/path/to/file.rs", "snippet": "pub async fn hybrid_search(...) { ... }", "start_line": 154, "end_line": 251, "chunk_kind": "function" }
  ]
}
```
Same shape the old CLI's `--json` mode produced. `citations` is only populated when the query reads as a question (`synthesize::is_question`) — see `docs/code-aware-chunking.md` for what `chunk_kind` values mean and how citation snippets are chosen.

## `/refsearch` — explicit invocation

Tool selection is a model decision; `CLAUDE.md` telling an agent to "prefer this over grep" is guidance, not an enforced rule, and in practice grep still sometimes wins when it's the faster-looking path for a given question. `.claude/commands/refsearch.md` is the reliable alternative: typing `/refsearch <query>` in a Claude Code session calls `mcp__reference-mcp__search` directly with the typed text, bypassing tool-choice uncertainty entirely.

## when to use this vs grep

Same rule as the old CLI: reach for `search` first, before grep, whenever the question describes behavior or intent rather than naming a literal string/identifier — "why does search skip fuzzy filename matching for some queries" has no single identifier to grep for. Once you already know the identifier, error message, or literal string, grep is still faster and more precise than semantic search.

## caveats

- **First run needs network.** `Embedder::load()` fetches the MiniLM model weights from Hugging Face Hub on first load, cached under `~/.cache/huggingface` afterward. This isn't new to the MCP server — the app and old CLI had the same one-time fetch — but "fully offline" starts after that first launch, not on it.
- **Startup takes a few seconds** (model load) — comfortably inside Claude Code's default 30s MCP startup timeout based on measurements so far, but `MCP_TIMEOUT=60000 claude` raises it if a slower machine ever needs it.
- Results only reflect folders a human has opted into watching via the app. Empty results usually mean nothing's indexed yet, not a broken query.
