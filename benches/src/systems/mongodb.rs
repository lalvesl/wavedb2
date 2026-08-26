//! The MongoDB adapter — the reference peer (RFC 0060, "Why MongoDB is the
//! reference peer").
//!
//! It is the closest thing to WaveDB's model in the comparison: whole documents
//! addressed by `_id`, no join, no query planner in the path a benchmark like
//! this exercises. So it is the row where a WaveDB loss is least explainable by
//! "the other system is doing something different".
//!
//! The durability knob is the write concern's `j` flag, which is per-operation
//! rather than per-server — it travels with the write, so the durable row
//! really does wait for the journal on every single insert.

use std::path::Path;

use mongodb::IndexModel;
use mongodb::bson::{Document, doc};
use mongodb::options::{Acknowledgment, ClientOptions, WriteConcern};
use mongodb::sync::{Client, Collection};

use super::server::{self, CACHE_GB, Server};
use super::{Cfg, Durability, SystemReport};
use crate::footprint::{Footprint, Point};
use crate::metrics::{self, Phase, Writer};
use crate::schema::{Rng, Thing, logical_bytes, thing, thing_v2};

pub fn run(cfg: &Cfg, durability: Durability) -> Result<SystemReport, String> {
    let dir = cfg.work_dir.join(format!("mongodb-{}", durability.name()));
    let data = dir.join("data");
    let mut materialise_ms = 0;
    let seeded = cfg.seed_mongodb.is_some();
    if let Some(src) = &cfg.seed_mongodb {
        materialise_ms =
            crate::seed::materialise(src, &dir)?.as_millis() as u64;
    } else {
        std::fs::create_dir_all(&data).map_err(|e| format!("mkdir: {e}"))?;
    }

    let journal = durability == Durability::Durable;
    let mut mongo = start(&dir)?;
    let client = connect(journal)?;
    let db = client.database("bench");
    let version = db
        .run_command(doc! { "buildInfo": 1 })
        .run()
        .map_err(drv)?
        .get_str("version")
        .unwrap_or("unknown")
        .to_string();
    let mut col: Collection<Document> = db.collection("thing");

    let mut phases = Vec::new();
    let mut footprints = Vec::new();
    if seeded {
        phases.push(read_phase("read_cold", cfg, &col, mongo.pid));
    } else {
        // A running, empty server. For MongoDB this is almost entirely the
        // preallocated journal, which is why it is measured (RFC 0060 §4.1).
        footprints.push((Point::Baseline, measure(&data)?));
        col.create_index(IndexModel::builder().keys(doc! { "tag": 1 }).build())
            .run()
            .map_err(drv)?;
        phases.push(insert_phase(cfg, &col, mongo.pid));
        phases.push(read_phase("read_hot", cfg, &col, mongo.pid));
        // Restart empties the WiredTiger cache, the counterpart of reopening
        // the WaveDB store. The OS page cache stays warm on both sides.
        drop(client);
        mongo = restart(mongo, &dir)?;
        let client = connect(journal)?;
        col = client.database("bench").collection("thing");
        phases.push(read_phase("read_cold", cfg, &col, mongo.pid));
    }
    phases.push(update_phase(cfg, &col, mongo.pid));

    footprints.push((Point::Hot, measure(&data)?));
    // A clean shutdown checkpoints WiredTiger and closes the journal, which is
    // as quiesced as this server gets without asking for compaction.
    stop(mongo, &dir)?;
    footprints.push((Point::Settled, measure(&data)?));

    let mongo = start(&dir)?;
    let client = connect(journal)?;
    client
        .database("bench")
        .run_command(doc! { "compact": "thing" })
        .run()
        .map_err(drv)?;
    drop(client);
    stop(mongo, &dir)?;
    footprints.push((Point::Compacted, measure(&data)?));

    Ok(SystemReport {
        system: "mongodb",
        bracket: "server",
        workload: "micro",
        durability,
        version,
        settings: vec![
            ("writeConcern".into(), format!("{{ w: 1, j: {journal} }}")),
            ("storage_engine".into(), "WiredTiger".into()),
            ("wiredTigerCacheSizeGB".into(), CACHE_GB.into()),
            ("transport".into(), "loopback TCP".into()),
            ("operation".into(), "one document per request".into()),
        ],
        compression: "snappy (WiredTiger default)",
        retains_history: false,
        phases,
        footprints,
        live_records: cfg.rows,
        logical_bytes: logical_bytes(cfg.rows, cfg.seed),
        notes: vec![
            "Retains no superseded versions: `replace_one` overwrites the \
             document and the previous one is unrecoverable."
                .into(),
            "The only peer that compresses its data by default, which is why \
             the compression column exists at all."
                .into(),
        ],
        seed_path: cfg.seed_mongodb.as_ref().map(|p| p.display().to_string()),
        materialise_ms,
    })
}

/// The document form of a record. `_id` is the dataset id on every system, so
/// the point lookup is the same key everywhere (RFC 0060 §3).
fn document(n: u64, t: &Thing) -> Document {
    doc! {
        "_id": n as i64,
        "kind": i64::from(t.kind),
        "score": t.score as i64,
        "name": t.name.clone(),
        "tag": t.tag.clone(),
        "body": t.body.clone(),
    }
}

