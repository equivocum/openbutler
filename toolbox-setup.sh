#!/bin/bash
# One-time system deps for building OpenButler INSIDE the toolbox container.
# Run: toolbox run -c openbutler "$AGENT_HOME/toolbox-setup.sh"
# (Replace `openbutler` with your container name from .env VOICE_CONTAINER.)
# The Rust toolchain itself rides along via the shared $HOME/.cargo — this
# script only ensures native build deps + runtime helpers. Idempotent.
set -u
have() { command -v "$1" > /dev/null 2>&1; }

echo "== openbutler container setup =="
if have cargo && have rustc; then
  echo "rust: $(rustc --version)"
else
  echo "rust NOT found in container (expected via shared ~/.cargo)."
  echo "Install with: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
fi

if have dnf; then
  echo "installing native build deps via dnf (needs sudo)..."
  sudo dnf install -y cmake gcc clang alsa-lib-devel espeak-ng espeak-ng-devel ffmpeg openssl-devel 2>&1 | tail -3
  # Optional sound-server headers for cpal backends (Phase 2); warn-only if missing:
  sudo dnf install -y pulseaudio-libs-devel pipewire-devel 2>&1 | tail -2 || echo "note: pulse/pipewire dev headers unavailable — ALSA backend still works"
else
  echo "no dnf here; ensure cmake/gcc/clang, alsa dev headers, espeak-ng(+dev), ffmpeg exist"
fi

for t in cmake gcc clang espeak-ng ffmpeg; do
  if have "$t"; then echo "ok: $t"; else echo "MISSING: $t"; fi
done
echo "done."
