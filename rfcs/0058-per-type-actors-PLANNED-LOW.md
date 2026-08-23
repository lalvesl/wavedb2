# RFC 0058 — Per-type actors, and a `Send` engine

- **Status:** **Planning — incomplete, low priority.** Opened 2026-08-01, parked
  2026-08-04: the design below is not accepted and the planning is not finished.
  It is kept because the *problem* it names is real and undocumented elsewhere;
  the *answer* it proposes has weak points that were not worked through.
- **Crates:** `wavedb-platform`, `wavedb-core`, `wavedb-storage`, `wavedb-quick-node`, `wavedb-macros`
- **Related:** [RFC 0057](0057-page-arena-and-checkpoint-staging.md) (the arena
  this reorganizes ownership around), [RFC 0041](0041-single-barrier-checkpoint.md)
  / [RFC 0046](0046-directory-deltas-in-the-window.md) (the checkpoint whose
  blocking is the motivation)
- **Reverses:** the deliberate non-`Send` stance recorded in `CLAUDE.md`
  (user decision, 2026-08-01 — *"when I started the database the priority was the
  database; now comes the refinement, which is decent multithreading"*). The
  decision stands on its own; the migration is parked with this RFC because this
  is its only vehicle. Nothing has changed in the tree — `CLAUDE.md` still
  describes the engine as it is, non-`Send`, and the `allow`s are still there.

## Why this is parked

**The gating requirement, stated first because the design below does not meet
it: the whole engine must be able to run as a *single actor on a single
thread*.** That is the wasm target, and it is not the same thing as "N actors as
N tasks on one worker", which is what this RFC actually describes. N mailboxes
interleaving on one thread is a different execution model from one mailbox, and
the degenerate case is where the failures would be — not in the parallel one.
Any successor has to make the single-actor configuration the *base* case and
multi-actor the elaboration, rather than the reverse.

The rest of what is unresolved, so reopening starts from a list rather than from
a re-read:

1. **Actor-to-actor request/reply is an unaddressed deadlock surface.** The rule
   here — *never await anything slow inside the loop* — covers IO and says
   nothing about awaiting another actor. A batch already spans at least three
   mailboxes (type → journal → writer). With one thread and one actor, every one
   of those hops is a self-call.
2. **The single-actor collapse has no design.** With everything in one actor,
   the per-type state separation buys nothing and costs a routing layer; the
   RFC never says what the structure degrades *to*.
3. **`Lane::Tree` charges a `STRUCT_HASH` change** — a schema break for every
   type with a secondary index — to buy a concurrency refactor. That price was
   accepted in a sentence.
4. **The `Send` migration is wide and reverses a documented hard rule**, against
   a benefit that has not been measured.
5. **Every number here is an estimate.** ~1 µs per message, ~10⁶ messages/s,
   "the mailbox cannot be the bottleneck" — none of it was measured.
6. **Ordering across actors was left open**, not answered (see the last open
   question below) — and it is exactly the invariant single-threadedness is
   hiding today.
7. **The read path's answer, `get_many`, does not exist**; it is a proposed API
   standing in for a proof.
8. **Arena ownership (RFC 0057) has no answer**, and it decides whether `Sync`
   actually disappears or quietly comes back.

What survives regardless of the answer is the **Motivation** section: the
synchronous `fn`s on the request path, and the two invariants single-threadedness
is silently supplying. Those are real today and documented nowhere else.

## Summary

Two changes that only make sense together:

1. **The engine becomes `Send` on native.** `#![allow(clippy::future_not_send)]`
   comes off the crate roots and the `Store` trait's `async fn`s desugar to
   futures that state their `Send`ness. Wasm keeps the single-threaded shape
   through a cfg'd bound, so nothing there pays for threads it does not have.
2. **The engine's mutable state moves out of process-global `static`s behind
   locks and into one actor per type** — each owning that type's whole family:
   the record cache, the Pivot, the recency/dead/list chains, the sparse index,
   the directory and the zstd dictionary. One mpsc mailbox each,
   multi-producer, single-consumer.

The partition is not new — it is already the storage layout
(`storage_entries()` returns six slots per type, and a `Store::apply` batch
never spans two user types). It simply is not in the concurrency, where a single
current-thread runtime serializes everything.

`Send` and actors are complements, not alternatives: **`Send` is what lets a
task move between workers; the actor is what makes sure only one task owns a
given piece of state.** Together they give work-stealing *and* lock-free
ownership. Which is why `Sync` mostly disappears — nothing is shared, only
moved.

## The shape, in one picture

```
  callers ── net handlers · #[server] bodies · client write-through cache
     │
     │  get · get_many · apply        a call is a message; nothing is shared
     ▼
  ┌────────────────────────────────────────────────────────────────────┐
  │ Store handle        Send + Clone · routes on STRUCT_HASH           │
  │                     holds one mpsc::Sender per type                │
  └───────┬────────────────────┬─────────────────────┬─────────────────┘
          ▼                    ▼                     ▼
  ┌─────────────────┐  ┌─────────────────┐  ┌─────────────────┐
  │ TYPE ACTOR      │  │ TYPE ACTOR      │  │ TYPE ACTOR      │  one task
  │   User          │  │   Todo          │  │   Session    …  │  each,
  └────────┬────────┘  └─────────────────┘  └─────────────────┘  work-
           │         an idle mailbox is a parked waker, 0 CPU    stolen
           │  inside any one of them:                            across
           ▼                                                     cores
  ┌────────────────────────────────────────────────────────────────────┐
  │ mpsc mailbox ◀── the only door: reads AND writes enter here        │
  │                                                                    │
  │ owns outright — a local binding in the loop, so exclusivity is     │
  │ the borrow checker's, not a lock's (no Mutex, no Sync, no static)  │
  │                                                                    │
  │   record cache  HashMap<Id, Slot>                                  │
  │                   Slot::Present(bytes) | Slot::Loading(waiters)    │
  │   Pivot · recency chain · dead chain · list chains                 │
  │   sparse index · Lane::Tree · page directory · zstd dictionary     │
  │   pending[]  settling[]       evict only when BOTH are empty       │
  │                                                                    │
  │ per message: hashmap work (~1 µs) then hand off — never awaits IO  │
  └────┬───────────────────────────────────────────┬───────────────────┘
       │ dispatch, does not await                  │ one batch = one msg
       ▼                                           ▼
  ┌────────────────────────────┐   ┌───────────────────────────────────┐
  │ run_blocking  (stateless)  │   │ JOURNAL ACTOR          (single)   │
  │  native: spawn_blocking    │   │  append + fsync                   │
  │  wasm:   direct call       │   │  awaits its own barrier, so the   │
  │                            │   │  mailbox fills behind it          │
  │  pread · zstd decode/enc   │   │  ⇒ group commit, nothing arranges │
  │  SlotPlan: merge+compress  │   └────────────────┬──────────────────┘
  └────┬───────────────────────┘                    │ committed
       │                                            ▼
       │ the result comes      ┌───────────────────────────────────────┐
       └─▶ back AS A MESSAGE   │ WRITER ACTOR                (single)  │
           to the type actor   │  one window per round                 │
                               │  free-space map · arena (RFC 0057)    │
                               │  data.bin fsync                       │
                               └───────────────────┬───────────────────┘
                                                   ▼
       Shared<BlockFile> ── read_at is positioned, so pread on a shared
       fd is parallel by construction: the one legitimate Sync left
```

Two traces make the rules concrete.

**Read, cache miss** — the actor never sits on the seek:

```
caller ──get(id)──▶ actor  miss ⇒ install Loading, dispatch, take next message
                             └──▶ run_blocking: pread + zstd decode
       ◀──oneshot──── actor ◀──ReadComplete(bytes)──┘   fills Present,
                                                        wakes every waiter
```

A second caller arriving mid-flight parks its `oneshot` in `Loading` and
dispatches nothing: N misses on one id, one IOp. A write arriving mid-flight
replaces `Loading(waiters)` with `Present(new)` and answers them at once; the
read's bytes are dropped on arrival.

**Write** — the barrier is the only slow thing, and only one actor waits on it:

```
caller ──apply(batch)──▶ actor  Expect guard · mutate cache + chains · pending+=
                                  └──▶ journal actor: append, fsync, group-commit
       ◀────── Ok once journaled     (mailbox order alone gives read-your-writes,
                                      so the cache-commit reply is not awaited)

later, on settle:       actor gathers the touched bytes only
                          └──▶ run_blocking(plan) ──▶ writer: one window,
                                                       one data.bin fsync
```

Everything below the actor row is shared by all types; everything at and above
it is per type. The journal and the writer are single **on purpose** — see
"What stays single".

## Motivation

`drain()`, `settle()`, `place_in()` and `commit_journal()` are synchronous `fn`s
with no await points, on a current-thread runtime. A checkpoint blocks the
request path completely — not by taking a lock, but by occupying the only
thread.

**The concurrency is wasted.** The database is partitioned by type at the
storage layer, and atomicity is only ever *needed* at that layer. One reader and
one writer for the whole engine throws that away.

**And single-threadedness is silently providing invariants** that nothing
documents as depending on it:

- *Batch application is atomic.* `commit_to_caches` takes each type's write lock
  separately; only the absence of interleaving keeps a reader from seeing half a
  batch.
- *`pending` empty ⇒ everything settled*, which `evict_settled` relies on
  (`apply.rs:22`). `drain()` does `std::mem::take` on the queue, so during a
  settle the queue is empty and the pages are not yet written. A concurrent
  eviction would drop a cached id whose page does not hold it yet, and the next
  read would fall to stale bytes — **a live-consistency fault with no crash
  involved.**

Any concurrency design has to replace both explicitly. Actors do it by
construction rather than by discipline, which is the argument for them over
finer-grained locking.

## Design

### Part 1 — the `Send` migration

**It is smaller than it looks.** `wavedb-storage` contains **zero** `Rc`/`RefCell`
— it is already lock-based and therefore already `Sync`-capable. Across
core, storage and quick-node there are 23 occurrences in **6 files**
(`core/store.rs`, `core/notify.rs`, and four quick-node serve/subscribe files).

The real friction is one thing: **`Store` uses async-fn-in-trait**, which gives
callers no way to state that the returned future is `Send` (hence the
`#![allow(async_fn_in_trait)]` already sitting in `wavedb/src/lib.rs`). Three
ways out, and only one is available:

| | verdict |
|---|---|
| `Pin<Box<dyn Future + Send>>` | **forbidden** — no `dyn`, hard rule |
| `trait_variant::make` | a dependency, against the minimal-dep + `cargo deny` stance |
| desugar to `-> impl Future<Output = …> + MaybeSend` | **chosen** — no dependency, and it retires `async_fn_in_trait` too |

```rust
pub trait Store {
    fn get(&self, id: Id)
        -> impl Future<Output = Result<Option<Vec<u8>>>> + MaybeSend;
    fn apply(&self, batch: &[Write])
        -> impl Future<Output = Result<()>> + MaybeSend;
    // …
}
```

Mechanical, but wide: every impl and every generic bound.

### The platform seam — three bounds and one pointer

`Send` must be **conditional**, or wasm pays for threads it does not have. All
of it lives in `wavedb-platform`, and **no `#[cfg]` appears in engine code**:

```rust
// Send where Send exists. No `'static` — futures borrow `&self`.
#[cfg(not(target_arch = "wasm32"))]
pub trait MaybeSend: Send {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Send + ?Sized> MaybeSend for T {}
#[cfg(target_arch = "wasm32")]
pub trait MaybeSend {}
#[cfg(target_arch = "wasm32")]
impl<T: ?Sized> MaybeSend for T {}

// Same shape for Sync — needed in exactly one place (see below).
pub trait MaybeSync { … }

// Send + 'static: mailbox messages and spawn factories.
pub trait Portable: MaybeSend + 'static {}

// A refcount that crosses threads where threads exist.
#[cfg(not(target_arch = "wasm32"))]
pub type Shared<T> = std::sync::Arc<T>;
#[cfg(target_arch = "wasm32")]
pub type Shared<T> = std::rc::Rc<T>;
```

The split between `MaybeSend` (no lifetime bound) and `Portable` (`+ 'static`)
is load-bearing: a future returned from `&self` is never `'static`, so a single
combined bound would not typecheck on the `Store` trait.

Two disciplines around `Shared<T>`:

- Use it only where a value crosses a thread. Sharing *within* one actor is
  plain `Rc` on both targets, and spelling it `Shared` silently buys `Arc` for
  nothing.
- Resist `Shared<Mutex<T>>`. Needing it means ownership did not actually move
  into an actor — it is the smell test this design rests on.

(Honest note on cost: on `wasm32-unknown-unknown` without `+atomics`, an `Arc`'s
atomics lower to plain non-atomic operations, so the runtime saving is small.
The win is one spelling instead of a `#[cfg]` at every use site.)

### The `allow` cannot come off first

Removing `#![allow(clippy::future_not_send)]` from the seven crate roots is the
**last** step of the migration, not the first — ahead of it, it only produces
warnings for work not yet done. And it should come off as:

```rust
#![cfg_attr(target_arch = "wasm32", allow(clippy::future_not_send))]
```

because the wasm build keeps `Rc`-holding futures on purpose. The ten
`#[allow(clippy::future_not_send)]` attributes `wavedb-macros` emits into user
crates get the same treatment.

### Part 2 — the partition is already there

| piece of a batch | slot |
|---|---|
| the record | `T::STRUCT_HASH` |
| recency / dead / list chains | `Lane::hash(tag ++ T::STRUCT_HASH)` |
| sparse index nodes | `Lane::Index` of T |
| the Pivot | `T::Pivot::STRUCT_HASH`, generated from T |
| **B+tree nodes** | **`BPTREE_NODE_STORAGE` — process-global** |

The first four are one family, and they are exactly the six slots
`storage_entries()` already emits. The fifth is the blocker.

### Prerequisite: `Lane::Tree`

```rust
pub static BPTREE_NODE_STORAGE: StructStorage =
    StructStorage::without_compression(BPTREE_NODE_STRUCT_HASH);
```

One slot for every type's B+tree nodes. Two types writing their secondary
indexes at once collide on its cache and directory: either a lock comes back —
defeating the design — or the slot becomes an actor every indexed write in the
database funnels through.

The fix is the mechanism already in use: a **`Lane::Tree`** derived per type,
like the other four (RFC 0054's lane split). Seven slots per type, one more
directory, and the partition becomes total.

This is the one part of this RFC that **reaches stored bytes**, so it folds into
`STRUCT_HASH` — a schema change for every type declaring a secondary index.

### What stays single, and why that is not a compromise

**The journal.** One file, one fsync. Per-type journals would mean N fsyncs per
wave of writes, and the barrier is *the* scarce resource. Keeping it single
turns the serialization into a feature: many types' batches ride **one** commit.

**The writer.** One `data.bin`, one free-space map, one window per round
(RFC 0041). Nothing to split.

### What `Send` buys that the `!Send` design could not

With `!Send` actors pinned to dedicated threads, a hot type saturates its
executor while others idle, and rebalancing is impossible — moving the state
would require exactly the `Send` that was refused. That cost was permanent, and
the workaround was an extra mechanism: a stateless pool to drain the expensive
work off the hot actor.

With `Send`:

- a type actor is a `tokio::spawn`ed task on the multi-thread runtime;
- **work-stealing balances actors across cores** — the actor's state travels
  with its task, which is legal precisely because it is `Send`;
- the stateless pool collapses into plain `spawn_blocking` for zstd and
  positioned reads, which is what that API is for.

One mechanism instead of two.

**What it does *not* fix — stated plainly, because an earlier draft of this RFC
claimed otherwise:** work-stealing balances *different* actors across cores. It
does not make *one* actor faster. An actor processes one message at a time, so a
single hot type is one core at a time whichever core that happens to be. See
"The hot-type ceiling" below for what actually bounds it.

### `Sync` still mostly disappears — and that is the actor's doing, not `Send`'s

`Send` is about **moving**; `Sync` is about **sharing**. Actors own rather than
share, so the locks that exist only to satisfy `Sync` on a `static` go away:

- `mem_cache: RwLock<HashMap>` → `HashMap`
- `directory: Mutex<Option<Directory>>` → `Option<Directory>`
- `dictionary: Mutex<Dictionary>` → `Dictionary`

Not "the locks stop contending" — **they stop existing.**

The one genuine `Sync` that remains is `BlockFile`, if page reads run on the
actors' threads. That is free: `std::fs::File` is already `Sync` and
`read_at(&self, …)` is positioned, so `pread` on a shared fd is parallel by
construction — which is where NVMe queue depth > 1 comes from.

### Exclusivity is ownership, and the mailbox is the only door

There is no lock on a type's cache because there is no *second reference* to it.
The `HashMap` is a local binding inside the actor's loop — not a `static`, not
behind a handle anyone else holds — so the borrow checker, not a runtime check,
is what guarantees single access.

The consequence has to be said out loud: **every read and every write goes
through the mailbox.** That is the mechanism, not a concession to it.

Which raises the only question that matters: is the mailbox fast enough?

**For writes, by two to three orders of magnitude.** The actor's per-message
work must stay at hashmap-and-hand-off scale — call it ~1 µs, so ~10⁶
messages/s — and one message is a whole `apply` batch, not one id. What that
message feeds is a journal append with an `fsync`, at 10²–10³ µs. The mailbox
cannot be the write path's bottleneck; the barrier is, as it always was.

**For reads it can bite, in exactly one shape:** a cache-hot loop resolving ids
one at a time. RFC 0054's `all()` is the example — a segment yields N anchors
and each is resolved separately, so N round-trips where a lock would be N
pointer chases.

The answer is that the actor boundary should be crossed at the granularity the
engine already reads at:

```rust
fn get_many(&self, hash: u64, ids: &[Id]) -> impl Future<Output = …> + MaybeSend;
```

One message, N lookups. This is not a workaround bolted on for the actors — a
segment read *already* produces a batch of anchors, and the current one-at-a-time
loop is an artefact of there having been nothing to amortize.

A second saving the actor model enables and a lock does not: because the same
actor serves a caller's write and its subsequent reads, **mailbox order alone
gives read-your-writes**. So `apply` can return once the journal has it, without
awaiting the cache-commit reply — the commit message is already queued ahead of
anything that could observe it.

### The hot-type ceiling, and why it needs no mechanism

One type is one core for the cheap work, and nothing here changes that. The
response is the one thing that was already the design: **keep the expensive work
out of the actor.** zstd, page assembly and positioned reads go to
`spawn_blocking`; the actor does hashmap work and hands off. That is what holds
the per-message cost at ~1 µs, and it is where all the headroom comes from.

At that cost one core carries ~10⁶ messages/s, against a write path capped by
`fsync` two to three orders of magnitude lower and a read path amortized by
`get_many`. The ceiling is theoretical, and **no sharding mechanism is proposed
for it** — if it ever binds, the answer starts with a measurement, not with a
preemptive partition.

(An earlier draft floated sharding a type's record cache by `id % k`. Dropped:
the runtime already distributes tasks across workers, so there is nothing manual
to arrange — and the shard would have broken batch visibility anyway, since a
batch touches the record cache *and* the chains, letting a reader see the chain
updated while the anchor is not yet: `Error::RecordMissing`.)

### Where the blocking work goes

`spawn_blocking` is tokio's and therefore native's, so it goes behind the
platform seam like every other target difference — and on wasm it is a direct
call, because there is one thread and nothing to hand off to:

```rust
// wavedb-platform::task
pub async fn run_blocking<F, R>(f: F) -> R where F: FnOnce() -> R + Portable, R: Portable;
// native: tokio::task::spawn_blocking(f).await
// wasm:   f()
```

Engine code writes `run_blocking(…).await` and never cfgs. `BlockFile` becomes
`Shared<BlockFile>` so a closure can own a handle — free, since `File` is
already `Sync` and `read_at(&self, …)` is positioned.

**The rule that decides placement: an actor never awaits anything slow inside
its loop.** Awaiting a page read there would serialize every message behind one
disk seek, which is worse than the lock it replaced. The actor reads its own
state — hashmap work — and for anything slow it *dispatches* and takes the next
message.

| work | who issues it | shape |
|---|---|---|
| page read on a cache miss | type actor **dispatches**, does not await | spawns a task; the result returns *as a message* |
| plan a slot: read pages, merge, compress | type actor gathers the touched bytes, hands off | one `run_blocking` per `SlotPlan` |
| window write + `data.bin` fsync | writer actor | `run_blocking` |
| journal append + fsync | journal actor | `run_blocking` |

Planning splits cleanly because only its middle step needs the cache: reading
and decompressing the current page needs the directory and dictionary, merging
needs the *touched ids' bytes only*, and re-compressing needs neither. So the
actor gathers those bytes (cheap, and bounded by what changed since the last
settle) and the blocking task does the rest.

**Dispatching a read needs one piece of state, and it earns its keep twice.**
The cache entry becomes:

```rust
enum Slot {
    Present(Vec<u8>),
    Loading(Vec<oneshot::Sender<Option<Vec<u8>>>>),
}
```

- **N concurrent misses on one id cost one IOp.** Without this the actor
  dispatches a read per miss and reads the same block N times — the real
  duplicate-IO case. The first miss installs `Loading` and dispatches; every
  later one parks its `oneshot` in the vector and dispatches nothing.
- **A write racing the in-flight read is answered correctly, and sooner.** The
  write replaces `Loading(waiters)` with `Present(new)` and replies to the
  parked waiters immediately; the read's result is dropped when it arrives.
  Without the slot this would still be *correct* — revalidating on
  `ReadComplete` — but the waiters would have paid for a seek whose answer was
  already stale.

Parking the waiters *inside* the entry is what keeps the actor from blocking
while avoiding the obvious alternative: re-enqueuing the request behind the read.
That spins through the mailbox, burning a pop-check-push per turn for as long as
the disk takes (~100 turns at a 100 µs seek and 10⁶ msg/s), and on a bounded
queue it leaves `ReadComplete` competing for a slot against the very waiters it
would release. What is suspended here instead is the *caller's* future, which
costs nothing.

Two small consequences: `Loading` holds no bytes, so cache-size accounting and
eviction must skip it, and an entry that resolves to absent replies `None` to
its waiters and removes itself rather than caching a negative.

With RFC 0057's arena in front, most of these reads never reach `run_blocking`
at all.

**zstd never gets a dispatch of its own.** It always rides the task that already
exists for the IO it accompanies: read-and-decompress on a cache miss,
read-merge-and-compress when planning a slot. Splitting it out would buy a
second task dispatch for work that is already sitting next to a ~10–100 µs seek.

This matters more than it looks, because `spawn_blocking` is designed for
**blocking IO, not CPU** — its pool defaults to hundreds of threads on the
assumption they will mostly sit in syscalls. Keeping zstd attached to the read
it belongs with keeps the pool's workload IO-shaped, which is the shape it is
sized for.

Two consequences to carry:

- **Size the pool deliberately.** Once RFC 0057's arena turns most of those reads
  into RAM hits, the same tasks become nearly pure CPU — and hundreds of threads
  compressing on a handful of cores is context switching, not throughput.
  `max_blocking_threads` wants to be set near the core count, not left at the
  default.
- **Index churn pays no zstd at all.** `BPTREE_NODE_STORAGE` is
  `without_compression` already ("node pages are rewritten on every index
  mutation, so zstd there is CPU for nothing"), and `Lane::Tree` inherits that.
  So the compression cost tracks record writes, not tree traffic.

On wasm `run_blocking` is a direct call, so compression runs inline on the one
thread. That is precisely where the macrotask yield belongs — one page, yield,
next page — and it is the difference between a checkpoint that stutters the UI
and one that freezes it.

**Group commit falls out here.** The journal actor *does* await its own fsync —
correctly, since it is the serialization point. While that barrier is in flight,
incoming appends queue in its mailbox, and the next turn writes them all and
syncs once. The batching is the queue's doing; nothing has to arrange it.

### Idle actors are free, which is what makes one-per-type viable

A mailbox with nothing in it is a registered waker and no more; the runtime
parks the worker rather than polling. So the cost of an actor per type is its
channel — on the order of a kilobyte or two empty, ~1–2 MiB at a thousand types
— and **zero CPU** for every type the workload is not currently touching. A
schema's long tail of cold types costs nothing to model this way.

The numbers in this section are engineering estimates, not measurements. The
mailbox round-trip in this specific setup should be measured before any of them
is treated as settled.

### Sharding is a hint, not a partition

Types are assigned to workers by `STRUCT_HASH`, but with work-stealing that is a
starting placement rather than a binding. Wasm runs one worker and every actor
as a task on it.

**Test at one worker and at four, both natively.** The single-worker run
exercises the wasm topology on a target with a debugger, and catches the
"I assumed two types run in parallel" class where it is diagnosable.

### The invariants, made explicit

```rust
struct TypeActor {
    pending:  Vec<Id>,   // awaiting a plan
    settling: Vec<Id>,   // plan in flight at the writer
}
// eviction requires BOTH empty
```

The actor is the only reader and writer of both fields and processes one message
at a time, so this stops being discipline and becomes structure. Batch atomicity
comes free for the same reason: a batch is one message to one actor.

Two small API changes fall out: `Write::Remove(id)` and `Write::Expect(id, …)`
carry no type today, so `owner_of`/`read_any` probe every slot — under actors
that is a broadcast to N mailboxes. The caller always knows the type, so both
variants should carry the `struct_hash`.

## What it costs

**The state has to leave the statics.** This is the bulk of the work and it is
not a wrapping exercise: once the locks come off, the values cannot live in a
`static` at all, because a `static` demands `Sync`. What stays static is the
immutable configuration — the `struct_hash`, the compression flag. Note this
does *not* relax the one-`PageStore`-per-process rule (`EngineBusy`); it changes
who owns the state, not how many engines exist.

**Wasm gains something different from native.** There, one thread means the
actors buy *interruptibility*, not parallelism — and only if the settle grows
real yield points. A synchronous `fn` blocks the one thread however many
mailboxes surround it. And the yield must be a **macrotask**
(`platform::time::sleep`, i.e. `setTimeout`): microtasks run to exhaustion
before the browser paints, so awaiting a resolved promise does not give the UI
its frame back.

**`CLAUDE.md`'s hard-rules section must be amended when this lands** — the
non-`Send` stance is stated there as an architectural invariant, and this
reverses it.

## Alternatives

- **Keep the engine `!Send`, actors pinned to dedicated threads via
  `spawn_detached`.** Real parallelism without touching a line of existing
  engine code, and `tokio::task::spawn_local` carries no `Send` bound, so it was
  available. Rejected by decision: it forfeits load balancing permanently (an
  actor cannot migrate without its state), and needs a second mechanism — the
  stateless pool — to work around exactly that. The `Send` migration is a
  one-time cost against a standing limitation.
- **A multi-thread runtime with locks instead of actors.** Keeps the statics and
  the `Sync` bound, and leaves both silent invariants above as discipline rather
  than structure — which is how they came to be undocumented in the first place.
- **One state actor for all types.** Argued for earlier in this design on the
  grounds that batch application must stay atomic across types — **that premise
  is false**: a batch is confined to one type's family. Recorded because it was
  the reason per-type actors were nearly dropped.
- **A journal per type.** N fsyncs where one would do, against the engine's
  scarcest resource, and it forfeits group commit.

## Open questions

- **Worker count default.** `available_parallelism()` is the obvious answer and
  probably the wrong one for a library sharing a process with an application.
  Wants a declared default with an override.
- **Whether the lock type needs a cfg'd alias too.** The storage crate's
  `Mutex`/`RwLock` compile for wasm and simply never contend there; whether that
  is true of the specific crate in use, and whether the code size justifies a
  `RefCell` variant, wants checking rather than assuming.
- **Who owns the arena** (RFC 0057). Naturally the writer's, but its whole value
  is serving the type actors' reads — so either it is `Shared` and read-mostly
  (and then `Sync` reappears there), or a read goes through the writer and the
  round-trip eats the saving.
- **Ordering across actors.** The journal fixes a total order for replay, but
  two actors may commit to their caches in the opposite order. Nothing observed
  today depends on cross-type ordering; that should be *stated* as an invariant
  rather than left as an accident, since it is exactly what single-threadedness
  used to hide.
