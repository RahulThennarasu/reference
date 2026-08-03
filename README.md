# reference

a macOS app that indexes your files locally and lets you search them semantically. no cloud, no api calls, nothing leaves your machine.

<img width="718" height="533" alt="Screenshot 2026-08-02 at 2 02 52 PM" src="https://github.com/user-attachments/assets/7807f502-7b99-4893-b4c5-911809834044" />

## the problem

existing ai-powered launchers (raycast's quick ai, spotlight, etc.) don't actually know anything about your files. ask raycast "how did i implement rate limiting here" and it has no index of your activity, it just tells you how to check manually (open file > open recent, run `git status`, etc.). that's a cloud llm wrapper with no real memory of your machine, not a tool that retrieves anything.

this app is the real version: an actual local index of your files, kept current automatically, queryable with real semantic search, answered with citations back to the source file.

## core idea

- you pick which folders to index (opt in, never automatic): type a path into the folder field, press **tab** to autocomplete/step into a folder, press **enter** to start watching it
- a background daemon watches those folders in real time and embeds new/changed files as they happen
- search combines fast fuzzy filename matching with gpu-accelerated semantic search
- question-shaped queries get a synthesized answer with clickable source citations
- everything runs locally, embeddings, storage, and search never touch the network

## why not just use raycast / spotlight / cloud rag tools

- raycast is mac only and its "ai" features are thin wrappers around cloud apis, not a real local index
- cloud rag tools require your files to leave the machine, a non starter for code and private notes
- this app is working to be cross-platform, not a mac-only tool

## tech stack

| piece           | choice                    | why                                                                                                                                                   |
| --------------- | ------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ |
| app shell       | tauri (rust)              | lightweight desktop shell, avoids bundling a python runtime                                                                            |
| file watching   | `notify` crate            | filesystem event watching                                                                                                              |
| embeddings      | `candle` (rust-native ml) | cuda support on pc, metal support on mac, no python dependency                                                                                        |
| embedding model | all-minilm-l6-v2          | smallest, fastest, most battle-tested with candle, safest starting point                                                                              |
| vector storage  | lancedb (rust crate)      | embedded (in process, no server), native rust bindings, hybrid vector + metadata filtering, versioned storage                                         |
| upsert behavior | `table::merge_insert`     | handles "file changed, re-embed it" as a delete-then-upsert keyed on path + start line, since one file is now many chunk rows                          |
| code chunking   | `tree-sitter`             | parses rust, python, typescript, javascript, go, java, c, c++, ruby, and swift into function/class-level chunks instead of embedding a whole file as one vector |
| agent access    | `rmcp` (official rust mcp sdk) | a long-running mcp server keeps the embedding model warm across calls instead of reloading it per invocation                                    |

## core features

- folder-level opt in indexing
- real-time file watching (background daemon, not manual re-scan)
- local gpu-accelerated embeddings (cuda / metal)
- hybrid search: fast fuzzy filename match + semantic match
- code-aware chunking: search and cite exact functions/classes, not just whole files
- answer synthesis with source citations, linking back to the actual file, code citations are syntax-highlighted with a high-contrast palette tuned for the app's dark theme
- a "send to agent" button on every result and citation: copies a formatted query and code/line-range context to the clipboard, so a human who found the right chunk can hand it straight to whatever coding agent they're using
- pick which app opens a result: right-click a result/citation for an "open with" popup, or use the toolbar button to set the default a plain click uses. lists whatever's actually installed (finder, cursor, zed, vs code, windsurf, xcode, sublime text, terminal, any jetbrains ide) with real icons extracted from each app's own bundle, no hardcoded logos, no dependency on any one editor being installed
- an mcp server exposing four read-only tools over the same index, for agents to query directly mid-task: `search` for natural-language lookup (with an `exclude_folder` param for cross-repo recall), `explain` for citation synthesis regardless of query phrasing, `find_similar` for finding near-duplicate chunks given one already found, `check_doc_drift` for flagging a doc chunk that no longer scores close to the code it describes

## explicitly out of scope

- cloud sync/backup of the index (undermines the whole privacy pitch)
- general-purpose chatbot mode (scope creep away from "search my stuff")
- auto-indexing everything by default (erodes trust, opt in only)
- a persistent "nothing leaves this machine" status indicator in the main ui (redundant clutter, anyone who installed a local-only tool already knows the deal)

## later / stretch ideas

- multi-machine indexing (search across a mac and a pc you own)
- structured fact extraction (claims with provenance, not just chunk retrieval)
- plugin/extension model for new source types (notion, browser history, calendar)
- natural-language file actions (rename, move)
- directly launching a coding agent with context, not just clipboard hand-off

## current status

watch, embed, store, and hybrid search all work end to end through the tauri app, backed by one local index (`~/.reference/`). code is chunked at function/class granularity for rust, python, typescript, javascript, go, java, c, c++, ruby, and swift (prose and other languages still index as one whole-file chunk). answer synthesis cites exact chunks, syntax-highlighted in the app, with a send-to-agent clipboard button on every result.

everything lives under `~/.reference/`: `watched_folders.json` is the plain-text list of folders you've added via the app, `index/` is the actual lancedb table (paths, chunks, and embeddings). removing a folder from the app doesn't just stop watching it, it purges every already-indexed row for that folder from `index/` too, so nothing stale is left searchable after you remove it.

there's a menu bar tray icon (the pixelated `&` mark) alongside the dock icon, click it to show/focus the main window. closing the window hides it instead of quitting the app, specifically so the tray icon has a window to bring back; quit fully via the dock icon or cmd+q.

## adding a folder to watch

open the folder picker (⌘7), type or paste a path, then:

- **tab** — autocomplete the path / step into the highlighted folder
- **enter** — start watching the current path

## building from source

```
git clone <this repo>
cd reference/app
pnpm install
pnpm tauri dev
```

requires rust and pnpm installed. no python dependency anywhere in the pipeline.

## using the mcp server with an agent

<img width="658" height="486" alt="Screenshot 2026-08-02 at 2 07 43 PM" src="https://github.com/user-attachments/assets/f5ce4a88-4a5c-410f-b185-a7ea5653d87f" />

build it once:

```
cargo build -p reference-mcp
```

then register it with claude code:

```
claude mcp add --scope local reference-mcp -- ${CLAUDE_PROJECT_DIR:-.}/target/debug/reference-mcp
```

or, if you're using the installed app instead of a source build, the `reference-mcp` binary ships inside the app bundle at `Contents/MacOS/reference-mcp`:

```
claude mcp add --scope user reference-mcp -- /Applications/reference.app/Contents/MacOS/reference-mcp
```

it only searches folders you've already added to the app. exposes four tools: `search`, `explain`, `find_similar`, and `check_doc_drift`.

## license

business source license 1.1 (`LICENSE`). free to use, copy, and modify for personal, educational, or evaluation purposes. reselling or redistributing it as your own product or service requires a separate commercial license. converts to apache 2.0 on 2030-08-01.
