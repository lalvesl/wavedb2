//! [`Journal`] — the append-only write-ahead log.
//!
//! Durability lives here: a batch is durable once [`Journal::append`] returns,
//! because append writes the frame and `fsync`s before yielding. The page
//! directories and the block allocator are reconstructions — on startup
//! [`Journal::replay`] re-derives the in-memory cache from the log, and a
//! background settle pushes it down into `data.bin`. A crash therefore loses
//! nothing that was acked, and an interrupted (torn) final write is discarded.
//!
//! ## Frame format
//!
//! ```text
//! [ payload_len (u32 LE) ][ payload = to_wire_checked(Vec<Write>) ]
//!                           └── [ crc32 (u32 LE) ][ wire bytes ] ──┘
//! ```
//!
//! The payload is the **checked wire encoding** of one batch of [`Write`]s —
//! the same all-or-nothing unit [`Store::apply`](wavedb_core::Store::apply)
//! commits. `Write` derives `WaveWire`, so there is no journal-private record
//! format: the log stores exactly what the codec defines, crc-framed.
//!
//! Replay stops at the first frame whose length runs past EOF, whose crc fails,
//! or whose payload does not decode — a torn tail from a crash mid-append — and
//! truncates the file back to the last whole frame.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;

use wavedb_core::Write;
use wavedb_core::wire::{from_wire_checked, to_wire_checked};

use crate::error::StorageResult;

/// Per-frame prefix: `payload_len (u32)`.
const FRAME_PREFIX: usize = 4;

/// An append-only write-ahead log of [`Write`] batches.
#[derive(Debug)]
pub struct Journal {
    file: File,
    /// Byte offset of the end of the last whole frame (where the next append lands).
    end: u64,
}

impl Journal {
    /// Open (or create) the journal at `path`. Does not read anything yet — call
    /// [`replay`](Self::replay) to recover committed batches.
    ///
    /// # Errors
    /// [`StorageError::Io`] on a filesystem failure.
    pub fn open(path: impl AsRef<Path>) -> StorageResult<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let end = file.metadata()?.len();
        Ok(Self { file, end })
    }

    /// Total bytes of committed frames.
    #[must_use]
    pub const fn len_bytes(&self) -> u64 {
        self.end
    }

    /// Append one batch and `fsync`. The batch is durable once this returns.
    ///
    /// # Errors
    /// [`StorageError::Io`] if the write or sync fails.
    pub fn append(&mut self, batch: &[Write]) -> StorageResult<()> {
        let payload = to_wire_checked(&batch.to_vec());
        let mut frame = Vec::with_capacity(FRAME_PREFIX + payload.len());
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(&payload);

        self.file.write_all_at(&frame, self.end)?;
        self.file.sync_all()?; // the durability point
        self.end += frame.len() as u64;
        Ok(())
    }

    /// Drop every committed frame — called **after** a checkpoint's
    /// superblock pointer is durable, so the log restarts empty (the
    /// checkpoint now carries everything the frames did).
    ///
    /// # Errors
    /// [`StorageError::Io`] if the truncate or sync fails.
    pub fn truncate(&mut self) -> StorageResult<()> {
        self.file.set_len(0)?;
        self.file.sync_all()?;
        self.end = 0;
        Ok(())
    }

    /// Read every committed batch back, in append order.
    ///
    /// A torn tail (a frame whose length overruns EOF, or whose crc mismatches —
    /// the signature of a crash mid-append) is dropped and the file truncated to
    /// the last whole frame, so the next [`append`](Self::append) writes clean.
    ///
    /// # Errors
    /// [`StorageError::Io`] on a read fault. Mid-log corruption surfaces as a
    /// truncated tail, not an error (an append-only log is unreliable past its
    /// first bad frame).
    pub fn replay(&mut self) -> StorageResult<Vec<Vec<Write>>> {
        let mut buf = vec![0u8; self.end as usize];
        self.file.read_exact_at(&mut buf, 0)?;

        let mut batches = Vec::new();
        let mut pos = 0usize;
        let mut good_end = 0usize;
        while pos + FRAME_PREFIX <= buf.len() {
            let len = u32::from_le_bytes(buf[pos..pos + 4].try_into().unwrap())
                as usize;
            let body_start = pos + FRAME_PREFIX;
            let Some(body_end) = body_start.checked_add(len) else {
                break; // absurd length — torn tail
            };
            if body_end > buf.len() {
                break; // frame overruns EOF — torn tail
            }
            // crc verification + decode in one step; any fault = torn tail.
            match from_wire_checked::<Vec<Write>>(&buf[body_start..body_end]) {
                Ok(batch) => batches.push(batch),
                Err(_) => break,
            }
            pos = body_end;
            good_end = body_end;
        }

        if (good_end as u64) < self.end {
            self.file.set_len(good_end as u64)?;
            self.file.sync_all()?;
            self.end = good_end as u64;
        }
        Ok(batches)
    }
}

