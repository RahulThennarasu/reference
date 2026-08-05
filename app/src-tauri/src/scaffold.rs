// Drops Claude Code agent-integration scaffolding (slash commands, tool-choice
// hooks, a CLAUDE.md section) into a folder the user has opted to watch, so a
// coding agent working there already prefers `reference-mcp`'s `search`/
// `explain` over grep without the user hand-authoring any of it. See
// docs/agent-integration-scaffolding-plan.md for the design.
//
// Every write here is idempotent: re-scaffolding an already-scaffolded folder
// (e.g. unwatch then re-watch) reproduces the same end state rather than
// duplicating content, so callers never need to track "did I already do
// this" themselves.

use serde_json::{json, Value};
use std::fs;
use std::io;
use std::path::Path;

const COMMANDS: &[(&str, &str)] = &[
    (
        "refsearch.md",
        include_str!("../../../.claude/commands/refsearch.md"),
    ),
    (
        "explain.md",
        include_str!("../../../.claude/commands/explain.md"),
    ),
    (
        "findsimilar.md",
        include_str!("../../../.claude/commands/findsimilar.md"),
    ),
    (
        "checkdocdrift.md",
        include_str!("../../../.claude/commands/checkdocdrift.md"),
    ),
    (
        "recall.md",
        include_str!("../../../.claude/commands/recall.md"),
    ),
];

// Marker substring looked for inside an existing hook's `command` to decide
// "this hook (or an equivalent) is already there" — every injected hook's
// `additionalContext` mentions `reference-mcp`, and no unrelated hook would.
const HOOK_MARKER: &str = "reference-mcp";

const CLAUDE_MD_MARKER: &str = "<!-- reference-mcp-scaffold -->";

const CLAUDE_MD_SECTION: &str = r#"
## semantic code search
<!-- reference-mcp-scaffold -->

this folder is indexed by [reference](https://github.com) (symbol: &), a local semantic code search tool. an MCP server exposes four read-only tools over that index: `search`, `explain`, `find_similar`, and `check_doc_drift`.

use `search` (or `explain`, which behaves the same but always synthesizes citations) first, before grep, whenever a question names no literal string/identifier to search for and instead describes *behavior* or *intent* — grep is still the right tool once you already know the identifier/string you're looking for.

- `mcp__reference-mcp__search` — natural-language query, returns matching chunks with citations.
- `mcp__reference-mcp__explain` — same as `search` but always synthesizes citations, useful for a bare identifier or short phrase.
- `mcp__reference-mcp__find_similar` — takes a chunk (path + start_line) and finds other chunks with the closest embedding, useful for catching duplicated logic before writing new code.
- `mcp__reference-mcp__check_doc_drift` — takes a doc chunk and checks whether it still matches actual code in the index, flagging `likely_stale` if not.

`/refsearch <query>`, `/explain <query>`, `/findsimilar <path> <start_line>`, `/checkdocdrift <path> <start_line>`, and `/recall <query>` are slash commands that call these tools directly, scoped to this folder via `${CLAUDE_PROJECT_DIR}`.
"#;

fn write_commands(folder: &Path) -> io::Result<()> {
    let dir = folder.join(".claude").join("commands");
    fs::create_dir_all(&dir)?;
    for (name, content) in COMMANDS {
        fs::write(dir.join(name), content)?;
    }
    Ok(())
}

fn pre_tool_use_hooks() -> Value {
    json!([
        {
            "matcher": "Grep",
            "hooks": [
                {
                    "type": "command",
                    "command": "jq -r '.tool_input.pattern // empty' | { read -r q; if [ -z \"$q\" ]; then echo '{}'; elif echo \"$q\" | grep -qE '^[A-Za-z0-9_.:/-]+$'; then echo '{}'; else jq -n --arg q \"$q\" '{hookSpecificOutput:{hookEventName:\"PreToolUse\", additionalContext:(\"grep pattern \\\"\" + $q + \"\\\" looks intent/behavior-shaped, not a known literal identifier -- this folder is indexed by reference-mcp, which exposes mcp__reference-mcp__search and mcp__reference-mcp__explain for exactly that case (see the semantic code search section in CLAUDE.md). consider those first.\")}}'; fi; } 2>/dev/null || true",
                    "timeout": 10
                }
            ]
        },
        {
            "matcher": "Bash",
            "hooks": [
                {
                    "type": "command",
                    "if": "Bash(grep *)",
                    "command": "jq -r '.tool_input.command // empty' | { read -r c; if [ -z \"$c\" ]; then echo '{}'; else jq -n --arg c \"$c\" '{hookSpecificOutput:{hookEventName:\"PreToolUse\", additionalContext:(\"about to run grep via bash (\" + $c + \") -- this folder is indexed by reference-mcp, which exposes mcp__reference-mcp__search and mcp__reference-mcp__explain for behavior/intent-shaped lookups (see CLAUDE.md). consider those first if you do not already know the exact identifier/string.\")}}'; fi; } 2>/dev/null || true",
                    "timeout": 10
                }
            ]
        }
    ])
}

fn post_tool_use_hooks() -> Value {
    json!([
        {
            "matcher": "Edit|Write",
            "hooks": [
                {
                    "type": "command",
                    "command": "jq -r '.tool_input.file_path // .tool_response.filePath // empty' | { read -r f; if [ -z \"$f\" ]; then echo '{}'; elif printf '%s' \"$f\" | grep -qiE '\\.md$'; then jq -n --arg f \"$f\" '{hookSpecificOutput:{hookEventName:\"PostToolUse\", additionalContext:(\"just edited doc file \" + $f + \" -- consider mcp__reference-mcp__check_doc_drift on the relevant chunk to confirm it still matches the code it describes.\")}}'; else jq -n --arg f \"$f\" '{hookSpecificOutput:{hookEventName:\"PostToolUse\", additionalContext:(\"just edited \" + $f + \" -- consider mcp__reference-mcp__find_similar on the changed chunk to catch duplicated/near-duplicate logic elsewhere in the index before moving on.\")}}'; fi; } 2>/dev/null || true",
                    "timeout": 10
                }
            ]
        }
    ])
}

// Appends `entries` (an array Value) into `settings[event]` (creating the
// array/object chain as needed), skipping any entry whose hooks already
// contain a command with `HOOK_MARKER` under a hook list that also matches
// on the same `matcher` — that's "an equivalent entry" per the plan's
// dedupe rule, so a user's own unrelated hooks on the same event are left
// alone and reference's own hooks aren't duplicated on re-scaffold.
fn merge_hook_event(settings: &mut Value, event: &str, entries: Value) {
    let hooks = settings
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| json!({}));
    let event_arr = hooks
        .as_object_mut()
        .unwrap()
        .entry(event)
        .or_insert_with(|| json!([]));
    let event_arr = event_arr.as_array_mut().unwrap();

    let has_marker = |entry: &Value| -> bool {
        entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .is_some_and(|hs| {
                hs.iter().any(|h| {
                    h.get("command")
                        .and_then(|c| c.as_str())
                        .is_some_and(|c| c.contains(HOOK_MARKER))
                })
            })
    };

    for entry in entries.as_array().unwrap() {
        let matcher = entry.get("matcher").and_then(|m| m.as_str());
        let already_present = event_arr.iter().any(|existing| {
            existing.get("matcher").and_then(|m| m.as_str()) == matcher && has_marker(existing)
        });
        if !already_present {
            event_arr.push(entry.clone());
        }
    }
}

