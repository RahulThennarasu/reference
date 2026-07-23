# reference-cli for coding agents

`reference-cli` is the agent-facing side of `reference`. the desktop app is for humans (folder picker, live search palette); the cli is what a coding agent (claude code, codex, or anything else that can shell out) calls to pull grounded context out of the same index, mid-task, without a human in the loop.

## why this exists

an agent exploring an unfamiliar codebase usually falls back to grep with guessed identifier names, or reads whole files hoping the relevant part is in there somewhere. both are token-expensive and miss things a plain string search can't find, "where do we retry failed api calls" has no single identifier to grep for.

`reference-cli search` runs the same hybrid (fuzzy filename + semantic) search the app uses, over an index chunked at function/class granularity (see `docs/code-aware-chunking.md`), and returns exact file:line hits ranked by meaning, not just files containing a matching string.

## install

```
cargo install --path cli
```

installs the `reference-cli` binary to `~/.cargo/bin` (make sure that's on `PATH`). no separate config step, no server to run.

## the shared index

the cli and the desktop app read and write the exact same index at `~/.reference/index` (see `core/src/paths.rs`), plus `~/.reference/watched_folders.json` for the list of folders currently being watched. there is no separate "cli index", whatever the app has indexed is what the cli searches, and vice versa.

practical implication: if `search` comes back empty, it usually means nothing's been watched yet, not that the query failed. add a folder from the app (`⌘7`) first, then the cli will see it.

## usage

```
reference-cli search "<query>" --top-k 8 --json
```

- `query`: natural language, not a grep pattern. describe behavior or intent, not literal strings.
- `--top-k`: how many results to return (default 5).
- `--json`: emit structured json on stdout instead of printed text. use this for agent consumption, the plain-text mode is for humans running it by hand in a terminal.

without `--json`, status lines (model loading, gpu backend detection) print to stderr and results print to stdout as formatted text. with `--json`, stdout is pure json, nothing else gets written there, safe to pipe into a parser.

## json shape

```json
{
  "results": [
    {
      "path": "/abs/path/to/file.rs",
      "start_line": 142,
      "end_line": 168,
      "chunk_kind": "function",
      "score": 0.61
    }
  ],
  "citations": [
    {
      "path": "/abs/path/to/file.rs",
      "snippet": "fn retry_with_backoff(...) { ... }",
      "start_line": 142,
      "end_line": 168,
      "chunk_kind": "function"
    }
  ]
}
```

- `results`: every hit, ranked by `score`, highest first. `chunk_kind` is one of `function`, `class`, `impl` (rust), `interface` (typescript), or `file` (prose, or a language with no chunker yet, so the whole file is one row).
- `citations`: a subset of the top results, only populated when the query reads as a question (see `synthesize::is_question`). for a `function`/`class`/`impl`/`interface` hit, `snippet` is the entire chunk verbatim, already a complete, coherent unit, safe to read or quote directly without needing a follow-up file read. for a `file` hit (prose), `snippet` is the single best-matching sentence, not the whole file.

## when to use this vs grep

use `reference-cli search` first, before grep, whenever the question names no literal string or identifier to search for and instead describes behavior or intent:

- "why don't impl methods get their own chunk separate from the impl block"
- "what stops a wip file with a syntax error from disappearing from the index"
- "how does a citation's line number stay correct when the chunk isn't the whole file"

grep is still the right tool once you already know the identifier, error message, or literal string you're looking for, semantic search doesn't beat an exact match on something you can already name.

## caveats

- `search` needs the embedding model loaded on every invocation (a few seconds on first run per process, cached after). it is not instant like grep, budget for that.
- `watch` (the other cli subcommand) is a leftover from the very first prototype: hardcoded to a `./watched` folder relative to wherever you run it, and blocks the terminal in the foreground. it is not meant for general use, the desktop app's folder watching is the real indexing path. don't reach for `reference-cli watch` expecting it to behave like a background daemon for an arbitrary project folder.
- results only reflect folders someone has actually opted into watching (via the app). there is no auto-indexing, and the cli cannot index folders you haven't explicitly added.

## example: wiring this into a project's own `CLAUDE.md`

drop something like this into any project you're indexing, so an agent working in that repo picks up the habit automatically:

```markdown
## semantic code search
this repo is indexed by `reference` (local, offline semantic search). prefer `reference-cli search "<query>" --json` over grep for conceptual lookups, "where do we validate X" beats guessing identifier names, it ranks by meaning and returns exact function/class hits with file:line.
```

this repo's own `CLAUDE.md` does exactly this, as a working example.
