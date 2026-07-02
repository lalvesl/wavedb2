//! [`BlockFile`] — `data.bin` as a block-addressed file.
//!
//! The block layer ([`crate::block`]) decides *which* runs of blocks exist; this
//! module is what actually puts bytes on disk. It owns the file handle, the
//! **superblock** in block 0 (magic + format version + the per-database hash
//! seed), and positioned run-granular reads/writes.
//!
//! ## Block 0 is the superblock
//!
//! Block 0 is reserved for the superblock and is **never** handed out by the
//! allocator — see [`RESERVED_BLOCKS`]. It persists the per-database random
//! `[u64; 4]` seed that [`crate::directory::hash_of`] routes every record with, so
//! a `data.bin` reopened (or rebuilt by journal replay on another machine) hashes
//! every `Id` into the same bucket.
//!
//! ## Positioned I/O
//!
//! Reads and writes take `&self` and use positioned syscalls (`pread`/`pwrite`),
//! so concurrent callers don't fight over a shared seek cursor. The engine is
//! native-only (the browser uses IndexedDB, never this file), so the Unix
//! [`FileExt`] is the portable-enough surface here.

use std::collections::hash_map::RandomState;
use std::fs::{File, OpenOptions};
use std::hash::{BuildHasher, Hasher};
use std::os::unix::fs::FileExt;
use std::path::Path;

use crate::block::{BLOCK_SIZE, Run};
use crate::error::{StorageError, StorageResult};

/// Magic at the head of every WaveDB `data.bin` superblock.
const MAGIC: &[u8; 8] = b"WAVEDBIN";

/// On-disk format version. Bump on any incompatible superblock/layout change.
const FORMAT_VERSION: u32 = 1;

/// Blocks reserved at the head of the file for engine metadata (the superblock).
/// The allocator must never hand these out.
pub const RESERVED_BLOCKS: u64 = 1;

// Superblock byte layout within block 0 (little-endian), the rest zero-padded:
//   [0..8)   magic
//   [8..12)  format version (u32)
//   [12..16) reserved (u32, 0)
//   [16..48) hash seed (4 × u64)
const OFF_VERSION: usize = 8;
const OFF_SEED: usize = 16;

/// `data.bin` opened as an array of fixed [`BLOCK_SIZE`]-byte blocks.
#[derive(Debug)]
pub struct BlockFile {
    file: File,
    seed: [u64; 4],
}

impl BlockFile {
    /// Open `path`, creating and initialising it if it does not yet exist.
    ///
    /// A fresh file gets a superblock with a freshly generated random seed; an
    /// existing file's superblock is validated (magic + version) and its seed
    /// loaded. Either way the returned handle's [`seed`](Self::seed) is the one
    /// records must be routed with.
    ///
    /// # Errors
    /// [`StorageError::Io`] on a filesystem failure, [`StorageError::BadMagic`] /
    /// [`StorageError::BadVersion`] if an existing file is not a compatible
    /// `data.bin`.
    pub fn open(path: impl AsRef<Path>) -> StorageResult<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;

