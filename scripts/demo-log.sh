#!/usr/bin/env bash
# Writes a fake service log for demo.tape, then rotates and truncates it so
# the recording shows rtail -F keeping up.
set -euo pipefail

log=$1
mkdir -p "$(dirname "$log")"
: > "$log"
levels=(INFO INFO INFO DEBUG WARN ERROR)

emit() {
  for ((i = 0; i < $1; i++)); do
    level=${levels[RANDOM % ${#levels[@]}]}
    echo "$(date +%H:%M:%S) $level GET /api/orders/$((RANDOM % 900)) $((RANDOM % 300))ms" >> "$log"
    sleep 0.25
  done
}

emit 12
mv "$log" "$log.1"      # logrotate: rename, then start a new file
emit 10
: > "$log"              # copytruncate
emit 10
