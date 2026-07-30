---
description: Find chunks in the reference index whose embedding is closest to an already-indexed chunk
argument-hint: [path] [start_line]
allowed-tools: mcp__reference-mcp__find_similar
---

Call the `mcp__reference-mcp__find_similar` tool with path and start_line parsed from $ARGUMENTS (first token is path, second is start_line), and folder: ${CLAUDE_PROJECT_DIR}

Report back the results it returns — do not use grep or Bash for this, the point of this command is to go straight through the MCP tool.
