---
description: Search the reference index for how something was solved in a different watched project, excluding the current one
argument-hint: [query]
allowed-tools: mcp__reference-mcp__search
---

Call the `mcp__reference-mcp__search` tool with query: $ARGUMENTS and exclude_folder: ${CLAUDE_PROJECT_DIR}

Report back the results and citations it returns — do not use grep or Bash for this, the point of this command is to go straight through the MCP tool.
