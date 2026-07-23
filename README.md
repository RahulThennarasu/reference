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
| code chunking    | `tree-sitter`             | parses rust/python/typescript/javascript into function/class-level chunks instead of embedding a whole file as one vector, see `docs/code-aware-chunking.md` |

## core features (v1 scope)

- folder-level opt-in indexing
- real-time file watching (background daemon, not manual re-scan)
- local gpu-accelerated embeddings (cuda / metal)
- hybrid search: fast fuzzy filename match + semantic match
- code-aware chunking: search and cite exact functions/classes, not just whole files
- answer synthesis with source citations, linking back to the actual file
- a cli (`reference-cli`) sharing the same index as the app, built for coding agents to query directly, see `docs/cli-agent-usage.md`

## explicitly out of scope for v1

- cloud sync/backup of the index (undermines the whole privacy pitch)
- general-purpose chatbot mode (scope creep away from "search my stuff")
- auto-indexing everything by default (erodes trust, opt-in only)
- a persistent "nothing leaves this machine" status indicator in the main ui (redundant clutter, anyone who installed a local-only tool already knows the deal, this can live one layer down in settings if at all)

## later / stretch ideas

- a "send to agent" button in the search ui: copy a formatted query + citation context to the clipboard, so a human who found the right chunk can hand it straight to whatever coding agent they're using
- multi-machine indexing (search across a mac and a pc you own)
- structured fact extraction (claims with provenance, not just chunk retrieval)
- plugin/extension model for new source types (notion, browser history, calendar)
- natural-language file actions (rename, move), bigger scope jump into agent territory, not v1

## current status

watch, embed, store, and hybrid search all work end-to-end, through both the tauri app and the cli, sharing one index. code is chunked at function/class granularity for rust, python, typescript, and javascript (prose and other languages still index as one whole-file chunk). answer synthesis cites exact chunks, syntax-highlighted in the app. the cli additionally supports `--json` output for coding agents to consume directly, see `docs/cli-agent-usage.md`. no packaged installer yet, building from source is the only way to run it today.
