//! WaveDB in the e-commerce workload — **one tenant per user**.
//!
//! This is the first workload here with more than one tenant, and the tenant is
//! not a column: it is 48 bits of the `Id`, so each user's data is a disjoint
//! region of one key space and `User::get(&db)` needs no key at all — the
//! identity is in the handle. The other four carry a `user_id` column and an
//! index on it, and that difference is the point of the profile row.

use std::path::Path;

use futures::TryStreamExt as _;
use futures::executor::block_on;
use wavedb_core::{LocalHandle, U48};
use wavedb_storage::{PageStore, StoreOptions};

use super::ShopCfg;
use crate::footprint::{Footprint, Point};
use crate::metrics::{self, Phase};
use crate::schema::Rng;
use crate::shop::{
    PAGE, Product, ProductLists, Shopping, ShoppingLists, User, logical_bytes,
    product_count, product_row, shopping_count, shopping_row, user_row,
};
use crate::systems::{Durability, SystemReport};

pub fn run(cfg: &ShopCfg, d: Durability) -> Result<SystemReport, String> {
    let dir = cfg.work_dir.join(match d {
        Durability::Durable => "shop-wavedb-durable",
        Durability::Relaxed => "shop-wavedb-relaxed",
    });
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir: {e}"))?;

    preload(cfg, &dir)?;

    // Reopened: the per-type cache is a *write* cache, so everything the
    // preload just wrote would otherwise still be warm and the read phases
    // would measure RAM. Every other adapter restarts its server here for the
    // same reason. The reopen is also where the measured durability is chosen
    // — the fill's window never survives it.
    let store = open_measured(&dir, d)?;
    let mut phases = vec![
        signup_phase(cfg, &store),
        checkout_phase(cfg, &store),
        profile_phase(cfg, &store),
        page_phase(cfg, &store),
        detail_phase(cfg, &store),
    ];
    phases.retain(|p| p.dist.count > 0);

    let footprints = vec![
        (Point::Hot, measure(&dir)?),
        (Point::Settled, quiesce(&store, &dir)?),
    ];
    drop(store);

    Ok(SystemReport {
        system: "wavedb",
        bracket: "embedded",
        workload: "shop",
        durability: d,
        version: env!("CARGO_PKG_VERSION").into(),
        settings: vec![
            ("tenancy".into(), "one tenant per user".into()),
            (
                "list".into(),
                format!("Shopping by bought_at, page = {PAGE}"),
            ),
            ("transaction".into(), "none: one op is one batch".into()),
            // The measured window, named so a reader can discount the row
            // against the competitors' own relaxed knobs (RFC 0061).
            (
                "relax_window".into(),
                match d {
                    Durability::Durable => {
                        "0 — one barrier per batch".to_string()
                    }
                    Durability::Relaxed => format!("{RELAXED_WINDOW:?}"),
                },
            ),
            // Recorded separately: the untimed fill runs its own window
            // whichever row this is, so it is never what the row reports.
            (
                "preload".into(),
                format!("relax_window {PRELOAD_WINDOW:?}, untimed"),
            ),
        ],
        compression: "per-type zstd dictionaries",
        retains_history: true,
        phases,
        footprints,
        live_records: cfg.live_records(),
        logical_bytes: logical_bytes(
            cfg.users,
            cfg.seed,
            cfg.orders_max,
            cfg.items_max,
        ),
        notes: vec![
            "A checkout is one order plus its line items, and WaveDB has no \
             multi-record transaction: it costs one batch — one barrier — per \
             record, where the other four commit the whole checkout once."
                .into(),
        ],
        seed_path: None,
        materialise_ms: 0,
    })
}

/// Fill: every user is a tenant, holding an order collection, each order
/// holding a line-item collection. Not timed.
fn preload(cfg: &ShopCfg, dir: &Path) -> Result<(), String> {
    let store = open_relaxed(dir)?;
    for u in 0..cfg.users {
        block_on(create_user(&store, cfg, u))?;
        for s in 0..shopping_count(u, cfg.seed, cfg.orders_max) {
            block_on(create_order(&store, cfg, u, s))?;
        }
        // A bare `PageStore` has no background maintenance — a node gets that
        // from `quick-node`'s loop — so **two** things grow unbounded here,
        // and an earlier version of this bounded only one: the write cache was
        // evicted while the journal grew for the whole fill. At ~43 records a
        // user this is millions of records against a 500 MB cage shared with
        // the servers. The fill is untimed, so the cost lands nowhere
        // measured; a node pays it on its own schedule.
        //
        // Triggered by journal **bytes**, not by a user count, for the reason
        // `quick-node`'s own policy is: per-operation log size depends on the
        // data. In the micro adapter a 5 000-op trigger never fired once while
        // 649 MB accumulated.
        if store.journal_len() > CHECKPOINT_AFTER_BYTES {
            store
                .commit_journal()
                .map_err(|e| format!("checkpoint: {e}"))?;
            store.evict_settled(FILL_CACHE_BYTES);
        }
    }
    store.drain().map_err(|e| format!("drain: {e}"))?;
    store
        .commit_journal()
        .map_err(|e| format!("checkpoint: {e}"))
}

