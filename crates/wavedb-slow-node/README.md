# wavedb-slow-node

> **Deferred.** The cold/history tier is intentionally out of scope for the
> current rebuild.

The plan is a cold tier that receives older versions and transaction logs flushed
down from serving nodes — sized for `$/TB` rather than IOPS — and serves history
reads off the latency path. Until it lands, serving nodes
([`wavedb-quick-node`](../wavedb-quick-node/README.md)) keep full history locally
and there is **no flush-down** path.

This crate exists in the workspace as a placeholder so `cargo build --workspace`
resolves; its design will be written when the tier is picked up again.
