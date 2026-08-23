//! The MySQL shop adapter's five measured phases.

use mysql::prelude::Queryable as _;
use mysql::{Conn, Transaction, TxOpts, params};

use super::ShopCfg;
use super::mysql::{
    SEL_FIRST, SEL_ITEMS, SEL_PAGE, SEL_USER, insert_item, insert_order,
    insert_user, next_id, sql,
};
use crate::metrics::{self, Phase, Writer};
use crate::schema::Rng;
use crate::shop::{PAGE, product_count, shopping_count};

pub(super) fn signup_phase(
    cfg: &ShopCfg,
    conn: &mut Conn,
    pid: u32,
) -> Result<Phase, String> {
    Ok(metrics::phase_of(
        "signup",
        Writer::Pid(pid),
        |lat| {
            for i in 0..cfg.signups {
                let u = cfg.users + i;
                lat.time(|| insert_user(conn, u, cfg).expect("signup"));
            }
        },
        cfg.signups as usize,
    ))
}

pub(super) fn checkout_phase(
    cfg: &ShopCfg,
    conn: &mut Conn,
    pid: u32,
) -> Result<Phase, String> {
    let mut rng = Rng::new(cfg.seed ^ 0xC0FF_EE00_C0FF_EE00);
    let base = next_id(conn, "shopping")?;
    let mut item_id = next_id(conn, "product")?;
    Ok(metrics::phase_of(
        "checkout",
        Writer::Pid(pid),
        |lat| {
            for i in 0..cfg.checkouts {
                let u = rng.below(cfg.users.max(1));
                let s = shopping_count(u, cfg.seed, cfg.orders_max) + i;
                let order = base + i;
                let items = product_count(u, s, cfg.seed, cfg.items_max);
                lat.time(|| {
                    let mut tx: Transaction<'_> = conn
                        .start_transaction(TxOpts::default())
                        .expect("begin");
                    insert_order(&mut tx, order, u, s, cfg).expect("order");
                    for p in 0..items {
                        item_id += 1;
                        insert_item(&mut tx, item_id, order, u, s, p, cfg)
                            .expect("item");
                    }
                    tx.commit().expect("commit");
                });
            }
        },
        cfg.checkouts as usize,
    ))
}

pub(super) fn profile_phase(
    cfg: &ShopCfg,
    conn: &mut Conn,
    pid: u32,
) -> Result<Phase, String> {
    let stmt = conn.prep(SEL_USER).map_err(sql)?;
    let mut rng = Rng::new(cfg.seed ^ 0x0000_0001);
    Ok(metrics::phase_of(
        "profile",
        Writer::Pid(pid),
        |lat| {
            for _ in 0..cfg.profile_reads {
                let u = rng.below(cfg.users.max(1));
                let name: Option<String> = lat.time(|| {
                    conn.exec_first(&stmt, params! { "id" => u })
                        .expect("profile")
                });
                assert!(name.is_some(), "profile: missing user");
            }
        },
        cfg.profile_reads as usize,
    ))
}

pub(super) fn page_phase(
    cfg: &ShopCfg,
    conn: &mut Conn,
    pid: u32,
) -> Result<Phase, String> {
    let user = conn.prep(SEL_USER).map_err(sql)?;
    let page = conn.prep(SEL_PAGE).map_err(sql)?;
    let mut rng = Rng::new(cfg.seed ^ 0x0000_0002);
    Ok(metrics::phase_of(
        "order_page",
        Writer::Pid(pid),
        |lat| {
            for _ in 0..cfg.page_reads {
                let u = rng.below(cfg.users.max(1));
                let p = rng.below(2);
                let n = lat.time(|| {
                    let _: Option<String> = conn
                        .exec_first(&user, params! { "id" => u })
                        .expect("user");
                    let ids: Vec<u64> = conn
                        .exec(
                            &page,
                            params! {
                                "id" => u, "lim" => PAGE as u64,
                                "off" => p * PAGE as u64,
                            },
                        )
                        .expect("page");
                    ids.len()
                });
                assert!(n > 0 || p > 0, "order_page: empty first page");
            }
        },
        cfg.page_reads as usize,
    ))
}

pub(super) fn detail_phase(
    cfg: &ShopCfg,
    conn: &mut Conn,
    pid: u32,
) -> Result<Phase, String> {
    let user = conn.prep(SEL_USER).map_err(sql)?;
    let first = conn.prep(SEL_FIRST).map_err(sql)?;
    let items = conn.prep(SEL_ITEMS).map_err(sql)?;
    let mut rng = Rng::new(cfg.seed ^ 0x0000_0003);
    Ok(metrics::phase_of(
        "order_detail",
        Writer::Pid(pid),
        |lat| {
            for _ in 0..cfg.detail_reads {
                let u = rng.below(cfg.users.max(1));
                let n = lat.time(|| {
                    let _: Option<String> = conn
                        .exec_first(&user, params! { "id" => u })
                        .expect("user");
                    let order: u64 = conn
                        .exec_first(&first, params! { "id" => u })
                        .expect("order")
                        .expect("order exists");
                    let names: Vec<String> = conn
                        .exec(
                            &items,
                            params! { "id" => order, "lim" => PAGE as u64 },
                        )
                        .expect("items");
                    names.len()
                });
                assert!(n > 0, "order_detail: order with no items");
            }
        },
        cfg.detail_reads as usize,
    ))
}

