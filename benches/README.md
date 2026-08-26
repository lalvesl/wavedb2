# WaveDB comparative benchmark

Insert / read / update against the systems a developer would otherwise reach
for. Design, methodology and the reasoning behind every choice live in
[RFC 0060](../rfcs/0060-comparative-benchmark-suite.md); this file is
how to run it.

**Both brackets are implemented**: the WaveDB engine in-process against SQLite
(embedded), and MongoDB, PostgreSQL and MySQL over a local connection (server).
Nine rows, since every competitor carries a durable and a relaxed one and
WaveDB has only durable to offer.

**Two workloads**, printed as two tables:

- **micro** — one operation on one flat type: insert, read hot, read cold,
  update. Reported as **operations per second**.
- **shop** — an e-commerce schema (users, orders, line items) where a user *is*
  a tenant, and the phases are things a customer waits on: signup, checkout,
  profile, order page, order detail. Reported as **milliseconds, p50 beside
  p99**, because a rate hides the tail that a page render lives on. RFC 0060
  §3.1 has the modelling.

```sh
nix run .#bench                 # both workloads, fills from scratch, records
nix run .#bench-seeded          # same, but from the cached seeds (no fill)
nix run .#bench -- --quick      # smoke pass, records nothing
nix run .#bench -- --workload shop
nix run .#bench -- --only wavedb,mongodb
nix run .#bench -- --rows 1000000 --reads 200000 --updates 200000
nix run .#bench -- --users 2000 --checkouts 500   # default is 200 000 users
nix run .#bench -- --keep       # leave the scratch data dirs for inspection
```

Each server adapter **starts its own server** in the run's scratch directory,
on a unix socket (or a port derived from the process id, for MongoDB), and
shuts it down cleanly. No row is ever measured against whatever the machine
happens to be running: the durability column has to describe a server this run
configured, or it is a guess.

## The cage

Every run executes inside a fixed budget, so all five systems get the same
machine instead of each inferring a different one from the host:

| | |
|---|---|
| CPUs | **4**, via `taskset -c 0-3` |
| Memory | **500 MB**, via a `systemd-run --user --scope` cgroup (`MemoryMax`, `MemorySwapMax=0`) |
| Namespace | `bwrap --dev-bind / / --unshare-pid` |
| Server cache | **256 MB each**, pinned: `--wiredTigerCacheSizeGB 0.25`, `--innodb-buffer-pool-size=256M`, `shared_buffers=256MB` |

Three things about that table are worth reading twice. **Bubblewrap caps
nothing** — it is the namespace only; the limits are cgroups and affinity. The
memory cap bounds the **page cache**, which is what makes a cold read actually
cold instead of a memory read, and at 500 MB against a multi-gigabyte dataset
it is what finally answers RFC 0060's "larger than RAM" question. And the
server caches are pinned because each server sizes its cache from the
*machine's* RAM rather than the cgroup's — an unpinned MongoDB asks for
gigabytes it cannot have and is OOM-killed.

256 MB is not a tuning choice: it is **MongoDB's floor**
(`--wiredTigerCacheSizeGB` refuses less than 0.25), and equal budgets matter
more than a smaller one, so the least-adjustable server sets the number for all
three. Only one server runs at a time, so the cgroup holds one server plus the
benchmark process.

