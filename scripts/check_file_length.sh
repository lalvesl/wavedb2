#!/usr/bin/env bash
# Enforce the max-lines-per-file budget for Rust sources.
#
# Clippy has no file-level line lint (its `too_many_lines` is per *function*),
# so this script is the enforcement for the file budget documented in
# docs/development_standards.md. Colocated tests don't count: lines are
# counted only up to the first `#[cfg(test)]` marker, matching the standards'
# "tests colocate and don't count" rule.
#
# Usage: scripts/check_file_length.sh [limit]   (default 350)

set -euo pipefail

LIMIT="${1:-350}"
root="$(cd "$(dirname "$0")/.." && pwd)"
fail=0

while IFS= read -r file; do
    [ -f "$root/$file" ] || continue # tracked but deleted in the working tree
    # Count lines before the colocated-test marker (whole file if absent).
    non_test=$(awk '/^#\[cfg\(test\)\]/ { exit } { n++ } END { print n+0 }' "$root/$file")
    if [ "$non_test" -gt "$LIMIT" ]; then
        echo "FAIL $file: $non_test non-test lines (limit $LIMIT)"
        fail=1
    fi
done < <(git -C "$root" ls-files 'crates/*.rs' 'examples/*.rs')

if [ "$fail" -ne 0 ]; then
    echo "error: files exceed the $LIMIT-line budget — split them (see docs/development_standards.md)"
    exit 1
fi
echo "ok: every Rust file is within the $LIMIT-line budget"
