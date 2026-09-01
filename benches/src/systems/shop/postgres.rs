//! PostgreSQL in the e-commerce workload.
//!
//! Same three tables as the SQLite row, same one-transaction checkout, over a
//! server this run started on a unix socket in its own scratch directory.

use std::path::Path;

use postgres::{Client, NoTls};

use super::ShopCfg;
use crate::footprint::{Footprint, Point};
use crate::shop::{
    logical_bytes, product_count, product_row, shopping_count, shopping_row,
    user_row,
};
use crate::systems::server::{self, Server};
use crate::systems::{Durability, SystemReport};

const DDL: &str = "
CREATE TABLE users (
  id      BIGINT PRIMARY KEY,
  name    TEXT NOT NULL,
  address TEXT NOT NULL,
  city    TEXT NOT NULL,
  email   TEXT NOT NULL
);
CREATE TABLE shopping (
  id              BIGINT PRIMARY KEY,
  user_id         BIGINT NOT NULL,
  bought_at       BIGINT NOT NULL,
  discount_cents  BIGINT NOT NULL,
  transport_cents BIGINT NOT NULL
);
CREATE INDEX idx_shopping_user ON shopping(user_id, bought_at);
CREATE TABLE product (
  id          BIGINT PRIMARY KEY,
  shopping_id BIGINT NOT NULL,
  name        TEXT NOT NULL,
  quantity    INTEGER NOT NULL,
  unit_cents  BIGINT NOT NULL
);
CREATE INDEX idx_product_shopping ON product(shopping_id, name);
";

const INS_USER: &str = "INSERT INTO users (id, name, address, city, email) \
                        VALUES ($1, $2, $3, $4, $5)";
const INS_ORDER: &str = "INSERT INTO shopping (id, user_id, bought_at, \
                         discount_cents, transport_cents) \
                         VALUES ($1, $2, $3, $4, $5)";
const INS_ITEM: &str = "INSERT INTO product (id, shopping_id, name, quantity, \
                        unit_cents) VALUES ($1, $2, $3, $4, $5)";
pub(super) const SEL_USER: &str = "SELECT name FROM users WHERE id = $1";
pub(super) const SEL_PAGE: &str = "SELECT id FROM shopping WHERE user_id = $1 \
                        ORDER BY bought_at LIMIT $2 OFFSET $3";
pub(super) const SEL_FIRST: &str = "SELECT id FROM shopping WHERE user_id = $1 \
                         ORDER BY bought_at LIMIT 1";
pub(super) const SEL_ITEMS: &str = "SELECT name FROM product WHERE shopping_id = $1 \
                         ORDER BY name LIMIT $2";

pub fn run(
    cfg: &ShopCfg,
    durability: Durability,
) -> Result<SystemReport, String> {
    let dir = cfg
        .work_dir
        .join(format!("shop-postgres-{}", durability.name()));
    let data = dir.join("data");
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir: {e}"))?;
    server::run(
        "initdb",
        &[
            "-D",
            &s(&data),
            "--no-locale",
            "--encoding=UTF8",
            "-U",
            "bench",
        ],
    )?;

    let sync = match durability {
        Durability::Durable => "on",
        Durability::Relaxed => "off",
    };
    let pg = start(&dir, "off")?;
    let mut client = connect(&dir)?;
    let version = client
        .query_one("SHOW server_version", &[])
        .map(|r| r.get::<_, String>(0))
        .map_err(sql)?;
    client.batch_execute(DDL).map_err(sql)?;
    preload(cfg, &mut client)?;
    drop(client);

    // Restart under the row's real durability, with an empty `shared_buffers`.
    stop(pg, &dir)?;
    let pg = start(&dir, sync)?;
    let mut client = connect(&dir)?;
    let mut phases = vec![
        super::postgres_phases::signup_phase(cfg, &mut client, pg.pid)?,
        super::postgres_phases::checkout_phase(cfg, &mut client, pg.pid)?,
        super::postgres_phases::profile_phase(cfg, &mut client, pg.pid)?,
        super::postgres_phases::page_phase(cfg, &mut client, pg.pid)?,
        super::postgres_phases::detail_phase(cfg, &mut client, pg.pid)?,
    ];
    phases.retain(|p| p.dist.count > 0);

    let mut footprints = vec![(Point::Hot, measure(&data)?)];
    drop(client);
    stop(pg, &dir)?;
    footprints.push((Point::Settled, measure(&data)?));

    Ok(SystemReport {
        system: "postgres",
        bracket: "server",
        workload: "shop",
        durability,
        version,
        settings: vec![
            ("synchronous_commit".into(), sync.into()),
            ("tenancy".into(), "user_id column + index".into()),
            ("cache".into(), server::CACHE_POSTGRES.into()),
            ("transport".into(), "unix socket".into()),
            ("transaction".into(), "one per checkout".into()),
        ],
        compression: "none (TOAST only for large values)",
        retains_history: false,
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
            "The order page is ORDER BY … LIMIT 10 OFFSET n over an index, \
             not a materialised list."
                .into(),
        ],
        seed_path: None,
        materialise_ms: 0,
    })
}

