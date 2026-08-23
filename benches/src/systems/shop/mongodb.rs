//! MongoDB in the e-commerce workload — the reference peer.
//!
//! Three collections with references, **not** an embedded line-item array.
//! Embedding is what a MongoDB application would usually do, and it would be a
//! different measurement: one document write per checkout instead of several,
//! and no join to render a detail. Keeping the reference model holds the
//! *shape* of the data equal across all five so the numbers compare storage and
//! access paths rather than modelling choices. The embedded variant is worth
//! measuring on its own; it is not this row.
//!
//! The checkout is a **transaction** — which on a standalone `mongod` means the
//! run needs a replica set of one, since multi-document transactions require
//! one. That is why this adapter starts `mongod` with `--replSet` and
//! initiates it.

use std::path::Path;

use mongodb::bson::{Document, doc};
use mongodb::options::{Acknowledgment, ClientOptions, WriteConcern};
use mongodb::sync::{Client, Collection, Database};
use mongodb::IndexModel;

use super::ShopCfg;
use crate::footprint::{Footprint, Point};
use crate::shop::{
    logical_bytes, product_count, product_row, shopping_count,
    shopping_row, user_row,
};
use crate::systems::server::{self, Server};
use crate::systems::{Durability, SystemReport};

pub fn run(
    cfg: &ShopCfg,
    durability: Durability,
) -> Result<SystemReport, String> {
    let dir = cfg
        .work_dir
        .join(format!("shop-mongodb-{}", durability.name()));
    let data = dir.join("data");
    std::fs::create_dir_all(&data).map_err(|e| format!("mkdir: {e}"))?;

    let journal = durability == Durability::Durable;
    let mongo = start(&dir)?;
    let client = connect(journal)?;
    let db = client.database("shop");
    let version = db
        .run_command(doc! { "buildInfo": 1 })
        .run()
        .map_err(drv)?
        .get_str("version")
        .unwrap_or("unknown")
        .to_string();
    indexes(&db)?;
    preload(cfg, &db)?;
    drop(client);

    // Restart empties the WiredTiger cache, matching every other row.
    stop(mongo, &dir)?;
    let mongo = start(&dir)?;
    let client = connect(journal)?;
    let db = client.database("shop");
    let mut session = client.start_session().run().map_err(drv)?;
    let mut phases = vec![
        super::mongodb_phases::signup_phase(cfg, &db, mongo.pid),
        super::mongodb_phases::checkout_phase(cfg, &db, &mut session, mongo.pid),
        super::mongodb_phases::profile_phase(cfg, &db, mongo.pid),
        super::mongodb_phases::page_phase(cfg, &db, mongo.pid),
        super::mongodb_phases::detail_phase(cfg, &db, mongo.pid),
    ];
    phases.retain(|p| p.dist.count > 0);

    let mut footprints = vec![(Point::Hot, measure(&data)?)];
    drop(session);
    drop(client);
    stop(mongo, &dir)?;
    footprints.push((Point::Settled, measure(&data)?));

    Ok(SystemReport {
        system: "mongodb",
        bracket: "server",
        workload: "shop",
        durability,
        version,
        settings: vec![
            ("writeConcern".into(), format!("{{ w: 1, j: {journal} }}")),
            ("tenancy".into(), "user_id field + index".into()),
            ("transport".into(), "loopback TCP".into()),
            ("transaction".into(), "one per checkout".into()),
        ],
        compression: "snappy (WiredTiger default)",
        retains_history: false,
        phases,
        footprints,
        live_records: cfg.live_records(),
        logical_bytes: logical_bytes(cfg.users, cfg.seed, cfg.orders_max, cfg.items_max),
        notes: vec![
            "Reference model, not embedded line items: the shape is held equal \
             to the other four so the row compares access paths rather than \
             modelling. Embedding is MongoDB's idiomatic answer and would be a \
             different measurement."
                .into(),
            "Runs as a one-node replica set, because multi-document \
             transactions need one."
                .into(),
        ],
        seed_path: None,
        materialise_ms: 0,
    })
}

fn indexes(db: &Database) -> Result<(), String> {
    orders(db)
        .create_index(
            IndexModel::builder()
                .keys(doc! { "user_id": 1, "bought_at": 1 })
                .build(),
        )
        .run()
        .map_err(drv)?;
    items(db)
        .create_index(
            IndexModel::builder()
                .keys(doc! { "shopping_id": 1, "name": 1 })
                .build(),
        )
        .run()
        .map_err(drv)?;
    Ok(())
}

fn preload(cfg: &ShopCfg, db: &Database) -> Result<(), String> {
    let mut order_id = 0u64;
    let mut item_id = 0u64;
    // Batched inserts: the preload is never timed, and the stored form is the
    // same either way.
    let mut users = Vec::new();
    let mut os = Vec::new();
    let mut is = Vec::new();
    for u in 0..cfg.users {
        users.push(user_doc(u, cfg));
        for s in 0..shopping_count(u, cfg.seed, cfg.orders_max) {
            order_id += 1;
            os.push(order_doc(order_id, u, s, cfg));
            for p in 0..product_count(u, s, cfg.seed, cfg.items_max) {
                item_id += 1;
                is.push(item_doc(item_id, order_id, u, s, p, cfg));
            }
        }
    }
    profiles(db).insert_many(users).run().map_err(drv)?;
    orders(db).insert_many(os).run().map_err(drv)?;
    items(db).insert_many(is).run().map_err(drv)?;
    Ok(())
}

