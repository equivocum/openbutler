#!/bin/bash
# OpenButler launcher: face + voice on this machine.
#
# Run it from your terminal (NOT from an AI session):
#   ./start.sh
#   ./start.sh --no-face
#
# The face is a web page served locally with no browser of its own, so
# open its URL yourself:
#   http://127.0.0.1:$FACE_PORT/faces/neural/
# (FACE_PORT from .env; faces: board, neural, radial, rain.)
#
# Talk: just speak (mode from voice.json: ptt / open hands-free / wake).
# Hang up: say "goodbye $AGENT_NAME" (AGENT_NAME from .env).
# Ctrl-C stops everything (the face server is killed too).
set -u
# shellcheck disable=SC1091
. "$(cd "$(dirname "$0")" && pwd)/.env"
cd "$AGENT_HOME"

FACE=1
[ "${1:-}" = "--no-face" ] && FACE=0

if [ "$FACE" = 1 ]; then
  ./target/debug/openbutler-face --no-open > /tmp/openbutler-face.log 2>&1 &
  FACE_PID=$!
  cleanup() {
    trap - EXIT INT TERM
    kill "$FACE_PID" 2>/dev/null
    echo
    echo "$AGENT_NAME stopped."
  }
  trap cleanup EXIT INT TERM
  sleep 2
  if curl -s -m 3 "http://127.0.0.1:$FACE_PORT/" > /dev/null 2>&1; then
    echo "face up: open http://127.0.0.1:$FACE_PORT/faces/neural/ in your browser"
  else
    echo "face failed to start; see /tmp/openbutler-face.log (voice still starting)"
  fi
fi

echo "starting voice (Ctrl-C stops everything)..."
./target/debug/openbutler-voice run
