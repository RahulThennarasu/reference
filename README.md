# reference

symbol: &

a cross-platform desktop app that indexes your files locally and lets you search them semantically. no cloud, no api calls, nothing leaves your machine.

## the problem

existing ai-powered launchers (raycast's quick ai, spotlight, etc.) don't actually know anything about your files. ask raycast "what did i work on in vs code recently" and it has no index of your activity, it just tells you how to check manually (open file > open recent, run `git status`, etc.). that's a cloud llm wrapper with no real memory of your machine, not a tool that retrieves anything.

this app is the real version: an actual local index of your files, kept current automatically, queryable with real semantic search, answered with citations back to the source file.

## core idea

- you pick which folders to index (opt-in, never automatic)
- a background daemon watches those folders in real time and embeds new/changed files as they happen
- search combines fast fuzzy filename matching with gpu-accelerated semantic search
- question-shaped queries get a synthesized answer with clickable source citations
- everything runs locally, embeddings, storage, and search never touch the network

## why not just use raycast / spotlight / cloud rag tools?

- **raycast is mac-only** and its "ai" features are thin wrappers around cloud apis, not a real local index.
- **cloud rag tools** require your files to leave the machine, which is a non-starter for code and private notes.
- **this app is cross-platform first**, genuinely good on windows/nvidia hardware, not a mac-only tool with windows as an afterthought.

## tech stack

| piece           | choice                    | why                                                                                                                                                   |
| --------------- | ------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------- |
| app shell       | tauri (rust)              | lightweight cross-platform desktop shell, avoids bundling a python runtime                                                                            |
| file watching   | `notify` crate            | cross-platform filesystem event watching                                                                                                              |
| embeddings      | `candle` (rust-native ml) | cuda support on pc, metal support on mac, no python dependency                                                                                        |
| embedding model | all-minilm-l6-v2          | smallest, fastest, most battle-tested with candle, safest starting point                                                                              |
| vector storage  | lancedb (rust crate)      | embedded (in-process, no server), native rust bindings, hybrid vector + metadata filtering, versioned storage                                         |
| upsert behavior | `table::merge_insert`     | confirmed available in the current lancedb rust crate (v0.31.0), handles "file changed, re-embed it" as a delete-then-upsert keyed on path + start line, since one file is now many chunk rows |
| code chunking    | `tree-sitter`             | parses rust/python/typescript/javascript/go/java/c/c++ into function/class-level chunks instead of embedding a whole file as one vector, see `docs/code-aware-chunking.md` |
| agent access    | `rmcp` (official Rust MCP SDK) | replaces the removed `reference-cli`; a long-running MCP server keeps the embedding model warm across calls instead of reloading it per invocation, see `docs/mcp-agent-usage.md` |

## core features (v1 scope)

- folder-level opt-in indexing
- real-time file watching (background daemon, not manual re-scan)
- local gpu-accelerated embeddings (cuda / metal)
- hybrid search: fast fuzzy filename match + semantic match
- code-aware chunking: search and cite exact functions/classes, not just whole files
- answer synthesis with source citations, linking back to the actual file, code citations are syntax-highlighted with a high-contrast palette tuned for the app's dark theme
- a "send to agent" button on every result and citation: copies a formatted query + code/line-range context to the clipboard, so a human who found the right chunk can hand it straight to whatever coding agent (claude code, codex, etc.) they're using
- an MCP server (`mcp/`) exposing a read-only `search` tool over the same index, for agents to query directly mid-task instead of shelling out to a CLI. `cargo build -p reference-mcp` once, then `.mcp.json` (checked into this repo) picks it up automatically — Claude Code still prompts for one-time approval per person, per its project-scoped server rules

## explicitly out of scope for v1

- cloud sync/backup of the index (undermines the whole privacy pitch)
- general-purpose chatbot mode (scope creep away from "search my stuff")
- auto-indexing everything by default (erodes trust, opt-in only)
- a persistent "nothing leaves this machine" status indicator in the main ui (redundant clutter, anyone who installed a local-only tool already knows the deal, this can live one layer down in settings if at all)

## later / stretch ideas

- multi-machine indexing (search across a mac and a pc you own)
- structured fact extraction (claims with provenance, not just chunk retrieval)
- plugin/extension model for new source types (notion, browser history, calendar)
- natural-language file actions (rename, move), bigger scope jump into agent territory, not v1
- directly launching a coding agent with context (not just clipboard hand-off), scoped to claude code specifically since there's no universal way to inject a prompt into an arbitrary running agent session

## current status

watch, embed, store, and hybrid search all work end-to-end through the tauri app, backed by one index (`~/.reference/`). code is chunked at function/class granularity for rust, python, typescript, javascript, go, java, c, and c++ (prose and other languages still index as one whole-file chunk). answer synthesis cites exact chunks, syntax-highlighted in the app, with a send-to-agent clipboard button on every result. the `reference-cli` binary (a shell-out-and-parse-json interface for coding agents) has been removed in favor of an MCP server (`mcp/`, package `reference-mcp`) exposing the same search over the same index, keeping the embedding model warm across calls instead of reloading it per invocation like the old CLI did.

no packaged installer/distribution yet (no code signing, no notarization, no release workflow), building from source (`pnpm tauri build`) is the only way to run it today. a release build currently produces a ~102mb `.app` (~41mb `.dmg`), down from an initial ~220mb before tuning `[profile.release]` (`strip`, `lto`, `codegen-units = 1`), most of the remaining size is from statically linking candle, lancedb (arrow + datafusion + lance), and four tree-sitter grammars into one binary. the `reference-mcp` binary ships inside the app bundle too, via tauri's `externalBin` sidecar mechanism (`app/src-tauri/scripts/prepare-mcp-sidecar.sh`, run automatically before every `tauri build`) — see below for how to point claude code at it.

## using the mcp server with claude code

once you have the app built/installed (see above — no packaged installer yet, so this still means building from source for now), the `reference-mcp` binary is already inside the app bundle, verified at `Contents/MacOS/reference-mcp`, right alongside the main app binary:

```
claude mcp add --scope user reference-mcp -- /path/to/reference.app/Contents/MacOS/reference-mcp
```

(on a normal install this would be `/Applications/reference.app/Contents/MacOS/reference-mcp`; adjust the path to wherever your build landed, e.g. `target/debug/bundle/macos/reference.app/...` for a local dev build).

verify with `claude mcp list` — should show `reference-mcp ✔ Connected`. it only searches folders you've already added to the app (⌘7); see `docs/mcp-agent-usage.md` for the full tool reference, the `/refsearch` command, and caveats.
