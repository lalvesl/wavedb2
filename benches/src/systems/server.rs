//! Process lifecycle for the server bracket (RFC 0060 §2).
//!
//! The three server adapters differ in almost everything — connection string,
//! durability knob, compaction command — but share four mechanics, and those
//! are here so each adapter reads as its own database rather than as plumbing.
//!
//! One of them is load-bearing for the measurement: **write bytes come from the
//! server's `/proc/<pid>/io`, not ours**. In the embedded bracket the engine
//! writes in the benchmark's own process, so `/proc/self/io` is exactly right;
//! in the server bracket our process writes nothing but socket traffic, and
//! reading `self` would report a flat zero for every phase.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A server this process started, and is responsible for stopping cleanly.
///
/// Clean shutdown is not politeness: a data directory left in crash state makes
/// the *next* thing that opens it pay recovery, which would land inside a
/// measured phase or a footprint (RFC 0060 §6).
pub struct Server {
    child: Child,
    pub pid: u32,
}

impl Server {
    /// Start `cmd` detached from our stdio, with its own log file.
    pub fn spawn(
        cmd: &str,
        args: &[&str],
        log: &Path,
    ) -> Result<Self, String> {
        let out = std::fs::File::create(log)
            .map_err(|e| format!("{cmd}: log {}: {e}", log.display()))?;
        let err = out
            .try_clone()
            .map_err(|e| format!("{cmd}: log clone: {e}"))?;
        let child = Command::new(cmd)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .spawn()
            .map_err(|e| format!("spawn {cmd}: {e}"))?;
        let pid = child.id();
        Ok(Self { child, pid })
    }

    /// Bytes this server sent to the block layer so far, from its own
    /// `/proc/<pid>/io`. Zero if the process is gone — a stopped server's
    /// counter is not recoverable, so phases must be measured while it runs.
    #[must_use]
    pub fn write_bytes(&self) -> u64 {
        write_bytes(self.pid)
    }

    /// Ask the server to stop with its own shutdown command, then reap it.
    ///
    /// The command is the system's own (`pg_ctl stop`, `mysqladmin shutdown`,
    /// `mongod --shutdown`) rather than a signal, because each of them means
    /// "flush and close", and a signal only means "die".
    pub fn stop(mut self, cmd: &str, args: &[&str]) -> Result<(), String> {
        let out = Command::new(cmd)
            .args(args)
            .output()
            .map_err(|e| format!("{cmd}: {e}"))?;
        if !out.status.success() {
            let _ = self.child.kill();
            let _ = self.child.wait();
            return Err(format!(
                "{cmd} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        self.child
            .wait()
            .map_err(|e| format!("{cmd}: wait: {e}"))
            .map(|_| ())
    }
}

/// `write_bytes` from `/proc/<pid>/io`.
#[must_use]
pub fn write_bytes(pid: u32) -> u64 {
    let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/io")) else {
        return 0;
    };
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("write_bytes:") {
            return v.trim().parse().unwrap_or(0);
        }
    }
    0
}

/// Poll `ready` until it answers true or `secs` elapse.
///
/// Every server here takes seconds to become connectable, and every one of them
/// reports "started" long before it accepts a connection. Polling the thing the
/// benchmark actually needs — a working connection — is the only honest probe.
pub fn wait_for(
    what: &str,
    secs: u64,
    mut ready: impl FnMut() -> bool,
) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if ready() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!("{what}: not ready after {secs}s"))
}

/// Run a setup command to completion, failing loudly. Nothing here is timed:
/// these are `initdb`, `mysqld --initialize-insecure` and friends.
pub fn run(cmd: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| format!("{cmd}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{cmd} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The tail of a server's log, for an error message that says what happened.
#[must_use]
pub fn log_tail(log: &Path, lines: usize) -> String {
    let Ok(text) = std::fs::read_to_string(log) else {
        return "no log".into();
    };
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}
