---
description: Check whether a doc chunk in the reference index still matches the code it describes
argument-hint: [path] [start_line]
allowed-tools: mcp__reference-mcp__check_doc_drift
---

Call the `mcp__reference-mcp__check_doc_drift` tool with path and start_line parsed from $ARGUMENTS (first token is path, second is start_line), and folder: ${CLAUDE_PROJECT_DIR}

Report back the results and the likely_stale flag it returns — do not use grep or Bash for this, the point of this command is to go straight through the MCP tool.
