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

# Mirrors tauri.macos.conf.json's `build.features: ["metal"]` override for
# the main app binary — reference-mcp is a plain cargo crate with no Tauri
# config layer of its own, so the equivalent has to happen here instead of
# via a platform-specific config file. Same reasoning: embedding without a
# GPU backend still works, it's just slower, and this is the sidecar an
# agent calls on every single search, so the speed matters here too.
FEATURE_ARGS=()
if [[ "$TARGET_TRIPLE" == *apple-darwin* ]]; then
  FEATURE_ARGS=(--features metal)
fi

echo "building reference-mcp (release) for $TARGET_TRIPLE${FEATURE_ARGS:+ with ${FEATURE_ARGS[1]}}..."
(cd "$WORKSPACE_ROOT" && cargo build -p reference-mcp --release "${FEATURE_ARGS[@]}")

mkdir -p "$SRC_TAURI_DIR/binaries"
cp "$WORKSPACE_ROOT/target/release/reference-mcp$EXT" \
   "$SRC_TAURI_DIR/binaries/reference-mcp-$TARGET_TRIPLE$EXT"

echo "sidecar ready: src-tauri/binaries/reference-mcp-$TARGET_TRIPLE$EXT"