pub(super) fn user_doc(u: u64, cfg: &ShopCfg) -> Document {
    let r = user_row(u, cfg.seed);
    doc! {
        "_id": u as i64, "name": r.name, "address": r.address,
        "city": r.city, "email": r.email,
    }
}

pub(super) fn order_doc(id: u64, u: u64, s: u64, cfg: &ShopCfg) -> Document {
    let r = shopping_row(u, s, cfg.seed);
    doc! {
        "_id": id as i64, "user_id": u as i64,
        "bought_at": r.bought_at as i64,
        "discount_cents": r.discount_cents as i64,
        "transport_cents": r.transport_cents as i64,
    }
}

pub(super) fn item_doc(id: u64, order: u64, u: u64, s: u64, p: u64, cfg: &ShopCfg) -> Document {
    let r = product_row(u, s, p, cfg.seed);
    doc! {
        "_id": id as i64, "shopping_id": order as i64, "name": r.name,
        "quantity": i64::from(r.quantity), "unit_cents": r.unit_cents as i64,
    }
}

pub(super) fn next_id(db: &Database, name: &str) -> u64 {
    db.collection::<Document>(name)
        .find_one(doc! {})
        .sort(doc! { "_id": -1 })
        .run()
        .ok()
        .flatten()
        .and_then(|d| d.get_i64("_id").ok())
        .map_or(1, |v| v as u64 + 1)
}

pub(super) fn profiles(db: &Database) -> Collection<Document> {
    db.collection("users")
}

pub(super) fn orders(db: &Database) -> Collection<Document> {
    db.collection("shopping")
}

pub(super) fn items(db: &Database) -> Collection<Document> {
    db.collection("product")
}

fn port() -> u16 {
    27_600 + u16::try_from(std::process::id() % 300).unwrap_or(0)
}

/// Starts a **one-node replica set**: multi-document transactions are not
/// available on a standalone `mongod`, and the checkout is a transaction.
fn start(dir: &Path) -> Result<Server, String> {
    let log = dir.join("mongod.log");
    let mongo = Server::spawn(
        "mongod",
        &[
            "--dbpath",
            &s(&dir.join("data")),
            "--bind_ip",
            "127.0.0.1",
            "--port",
            &port().to_string(),
            "--replSet",
            "bench",
            "--logpath",
            &s(&log),
        ],
        &dir.join("mongod.out"),
    )?;
    server::wait_for("mongod", 60, || {
        direct().is_ok_and(|c| {
            c.database("admin")
                .run_command(doc! { "ping": 1 })
                .run()
                .is_ok()
        })
    })
    .map_err(|e| format!("{e}\n{}", server::log_tail(&log, 10)))?;

    // Idempotent: already-initiated on the restart, which answers an error we
    // deliberately ignore.
    if let Ok(c) = direct() {
        let _ = c
            .database("admin")
            .run_command(doc! {
                "replSetInitiate": doc! {
                    "_id": "bench",
                    "members": [doc! { "_id": 0, "host": format!("127.0.0.1:{}", port()) }],
                }
            })
            .run();
    }
    // Wait for the node to become primary, or the first write refuses.
    server::wait_for("mongod primary", 60, || {
        direct().is_ok_and(|c| {
            c.database("admin")
                .run_command(doc! { "hello": 1 })
                .run()
                .is_ok_and(|d| d.get_bool("isWritablePrimary").unwrap_or(false))
        })
    })
    .map_err(|e| format!("{e}\n{}", server::log_tail(&log, 10)))?;
    Ok(mongo)
}

fn stop(mongo: Server, dir: &Path) -> Result<(), String> {
    mongo.stop("mongod", &["--dbpath", &s(&dir.join("data")), "--shutdown"])
}

fn direct() -> Result<Client, String> {
    Client::with_options(
        ClientOptions::parse(format!(
            "mongodb://127.0.0.1:{}/?directConnection=true",
            port()
        ))
        .run()
        .map_err(drv)?,
    )
    .map_err(drv)
}

fn connect(journal: bool) -> Result<Client, String> {
    let mut opts = ClientOptions::parse(format!(
        "mongodb://127.0.0.1:{}/?directConnection=true",
        port()
    ))
    .run()
    .map_err(drv)?;
    opts.write_concern = Some(
        WriteConcern::builder()
            .w(Acknowledgment::Nodes(1))
            .journal(journal)
            .build(),
    );
    opts.max_pool_size = Some(1);
    Client::with_options(opts).map_err(drv)
}

fn measure(data: &Path) -> Result<Footprint, String> {
    Footprint::split(data, is_log).map_err(|e| format!("footprint: {e}"))
}

fn is_log(path: &Path) -> bool {
    path.components()
        .any(|c| c.as_os_str() == "journal" || c.as_os_str() == "diagnostic.data")
}

fn s(p: &Path) -> String {
    p.display().to_string()
}

fn drv(e: mongodb::error::Error) -> String {
    format!("mongodb: {e}")
}
