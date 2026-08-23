//! The SQLite shop adapter's five measured phases.

use rusqlite::{Connection, params};

use super::ShopCfg;
use super::sqlite::{insert_item, insert_order, insert_user, next_id, sql};
use crate::metrics::{self, Phase};
use crate::schema::Rng;
use crate::shop::{PAGE, product_count, shopping_count};

pub(super) fn signup_phase(cfg: &ShopCfg, conn: &mut Connection) -> Result<Phase, String> {
    Ok(metrics::phase(
        "signup",
        |lat| {
            for i in 0..cfg.signups {
                let u = cfg.users + i;
                lat.time(|| insert_user(conn, u, cfg).expect("signup"));
            }
        },
        cfg.signups as usize,
    ))
}

/// One order and its line items, in **one transaction** — the realistic shape,
/// and one commit barrier for the whole checkout.
pub(super) fn checkout_phase(
    cfg: &ShopCfg,
    conn: &mut Connection,
) -> Result<Phase, String> {
    let mut rng = Rng::new(cfg.seed ^ 0xC0FF_EE00_C0FF_EE00);
    let base: u64 = next_id(conn, "shopping")?;
    let mut item_id: u64 = next_id(conn, "product")?;
    Ok(metrics::phase(
        "checkout",
        |lat| {
            for i in 0..cfg.checkouts {
                let u = rng.below(cfg.users.max(1));
                let s = shopping_count(u, cfg.seed, cfg.orders_max) + i;
                let order = base + i;
                let items = product_count(u, s, cfg.seed, cfg.items_max);
                lat.time(|| {
                    let tx = conn.transaction().expect("begin");
                    insert_order(&tx, order, u, s, cfg).expect("order");
                    for p in 0..items {
                        item_id += 1;
                        insert_item(&tx, item_id, order, u, s, p, cfg)
                            .expect("item");
                    }
                    tx.commit().expect("commit");
                });
            }
        },
        cfg.checkouts as usize,
    ))
}

pub(super) fn profile_phase(cfg: &ShopCfg, conn: &Connection) -> Result<Phase, String> {
    let mut stmt = conn
        .prepare("SELECT name, address, city, email FROM users WHERE id = ?1")
        .map_err(sql)?;
    let mut rng = Rng::new(cfg.seed ^ 0x0000_0001);
    Ok(metrics::phase(
        "profile",
        |lat| {
            for _ in 0..cfg.profile_reads {
                let u = rng.below(cfg.users.max(1)) as i64;
                let name: String = lat.time(|| {
                    stmt.query_row(params![u], |r| r.get(0)).expect("profile")
                });
                assert!(!name.is_empty(), "profile: empty user");
            }
        },
        cfg.profile_reads as usize,
    ))
}

pub(super) fn page_phase(cfg: &ShopCfg, conn: &Connection) -> Result<Phase, String> {
    let mut user = conn
        .prepare("SELECT name FROM users WHERE id = ?1")
        .map_err(sql)?;
    let mut page = conn
        .prepare(
            "SELECT id, bought_at, discount_cents, transport_cents \
             FROM shopping WHERE user_id = ?1 \
             ORDER BY bought_at LIMIT ?2 OFFSET ?3",
        )
        .map_err(sql)?;
    let mut rng = Rng::new(cfg.seed ^ 0x0000_0002);
    Ok(metrics::phase(
        "order_page",
        |lat| {
            for _ in 0..cfg.page_reads {
                let u = rng.below(cfg.users.max(1)) as i64;
                let p = rng.below(2) as i64;
                let n = lat.time(|| {
                    let _: String =
                        user.query_row(params![u], |r| r.get(0)).expect("user");
                    page.query_map(params![u, PAGE as i64, p * PAGE as i64], |r| {
                        r.get::<_, i64>(0)
                    })
                    .expect("page")
                    .count()
                });
                assert!(n > 0 || p > 0, "order_page: empty first page");
            }
        },
        cfg.page_reads as usize,
    ))
}

pub(super) fn detail_phase(cfg: &ShopCfg, conn: &Connection) -> Result<Phase, String> {
    let mut user = conn
        .prepare("SELECT name FROM users WHERE id = ?1")
        .map_err(sql)?;
    let mut first = conn
        .prepare(
            "SELECT id FROM shopping WHERE user_id = ?1 \
             ORDER BY bought_at LIMIT 1",
        )
        .map_err(sql)?;
    let mut items = conn
        .prepare(
            "SELECT name, quantity, unit_cents FROM product \
             WHERE shopping_id = ?1 ORDER BY name LIMIT ?2",
        )
        .map_err(sql)?;
    let mut rng = Rng::new(cfg.seed ^ 0x0000_0003);
    Ok(metrics::phase(
        "order_detail",
        |lat| {
            for _ in 0..cfg.detail_reads {
                let u = rng.below(cfg.users.max(1)) as i64;
                let n = lat.time(|| {
                    let _: String =
                        user.query_row(params![u], |r| r.get(0)).expect("user");
                    let order: i64 = first
                        .query_row(params![u], |r| r.get(0))
                        .expect("order");
                    items
                        .query_map(params![order, PAGE as i64], |r| {
                            r.get::<_, String>(0)
                        })
                        .expect("items")
                        .count()
                });
                assert!(n > 0, "order_detail: order with no items");
            }
        },
        cfg.detail_reads as usize,
    ))
}

