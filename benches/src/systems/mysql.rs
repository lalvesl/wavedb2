//! The MySQL adapter — server bracket (RFC 0060 §2).
//!
//! Same shape as the PostgreSQL row: a server started on a unix socket in this
//! run's scratch directory, one implicit transaction per operation, both
//! durability rows. The durability knob is `innodb_flush_log_at_trx_commit`,
//! passed at startup rather than `SET GLOBAL` so the value is what the server
//! ran under from its first write, not from its first client.

use std::path::Path;

use mysql::prelude::Queryable as _;
use mysql::{Conn, OptsBuilder, params};

use super::server::{self, Server};
use super::{Cfg, Durability, SystemReport};
use crate::footprint::{Footprint, Point};
use crate::metrics::{self, Phase, Writer};
use crate::schema::{Rng, logical_bytes, thing, thing_v2};

const DDL: &str = "
CREATE TABLE thing (
  id    BIGINT PRIMARY KEY,
  kind  INT    NOT NULL,
  score BIGINT NOT NULL,
  name  TEXT   NOT NULL,
  tag   VARCHAR(64) NOT NULL,
  body  TEXT   NOT NULL
) ENGINE=InnoDB;
";

const INSERT: &str = "INSERT INTO thing (id, kind, score, name, tag, body) \
                      VALUES (:id, :kind, :score, :name, :tag, :body)";
const SELECT: &str = "SELECT name FROM thing WHERE id = :id";
const UPDATE: &str = "UPDATE thing SET kind = :kind, score = :score, \
                      name = :name, tag = :tag, body = :body WHERE id = :id";

pub fn run(cfg: &Cfg, durability: Durability) -> Result<SystemReport, String> {
    let dir = cfg.work_dir.join(format!("mysql-{}", durability.name()));
    let data = dir.join("data");
    let mut materialise_ms = 0;
    let seeded = cfg.seed_mysql.is_some();
    if let Some(src) = &cfg.seed_mysql {
        materialise_ms =
            crate::seed::materialise(src, &dir)?.as_millis() as u64;
    } else {
        std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir: {e}"))?;
        server::run(
            "mysqld",
            &[
                "--initialize-insecure",
                &format!("--datadir={}", s(&data)),
                &format!("--log-error={}", s(&dir.join("init.log"))),
            ],
        )?;
    }

    let flush = match durability {
        Durability::Durable => "1",
        Durability::Relaxed => "2",
    };
    let mut my = start(&dir, flush)?;
    let mut conn = connect(&dir)?;
    let version: String = conn
        .query_first("SELECT VERSION()")
        .map_err(sql)?
        .unwrap_or_default();

    let mut phases = Vec::new();
    let mut footprints = Vec::new();
    if seeded {
        conn.query_drop("USE bench").map_err(sql)?;
        phases.push(read_phase("read_cold", cfg, &mut conn, my.pid)?);
    } else {
        // An initialised, running, empty server: catalogs, redo and undo, none
        // of it this dataset (RFC 0060 §4.1).
        footprints.push((Point::Baseline, measure(&data)?));
        conn.query_drop("CREATE DATABASE bench").map_err(sql)?;
        conn.query_drop("USE bench").map_err(sql)?;
        conn.query_drop(DDL).map_err(sql)?;
        conn.query_drop("CREATE INDEX idx_thing_tag ON thing(tag)")
            .map_err(sql)?;
        phases.push(insert_phase(cfg, &mut conn, my.pid)?);
        phases.push(read_phase("read_hot", cfg, &mut conn, my.pid)?);
        // Restart empties the InnoDB buffer pool, the counterpart of reopening
        // the WaveDB store. The OS page cache stays warm on both sides.
        drop(conn);
        my = restart(my, &dir, flush)?;
        conn = connect(&dir)?;
        conn.query_drop("USE bench").map_err(sql)?;
        phases.push(read_phase("read_cold", cfg, &mut conn, my.pid)?);
    }
    phases.push(update_phase(cfg, &mut conn, my.pid)?);

    footprints.push((Point::Hot, measure(&data)?));
    // InnoDB's quiescence is a clean shutdown: it flushes the buffer pool and
    // completes purge, so what is left on disk is what the data really costs.
    drop(conn);
    stop(my, &dir)?;
    footprints.push((Point::Settled, measure(&data)?));

    let my = start(&dir, flush)?;
    let mut conn = connect(&dir)?;
    conn.query_drop("OPTIMIZE TABLE bench.thing").map_err(sql)?;
    drop(conn);
    stop(my, &dir)?;
    footprints.push((Point::Compacted, measure(&data)?));

    Ok(SystemReport {
        system: "mysql",
        bracket: "server",
        workload: "micro",
        durability,
        version,
        settings: vec![
            ("innodb_flush_log_at_trx_commit".into(), flush.into()),
            ("engine".into(), "InnoDB".into()),
            ("innodb_buffer_pool_size".into(), server::CACHE_MYSQL.into()),
            ("transport".into(), "unix socket".into()),
            (
                "transaction".into(),
                "one per operation (autocommit)".into(),
            ),
        ],
        compression: "none (InnoDB, no page compression)",
        retains_history: false,
        phases,
        footprints,
        live_records: cfg.rows,
        logical_bytes: logical_bytes(cfg.rows, cfg.seed),
        notes: vec![
            "Retains no superseded versions: undo holds them only until purge, \
             and the clean shutdown before the settled measurement completes it."
                .into(),
        ],
        seed_path: cfg.seed_mysql.as_ref().map(|p| p.display().to_string()),
        materialise_ms,
    })
}

