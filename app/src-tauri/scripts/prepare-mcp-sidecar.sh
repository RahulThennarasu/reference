#!/usr/bin/env bash
# Runs via tauri.conf.json's `build.beforeBundleCommand`, right before `tauri
# build` packages the app — builds reference-mcp and drops it at the
# target-triple-suffixed path Tauri's `externalBin` sidecar mechanism expects
# (see docs/mcp-agent-usage.md), so the packaged app ships an MCP server
# binary a real end user can point Claude Code at, not just repo developers.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SRC_TAURI_DIR="$(dirname "$SCRIPT_DIR")"
WORKSPACE_ROOT="$(cd "$SRC_TAURI_DIR/../.." && pwd)"

TARGET_TRIPLE="$(rustc --print host-tuple)"
EXT=""
if [[ "$TARGET_TRIPLE" == *windows* ]]; then
  EXT=".exe"
fi

echo "building reference-mcp (release) for $TARGET_TRIPLE..."
(cd "$WORKSPACE_ROOT" && cargo build -p reference-mcp --release)

mkdir -p "$SRC_TAURI_DIR/binaries"
cp "$WORKSPACE_ROOT/target/release/reference-mcp$EXT" \
   "$SRC_TAURI_DIR/binaries/reference-mcp-$TARGET_TRIPLE$EXT"

echo "sidecar ready: src-tauri/binaries/reference-mcp-$TARGET_TRIPLE$EXT"
