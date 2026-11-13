#!/usr/bin/env bash
# CI helper: installs build dependencies on Ubuntu 22.04 (apt + rustup stable). Used by the Gitea/GitHub workflows.
# Sound recording needs no extra build packages: the AAC encoder is pure Rust and libpulse is loaded at run time (dlopen).
# Optional for the container-free tests: `ffmpeg` (also provides ffprobe); the audio/video e2e tests skip themselves without it.
# After running, `. "$HOME/.cargo/env"` (or add ~/.cargo/bin to PATH). Runs as root or via sudo.
set -euo pipefail
SUDO=""; [[ "$(id -u)" != 0 ]] && SUDO="sudo"
export DEBIAN_FRONTEND=noninteractive
$SUDO apt-get update
$SUDO apt-get install -y --no-install-recommends ca-certificates curl git build-essential pkg-config \
    glslang-tools binutils file desktop-file-utils nasm
if ! command -v cargo >/dev/null 2>&1 && [[ ! -x "$HOME/.cargo/bin/cargo" ]]; then
    curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --component clippy --default-toolchain stable
fi