#[cfg(test)]
mod tests {
    use super::Journal;
    use crate::error::StorageResult;
    use std::os::unix::fs::FileExt;
    use wavedb_core::{Id, U48, Write};

    fn id(key: u64) -> Id {
        Id::new(key, U48::from(1u32), false, key as u16)
    }

    fn temp() -> (tempfile::TempDir, std::path::PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("journal.log");
        (d, p)
    }

    #[test]
    fn append_then_replay() -> StorageResult<()> {
        let (_d, p) = temp();
        {
            let mut j = Journal::open(&p)?;
            j.append(&[
                Write::Put(id(1), vec![1, 2, 3]),
                Write::Put(id(2), vec![4]),
            ])?;
            j.append(&[Write::Remove(id(1))])?;
        }
        let mut j = Journal::open(&p)?;
        let batches = j.replay()?;
        assert_eq!(
            batches,
            vec![
                vec![
                    Write::Put(id(1), vec![1, 2, 3]),
                    Write::Put(id(2), vec![4])
                ],
                vec![Write::Remove(id(1))],
            ]
        );
        Ok(())
    }

    #[test]
    fn replay_then_append_continues() -> StorageResult<()> {
        let (_d, p) = temp();
        {
            let mut j = Journal::open(&p)?;
            j.append(&[Write::Put(id(1), vec![9])])?;
        }
        let mut j = Journal::open(&p)?;
        assert_eq!(j.replay()?.len(), 1);
        j.append(&[Write::Put(id(2), vec![8])])?; // append after replay
        let mut j2 = Journal::open(&p)?;
        assert_eq!(j2.replay()?.len(), 2);
        Ok(())
    }

    #[test]
    fn empty_journal_replays_nothing() -> StorageResult<()> {
        let (_d, p) = temp();
        let mut j = Journal::open(&p)?;
        assert!(j.replay()?.is_empty());
        Ok(())
    }

    #[test]
    fn torn_tail_is_discarded_and_truncated() -> StorageResult<()> {
        let (_d, p) = temp();
        {
            let mut j = Journal::open(&p)?;
            j.append(&[Write::Put(id(1), vec![1, 2, 3])])?;
            j.append(&[Write::Put(id(2), vec![4, 5, 6])])?;
        }
        let whole = std::fs::metadata(&p)?.len();
        // Simulate a crash mid-append: tack on a half-written frame.
        {
            let f = std::fs::OpenOptions::new().write(true).open(&p)?;
            f.write_all_at(&[0xFF; 5], whole)?; // garbage, shorter than a frame
        }
        let mut j = Journal::open(&p)?;
        let batches = j.replay()?;
        assert_eq!(batches.len(), 2, "torn tail must be ignored");
        // File truncated back to the last whole frame.
        assert_eq!(std::fs::metadata(&p)?.len(), whole);
        Ok(())
    }

    #[test]
    fn corrupt_frame_body_treated_as_tail() -> StorageResult<()> {
        let (_d, p) = temp();
        {
            let mut j = Journal::open(&p)?;
            j.append(&[Write::Put(id(1), vec![1, 2, 3])])?;
        }
        // Flip a byte in the payload → crc fails → whole frame dropped.
        {
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&p)?;
            let len = std::fs::metadata(&p)?.len();
            let last = len - 1;
            let mut b = [0u8];
            f.read_exact_at(&mut b, last)?;
            b[0] ^= 0xFF;
            f.write_all_at(&b, last)?;
        }
        let mut j = Journal::open(&p)?;
        assert!(j.replay()?.is_empty());
        Ok(())
    }
}
