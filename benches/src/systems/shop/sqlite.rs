//! SQLite in the e-commerce workload.
//!
//! Three tables and two foreign keys, which is where the model differs: WaveDB
//! puts the user in the `Id` and the relationship in a held `PivotId`, and the
//! SQL schema puts both in columns with an index on each. The order page is
//! `ORDER BY bought_at LIMIT 10 OFFSET n` against a declared list's page
//! descent — that comparison is the reason this workload exists.
//!
//! A **checkout is one transaction**, because that is how an application would
//! write it: the order and its line items commit together or not at all, and it
//! costs one barrier. WaveDB pays one per record. The micro workload's
//! autocommit-per-row rule does not apply here — this is the realistic shape.

use std::path::Path;

use rusqlite::{Connection, params};

use super::ShopCfg;
use crate::footprint::{Footprint, Point};
use crate::shop::{
    logical_bytes, product_count, product_row, shopping_count, shopping_row,
    user_row,
};
use crate::systems::{Durability, SystemReport};

pub const DDL: &str = "
CREATE TABLE users (
  id      INTEGER PRIMARY KEY,
  name    TEXT NOT NULL,
  address TEXT NOT NULL,
  city    TEXT NOT NULL,
  email   TEXT NOT NULL
);
CREATE TABLE shopping (
  id              INTEGER PRIMARY KEY,
  user_id         INTEGER NOT NULL,
  bought_at       INTEGER NOT NULL,
  discount_cents  INTEGER NOT NULL,
  transport_cents INTEGER NOT NULL
);
CREATE INDEX idx_shopping_user ON shopping(user_id, bought_at);
CREATE TABLE product (
  id          INTEGER PRIMARY KEY,
  shopping_id INTEGER NOT NULL,
  name        TEXT NOT NULL,
  quantity    INTEGER NOT NULL,
  unit_cents  INTEGER NOT NULL
);
CREATE INDEX idx_product_shopping ON product(shopping_id, name);
";

pub fn run(
    cfg: &ShopCfg,
    durability: Durability,
) -> Result<SystemReport, String> {
    let dir = cfg
        .work_dir
        .join(format!("shop-sqlite-{}", durability.name()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir: {e}"))?;
    let path = dir.join("shop.db");
    let sync = match durability {
        Durability::Durable => "FULL",
        Durability::Relaxed => "NORMAL",
    };

    {
        // Preload in bulk, and deliberately so: it is never timed, and the
        // stored form it produces is the same either way.
        // A fill is not a measurement: it gets the machine, not the cage.
        let _fill = crate::cage::for_fill();
        let mut conn = connect(&path, "OFF")?;
        conn.execute_batch(DDL).map_err(sql)?;
        preload(cfg, &mut conn)?;
    }

    // Reopen so the connection's page cache is empty, matching the reopened
    // WaveDB store and the restarted servers.
    let mut conn = connect(&path, sync)?;
    let mut phases = vec![
        super::sqlite_phases::signup_phase(cfg, &mut conn)?,
        super::sqlite_phases::checkout_phase(cfg, &mut conn)?,
        super::sqlite_phases::profile_phase(cfg, &conn)?,
        super::sqlite_phases::page_phase(cfg, &conn)?,
        super::sqlite_phases::detail_phase(cfg, &conn)?,
    ];
    phases.retain(|p| p.dist.count > 0);

    let footprints = vec![
        (Point::Hot, measure(&dir)?),
        (Point::Settled, {
            checkpoint(&conn)?;
            measure(&dir)?
        }),
    ];

    Ok(SystemReport {
        system: "sqlite",
        bracket: "embedded",
        workload: "shop",
        durability,
        version: rusqlite::version().to_string(),
        settings: vec![
            ("journal_mode".into(), "WAL".into()),
            ("synchronous".into(), sync.into()),
            ("tenancy".into(), "user_id column + index".into()),
            ("transaction".into(), "one per checkout".into()),
        ],
        compression: "none",
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

fn preload(cfg: &ShopCfg, conn: &mut Connection) -> Result<(), String> {
    let tx = conn.transaction().map_err(sql)?;
    let mut order_id = 0u64;
    let mut item_id = 0u64;
    for u in 0..cfg.users {
        insert_user(&tx, u, cfg)?;
        for s in 0..shopping_count(u, cfg.seed, cfg.orders_max) {
            order_id += 1;
            insert_order(&tx, order_id, u, s, cfg)?;
            for p in 0..product_count(u, s, cfg.seed, cfg.items_max) {
                item_id += 1;
                insert_item(&tx, item_id, order_id, u, s, p, cfg)?;
            }
        }
    }
    tx.commit().map_err(sql)
}

pub(super) fn insert_user(
    conn: &Connection,
    u: u64,
    cfg: &ShopCfg,
) -> Result<(), String> {
    let r = user_row(u, cfg.seed);
    conn.execute(
        "INSERT INTO users (id, name, address, city, email) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![u as i64, r.name, r.address, r.city, r.email],
    )
    .map(|_| ())
    .map_err(sql)
}

pub(super) fn insert_order(
    conn: &Connection,
    id: u64,
    u: u64,
    s: u64,
    cfg: &ShopCfg,
) -> Result<(), String> {
    let r = shopping_row(u, s, cfg.seed);
    conn.execute(
        "INSERT INTO shopping (id, user_id, bought_at, discount_cents, \
         transport_cents) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            id as i64,
            u as i64,
            r.bought_at as i64,
            r.discount_cents as i64,
            r.transport_cents as i64
        ],
    )
    .map(|_| ())
    .map_err(sql)
}

pub(super) fn insert_item(
    conn: &Connection,
    id: u64,
    order: u64,
    u: u64,
    s: u64,
    p: u64,
    cfg: &ShopCfg,
) -> Result<(), String> {
    let r = product_row(u, s, p, cfg.seed);
    conn.execute(
        "INSERT INTO product (id, shopping_id, name, quantity, unit_cents) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            id as i64,
            order as i64,
            r.name,
            r.quantity,
            r.unit_cents as i64
        ],
    )
    .map(|_| ())
    .map_err(sql)
}

pub(super) fn next_id(conn: &Connection, table: &str) -> Result<u64, String> {
    conn.query_row(
        &format!("SELECT COALESCE(MAX(id), 0) + 1 FROM {table}"),
        [],
        |r| r.get::<_, i64>(0),
    )
    .map(|v| v as u64)
    .map_err(sql)
}

fn connect(path: &Path, sync: &str) -> Result<Connection, String> {
    let conn = Connection::open(path).map_err(sql)?;
    conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get::<_, String>(0))
        .map_err(sql)?;
    conn.pragma_update(None, "synchronous", sync).map_err(sql)?;
    Ok(conn)
}

fn checkpoint(conn: &Connection) -> Result<(), String> {
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .map_err(sql)
}

fn measure(dir: &Path) -> Result<Footprint, String> {
    Footprint::split(dir, is_log).map_err(|e| format!("footprint: {e}"))
}

fn is_log(path: &Path) -> bool {
    path.file_name().is_some_and(|n| {
        let n = n.to_string_lossy();
        n.ends_with("-wal") || n.ends_with("-shm")
    })
}

pub(super) fn sql(e: rusqlite::Error) -> String {
    format!("sqlite: {e}")
}
