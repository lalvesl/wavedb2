# Benchmark results

Append-only (RFC 0060 §7). One line per recorded run; the JSON record beside it
carries the full configuration. **Rows are comparable only within one host
key** — a different machine is a different lane, never a trend line.

`update` rows: WaveDB retains every superseded version and the others retain
none, so read them beside the space column.