The budget is part of the **host key**, so a caged run and an uncaged one on the
same hardware are different lanes. Both budgets are read back from the kernel
(`Cpus_allowed_list`, the cgroup's `memory.max`) rather than assumed, so a
recorded row states what it really ran under.

## Seeds

Refilling every database before every run is the tedium that stops a benchmark
from being run at all, and a filled data directory is a pure function of
(system, version, rows, seed) — which is what a derivation is for.

```sh
nix build .#bench-dataset       # the portable TSV, generated once
nix build .#bench-seed-sqlite   # …-wavedb, -postgres, -mysql, -mongodb
```

All five exist, including the three whose Rust adapters do not: the dataset is
emitted once as TSV and each system loads that same file with its own bulk tool
(`.import`, `\copy`, `LOAD DATA`, `mongoimport`) inside the builder, so those
seeds are pinned and verified before the client code that will use them.

**Every fill opens WaveDB with a durability window** (`FILL_WINDOW`,
[RFC 0061](../rfcs/0061-relaxed-durability-window.md)) — the Nix seed and the
shop preload both. A fill is not a measurement, and one op is one batch is one
`fsync`, so a durable fill of a few million records is a few million barriers:
that is why the WaveDB seed used to take minutes where the others took seconds,
and why a 200 000-user preload was not affordable at all. Every **measured**
store reopens at the default, one barrier per batch, so no recorded number is
taken against a relaxed engine. The other four get the same courtesy under
other names — `.import`, `\copy`, `LOAD DATA` and `mongoimport` are not the
per-statement commit path either.

The MongoDB seed additionally needs **unprivileged user namespaces** on the
build host: `mongod` aborts in the Nix sandbox (its tcmalloc `CHECK`s on
`/sys/devices/system/cpu/possible`, which the sandbox does not provide), so that
one fill runs inside a `bubblewrap` namespace with a synthetic `/sys`. RFC 0060
§6 has the details.

Two properties are why this is Nix and not `~/.cache`: the server package is a
derivation input, so bumping it in `flake.lock` invalidates the seed (an ad-hoc
cache would hand a PostgreSQL 18 datadir to PostgreSQL 19); and every seed
**verifies its own row count** in the builder, because a bulk loader that fails
quietly produces a green build and an empty database.

Keep the `result` symlinks as GC roots, or `nix-collect-garbage` eats the fill.
A seeded run has no insert phase — the insert benchmark *is* a fill, so it can
never be served from a seed.

This crate is **outside the cargo workspace** on purpose: it links four
competitor drivers (`rusqlite`, `postgres`, `mysql`, `mongodb`), and none of
those may reach the shipped dependency graph or `cargo deny check`. SQLite comes
from the system library pinned by `flake.lock`, never `rusqlite`'s bundled copy
— that pinning is why the suite runs under Nix.

The three server drivers sit behind a `servers` feature, on by default. It
exists for `bench-gen`, which fills WaveDB and writes a TSV and needs no
database client: it is a build input of *every* seed, so without the gate each
seed rebuild would compile three drivers first. `nix build .#bench-gen` passes
`--no-default-features`.

## Reading a result

Runs are recorded to `results/<host-key>/<timestamp>-<sha>.json`, one appended
line per run in `results/index.md`. The corpus is **append-only**: a past run is
never edited to look better.

> **Two rows are comparable only when their host keys match.** A different
> machine is a different lane, never a trend line.

Five things the numbers do not mean without their context:

- **Embedded and server rows are different brackets.** A server row carries a
  connection round trip per operation that an in-process row does not. Compare
  WaveDB to SQLite, or MongoDB to PostgreSQL to MySQL — never across.
- **The shop table's `checkout` is not a like-for-like race either.** The other
  four commit the order and its line items in one transaction, one barrier;
  WaveDB has no multi-record transaction, so it is one batch and one barrier per
  record. That gap is the data model, not tuning.
- **`payload` and `log` are separate on purpose.** `log` is preallocated
  recovery capacity (`pg_wal`, `#innodb_redo`, the WiredTiger journal): a
  configured constant, the same size at 200 000 rows and at 20. Summing them
  would make the space headline a comparison of default log settings. `amp` is
  payload only, and it still includes the **empty-system baseline** printed
  under the table — an empty PostgreSQL cluster is ~24 MB of catalogs and an
  empty MySQL ~55 MB, which at small row counts is most of the ratio.

- **The update row is not a like-for-like race.** WaveDB retains every
  superseded version; nobody else does. Read it beside the footprint. The
  `+history` control that would make it comparable is RFC 0060 phase 4.
- **The relaxed row is asymmetric on purpose.** WaveDB has no relaxed
  durability mode to offer — one op is one batch is one `fsync`, always. The
  gap in that row is the measured price of the missing group commit.
- **Small `--rows` exaggerates fixed overhead.** Superblock, directory pages,
  dictionaries and B+tree nodes are constant; at a few thousand rows they
  dominate the footprint ratio.

## Guards

The runner refuses to record when the 1-minute load average exceeds 1.0
(`--force` overrides). A missing row beats a slow row that a future bisect
blames on a commit. A run from an uncommitted tree records but is permanently
marked `dirty`.
