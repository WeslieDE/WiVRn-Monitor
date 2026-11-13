#!/usr/bin/env bash
# Install (or --uninstall) the WiVRn-Monitor *server* Vulkan layer for the current user.
# Usage: scripts/install-server-layer.sh [--debug] [--uninstall]
# The layer is implicit but only ever loads inside `wivrn-server`; restart wivrn-server afterwards.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET="${CARGO_TARGET_DIR:-/run/media/weslie/Daten/cargo-target/wivrn-monitor}"
PROFILE=release; [[ "${1:-}" == "--debug" ]] && { PROFILE=debug; shift; }
DEST="$HOME/.local/share/wivrn-monitor"
LAYERS="${XDG_DATA_HOME:-$HOME/.local/share}/vulkan/implicit_layer.d"
NAME=libwivrn_monitor_server_layer.so
MANIFEST=wivrn-monitor-server-layer.json

if [[ "${1:-}" == "--uninstall" ]]; then
    rm -f "$LAYERS/$MANIFEST" "$DEST/$NAME" "$DEST/$MANIFEST"
    echo "uninstalled (restart wivrn-server to unload the layer)"; exit 0
fi

SO="$TARGET/$PROFILE/$NAME"
[[ -f "$SO" ]] || { echo "missing $SO - run: cargo build --$PROFILE -p wivrn-monitor-server-layer" >&2; exit 1; }
mkdir -p "$DEST" "$LAYERS"
# Atomic rename: a running wivrn-server keeps the old inode mapped (overwriting in place could crash it).
install -m755 "$SO" "$DEST/.$NAME.new"
mv -f "$DEST/.$NAME.new" "$DEST/$NAME"
sed "s|\"library_path\": \"[^\"]*\"|\"library_path\": \"$DEST/$NAME\"|" "$ROOT/server-layer/$MANIFEST" > "$DEST/.$MANIFEST.new"
mv -f "$DEST/.$MANIFEST.new" "$DEST/$MANIFEST"
install -m644 "$DEST/$MANIFEST" "$LAYERS/.$MANIFEST.new"
mv -f "$LAYERS/.$MANIFEST.new" "$LAYERS/$MANIFEST"
echo "installed ($PROFILE): $LAYERS/$MANIFEST -> $DEST/$NAME"
echo "restart wivrn-server for it to take effect; log: $DEST/server-layer.log"
