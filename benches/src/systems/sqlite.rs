//! The SQLite adapter — in the caller's process, the embedded bracket's peer.
//!
//! Every operation runs in its own implicit transaction (rusqlite's autocommit
//! default), which is the fair match for "one WaveDB collection op is one apply
//! batch": both pay their barrier per operation. Batching inserts into one
//! transaction would compare a bulk load against a per-record engine, which is
//! a real difference but a different measurement.

use std::path::Path;

use rusqlite::{Connection, params};

use super::{Cfg, Durability, SystemReport};
use crate::footprint::{Footprint, Point};
use crate::metrics::{self, Phase};
use crate::schema::{Rng, logical_bytes, thing, thing_v2};

const DDL: &str = "
CREATE TABLE thing (
  id    INTEGER PRIMARY KEY,
  kind  INTEGER NOT NULL,
  score INTEGER NOT NULL,
  name  TEXT    NOT NULL,
  tag   TEXT    NOT NULL,
  body  TEXT    NOT NULL
);
CREATE INDEX idx_thing_tag ON thing(tag);
";

pub fn run(cfg: &Cfg, durability: Durability) -> Result<SystemReport, String> {
    let dir = cfg.work_dir.join(format!("sqlite-{}", durability.name()));
    let mut materialise_ms = 0;
    let seeded = cfg.seed_sqlite.is_some();
    if let Some(src) = &cfg.seed_sqlite {
        materialise_ms =
            crate::seed::materialise(src, &dir)?.as_millis() as u64;
    } else {
        std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir: {e}"))?;
    }
    let path = dir.join("bench.db");

    let sync = match durability {
        Durability::Durable => "FULL",
        Durability::Relaxed => "NORMAL",
    };
    let conn = connect(&path, sync)?;
    let mut phases = Vec::new();
    if seeded {
        // The seed is loaded by `sqlite3 .import` in the builder, so the
        // schema is already there; a seeded run has nothing left to insert.
        phases.push(read_phase("read_cold", cfg, &conn)?);
    } else {
        conn.execute_batch(DDL).map_err(sql)?;
        phases.push(insert_phase(cfg, &conn)?);
        // Quiesce before reading, matching what the WaveDB row does.
        checkpoint(&conn)?;
        phases.push(read_phase("read_hot", cfg, &conn)?);
    }
    drop(conn);

    // Reopen so the connection's own page cache is empty — the counterpart of
    // reopening the WaveDB store. The OS page cache stays warm on both sides.
    let conn = connect(&path, sync)?;
    if !seeded {
        phases.push(read_phase("read_cold", cfg, &conn)?);
    }
    phases.push(update_phase(cfg, &conn)?);

    let mut footprints = vec![(Point::Hot, measure(&dir)?)];
    checkpoint(&conn)?;
    footprints.push((Point::Settled, measure(&dir)?));
    conn.execute_batch("VACUUM").map_err(sql)?;
    checkpoint(&conn)?;
    footprints.push((Point::Compacted, measure(&dir)?));

    Ok(SystemReport {
        system: "sqlite",
        bracket: "embedded",
        workload: "micro",
        durability,
        version: rusqlite::version().to_string(),
        settings: vec![
            ("journal_mode".into(), "WAL".into()),
            ("synchronous".into(), sync.into()),
            (
                "transaction".into(),
                "one per operation (autocommit)".into(),
            ),
        ],
        compression: "none",
        retains_history: false,
        phases,
        footprints,
        live_records: cfg.rows,
        logical_bytes: logical_bytes(cfg.rows, cfg.seed),
        notes: vec![
            "Retains no superseded versions: an update overwrites in place and \
             the previous row is unrecoverable."
                .into(),
        ],
        seed_path: cfg.seed_sqlite.as_ref().map(|p| p.display().to_string()),
        materialise_ms,
    })
}

fn insert_phase(cfg: &Cfg, conn: &Connection) -> Result<Phase, String> {
    let mut stmt = conn
        .prepare(
            "INSERT INTO thing (id, kind, score, name, tag, body) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .map_err(sql)?;
    Ok(metrics::phase(
        "insert",
        |lat| {
            for n in 0..cfg.rows {
                let t = thing(n, cfg.seed);
                lat.time(|| {
                    stmt.execute(params![
                        n as i64,
                        t.kind,
                        t.score as i64,
                        t.name,
                        t.tag,
                        t.body
                    ])
                    .expect("insert");
                });
            }
        },
        cfg.rows as usize,
    ))
}

fn read_phase(
    name: &'static str,
    cfg: &Cfg,
    conn: &Connection,
) -> Result<Phase, String> {
    let mut stmt = conn
        .prepare("SELECT kind, score, name, tag, body FROM thing WHERE id = ?1")
        .map_err(sql)?;
    let mut rng = Rng::new(cfg.seed ^ u64::from(name.len() as u32));
    Ok(metrics::phase(
        name,
        |lat| {
            for _ in 0..cfg.reads {
                let id = rng.below(cfg.rows) as i64;
                let got: String = lat.time(|| {
                    stmt.query_row(params![id], |r| r.get(2)).expect("select")
                });
                assert!(!got.is_empty(), "{name}: missing record");
            }
        },
        cfg.reads as usize,
    ))
}

/// Whole-row rewrite, not a `$set`-style partial: WaveDB writes whole records,
/// and comparing a field patch to that would flatter the SQL side for free.
fn update_phase(cfg: &Cfg, conn: &Connection) -> Result<Phase, String> {
    let mut stmt = conn
        .prepare(
            "UPDATE thing SET kind = ?2, score = ?3, name = ?4, tag = ?5, \
             body = ?6 WHERE id = ?1",
        )
        .map_err(sql)?;
    let mut rng = Rng::new(cfg.seed ^ 0x0DDB_A11B_EEF0_0D15);
    Ok(metrics::phase(
        "update",
        |lat| {
            for _ in 0..cfg.updates {
                let n = rng.below(cfg.rows);
                let t = thing_v2(n, cfg.seed);
                lat.time(|| {
                    stmt.execute(params![
                        n as i64,
                        t.kind,
                        t.score as i64,
                        t.name,
                        t.tag,
                        t.body
                    ])
                    .expect("update");
                });
            }
        },
        cfg.updates as usize,
    ))
}

fn connect(path: &Path, sync: &str) -> Result<Connection, String> {
    let conn = Connection::open(path).map_err(sql)?;
    // `journal_mode` answers with a row, so it cannot go through
    // `pragma_update`.
    conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get::<_, String>(0))
        .map_err(sql)?;
    conn.pragma_update(None, "synchronous", sync).map_err(sql)?;
    Ok(conn)
}

/// The WAL is SQLite's whole recovery area, and it is not preallocated — it is
/// truncated at every checkpoint. Split out anyway so every row's `log` column
/// means the same thing (RFC 0060 §4.1).
fn measure(dir: &Path) -> Result<Footprint, String> {
    Footprint::split(dir, is_log).map_err(io)
}

fn is_log(path: &Path) -> bool {
    path.file_name().is_some_and(|n| {
        let n = n.to_string_lossy();
        n.ends_with("-wal") || n.ends_with("-shm")
    })
}

fn checkpoint(conn: &Connection) -> Result<(), String> {
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .map_err(sql)
}

fn sql(e: rusqlite::Error) -> String {
    format!("sqlite: {e}")
}

fn io(e: std::io::Error) -> String {
    format!("footprint: {e}")
}
