# Benchmark results

Append-only (RFC 0060 §7). One section per recorded run — the table it printed,
under the machine it ran on; the JSON record beside it carries the full
configuration. **Rows are comparable only within one host key**: a different
machine is a different lane, never a trend line, and every heading names its
lane for that reason.

`update` columns: WaveDB retains every superseded version and the others retain
none, so read them beside `payload` and `amp`.

The entries below the header and above the first `##` heading are older runs in
the previous one-line-per-run format. They are left exactly as they were
recorded: the format changed, the history did not.

- `2026-08-22T22-30Z` · intel-r-core-tm-i5-8300h-cpu-2-30ghz-4c-500m-btrfs-e636 · `3e171bf` **(dirty tree)** — wavedb/durable: insert 221/s, read_cold 774/s, update 140/s, 6.81× space; wavedb/relaxed: insert 1053/s, read_cold 775/s, update 327/s, 7.07× space
- `2026-08-23T02-50Z` · intel-r-core-tm-i5-8300h-cpu-2-30ghz-4c-500m-btrfs-e636 · `3e171bf` **(dirty tree)** — wavedb/durable: insert 206/s, read_cold 778/s, update 155/s, 6.57× space; wavedb/relaxed: insert 984/s, read_cold 778/s, update 326/s, 6.35× space; wavedb-sharded/durable: insert 230/s, read_cold 919/s, update 156/s, 6.58× space; wavedb-sharded/relaxed: insert 726/s, read_cold 917/s, update 296/s, 6.59× space
