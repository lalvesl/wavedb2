//! [`Dictionary`] — the per-`STRUCT_HASH` raw-content zstd dictionary.
//!
//! A page holds records of exactly one type, so each type gets one dictionary
//! with nothing foreign diluting it. zstd accepts **any bytes** as a
//! dictionary (no `ZDICT` trainer): this is simply a capped, **append-only**
//! buffer of record bytes sampled as writes settle.
//!
//! ## Versioning — a version is a prefix
//!
//! A page compressed against dictionary state X must be decompressed against
//! the exact same bytes. Because the buffer only ever appends, a state is
//! fully identified by its **length**: a page stamps the buffer length at
//! compression time (`dict_len`), and decompression binds `buf[..dict_len]`.
//! Every superseded version is a prefix of the same live buffer, so nothing
//! is ever freed or recompressed to stay readable.
//!
//! ## Storage & recovery
//!
//! The dictionary is stored in `data.bin` like a page: a block run handed out
//! by the allocator, holding `[len (u32 LE)][to_wire_checked(buf)]`, repointed
//! by its [`Directory`](crate::directory::Directory) whenever the buffer
//! grows (which stops at the cap). On open, `data.bin` is a journal-replay
//! projection: the settle path re-samples the same records in the same order,
//! so the buffer — and every `dict_len` a rebuilt page stamps — is reproduced
//! deterministically and re-persisted. Reading the stored run back becomes
//! load-bearing only when settling checkpoints (stops truncating `data.bin`
//! on open).

use wavedb_core::wire::{from_wire_checked, to_wire_checked};

use crate::error::{StorageError, StorageResult};

/// Per-run prefix: `payload_len (u32)`.
const LEN_PREFIX: usize = 4;

/// Sampling stops once the buffer holds this many bytes. Sized like a typical
/// trained zstd dictionary: big enough to seed repetitive record prefixes,
/// small enough to sit in cache during (de)compression.
pub const DICT_CAP: usize = 64 * 1024;

/// A capped, append-only byte buffer used as a zstd dictionary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Dictionary {
    buf: Vec<u8>,
}

impl Dictionary {
    /// An empty dictionary (compression starts dictionary-less and warms up).
    #[must_use]
    pub const fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Sample a settling record's bytes into the buffer, until the cap.
    /// Append-only: bytes already sampled never change or move.
    pub fn sample(&mut self, record: &[u8]) {
        let room = DICT_CAP.saturating_sub(self.buf.len());
        if room == 0 {
            return;
        }
        let take = record.len().min(room);
        self.buf.extend_from_slice(&record[..take]);
    }

    /// The current state's identity: the buffer length a page stamps as its
    /// `dict_len` at compression time.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.buf.len()
    }

    /// `true` while nothing has been sampled.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// The exact dictionary bytes for state `len` — the prefix a page stamped
    /// with `dict_len == len` must decompress against. `None` if `len`
    /// overruns the buffer (a foreign or corrupt stamp).
    #[must_use]
    pub fn state(&self, len: usize) -> Option<&[u8]> {
        self.buf.get(..len)
    }

    /// The latest state, for compressing a page being written now.
    #[must_use]
    pub fn latest(&self) -> &[u8] {
        &self.buf
    }

    /// Serialise for the dictionary's block run: `[len][checked wire]` — the
    /// same framing every engine structure uses.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let payload = to_wire_checked(&self.buf);
        let mut out = Vec::with_capacity(LEN_PREFIX + payload.len());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&payload);
        out
    }

    /// Parse a dictionary back from its (zero-padded) block run.
    ///
    /// # Errors
    /// [`StorageError::Corrupt`] on a truncated buffer, an out-of-range
    /// length, a crc mismatch, or an undecodable body.
    pub fn from_bytes(buf: &[u8]) -> StorageResult<Self> {
        let len_bytes: [u8; LEN_PREFIX] = buf
            .get(..LEN_PREFIX)
            .and_then(|s| s.try_into().ok())
            .ok_or(StorageError::Corrupt("dictionary length truncated"))?;
        let len = u32::from_le_bytes(len_bytes) as usize;
        let end = LEN_PREFIX
            .checked_add(len)
            .filter(|&end| end <= buf.len())
            .ok_or(StorageError::Corrupt("dictionary length out of range"))?;
        let buf = from_wire_checked(&buf[LEN_PREFIX..end])
            .map_err(|_| StorageError::Corrupt("dictionary body"))?;
        Ok(Self { buf })
    }
}

#[cfg(test)]
mod tests {
    use super::{DICT_CAP, Dictionary};

    #[test]
    fn samples_append_only_and_cap() {
        let mut d = Dictionary::new();
        assert!(d.is_empty());
        d.sample(b"alpha");
        d.sample(b"beta");
        assert_eq!(d.latest(), b"alphabeta");
        // A prior state stays byte-identical after growth.
        assert_eq!(d.state(5), Some(&b"alpha"[..]));
        // Beyond the buffer = unknown state.
        assert_eq!(d.state(100), None);

        // The cap truncates the sample that crosses it, then freezes.
        d.sample(&vec![7u8; DICT_CAP]);
        assert_eq!(d.len(), DICT_CAP);
        d.sample(b"more");
        assert_eq!(d.len(), DICT_CAP, "sampling past the cap must be a no-op");
        assert_eq!(&d.latest()[..9], b"alphabeta");
    }
}