fn insert_phase(cfg: &Cfg, conn: &mut Conn, pid: u32) -> Result<Phase, String> {
    let stmt = conn.prep(INSERT).map_err(sql)?;
    Ok(metrics::phase_of(
        "insert",
        Writer::Pid(pid),
        |lat| {
            for n in 0..cfg.rows {
                let t = thing(n, cfg.seed);
                lat.time(|| {
                    conn.exec_drop(
                        &stmt,
                        params! {
                            "id" => n,
                            "kind" => t.kind,
                            "score" => t.score,
                            "name" => &t.name,
                            "tag" => &t.tag,
                            "body" => &t.body,
                        },
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
    conn: &mut Conn,
    pid: u32,
) -> Result<Phase, String> {
    let stmt = conn.prep(SELECT).map_err(sql)?;
    let mut rng = Rng::new(cfg.seed ^ u64::from(name.len() as u32));
    Ok(metrics::phase_of(
        name,
        Writer::Pid(pid),
        |lat| {
            for _ in 0..cfg.reads {
                let id = rng.below(cfg.rows);
                let got: Option<String> = lat.time(|| {
                    conn.exec_first(&stmt, params! { "id" => id })
                        .expect("select")
                });
                assert!(
                    got.is_some_and(|v| !v.is_empty()),
                    "{name}: missing record"
                );
            }
        },
        cfg.reads as usize,
    ))
}

fn update_phase(cfg: &Cfg, conn: &mut Conn, pid: u32) -> Result<Phase, String> {
    let stmt = conn.prep(UPDATE).map_err(sql)?;
    let mut rng = Rng::new(cfg.seed ^ 0x0DDB_A11B_EEF0_0D15);
    Ok(metrics::phase_of(
        "update",
        Writer::Pid(pid),
        |lat| {
            for _ in 0..cfg.updates {
                let n = rng.below(cfg.rows);
                let t = thing_v2(n, cfg.seed);
                lat.time(|| {
                    conn.exec_drop(
                        &stmt,
                        params! {
                            "id" => n,
                            "kind" => t.kind,
                            "score" => t.score,
                            "name" => &t.name,
                            "tag" => &t.tag,
                            "body" => &t.body,
                        },
                    )
                    .expect("update");
                });
            }
        },
        cfg.updates as usize,
    ))
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

fn restart(my: Server, dir: &Path, flush: &str) -> Result<Server, String> {
    stop(my, dir)?;
    start(dir, flush)
}

fn connect(dir: &Path) -> Result<Conn, String> {
    Conn::new(
        OptsBuilder::new()
            .socket(Some(s(&sock(dir))))
            .user(Some("root")),
    )
    .map_err(sql)
}

/// Kept short and inside the scratch directory: a unix socket path is capped at
/// ~107 bytes, which a nested temp directory can reach on its own.
fn sock(dir: &Path) -> std::path::PathBuf {
    dir.join("s")
}

/// The data directory only: the error log and our captured stdio sit beside it,
/// and a chattier server must not read as a larger database.
fn measure(data: &Path) -> Result<Footprint, String> {
    Footprint::split(data, is_log).map_err(|e| format!("footprint: {e}"))
}

/// Redo, undo, binlog and the doublewrite buffer are all preallocated recovery
/// capacity — 150 MB of it at MySQL 8.4's defaults, whatever the table holds
/// (RFC 0060 §4.1).
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

fn sql(e: mysql::Error) -> String {
    format!("mysql: {e}")
}