fn merge_settings_hooks(folder: &Path) -> io::Result<()> {
    let dir = folder.join(".claude");
    fs::create_dir_all(&dir)?;
    let path = dir.join("settings.json");

    let mut settings: Value = if path.exists() {
        let raw = fs::read_to_string(&path)?;
        serde_json::from_str(&raw).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };
    if !settings.is_object() {
        settings = json!({});
    }

    merge_hook_event(&mut settings, "PreToolUse", pre_tool_use_hooks());
    merge_hook_event(&mut settings, "PostToolUse", post_tool_use_hooks());

    let pretty = serde_json::to_string_pretty(&settings).map_err(io::Error::other)?;
    fs::write(path, pretty + "\n")
}

fn append_claude_md(folder: &Path) -> io::Result<()> {
    let path = folder.join("CLAUDE.md");
    if path.exists() {
        let existing = fs::read_to_string(&path)?;
        if existing.contains(CLAUDE_MD_MARKER) {
            return Ok(());
        }
        let mut updated = existing;
        if !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push_str(CLAUDE_MD_SECTION);
        fs::write(path, updated)
    } else {
        fs::write(path, format!("# claude.md\n{CLAUDE_MD_SECTION}"))
    }
}

/// Entry point: drop all agent-integration scaffolding into `folder`. Safe
/// to call repeatedly on the same folder (e.g. unwatch then re-watch) —
/// every step is idempotent, see module docs.
pub fn scaffold_folder(folder: &Path) -> io::Result<()> {
    write_commands(folder)?;
    merge_settings_hooks(folder)?;
    append_claude_md(folder)
}

/// Whether `folder` already carries the scaffold marker — checked against
/// disk rather than the currently-watched-folders list, since unwatching a
/// folder drops it from that list but leaves the scaffolding files in
/// place (see the plan's "leave it, don't remove" call). Callers use this
/// to decide whether to offer the opt-in prompt again: a folder that's
/// already scaffolded shouldn't re-prompt just because it was
/// unwatched and re-watched.
pub fn is_scaffolded(folder: &Path) -> bool {
    fs::read_to_string(folder.join("CLAUDE.md"))
        .is_ok_and(|contents| contents.contains(CLAUDE_MD_MARKER))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn scaffold_is_idempotent_and_preserves_user_hooks() {
        let dir = std::env::temp_dir().join(format!("scaffold-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        fs::create_dir_all(dir.join(".claude")).unwrap();
        fs::write(
            dir.join(".claude/settings.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"echo mine"}]}]}}"#,
        )
        .unwrap();
        fs::write(dir.join("CLAUDE.md"), "# my project\n\nsome notes.\n").unwrap();

        scaffold_folder(&dir).unwrap();
        let after_first = fs::read_to_string(dir.join(".claude/settings.json")).unwrap();
        let claude_md_after_first = fs::read_to_string(dir.join("CLAUDE.md")).unwrap();
        assert!(after_first.contains("echo mine"));
        assert!(after_first.contains(HOOK_MARKER));
        assert!(claude_md_after_first.contains("some notes."));
        assert!(claude_md_after_first.contains(CLAUDE_MD_MARKER));
        assert!(dir.join(".claude/commands/refsearch.md").exists());

        scaffold_folder(&dir).unwrap();
        let after_second = fs::read_to_string(dir.join(".claude/settings.json")).unwrap();
        let claude_md_after_second = fs::read_to_string(dir.join("CLAUDE.md")).unwrap();
        assert_eq!(
            after_first, after_second,
            "settings.json should be stable across re-scaffold"
        );
        assert_eq!(
            claude_md_after_first, claude_md_after_second,
            "CLAUDE.md should be stable across re-scaffold"
        );

        let v: Value = serde_json::from_str(&after_second).unwrap();
        let pre = v["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 3, "1 user hook + Grep + Bash, no dupes");

        fs::remove_dir_all(&dir).unwrap();
    }
}