fn insert_phase(cfg: &Cfg, col: &Collection<Document>, pid: u32) -> Phase {
    metrics::phase_of(
        "insert",
        Writer::Pid(pid),
        |lat| {
            for n in 0..cfg.rows {
                let d = document(n, &thing(n, cfg.seed));
                lat.time(|| col.insert_one(&d).run().expect("insert"));
            }
        },
        cfg.rows as usize,
    )
}

fn read_phase(
    name: &'static str,
    cfg: &Cfg,
    col: &Collection<Document>,
    pid: u32,
) -> Phase {
    let mut rng = Rng::new(cfg.seed ^ u64::from(name.len() as u32));
    metrics::phase_of(
        name,
        Writer::Pid(pid),
        |lat| {
            for _ in 0..cfg.reads {
                let id = rng.below(cfg.rows) as i64;
                let got = lat.time(|| {
                    col.find_one(doc! { "_id": id }).run().expect("find")
                });
                assert!(got.is_some(), "{name}: missing record");
            }
        },
        cfg.reads as usize,
    )
}

/// Whole-document replace, not `$set`: WaveDB writes whole records, and a field
/// patch would flatter the document store for free.
fn update_phase(cfg: &Cfg, col: &Collection<Document>, pid: u32) -> Phase {
    let mut rng = Rng::new(cfg.seed ^ 0x0DDB_A11B_EEF0_0D15);
    metrics::phase_of(
        "update",
        Writer::Pid(pid),
        |lat| {
            for _ in 0..cfg.updates {
                let n = rng.below(cfg.rows);
                let d = document(n, &thing_v2(n, cfg.seed));
                lat.time(|| {
                    col.replace_one(doc! { "_id": n as i64 }, &d)
                        .run()
                        .expect("replace");
                });
            }
        },
        cfg.updates as usize,
    )
}

/// A port derived from this process, so two benchmark runs on one machine do
/// not fight over 27017 — and neither touches a mongod the user is running.
fn port() -> u16 {
    27_100 + u16::try_from(std::process::id() % 400).unwrap_or(0)
}

fn start(dir: &Path) -> Result<Server, String> {
    let log = dir.join("mongod.log");
    // No `--fork`: the forked daemon would leave us holding the pid of a
    // process that exits immediately, and the write-bytes column would read
    // zero for every phase.
    let mongo = Server::spawn(
        "mongod",
        &[
            "--dbpath",
            &s(&dir.join("data")),
            "--bind_ip",
            "127.0.0.1",
            "--port",
            &port().to_string(),
            // Pinned, not inferred: WiredTiger sizes its cache from the
            // HOST's RAM, not the cgroup's, so under the 2 GiB cage an
            // unpinned mongod asks for gigabytes it cannot have and is
            // OOM-killed. Every server gets the same budget (RFC 0060 §5).
            "--wiredTigerCacheSizeGB",
            CACHE_GB,
            "--logpath",
            &s(&log),
        ],
        &dir.join("mongod.out"),
    )?;
    server::wait_for("mongod", 60, || {
        connect(false).is_ok_and(|c| {
            c.database("admin")
                .run_command(doc! { "ping": 1 })
                .run()
                .is_ok()
        })
    })
    .map_err(|e| format!("{e}\n{}", server::log_tail(&log, 10)))?;
    Ok(mongo)
}

fn stop(mongo: Server, dir: &Path) -> Result<(), String> {
    mongo.stop("mongod", &["--dbpath", &s(&dir.join("data")), "--shutdown"])
}

fn restart(mongo: Server, dir: &Path) -> Result<Server, String> {
    stop(mongo, dir)?;
    start(dir)
}

fn connect(journal: bool) -> Result<Client, String> {
    let mut opts =
        ClientOptions::parse(format!("mongodb://127.0.0.1:{}", port()))
            .run()
            .map_err(drv)?;
    opts.write_concern = Some(
        WriteConcern::builder()
            .w(Acknowledgment::Nodes(1))
            .journal(journal)
            .build(),
    );
    // One connection, like every other row: the workload is sequential and a
    // pool would quietly measure concurrency the other adapters do not have.
    opts.max_pool_size = Some(1);
    Client::with_options(opts).map_err(drv)
}

/// The `--dbpath` only: `mongod.log` sits beside it, and a log is not storage.
fn measure(data: &Path) -> Result<Footprint, String> {
    Footprint::split(data, is_log).map_err(|e| format!("footprint: {e}"))
}

/// WiredTiger preallocates its journal in 100 MB files — 200 MB beside a 22 MB
/// collection in the seed, and the same 200 MB beside 20 documents
/// (RFC 0060 §4.1). `diagnostic.data` is FTDC telemetry, not stored data.
fn is_log(path: &Path) -> bool {
    path.components().any(|c| {
        c.as_os_str() == "journal" || c.as_os_str() == "diagnostic.data"
    })
}

fn s(p: &Path) -> String {
    p.display().to_string()
}

fn drv(e: mongodb::error::Error) -> String {
    format!("mongodb: {e}")
}