async fn create_user(
    store: &PageStore,
    cfg: &ShopCfg,
    u: u64,
) -> Result<(), String> {
    let db = tenant(store, u);
    let shoppings = Shopping::create_pivot(&db)
        .await
        .map_err(|e| format!("create shopping pivot: {e}"))?;
    let r = user_row(u, cfg.seed);
    User {
        name: r.name,
        address: r.address,
        city: r.city,
        email: r.email,
        shoppings,
    }
    .save(&db)
    .await
    .map_err(|e| format!("save user: {e}"))
    .map(|_| ())
}

/// One whole order: its own line-item collection, the order, then the items.
/// This is the checkout, and it is `2 + items` batches.
async fn create_order(
    store: &PageStore,
    cfg: &ShopCfg,
    u: u64,
    s: u64,
) -> Result<(), String> {
    let db = tenant(store, u);
    let user = User::get(&db)
        .await
        .map_err(|e| format!("get user: {e}"))?
        .ok_or("checkout: user is missing")?;
    let items = Product::create_pivot(&db)
        .await
        .map_err(|e| format!("create product pivot: {e}"))?;
    let r = shopping_row(u, s, cfg.seed);
    Shopping::collection(user.shoppings)
        .insert(
            &db,
            &Shopping {
                bought_at: r.bought_at,
                discount_cents: r.discount_cents,
                transport_cents: r.transport_cents,
                items,
            },
        )
        .await
        .map_err(|e| format!("insert shopping: {e}"))?;
    let products = Product::collection(items);
    for p in 0..product_count(u, s, cfg.seed, cfg.items_max) {
        let pr = product_row(u, s, p, cfg.seed);
        products
            .insert(
                &db,
                &Product {
                    name: pr.name,
                    quantity: pr.quantity,
                    unit_cents: pr.unit_cents,
                },
            )
            .await
            .map_err(|e| format!("insert product: {e}"))?;
    }
    Ok(())
}

fn signup_phase(cfg: &ShopCfg, store: &PageStore) -> Phase {
    metrics::phase(
        "signup",
        |lat| {
            for i in 0..cfg.signups {
                let u = cfg.users + i;
                lat.time(|| {
                    block_on(create_user(store, cfg, u)).expect("signup");
                });
            }
        },
        cfg.signups as usize,
    )
}

fn checkout_phase(cfg: &ShopCfg, store: &PageStore) -> Phase {
    let mut rng = Rng::new(cfg.seed ^ 0xC0FF_EE00_C0FF_EE00);
    metrics::phase(
        "checkout",
        |lat| {
            for i in 0..cfg.checkouts {
                let u = rng.below(cfg.users.max(1));
                // Past the preloaded orders, so a checkout always appends.
                let s = shopping_count(u, cfg.seed, cfg.orders_max) + i;
                lat.time(|| {
                    block_on(create_order(store, cfg, u, s)).expect("checkout");
                });
            }
        },
        cfg.checkouts as usize,
    )
}

fn profile_phase(cfg: &ShopCfg, store: &PageStore) -> Phase {
    let mut rng = Rng::new(cfg.seed ^ 0x0000_0001);
    metrics::phase(
        "profile",
        |lat| {
            for _ in 0..cfg.profile_reads {
                let u = rng.below(cfg.users.max(1));
                let db = tenant(store, u);
                let got = lat.time(|| block_on(User::get(&db)).expect("get"));
                assert!(got.is_some(), "profile: user {u} is missing");
            }
        },
        cfg.profile_reads as usize,
    )
}

/// The order-history page: the user, then one page of ten orders straight off
/// the declared list — one descent to the page boundary, not a walk.
fn page_phase(cfg: &ShopCfg, store: &PageStore) -> Phase {
    let mut rng = Rng::new(cfg.seed ^ 0x0000_0002);
    metrics::phase(
        "order_page",
        |lat| {
            for _ in 0..cfg.page_reads {
                let u = rng.below(cfg.users.max(1));
                let page = rng.below(2) as usize;
                let db = tenant(store, u);
                let n = lat.time(|| {
                    block_on(async {
                        let user = User::get(&db).await?.expect("user");
                        let orders: Vec<Shopping> =
                            Shopping::collection(user.shoppings)
                                .listed_by_bought_at_at_page(&db, page, PAGE)
                                .try_collect()
                                .await?;
                        Ok::<usize, wavedb_core::Error>(orders.len())
                    })
                    .expect("order page")
                });
                assert!(n > 0 || page > 0, "order_page: empty first page");
            }
        },
        cfg.page_reads as usize,
    )
}

