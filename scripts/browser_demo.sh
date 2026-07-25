#!/usr/bin/env bash
# The M5 exit, executed: start a live node (contact-book's registry) and run
# the typed browser demo against it in headless Chrome —
# crates/wavedb-wasm/tests/live_node.rs (a #[server] call, a typed Unique
# save, a streamed collection read, IndexedDB caching reads).
#
# Usage:
#   scripts/browser_demo.sh
#
# Needs a runnable chromedriver: exported as $CHROMEDRIVER, found on $PATH,
# or (NixOS — the wasm-pack-downloaded binary can't run there) fetched via
# `nix shell nixpkgs#chromedriver`.

set -euo pipefail
cd "$(dirname "$0")/.."

# ── 1. The node: contact-book's demo binary on a fresh loopback port ───────
cargo build -p contact-book --example node
NODE_LOG=$(mktemp)
cargo run -q -p contact-book --example node >"$NODE_LOG" 2>&1 &
NODE_PID=$!
trap 'kill "$NODE_PID" 2>/dev/null || true' EXIT

ADDR=""
for _ in $(seq 1 50); do
  ADDR=$(sed -n 's/^LISTENING //p' "$NODE_LOG" | head -n1)
  [[ -n "$ADDR" ]] && break
  kill -0 "$NODE_PID" 2>/dev/null || { cat "$NODE_LOG" >&2; echo "node died before binding" >&2; exit 1; }
  sleep 0.2
done
[[ -n "$ADDR" ]] || { cat "$NODE_LOG" >&2; echo "node never printed its address" >&2; exit 1; }
echo "node listening on $ADDR"

# ── 2. The browser test, with the node address baked in ────────────────────
run_wasm_pack() {
  (cd crates/wavedb-wasm &&
    WAVEDB_DEMO_NODE="$ADDR" CHROMEDRIVER="$1" \
      wasm-pack test --headless --chrome)
}

if [[ -n "${CHROMEDRIVER:-}" ]]; then
  run_wasm_pack "$CHROMEDRIVER"
elif command -v chromedriver >/dev/null; then
  run_wasm_pack "$(command -v chromedriver)"
else
  # NixOS path: wasm-pack's auto-downloaded chromedriver is a generic-linux
  # dynamic binary and cannot run there — borrow the nixpkgs one.
  nix shell nixpkgs#chromedriver --command bash -c \
    "cd crates/wavedb-wasm && WAVEDB_DEMO_NODE='$ADDR' CHROMEDRIVER=\$(command -v chromedriver) wasm-pack test --headless --chrome"
fi

echo "browser demo green against $ADDR"
