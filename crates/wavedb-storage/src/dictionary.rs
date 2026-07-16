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

use crate::block::{BLOCK_SIZE, BlockAllocator, BlockDescriptor};
use crate::block_file::BlockFile;
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

/// One type's whole compression state.
///
/// The policy (on/off, fixed per type at compile time), the append-only
/// [`Dictionary`], and the block run it is persisted in. This is the third
/// slot of a type's `StructStorage` static — compression is per-type, so its
/// state lives with the type.
#[derive(Debug)]
pub struct DictState {
    enabled: bool,
    dict: Dictionary,
    desc: BlockDescriptor,
}

impl DictState {
    /// Fresh state. `enabled = false` never samples, never persists a run —
    /// that type's pages are always stored `Raw`.
    #[must_use]
    pub const fn new(enabled: bool) -> Self {
        Self {
            enabled,
            dict: Dictionary::new(),
            desc: BlockDescriptor::EMPTY,
        }
    }

    /// Whether this type's pages run through zstd at all.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    /// The dictionary itself (what pages (de)compress against).
    #[must_use]
    pub const fn dictionary(&self) -> &Dictionary {
        &self.dict
    }

    /// The block run the dictionary is persisted in
    /// ([`BlockDescriptor::EMPTY`] while nothing has been sampled).
    #[must_use]
    pub const fn descriptor(&self) -> BlockDescriptor {
        self.desc
    }

    /// Warm the dictionary with a settling record (append-only, capped) and —
    /// when the buffer actually grew — re-persist it to its own block run:
    /// allocate + write the new run, repoint, then free the old one (the same
    /// crash-safe ordering pages use). A disabled state is a no-op.
    ///
    /// # Errors
    /// Propagates a write fault from persisting the grown buffer.
    pub fn warm(
        &mut self,
        record: &[u8],
        file: &BlockFile,
        alloc: &mut BlockAllocator,
    ) -> StorageResult<()> {
        if !self.enabled {
            return Ok(());
        }
        let before = self.dict.len();
        self.dict.sample(record);
        if self.dict.len() == before {
            return Ok(());
        }
        let bytes = self.dict.to_bytes();
        let run = alloc.alloc(bytes.len().div_ceil(BLOCK_SIZE) as u64);
        file.write_run(run, &bytes)?;
        let old = self.desc;
        self.desc = BlockDescriptor::from_run_used(run, bytes.len() as u64);
        if old.is_allocated() {
            alloc.free(old.run());
        }
        Ok(())
    }

    /// Adopt a checkpoint-persisted dictionary and the run it lives in —
    /// the open-from-checkpoint path, where the buffer is loaded instead of
    /// re-sampled by replay.
    pub(crate) fn load(&mut self, dict: Dictionary, desc: BlockDescriptor) {
        self.dict = dict;
        self.desc = desc;
    }

    /// Drop the sampled buffer and its run pointer, keeping the policy — the
    /// open path resets before a journal replay rebuilds the same state.
    pub(crate) fn reset(&mut self) {
        self.dict = Dictionary::new();
        self.desc = BlockDescriptor::EMPTY;
    }
}

#[cfg(test)]
mod tests {
    use super::{DICT_CAP, DictState, Dictionary};
    use crate::block::BlockAllocator;
    use crate::block_file::{BlockFile, RESERVED_BLOCKS};

    // The dictionary lives in `data.bin` too: warming allocates and repoints
    // its own block run, round-tripping byte-identically; a disabled state
    // never samples, never allocates.
    #[test]
    fn warm_persists_own_run_and_disabled_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let bf = BlockFile::open(dir.path().join("data.bin")).unwrap();
        let mut alloc = BlockAllocator::new();
        alloc.alloc(RESERVED_BLOCKS);

        let mut on = DictState::new(true);
        assert!(!on.descriptor().is_allocated());
        on.warm(&[0xAB; 100], &bf, &mut alloc).unwrap();
        assert!(
            on.descriptor().is_allocated(),
            "sampling must persist the dictionary"
        );
        let stored = Dictionary::from_bytes(
            &bf.read_run(on.descriptor().run()).unwrap(),
        )
        .unwrap();
        assert_eq!(stored.latest(), &[0xAB; 100][..]);

        let mut off = DictState::new(false);
        off.warm(&[0xCD; 100], &bf, &mut alloc).unwrap();
        assert!(
            !off.descriptor().is_allocated(),
            "compression off ⇒ no dictionary run"
        );
        assert!(off.dictionary().is_empty());
    }

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
