//! The MongoDB shop adapter's five measured phases.
//!
//! Split from the lifecycle beside it so each file is one layer: that one
//! starts and stops a server and describes the documents, this one is only
//! what gets timed.

use mongodb::bson::{Document, doc};
use mongodb::sync::{ClientSession, Database};

use super::ShopCfg;
use super::mongodb::{item_doc, items, next_id, order_doc, orders, profiles, user_doc};
use crate::metrics::{self, Phase, Writer};
use crate::schema::Rng;
use crate::shop::{PAGE, product_count, shopping_count};

pub(super) fn signup_phase(cfg: &ShopCfg, db: &Database, pid: u32) -> Phase {
    metrics::phase_of(
        "signup",
        Writer::Pid(pid),
        |lat| {
            for i in 0..cfg.signups {
                let d = user_doc(cfg.users + i, cfg);
                lat.time(|| {
                    profiles(db).insert_one(&d).run().expect("signup");
                });
            }
        },
        cfg.signups as usize,
    )
}

pub(super) fn checkout_phase(
    cfg: &ShopCfg,
    db: &Database,
    session: &mut ClientSession,
    pid: u32,
) -> Phase {
    let mut rng = Rng::new(cfg.seed ^ 0xC0FF_EE00_C0FF_EE00);
    let base = next_id(db, "shopping");
    let mut item_id = next_id(db, "product");
    metrics::phase_of(
        "checkout",
        Writer::Pid(pid),
        |lat| {
            for i in 0..cfg.checkouts {
                let u = rng.below(cfg.users.max(1));
                let s = shopping_count(u, cfg.seed, cfg.orders_max) + i;
                let order = base + i;
                let count = product_count(u, s, cfg.seed, cfg.items_max);
                let od = order_doc(order, u, s, cfg);
                let ids: Vec<Document> = (0..count)
                    .map(|p| {
                        item_id += 1;
                        item_doc(item_id, order, u, s, p, cfg)
                    })
                    .collect();
                lat.time(|| {
                    session.start_transaction().run().expect("begin");
                    orders(db)
                        .insert_one(&od)
                        .session(&mut *session)
                        .run()
                        .expect("order");
                    items(db)
                        .insert_many(&ids)
                        .session(&mut *session)
                        .run()
                        .expect("items");
                    session.commit_transaction().run().expect("commit");
                });
            }
        },
        cfg.checkouts as usize,
    )
}

pub(super) fn profile_phase(cfg: &ShopCfg, db: &Database, pid: u32) -> Phase {
    let mut rng = Rng::new(cfg.seed ^ 0x0000_0001);
    metrics::phase_of(
        "profile",
        Writer::Pid(pid),
        |lat| {
            for _ in 0..cfg.profile_reads {
                let u = rng.below(cfg.users.max(1)) as i64;
                let got = lat.time(|| {
                    profiles(db)
                        .find_one(doc! { "_id": u })
                        .run()
                        .expect("profile")
                });
                assert!(got.is_some(), "profile: missing user");
            }
        },
        cfg.profile_reads as usize,
    )
}

pub(super) fn page_phase(cfg: &ShopCfg, db: &Database, pid: u32) -> Phase {
    let mut rng = Rng::new(cfg.seed ^ 0x0000_0002);
    metrics::phase_of(
        "order_page",
        Writer::Pid(pid),
        |lat| {
            for _ in 0..cfg.page_reads {
                let u = rng.below(cfg.users.max(1)) as i64;
                let p = rng.below(2);
                let n = lat.time(|| {
                    profiles(db)
                        .find_one(doc! { "_id": u })
                        .run()
                        .expect("user");
                    orders(db)
                        .find(doc! { "user_id": u })
                        .sort(doc! { "bought_at": 1 })
                        .skip(p * PAGE as u64)
                        .limit(PAGE as i64)
                        .run()
                        .expect("page")
                        .count()
                });
                assert!(n > 0 || p > 0, "order_page: empty first page");
            }
        },
        cfg.page_reads as usize,
    )
}

pub(super) fn detail_phase(cfg: &ShopCfg, db: &Database, pid: u32) -> Phase {
    let mut rng = Rng::new(cfg.seed ^ 0x0000_0003);
    metrics::phase_of(
        "order_detail",
        Writer::Pid(pid),
        |lat| {
            for _ in 0..cfg.detail_reads {
                let u = rng.below(cfg.users.max(1)) as i64;
                let n = lat.time(|| {
                    profiles(db)
                        .find_one(doc! { "_id": u })
                        .run()
                        .expect("user");
                    let order = orders(db)
                        .find_one(doc! { "user_id": u })
                        .sort(doc! { "bought_at": 1 })
                        .run()
                        .expect("order")
                        .expect("order exists");
                    let id = order.get_i64("_id").unwrap_or_default();
                    items(db)
                        .find(doc! { "shopping_id": id })
                        .sort(doc! { "name": 1 })
                        .limit(PAGE as i64)
                        .run()
                        .expect("items")
                        .count()
                });
                assert!(n > 0, "order_detail: order with no items");
            }
        },
        cfg.detail_reads as usize,
    )
}