fn preload(cfg: &ShopCfg, client: &mut Client) -> Result<(), String> {
    let mut tx = client.transaction().map_err(sql)?;
    let mut order_id = 0u64;
    let mut item_id = 0u64;
    for u in 0..cfg.users {
        insert_user(&mut tx, u, cfg)?;
        for s in 0..shopping_count(u, cfg.seed, cfg.orders_max) {
            order_id += 1;
            insert_order(&mut tx, order_id, u, s, cfg)?;
            for p in 0..product_count(u, s, cfg.seed, cfg.items_max) {
                item_id += 1;
                insert_item(&mut tx, item_id, order_id, u, s, p, cfg)?;
            }
        }
    }
    tx.commit().map_err(sql)
}

pub(super) fn insert_user(
    c: &mut impl postgres::GenericClient,
    u: u64,
    cfg: &ShopCfg,
) -> Result<(), String> {
    let r = user_row(u, cfg.seed);
    c.execute(
        INS_USER,
        &[&(u as i64), &r.name, &r.address, &r.city, &r.email],
    )
    .map(|_| ())
    .map_err(sql)
}

pub(super) fn insert_order(
    c: &mut impl postgres::GenericClient,
    id: u64,
    u: u64,
    s: u64,
    cfg: &ShopCfg,
) -> Result<(), String> {
    let r = shopping_row(u, s, cfg.seed);
    c.execute(
        INS_ORDER,
        &[
            &(id as i64),
            &(u as i64),
            &(r.bought_at as i64),
            &(r.discount_cents as i64),
            &(r.transport_cents as i64),
        ],
    )
    .map(|_| ())
    .map_err(sql)
}

pub(super) fn insert_item(
    c: &mut impl postgres::GenericClient,
    id: u64,
    order: u64,
    u: u64,
    s: u64,
    p: u64,
    cfg: &ShopCfg,
) -> Result<(), String> {
    let r = product_row(u, s, p, cfg.seed);
    c.execute(
        INS_ITEM,
        &[
            &(id as i64),
            &(order as i64),
            &r.name,
            &(r.quantity as i32),
            &(r.unit_cents as i64),
        ],
    )
    .map(|_| ())
    .map_err(sql)
}

pub(super) fn next_id(client: &mut Client, table: &str) -> Result<u64, String> {
    client
        .query_one(
            &format!("SELECT COALESCE(MAX(id), 0) + 1 FROM {table}"),
            &[],
        )
        .map(|r| r.get::<_, i64>(0) as u64)
        .map_err(sql)
}

fn start(dir: &Path, sync: &str) -> Result<Server, String> {
    let log = dir.join("postgres.log");
    let pg = Server::spawn(
        "postgres",
        &[
            "-D",
            &s(&dir.join("data")),
            "-k",
            &s(dir),
            "-c",
            "listen_addresses=",
            "-c",
            &format!("synchronous_commit={sync}"),
            "-c",
            &format!("shared_buffers={}", server::CACHE_POSTGRES),
        ],
        &log,
    )?;
    server::wait_for("postgres", server::STARTUP_SECS, || connect(dir).is_ok())
        .map_err(|e| format!("{e}\n{}", server::log_tail(&log, 10)))?;
    Ok(pg)
}

fn stop(pg: Server, dir: &Path) -> Result<(), String> {
    pg.stop("pg_ctl", &["-D", &s(&dir.join("data")), "-w", "stop"])
}

fn connect(dir: &Path) -> Result<Client, String> {
    Client::connect(
        &format!("host={} user=bench dbname=postgres", s(dir)),
        NoTls,
    )
    .map_err(sql)
}

fn measure(data: &Path) -> Result<Footprint, String> {
    Footprint::split(data, is_log).map_err(|e| format!("footprint: {e}"))
}

fn is_log(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == "pg_wal")
}

fn s(p: &Path) -> String {
    p.display().to_string()
}

pub(super) fn sql(e: postgres::Error) -> String {
    format!("postgres: {e}")
}
