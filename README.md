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
| upsert behavior | `table::merge_insert`     | confirmed available in the current lancedb rust crate (v0.31.0), handles "file changed, re-embed it" as a single upsert operation, keyed on file path |

## core features (v1 scope)

- folder-level opt-in indexing
- real-time file watching (background daemon, not manual re-scan)
- local gpu-accelerated embeddings (cuda / metal)
- hybrid search: fast fuzzy filename match + semantic match
- answer synthesis with source citations, linking back to the actual file

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

## current status

scaffolding stage. first slice being built: a cli (not yet the tauri app) that watches one hardcoded folder, embeds new/changed files with minilm on cpu, stores them in lancedb, and supports a basic `search <query>` command. gpu backend, the tauri shell, and the ui come after this loop works end-to-end.
