//! The PostgreSQL shop adapter's five measured phases.

use postgres::Client;

use super::ShopCfg;
use super::postgres::{
    SEL_FIRST, SEL_ITEMS, SEL_PAGE, SEL_USER, insert_item, insert_order,
    insert_user, next_id, sql,
};
use crate::metrics::{self, Phase, Writer};
use crate::schema::Rng;
use crate::shop::{PAGE, product_count, shopping_count};

pub(super) fn signup_phase(
    cfg: &ShopCfg,
    client: &mut Client,
    pid: u32,
) -> Result<Phase, String> {
    Ok(metrics::phase_of(
        "signup",
        Writer::Pid(pid),
        |lat| {
            for i in 0..cfg.signups {
                let u = cfg.users + i;
                lat.time(|| insert_user(client, u, cfg).expect("signup"));
            }
        },
        cfg.signups as usize,
    ))
}

pub(super) fn checkout_phase(
    cfg: &ShopCfg,
    client: &mut Client,
    pid: u32,
) -> Result<Phase, String> {
    let mut rng = Rng::new(cfg.seed ^ 0xC0FF_EE00_C0FF_EE00);
    let base = next_id(client, "shopping")?;
    let mut item_id = next_id(client, "product")?;
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
                    let mut tx = client.transaction().expect("begin");
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
    client: &mut Client,
    pid: u32,
) -> Result<Phase, String> {
    let stmt = client.prepare(SEL_USER).map_err(sql)?;
    let mut rng = Rng::new(cfg.seed ^ 0x0000_0001);
    Ok(metrics::phase_of(
        "profile",
        Writer::Pid(pid),
        |lat| {
            for _ in 0..cfg.profile_reads {
                let u = rng.below(cfg.users.max(1)) as i64;
                let name: String = lat.time(|| {
                    client.query_one(&stmt, &[&u]).expect("profile").get(0)
                });
                assert!(!name.is_empty(), "profile: empty user");
            }
        },
        cfg.profile_reads as usize,
    ))
}

pub(super) fn page_phase(
    cfg: &ShopCfg,
    client: &mut Client,
    pid: u32,
) -> Result<Phase, String> {
    let user = client.prepare(SEL_USER).map_err(sql)?;
    let page = client.prepare(SEL_PAGE).map_err(sql)?;
    let mut rng = Rng::new(cfg.seed ^ 0x0000_0002);
    Ok(metrics::phase_of(
        "order_page",
        Writer::Pid(pid),
        |lat| {
            for _ in 0..cfg.page_reads {
                let u = rng.below(cfg.users.max(1)) as i64;
                let p = rng.below(2) as i64;
                let n = lat.time(|| {
                    let _: String =
                        client.query_one(&user, &[&u]).expect("user").get(0);
                    client
                        .query(&page, &[&u, &(PAGE as i64), &(p * PAGE as i64)])
                        .expect("page")
                        .len()
                });
                assert!(n > 0 || p > 0, "order_page: empty first page");
            }
        },
        cfg.page_reads as usize,
    ))
}

pub(super) fn detail_phase(
    cfg: &ShopCfg,
    client: &mut Client,
    pid: u32,
) -> Result<Phase, String> {
    let user = client.prepare(SEL_USER).map_err(sql)?;
    let first = client.prepare(SEL_FIRST).map_err(sql)?;
    let items = client.prepare(SEL_ITEMS).map_err(sql)?;
    let mut rng = Rng::new(cfg.seed ^ 0x0000_0003);
    Ok(metrics::phase_of(
        "order_detail",
        Writer::Pid(pid),
        |lat| {
            for _ in 0..cfg.detail_reads {
                let u = rng.below(cfg.users.max(1)) as i64;
                let n = lat.time(|| {
                    let _: String =
                        client.query_one(&user, &[&u]).expect("user").get(0);
                    let order: i64 =
                        client.query_one(&first, &[&u]).expect("order").get(0);
                    client
                        .query(&items, &[&order, &(PAGE as i64)])
                        .expect("items")
                        .len()
                });
                assert!(n > 0, "order_detail: order with no items");
            }
        },
        cfg.detail_reads as usize,
    ))
}

