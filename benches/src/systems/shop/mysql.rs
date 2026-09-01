//! MySQL in the e-commerce workload.
//!
//! Same three tables and one-transaction checkout as the other two SQL rows.

use std::path::Path;

use mysql::prelude::Queryable;
use mysql::{Conn, OptsBuilder, TxOpts, params};

use super::ShopCfg;
use crate::footprint::{Footprint, Point};
use crate::shop::{
    logical_bytes, product_count, product_row, shopping_count, shopping_row,
    user_row,
};
use crate::systems::server::{self, Server};
use crate::systems::{Durability, SystemReport};

const DDL_USERS: &str = "CREATE TABLE users (
  id      BIGINT PRIMARY KEY,
  name    VARCHAR(64)  NOT NULL,
  address VARCHAR(128) NOT NULL,
  city    VARCHAR(64)  NOT NULL,
  email   VARCHAR(128) NOT NULL
) ENGINE=InnoDB";
const DDL_SHOPPING: &str = "CREATE TABLE shopping (
  id              BIGINT PRIMARY KEY,
  user_id         BIGINT NOT NULL,
  bought_at       BIGINT NOT NULL,
  discount_cents  BIGINT NOT NULL,
  transport_cents BIGINT NOT NULL,
  INDEX idx_shopping_user (user_id, bought_at)
) ENGINE=InnoDB";
const DDL_PRODUCT: &str = "CREATE TABLE product (
  id          BIGINT PRIMARY KEY,
  shopping_id BIGINT NOT NULL,
  name        VARCHAR(64) NOT NULL,
  quantity    INT    NOT NULL,
  unit_cents  BIGINT NOT NULL,
  INDEX idx_product_shopping (shopping_id, name)
) ENGINE=InnoDB";

const INS_USER: &str = "INSERT INTO users (id, name, address, city, email) \
                        VALUES (:id, :name, :address, :city, :email)";
const INS_ORDER: &str = "INSERT INTO shopping (id, user_id, bought_at, \
                         discount_cents, transport_cents) \
                         VALUES (:id, :user_id, :bought_at, :discount, :transport)";
const INS_ITEM: &str = "INSERT INTO product (id, shopping_id, name, quantity, \
                        unit_cents) VALUES (:id, :shopping_id, :name, :qty, :unit)";
pub(super) const SEL_USER: &str = "SELECT name FROM users WHERE id = :id";
pub(super) const SEL_PAGE: &str = "SELECT id FROM shopping WHERE user_id = :id \
                        ORDER BY bought_at LIMIT :lim OFFSET :off";
pub(super) const SEL_FIRST: &str = "SELECT id FROM shopping WHERE user_id = :id \
                         ORDER BY bought_at LIMIT 1";
pub(super) const SEL_ITEMS: &str = "SELECT name FROM product WHERE shopping_id = :id \
                         ORDER BY name LIMIT :lim";