        let len = file.metadata()?.len();
        if len == 0 {
            let seed = random_seed();
            let bf = Self { file, seed };
            bf.write_superblock()?;
            bf.file.sync_all()?;
            Ok(bf)
        } else {
            let seed = read_and_check_superblock(&file)?;
            Ok(Self { file, seed })
        }
    }

    /// The per-database hash seed read from (or written to) the superblock.
    #[must_use]
    pub const fn seed(&self) -> [u64; 4] {
        self.seed
    }

    /// Current file length in whole blocks (rounding any partial tail down).
    ///
    /// # Errors
    /// [`StorageError::Io`] if the file's metadata can't be read.
    pub fn len_blocks(&self) -> StorageResult<u64> {
        Ok(self.file.metadata()?.len() / BLOCK_SIZE as u64)
    }

    /// Read a run's bytes (`run.byte_len()` of them) out of `data.bin`.
    ///
    /// # Errors
    /// [`StorageError::OutOfBounds`] if the run extends past the file;
    /// [`StorageError::Io`] on a read fault.
    pub fn read_run(&self, run: Run) -> StorageResult<Vec<u8>> {
        let have = self.file.metadata()?.len();
        let need = run.end() * BLOCK_SIZE as u64;
        if need > have {
            return Err(StorageError::OutOfBounds { need, have });
        }
        let mut buf = vec![0u8; run.byte_len() as usize];
        self.file.read_exact_at(&mut buf, run.byte_offset())?;
        Ok(buf)
    }

    /// Write `bytes` to the start of `run`. `bytes` must fit in the run; any
    /// remaining tail of the run is left untouched (a page records its own
    /// length, so trailing bytes are irrelevant). The file grows if the run
    /// reaches past the current end.
    ///
    /// # Errors
    /// [`StorageError::RunOverflow`] if `bytes` is larger than the run;
    /// [`StorageError::Io`] on a write fault.
    pub fn write_run(&self, run: Run, bytes: &[u8]) -> StorageResult<()> {
        let cap = run.byte_len();
        if bytes.len() as u64 > cap {
            return Err(StorageError::RunOverflow {
                got: bytes.len() as u64,
                cap,
            });
        }
        self.ensure_len(run.end() * BLOCK_SIZE as u64)?;
        self.file.write_all_at(bytes, run.byte_offset())?;
        Ok(())
    }

    /// Flush all buffered data and metadata to stable storage (`fsync`).
    ///
    /// # Errors
    /// [`StorageError::Io`] if the sync fails.
    pub fn sync(&self) -> StorageResult<()> {
        self.file.sync_all()?;
        Ok(())
    }

    /// Truncate the file to exactly `blocks` blocks, discarding everything past
    /// them. Used to reset `data.bin` to the superblock before a journal-replay
    /// rebuild of the pages. `blocks` must be `>= RESERVED_BLOCKS`.
    ///
    /// # Errors
    /// [`StorageError::Io`] if the resize fails.
    pub fn truncate_to_blocks(&self, blocks: u64) -> StorageResult<()> {
        debug_assert!(blocks >= RESERVED_BLOCKS, "would drop the superblock");
        self.file.set_len(blocks * BLOCK_SIZE as u64)?;
        Ok(())
    }

    /// Grow the file to at least `min_len` bytes (never shrinks).
    fn ensure_len(&self, min_len: u64) -> StorageResult<()> {
        if self.file.metadata()?.len() < min_len {
            self.file.set_len(min_len)?;
        }
        Ok(())
    }

    /// Serialise the superblock into block 0 and persist it.
    fn write_superblock(&self) -> StorageResult<()> {
        let mut block = vec![0u8; BLOCK_SIZE];
        block[..MAGIC.len()].copy_from_slice(MAGIC);
        block[OFF_VERSION..OFF_VERSION + 4]
            .copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        for (i, lane) in self.seed.iter().enumerate() {
            let at = OFF_SEED + i * 8;
            block[at..at + 8].copy_from_slice(&lane.to_le_bytes());
        }
        self.ensure_len(BLOCK_SIZE as u64)?;
        self.file.write_all_at(&block, 0)?;
        Ok(())
    }
}

/// Read block 0, verify magic + version, and return the stored seed.
fn read_and_check_superblock(file: &File) -> StorageResult<[u64; 4]> {
    let have = file.metadata()?.len();
    if have < BLOCK_SIZE as u64 {
        return Err(StorageError::OutOfBounds {
            need: BLOCK_SIZE as u64,
            have,
        });
    }
    let mut block = vec![0u8; BLOCK_SIZE];
    file.read_exact_at(&mut block, 0)?;

    if &block[..MAGIC.len()] != MAGIC {
        return Err(StorageError::BadMagic);
    }
    let version = u32::from_le_bytes(
        block[OFF_VERSION..OFF_VERSION + 4].try_into().unwrap(),
    );
    if version != FORMAT_VERSION {
        return Err(StorageError::BadVersion(version));
    }
    let mut seed = [0u64; 4];
    for (i, lane) in seed.iter_mut().enumerate() {
        let at = OFF_SEED + i * 8;
        *lane = u64::from_le_bytes(block[at..at + 8].try_into().unwrap());
    }
    Ok(seed)
}

/// A per-database random `[u64; 4]` seed, drawn from the OS via the stdlib's
/// randomly-keyed [`RandomState`] (no extra crate dependency). Each lane uses an
/// independently constructed hasher, so the four words are independent.
fn random_seed() -> [u64; 4] {
    let mut seed = [0u64; 4];
    for (i, lane) in seed.iter_mut().enumerate() {
        let mut h = RandomState::new().build_hasher();
        h.write_u64(i as u64);
        *lane = h.finish();
    }
    seed
}

