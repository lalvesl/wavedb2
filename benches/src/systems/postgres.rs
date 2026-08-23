//! The PostgreSQL adapter — server bracket (RFC 0060 §2).
//!
//! Started on a unix socket inside the run's own scratch directory, never on a
//! TCP port and never against a machine-wide instance: the row must measure a
//! server this run configured, on this run's data, or the durability column is
//! a guess.
//!
//! Every operation is its own implicit transaction, matching the SQLite row and
//! matching "one WaveDB collection op is one apply batch". Wrapping the inserts
//! in one transaction would measure a bulk load instead.

use std::path::Path;

use postgres::{Client, NoTls};

use super::server::{self, Server};
use super::{Cfg, Durability, SystemReport};
use crate::footprint::{Footprint, Point};
use crate::metrics::{self, Phase, Writer};
use crate::schema::{Rng, logical_bytes, thing, thing_v2};

const DDL: &str = "
CREATE TABLE thing (
  id    BIGINT PRIMARY KEY,
  kind  INTEGER NOT NULL,
  score BIGINT  NOT NULL,
  name  TEXT    NOT NULL,
  tag   TEXT    NOT NULL,
  body  TEXT    NOT NULL
);
CREATE INDEX idx_thing_tag ON thing(tag);
";

const INSERT: &str = "INSERT INTO thing (id, kind, score, name, tag, body) \
                      VALUES ($1, $2, $3, $4, $5, $6)";
const SELECT: &str = "SELECT name FROM thing WHERE id = $1";
const UPDATE: &str = "UPDATE thing SET kind = $2, score = $3, name = $4, \
                      tag = $5, body = $6 WHERE id = $1";

pub fn run(cfg: &Cfg, durability: Durability) -> Result<SystemReport, String> {
    let dir = cfg.work_dir.join(format!("postgres-{}", durability.name()));
    let data = dir.join("data");
    let mut materialise_ms = 0;
    let seeded = cfg.seed_postgres.is_some();
    if let Some(src) = &cfg.seed_postgres {
        materialise_ms =
            crate::seed::materialise(src, &dir)?.as_millis() as u64;
    } else {
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
    }

    let sync = match durability {
        Durability::Durable => "on",
        Durability::Relaxed => "off",
    };
    let mut pg = start(&dir, sync)?;
    let mut client = connect(&dir)?;
    let version = client
        .query_one("SHOW server_version", &[])
        .map(|r| r.get::<_, String>(0))
        .map_err(sql)?;

    let mut phases = Vec::new();
    let mut footprints = Vec::new();
    if seeded {
        phases.push(read_phase("read_cold", cfg, &mut client, pg.pid)?);
    } else {
        // An initialised, running, empty cluster: the fixed cost that is not
        // this dataset (RFC 0060 §4.1).
        footprints.push((Point::Baseline, measure(&data)?));
        client.batch_execute(DDL).map_err(sql)?;
        phases.push(insert_phase(cfg, &mut client, pg.pid)?);
        client.batch_execute("CHECKPOINT").map_err(sql)?;
        phases.push(read_phase("read_hot", cfg, &mut client, pg.pid)?);
        // Restart: `shared_buffers` is emptied, so the next reads fall through
        // to the heap. This is the counterpart of reopening the WaveDB store;
        // the OS page cache stays warm on both sides.
        drop(client);
        pg = restart(pg, &dir, sync)?;
        client = connect(&dir)?;
        phases.push(read_phase("read_cold", cfg, &mut client, pg.pid)?);
    }
    phases.push(update_phase(cfg, &mut client, pg.pid)?);

    footprints.push((Point::Hot, measure(&data)?));
    // A clean shutdown *is* PostgreSQL's quiescence: it checkpoints and closes,
    // so nothing measured afterwards is work the server still owed.
    drop(client);
    stop(pg, &dir)?;
    footprints.push((Point::Settled, measure(&data)?));

    let pg = start(&dir, sync)?;
    let mut client = connect(&dir)?;
    client.batch_execute("VACUUM FULL").map_err(sql)?;
    drop(client);
    stop(pg, &dir)?;
    footprints.push((Point::Compacted, measure(&data)?));

    Ok(SystemReport {
        system: "postgres",
        bracket: "server",
        workload: "micro",
        durability,
        version,
        settings: vec![
            ("synchronous_commit".into(), sync.into()),
            ("fsync".into(), "on".into()),
            ("transport".into(), "unix socket".into()),
            (
                "transaction".into(),
                "one per operation (autocommit)".into(),
            ),
        ],
        compression: "none (TOAST only for large values)",
        retains_history: false,
        phases,
        footprints,
        live_records: cfg.rows,
        logical_bytes: logical_bytes(cfg.rows, cfg.seed),
        notes: vec![
            "Retains no superseded versions: MVCC keeps dead tuples only until \
             VACUUM, and the run ends with them collected."
                .into(),
        ],
        seed_path: cfg.seed_postgres.as_ref().map(|p| p.display().to_string()),
        materialise_ms,
    })
}

