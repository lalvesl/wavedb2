#!/usr/bin/env bash
# Measure the per-struct wasm cost of the exposure registry `match` — the M1
# risk item, closed in M5. Builds `examples/registry-size` (64 structs
# defined; 1 / 16 / 64 of them exposed via feature width) for
# wasm32-unknown-unknown and reports the marginal bytes per exposed struct.
#
# Sizes are the cargo `wasm-release` artifact — pre-bindgen, no wasm-opt
# (same caveat as `wasm_size.sh --cargo`); absolute numbers are an upper
# bound, the *deltas* are the measurement.
#
# Usage: scripts/registry_size.sh

set -euo pipefail
cd "$(dirname "$0")/.."

WASM=target/wasm32-unknown-unknown/wasm-release/registry_size.wasm

# measure <label> [--features …] → "<raw> <gzip>"
measure() {
  local label=$1
  shift
  cargo build -q --target wasm32-unknown-unknown --profile wasm-release \
    -p registry-size "$@"
  local raw gz
  raw=$(stat -c%s "$WASM")
  gz=$(gzip -9 -c "$WASM" | wc -c)
  printf '%-10s %8d raw  %8d gzip\n' "$label" "$raw" "$gz" >&2
  echo "$raw $gz"
}

read -r RAW1 GZ1 <<<"$(measure "n=1")"
read -r RAW16 GZ16 <<<"$(measure "n=16" --features n16)"
read -r RAW64 GZ64 <<<"$(measure "n=64" --features n64)"

echo
echo "marginal cost per exposed struct (Unique; knows + decode_check kept):"
printf '  1 → 16:  %5d B raw   %5d B gzip\n' \
  $(((RAW16 - RAW1) / 15)) $(((GZ16 - GZ1) / 15))
printf '  16 → 64: %5d B raw   %5d B gzip\n' \
  $(((RAW64 - RAW16) / 48)) $(((GZ64 - GZ16) / 48))