#[cfg(test)]
mod tests {
    use super::{BlockFile, FORMAT_VERSION, MAGIC, OFF_VERSION};
    use crate::block::{BLOCK_SIZE, Run};
    use crate::error::StorageError;
    use std::os::unix::fs::FileExt;

    fn temp_path() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        (dir, path)
    }

    #[test]
    fn fresh_file_gets_superblock_and_nonzero_seed() {
        let (_d, path) = temp_path();
        let bf = BlockFile::open(&path).unwrap();
        assert_ne!(bf.seed(), [0; 4], "seed must be randomised");
        // Block 0 exists on disk.
        assert_eq!(bf.len_blocks().unwrap(), 1);
    }

    #[test]
    fn seed_persists_across_reopen() {
        let (_d, path) = temp_path();
        let seed = {
            let bf = BlockFile::open(&path).unwrap();
            bf.seed()
        };
        let reopened = BlockFile::open(&path).unwrap();
        assert_eq!(reopened.seed(), seed, "seed must survive reopen");
    }

    #[test]
    fn run_write_then_read_roundtrips() {
        let (_d, path) = temp_path();
        let bf = BlockFile::open(&path).unwrap();
        // Block 0 is the superblock; write user data at block 1.
        let run = Run::new(1, 2);
        let mut payload = vec![0u8; run.byte_len() as usize];
        for (i, b) in payload.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        bf.write_run(run, &payload).unwrap();
        assert_eq!(bf.read_run(run).unwrap(), payload);
    }

    #[test]
    fn write_grows_the_file() {
        let (_d, path) = temp_path();
        let bf = BlockFile::open(&path).unwrap();
        assert_eq!(bf.len_blocks().unwrap(), 1);
        bf.write_run(Run::new(5, 1), &[1, 2, 3]).unwrap();
        assert_eq!(bf.len_blocks().unwrap(), 6, "file grew to hold block 5");
    }

    #[test]
    fn short_write_leaves_run_tail_untouched() {
        let (_d, path) = temp_path();
        let bf = BlockFile::open(&path).unwrap();
        let run = Run::new(1, 1);
        bf.write_run(run, &[0xAB, 0xCD]).unwrap(); // only first 2 bytes
        let back = bf.read_run(run).unwrap();
        assert_eq!(&back[..2], &[0xAB, 0xCD]);
        assert_eq!(back.len(), BLOCK_SIZE);
    }

    #[test]
    fn read_past_end_is_out_of_bounds() {
        let (_d, path) = temp_path();
        let bf = BlockFile::open(&path).unwrap();
        let err = bf.read_run(Run::new(10, 1)).unwrap_err();
        assert!(matches!(err, StorageError::OutOfBounds { .. }));
    }

    #[test]
    fn oversized_write_is_run_overflow() {
        let (_d, path) = temp_path();
        let bf = BlockFile::open(&path).unwrap();
        let big = vec![0u8; BLOCK_SIZE + 1];
        let err = bf.write_run(Run::new(1, 1), &big).unwrap_err();
        assert!(matches!(err, StorageError::RunOverflow { .. }));
    }

    #[test]
    fn bad_magic_is_rejected() {
        let (_d, path) = temp_path();
        {
            let f = std::fs::File::create(&path).unwrap();
            f.set_len(BLOCK_SIZE as u64).unwrap();
            f.write_all_at(b"NOTWAVE!", 0).unwrap();
        }
        assert!(matches!(
            BlockFile::open(&path).unwrap_err(),
            StorageError::BadMagic
        ));
    }

    #[test]
    fn wrong_version_is_rejected() {
        let (_d, path) = temp_path();
        {
            let f = std::fs::File::create(&path).unwrap();
            f.set_len(BLOCK_SIZE as u64).unwrap();
            f.write_all_at(MAGIC, 0).unwrap();
            f.write_all_at(
                &(FORMAT_VERSION + 1).to_le_bytes(),
                OFF_VERSION as u64,
            )
            .unwrap();
        }
        assert!(matches!(
            BlockFile::open(&path).unwrap_err(),
            StorageError::BadVersion(_)
        ));
    }
}