fn insert_phase(
    cfg: &Cfg,
    client: &mut Client,
    pid: u32,
) -> Result<Phase, String> {
    let stmt = client.prepare(INSERT).map_err(sql)?;
    Ok(metrics::phase_of(
        "insert",
        Writer::Pid(pid),
        |lat| {
            for n in 0..cfg.rows {
                let t = thing(n, cfg.seed);
                lat.time(|| {
                    client
                        .execute(
                            &stmt,
                            &[
                                &(n as i64),
                                &(t.kind as i32),
                                &(t.score as i64),
                                &t.name,
                                &t.tag,
                                &t.body,
                            ],
                        )
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
    client: &mut Client,
    pid: u32,
) -> Result<Phase, String> {
    let stmt = client.prepare(SELECT).map_err(sql)?;
    let mut rng = Rng::new(cfg.seed ^ u64::from(name.len() as u32));
    Ok(metrics::phase_of(
        name,
        Writer::Pid(pid),
        |lat| {
            for _ in 0..cfg.reads {
                let id = rng.below(cfg.rows) as i64;
                let got: String = lat.time(|| {
                    client
                        .query_one(&stmt, &[&id])
                        .expect("select")
                        .get::<_, String>(0)
                });
                assert!(!got.is_empty(), "{name}: missing record");
            }
        },
        cfg.reads as usize,
    ))
}

fn update_phase(
    cfg: &Cfg,
    client: &mut Client,
    pid: u32,
) -> Result<Phase, String> {
    let stmt = client.prepare(UPDATE).map_err(sql)?;
    let mut rng = Rng::new(cfg.seed ^ 0x0DDB_A11B_EEF0_0D15);
    Ok(metrics::phase_of(
        "update",
        Writer::Pid(pid),
        |lat| {
            for _ in 0..cfg.updates {
                let n = rng.below(cfg.rows);
                let t = thing_v2(n, cfg.seed);
                lat.time(|| {
                    client
                        .execute(
                            &stmt,
                            &[
                                &(n as i64),
                                &(t.kind as i32),
                                &(t.score as i64),
                                &t.name,
                                &t.tag,
                                &t.body,
                            ],
                        )
                        .expect("update");
                });
            }
        },
        cfg.updates as usize,
    ))
}

/// Spawn the postmaster **directly** rather than through `pg_ctl`: `pg_ctl`
/// forks and returns, which would leave us holding the pid of a process that
/// wrote nothing, and the write-bytes column reading zero.
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
        ],
        &log,
    )?;
    server::wait_for("postgres", 60, || connect(dir).is_ok())
        .map_err(|e| format!("{e}\n{}", server::log_tail(&log, 10)))?;
    Ok(pg)
}

fn stop(pg: Server, dir: &Path) -> Result<(), String> {
    pg.stop("pg_ctl", &["-D", &s(&dir.join("data")), "-w", "stop"])
}

fn restart(pg: Server, dir: &Path, sync: &str) -> Result<Server, String> {
    stop(pg, dir)?;
    start(dir, sync)
}

fn connect(dir: &Path) -> Result<Client, String> {
    Client::connect(
        &format!("host={} user=bench dbname=postgres", s(dir)),
        NoTls,
    )
    .map_err(sql)
}

/// Measures the **data directory**, never the scratch directory around it: the
/// server's log and our own captured stdio live beside it, and counting those
/// as storage would grow the footprint with how chatty the server was.
fn measure(data: &Path) -> Result<Footprint, String> {
    Footprint::split(data, is_log).map_err(|e| format!("footprint: {e}"))
}

/// `pg_wal` is preallocated recovery capacity: 80 MB of segments whether the
/// table holds 200 000 rows or 20 (RFC 0060 §4.1).
fn is_log(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == "pg_wal")
}

fn s(p: &Path) -> String {
    p.display().to_string()
}

fn sql(e: postgres::Error) -> String {
    format!("postgres: {e}")
}
