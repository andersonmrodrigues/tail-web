#!/usr/bin/env bash
# Compares rtail with the system tail on a large log file using hyperfine.
# Usage: scripts/bench.sh [size in GiB, default 5]
set -euo pipefail

size_gib=${1:-5}
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
log="$dir/big.log"

command -v hyperfine >/dev/null || { echo "hyperfine is required: brew install hyperfine" >&2; exit 1; }
cargo build --release --quiet
rtail=./target/release/rtail

echo "Writing a ${size_gib} GiB log file..." >&2
line='2026-10-17T11:35:10Z INFO request id=12345 path=/api/orders/42 took=42ms'
# yes is killed by SIGPIPE when head has enough, which pipefail would report.
{ yes "$line" || true; } | head -c "$((size_gib * 1024 * 1024 * 1024))" > "$log"

hyperfine --warmup 3 --shell=none --output=null --export-markdown "$dir/last.md" \
  -n "tail -n 10" "tail -n 10 $log" \
  -n "rtail -n 10" "$rtail -n 10 $log" \
  -n "tail -c 1048576" "tail -c 1048576 $log" \
  -n "rtail -c 1048576" "$rtail -c 1048576 $log"

# Reading from line N streams the whole rest of the file, so it is I/O bound.
hyperfine --warmup 1 --runs 5 --shell=none --output=null --export-markdown "$dir/from.md" \
  -n "tail -n +2" "tail -n +2 $log" \
  -n "rtail -n +2" "$rtail -n +2 $log"

cat "$dir/last.md" "$dir/from.md"