pub fn run(
    cfg: &ShopCfg,
    durability: Durability,
) -> Result<SystemReport, String> {
    let dir = cfg
        .work_dir
        .join(format!("shop-mysql-{}", durability.name()));
    let data = dir.join("data");
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir: {e}"))?;
    server::run(
        "mysqld",
        &[
            "--initialize-insecure",
            &format!("--datadir={}", s(&data)),
            &format!("--log-error={}", s(&dir.join("init.log"))),
        ],
    )?;

    let flush = match durability {
        Durability::Durable => "1",
        Durability::Relaxed => "2",
    };
    let my = start(&dir, "2")?;
    let mut conn = connect(&dir)?;
    let version: String = conn
        .query_first("SELECT VERSION()")
        .map_err(sql)?
        .unwrap_or_default();
    conn.query_drop("CREATE DATABASE bench").map_err(sql)?;
    conn.query_drop("USE bench").map_err(sql)?;
    for ddl in [DDL_USERS, DDL_SHOPPING, DDL_PRODUCT] {
        conn.query_drop(ddl).map_err(sql)?;
    }
    preload(cfg, &mut conn)?;
    drop(conn);

    // Restart under the row's real durability, with an empty buffer pool.
    stop(my, &dir)?;
    let my = start(&dir, flush)?;
    let mut conn = connect(&dir)?;
    conn.query_drop("USE bench").map_err(sql)?;
    let mut phases = vec![
        super::mysql_phases::signup_phase(cfg, &mut conn, my.pid)?,
        super::mysql_phases::checkout_phase(cfg, &mut conn, my.pid)?,
        super::mysql_phases::profile_phase(cfg, &mut conn, my.pid)?,
        super::mysql_phases::page_phase(cfg, &mut conn, my.pid)?,
        super::mysql_phases::detail_phase(cfg, &mut conn, my.pid)?,
    ];
    phases.retain(|p| p.dist.count > 0);

    let mut footprints = vec![(Point::Hot, measure(&data)?)];
    drop(conn);
    stop(my, &dir)?;
    footprints.push((Point::Settled, measure(&data)?));

    Ok(SystemReport {
        system: "mysql",
        bracket: "server",
        workload: "shop",
        durability,
        version,
        settings: vec![
            ("innodb_flush_log_at_trx_commit".into(), flush.into()),
            ("tenancy".into(), "user_id column + index".into()),
            ("cache".into(), server::CACHE_MYSQL.into()),
            ("transport".into(), "unix socket".into()),
            ("transaction".into(), "one per checkout".into()),
        ],
        compression: "none (InnoDB, no page compression)",
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

fn preload(cfg: &ShopCfg, conn: &mut Conn) -> Result<(), String> {
    let mut tx = conn.start_transaction(TxOpts::default()).map_err(sql)?;
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
    q: &mut impl Queryable,
    u: u64,
    cfg: &ShopCfg,
) -> Result<(), String> {
    let r = user_row(u, cfg.seed);
    q.exec_drop(
        INS_USER,
        params! {
            "id" => u, "name" => &r.name, "address" => &r.address,
            "city" => &r.city, "email" => &r.email,
        },
    )
    .map_err(sql)
}

pub(super) fn insert_order(
    q: &mut impl Queryable,
    id: u64,
    u: u64,
    s: u64,
    cfg: &ShopCfg,
) -> Result<(), String> {
    let r = shopping_row(u, s, cfg.seed);
    q.exec_drop(
        INS_ORDER,
        params! {
            "id" => id, "user_id" => u, "bought_at" => r.bought_at,
            "discount" => r.discount_cents, "transport" => r.transport_cents,
        },
    )
    .map_err(sql)
}

pub(super) fn insert_item(
    q: &mut impl Queryable,
    id: u64,
    order: u64,
    u: u64,
    s: u64,
    p: u64,
    cfg: &ShopCfg,
) -> Result<(), String> {
    let r = product_row(u, s, p, cfg.seed);
    q.exec_drop(
        INS_ITEM,
        params! {
            "id" => id, "shopping_id" => order, "name" => &r.name,
            "qty" => r.quantity, "unit" => r.unit_cents,
        },
    )
    .map_err(sql)
}

pub(super) fn next_id(conn: &mut Conn, table: &str) -> Result<u64, String> {
    conn.query_first(format!(
        "SELECT COALESCE(MAX(id), 0) + 1 FROM bench.{table}"
    ))
    .map_err(sql)
    .map(|v: Option<u64>| v.unwrap_or(1))
}

fn start(dir: &Path, flush: &str) -> Result<Server, String> {
    let log = dir.join("mysqld.log");
    let my = Server::spawn(
        "mysqld",
        &[
            &format!("--datadir={}", s(&dir.join("data"))),
            &format!("--socket={}", s(&sock(dir))),
            &format!("--pid-file={}", s(&dir.join("mysqld.pid"))),
            &format!("--log-error={}", s(&log)),
            &format!("--innodb-flush-log-at-trx-commit={flush}"),
            &format!("--innodb-buffer-pool-size={}", server::CACHE_MYSQL),
            "--skip-networking",
        ],
        &dir.join("mysqld.out"),
    )?;
    server::wait_for("mysqld", server::STARTUP_SECS, || connect(dir).is_ok())
        .map_err(|e| format!("{e}\n{}", server::log_tail(&log, 10)))?;
    Ok(my)
}

fn stop(my: Server, dir: &Path) -> Result<(), String> {
    my.stop(
        "mysqladmin",
        &["--socket", &s(&sock(dir)), "-u", "root", "shutdown"],
    )
}

fn connect(dir: &Path) -> Result<Conn, String> {
    Conn::new(
        OptsBuilder::new()
            .socket(Some(s(&sock(dir))))
            .user(Some("root")),
    )
    .map_err(sql)
}

fn sock(dir: &Path) -> std::path::PathBuf {
    dir.join("s")
}

fn measure(data: &Path) -> Result<Footprint, String> {
    Footprint::split(data, is_log).map_err(|e| format!("footprint: {e}"))
}

fn is_log(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    path.components().any(|c| c.as_os_str() == "#innodb_redo")
        || name.starts_with("binlog")
        || name.starts_with("undo_")
        || name.starts_with("ib_logfile")
        || name.ends_with(".dblwr")
}

fn s(p: &Path) -> String {
    p.display().to_string()
}

pub(super) fn sql(e: mysql::Error) -> String {
    format!("mysql: {e}")
}
