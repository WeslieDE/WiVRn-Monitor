#!/usr/bin/env bash
# Builds WiVRn-Monitor-x86_64.AppImage reproducibly: the Rust build runs in an Ubuntu 22.04 container
# (glibc 2.35) via podman (or docker), then appimagetool packs it.
# Usage: scripts/build-appimage.sh            output: $OUT_DIR/WiVRn-Monitor-x86_64.AppImage
#        scripts/build-appimage.sh --native   (or APPIMAGE_NATIVE=1) build with the local toolchain, no container (used by CI
#        inside an ubuntu:22.04 job; needs rust, build-essential, pkg-config, glslang-tools/glslc, binutils, curl)
# Env: OUT_DIR (default /run/media/weslie/Daten/appimage-out), WORK (build data), TOOLS (appimagetool dir)
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
NATIVE="${APPIMAGE_NATIVE:-0}"
[[ "${1:-}" == "--native" ]] && NATIVE=1
if [[ "$NATIVE" == 1 ]]; then DATA="${DATA:-${RUNNER_TEMP:-${TMPDIR:-/tmp}}/wivrn-monitor-build}"; else DATA="${DATA:-/run/media/weslie/Daten}"; fi
mkdir -p "$DATA"
OUT_DIR="${OUT_DIR:-$DATA/appimage-out}"
# Build data (~3 GB) goes to the data drive; falls back to ~/.cache if that drive has < 6 GB free.
if [[ -z "${WORK:-}" ]]; then
    if [[ "$(df --output=avail -BG "$DATA" 2>/dev/null | tail -1 | tr -dc 0-9)" -ge 6 ]]; then
        WORK="$DATA/appimage-work/wivrn-monitor"
    else
        WORK="${XDG_CACHE_HOME:-$HOME/.cache}/wivrn-monitor-appimage"
    fi
fi
TOOLS="${TOOLS:-$DATA/tools/appimagetool-dl}"
TOOL="$TOOLS/appimagetool-x86_64.AppImage"
IMAGE=wivrn-monitor-build:jammy
if [[ "$NATIVE" != 1 ]]; then
    CT="$(command -v podman || command -v docker)" || { echo "podman or docker required" >&2; exit 1; }
fi

if [[ ! -x "$TOOL" ]]; then
    mkdir -p "$TOOLS"
    curl -fsSL -o "$TOOL" https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage
    chmod +x "$TOOL"
fi

mkdir -p "$WORK/target" "$WORK/cargo-home" "$OUT_DIR"
BUILD_CMD='
    set -e
    cargo build --release --locked -p wivrn-monitor-viewer -p wivrn-monitor-server-layer
    for f in "$CARGO_TARGET_DIR"/release/wivrn-monitor-viewer "$CARGO_TARGET_DIR"/release/libwivrn_monitor_server_layer.so; do
        echo "== $(basename $f): max glibc $(objdump -T $f | grep -o "GLIBC_[0-9.]*" | sort -uV | tail -1)"
    done'
if [[ "$NATIVE" == 1 ]]; then
    # CARGO_HOME stays as set by the environment (rustup) so the installed toolchain is found.
    (cd "$ROOT" && CARGO_TARGET_DIR="$WORK/target" bash -c "$BUILD_CMD")
else
    "$CT" build -q -t "$IMAGE" -f "$ROOT/scripts/Containerfile.build" "$ROOT/scripts" >/dev/null
    # Source is mounted read-only; build data and the cargo registry cache live in $WORK.
    "$CT" run --rm --userns=keep-id -v "$ROOT":/src:ro -v "$WORK/target":/target -v "$WORK/cargo-home":/cargo-home \
        -e CARGO_TARGET_DIR=/target -e CARGO_HOME=/cargo-home -e RUSTUP_HOME=/opt/rustup -e HOME=/tmp \
        -w /src "$IMAGE" bash -c "export PATH=/opt/cargo/bin:\$PATH; $BUILD_CMD"
fi

APPDIR="$WORK/AppDir"
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/lib/wivrn-monitor"
for sz in 64 256 512; do mkdir -p "$APPDIR/usr/share/icons/hicolor/${sz}x${sz}/apps"; done
install -m755 "$WORK/target/release/wivrn-monitor-viewer" "$APPDIR/usr/bin/"
install -m755 "$WORK/target/release/libwivrn_monitor_server_layer.so" "$APPDIR/usr/lib/wivrn-monitor/"
install -m644 "$ROOT/server-layer/wivrn-monitor-server-layer.json" "$APPDIR/usr/lib/wivrn-monitor/"
install -m755 "$ROOT/packaging/AppRun" "$APPDIR/AppRun"
install -m644 "$ROOT/packaging/wivrn-monitor.desktop" "$APPDIR/"
install -m644 "$ROOT/packaging/wivrn-monitor.png" "$APPDIR/wivrn-monitor.png"
cp "$APPDIR/wivrn-monitor.png" "$APPDIR/.DirIcon"
for sz in 64 256 512; do install -m644 "$ROOT/assets/icon-$sz.png" "$APPDIR/usr/share/icons/hicolor/${sz}x${sz}/apps/wivrn-monitor.png"; done

# --appimage-extract-and-run: no FUSE needed on the build host.
ARCH=x86_64 "$TOOL" --appimage-extract-and-run --no-appstream "$APPDIR" "$OUT_DIR/WiVRn-Monitor-x86_64.AppImage"
ls -l "$OUT_DIR/WiVRn-Monitor-x86_64.AppImage"