/// One order's line items: resolve the order from its page, then read its own
/// collection's first page.
fn detail_phase(cfg: &ShopCfg, store: &PageStore) -> Phase {
    let mut rng = Rng::new(cfg.seed ^ 0x0000_0003);
    metrics::phase(
        "order_detail",
        |lat| {
            for _ in 0..cfg.detail_reads {
                let u = rng.below(cfg.users.max(1));
                let db = tenant(store, u);
                let n = lat.time(|| {
                    block_on(async {
                        let user = User::get(&db).await?.expect("user");
                        let orders: Vec<Shopping> =
                            Shopping::collection(user.shoppings)
                                .listed_by_bought_at_at_page(&db, 0, PAGE)
                                .try_collect()
                                .await?;
                        let order = orders.first().expect("order");
                        let items: Vec<Product> =
                            Product::collection(order.items)
                                .listed_by_name_at_page(&db, 0, PAGE)
                                .try_collect()
                                .await?;
                        Ok::<usize, wavedb_core::Error>(items.len())
                    })
                    .expect("order detail")
                });
                assert!(n > 0, "order_detail: order with no items");
            }
        },
        cfg.detail_reads as usize,
    )
}

fn tenant(store: &PageStore, u: u64) -> LocalHandle<'_, PageStore> {
    LocalHandle::new(store, U48::from(u32::try_from(u + 1).unwrap_or(1)))
}

/// The **measured** store, opened at the row's own durability: the default
/// (one barrier per batch) or [`RELAXED_WINDOW`](crate::RELAXED_WINDOW).
///
/// Each type contributes a different number of `StructStorage` slots — a
/// Unique one, a NonUnique with a declared list six — so they are collected
/// rather than concatenated as arrays.
fn open_measured(dir: &Path, d: Durability) -> Result<PageStore, String> {
    open_with(
        dir,
        StoreOptions {
            relax_window: match d {
                Durability::Durable => std::time::Duration::ZERO,
                Durability::Relaxed => RELAXED_WINDOW,
            },
        },
    )
}

/// The **fill**'s store: always a durability window, whichever row is running,
/// because a preload is not a measurement (RFC 0061).
///
/// This is the bulk-load-then-serve pattern the window exists for, and it is
/// what makes a large preload affordable at all: one op is one batch is one
/// barrier, so filling millions of records durably is millions of `fsync`s.
/// The measured phases reopen through [`open_measured`], so the fill's window
/// is never what a row reports. The other four systems do the same thing by
/// different names: their bulk loaders are not running the per-statement
/// commit path either.
fn open_relaxed(dir: &Path) -> Result<PageStore, String> {
    open_with(
        dir,
        StoreOptions {
            relax_window: PRELOAD_WINDOW,
        },
    )
}

use crate::{FILL_WINDOW as PRELOAD_WINDOW, RELAXED_WINDOW};

/// Users between settle rounds during the fill — ~2 000 records a round at
/// the default order/item spread, which keeps the write cache bounded well
/// inside the cage without making the fill a checkpoint benchmark.
/// Journal bytes that trigger a checkpoint during the fill —
/// `quick-node`'s own default (`Maintenance::checkpoint_after_bytes`).
const CHECKPOINT_AFTER_BYTES: u64 = 64 << 20;

/// What the fill's write cache is evicted **down to** — not to zero.
///
/// Evicting to zero costs far more than it saves: it drops the hot B+tree
/// interior nodes with everything else, so the next insert descends through
/// the page store instead of RAM, and a fill that took 11 s at 4 000 users
/// took 164 s at 8 000. Cold reads are this engine's known weak path; a fill
/// is the last place to force them.
///
/// 192 MB: comfortably inside the 500 MB cage the fill also runs in (see
/// [`crate::cage`] — there is one configuration and the fill is not an
/// exception to it), and still a bound, since an unbounded write cache would
/// simply move the failure to whatever ceiling is in force.
const FILL_CACHE_BYTES: usize = 192 << 20;

fn open_with(dir: &Path, options: StoreOptions) -> Result<PageStore, String> {
    let mut entries = Vec::new();
    entries.extend_from_slice(&User::storage_entries());
    entries.extend_from_slice(&Shopping::storage_entries());
    entries.extend_from_slice(&Product::storage_entries());
    PageStore::open_with(dir, &entries, options)
        .map_err(|e| format!("open: {e}"))
}

/// Checkpoint until the footprint stops moving — journal retirement is
/// generational (RFC 0047), so one round proves nothing.
fn quiesce(store: &PageStore, dir: &Path) -> Result<Footprint, String> {
    const MAX_ROUNDS: usize = 6;
    let mut last = u64::MAX;
    for _ in 0..MAX_ROUNDS {
        store.drain().map_err(|e| format!("drain: {e}"))?;
        store
            .commit_journal()
            .map_err(|e| format!("checkpoint: {e}"))?;
        let now = measure(dir)?;
        if now.allocated_bytes == last && !store.has_pending() {
            return Ok(now);
        }
        last = now.allocated_bytes;
    }
    measure(dir)
}

fn measure(dir: &Path) -> Result<Footprint, String> {
    Footprint::split(dir, is_log).map_err(|e| format!("footprint: {e}"))
}

fn is_log(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with("journal_"))
}
