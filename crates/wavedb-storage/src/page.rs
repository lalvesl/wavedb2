//! [`SlotPage`] — the on-disk page format for a homogeneous run of records.
//!
//! A page holds records of **exactly one `STRUCT_HASH`** (Unique anchors, NonUnique
//! records, Pivots, `BpTree` nodes — the node encoding lives in `wavedb_core`'s
//! index layer and is stored here as ordinary bytes). The layout, little-endian
//! throughout:
//!
//! ```text
//! ┌───────────────────────────────────────────────────────────┐
//! │ crc32 (u32) │ struct_hash (u64) │ total_len (u32) │ N (u32) │  header (20 B)
//! ├───────────────────────────────────────────────────────────┤
//! │ id list:  [ id (u128) · offset (u32) · size (u32) ] × N     │  24 B / entry
//! ├───────────────────────────────────────────────────────────┤
//! │ blob:     [ record wire bytes … ]                           │
//! └───────────────────────────────────────────────────────────┘
//! ```
//!
//! `total_len` is the page's real byte length, so a page read back from a run of
//! whole blocks (padded with zeros past `total_len`) knows where its data ends.
//! `crc32` covers bytes `[4, total_len)` — struct_hash through blob — and is
//! verified on read. `offset` is relative to the start of the blob section.
//!
//! In memory a page is a `BTreeMap<u128, Vec<u8>>` keyed by the record `Id`'s raw
//! `u128`, so iteration is in `Id` order (which, `KEY`-first, is type/time order)
//! and serialisation is deterministic.

use std::collections::BTreeMap;

use wavedb_core::Id;

use crate::block::BLOCK_SIZE;
use crate::error::{StorageError, StorageResult};

/// Header: `crc32 (4) + struct_hash (8) + total_len (4) + entry_count (4)`.
const HEADER_LEN: usize = 20;
/// One id-list entry: `id (16) + offset (4) + size (4)`.
const ENTRY_LEN: usize = 24;

const OFF_STRUCT_HASH: usize = 4;
const OFF_TOTAL_LEN: usize = 12;
const OFF_COUNT: usize = 16;

/// A homogeneous in-memory page of records, all of one `STRUCT_HASH`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotPage {
    struct_hash: u64,
    /// `Id.raw()` → wire-encoded record bytes, ordered by `Id`.
    records: BTreeMap<u128, Vec<u8>>,
}

impl SlotPage {
    /// A new, empty page for `struct_hash`.
    #[must_use]
    pub const fn new(struct_hash: u64) -> Self {
        Self {
            struct_hash,
            records: BTreeMap::new(),
        }
    }

    /// The `STRUCT_HASH` every record in this page shares.
    #[must_use]
    pub const fn struct_hash(&self) -> u64 {
        self.struct_hash
    }

    /// Number of records held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// `true` if the page holds no records.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Insert or overwrite the record stored at `id`.
    pub fn upsert(&mut self, id: Id, bytes: Vec<u8>) {
        self.records.insert(id.raw(), bytes);
    }

    /// The record bytes stored at `id`, if present.
    #[must_use]
    pub fn get(&self, id: Id) -> Option<&[u8]> {
        self.records.get(&id.raw()).map(Vec::as_slice)
    }

    /// Remove the record at `id`, returning its bytes if it was present.
    pub fn remove(&mut self, id: Id) -> Option<Vec<u8>> {
        self.records.remove(&id.raw())
    }

    /// Iterate `(Id, bytes)` in `Id` order.
    pub fn iter(&self) -> impl Iterator<Item = (Id, &[u8])> {
        self.records
            .iter()
            .map(|(&raw, bytes)| (Id::from_raw(raw), bytes.as_slice()))
    }

    /// Consume the page into its `(Id, bytes)` entries, in `Id` order.
    pub fn into_entries(self) -> impl Iterator<Item = (Id, Vec<u8>)> {
        self.records
            .into_iter()
            .map(|(raw, bytes)| (Id::from_raw(raw), bytes))
    }

    /// Serialised byte length of this page (header + id list + blob).
    #[must_use]
    pub fn byte_len(&self) -> usize {
        let blob: usize = self.records.values().map(Vec::len).sum();
        HEADER_LEN + self.records.len() * ENTRY_LEN + blob
    }

    /// Number of [`BLOCK_SIZE`] blocks this page needs (`ceil`).
    #[must_use]
    pub fn blocks_needed(&self) -> u64 {
        self.byte_len().div_ceil(BLOCK_SIZE) as u64
    }

    /// Serialise to bytes, computing and prefixing the crc32.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.byte_len());
        buf.extend_from_slice(&[0u8; 4]); // crc32 placeholder
        buf.extend_from_slice(&self.struct_hash.to_le_bytes());
        buf.extend_from_slice(&[0u8; 4]); // total_len placeholder
        buf.extend_from_slice(&(self.records.len() as u32).to_le_bytes());

        // Id list: offsets are relative to the blob start, filled as we go.
        let mut running = 0u32;
        for (&raw, bytes) in &self.records {
            buf.extend_from_slice(&raw.to_le_bytes());
            buf.extend_from_slice(&running.to_le_bytes());
            buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            running += bytes.len() as u32;
        }
        // Blob, in the same Id order.
        for bytes in self.records.values() {
            buf.extend_from_slice(bytes);
        }

        let total_len = buf.len() as u32;
        buf[OFF_TOTAL_LEN..OFF_TOTAL_LEN + 4]
            .copy_from_slice(&total_len.to_le_bytes());
        let crc = crc32fast::hash(&buf[4..]);
        buf[..4].copy_from_slice(&crc.to_le_bytes());
        buf
    }

    /// Parse a page from bytes, verifying the crc32 and all internal bounds.
    ///
    /// # Errors
    /// [`StorageError::Corrupt`] on a crc mismatch, a truncated buffer, or an
    /// id-list entry that points outside the blob.
    pub fn from_bytes(buf: &[u8]) -> StorageResult<Self> {
        if buf.len() < HEADER_LEN {
            return Err(StorageError::Corrupt("page header truncated"));
        }
        let total_len = u32::from_le_bytes(
            buf[OFF_TOTAL_LEN..OFF_TOTAL_LEN + 4].try_into().unwrap(),
        ) as usize;
        if total_len < HEADER_LEN || total_len > buf.len() {
            return Err(StorageError::Corrupt("page total_len out of range"));
        }
        let buf = &buf[..total_len]; // ignore run padding past the real page
        let stored_crc = u32::from_le_bytes(buf[..4].try_into().unwrap());
        if crc32fast::hash(&buf[4..]) != stored_crc {
            return Err(StorageError::Corrupt("page crc mismatch"));
        }
        let struct_hash = u64::from_le_bytes(
            buf[OFF_STRUCT_HASH..OFF_STRUCT_HASH + 8]
                .try_into()
                .unwrap(),
        );
        let count = u32::from_le_bytes(
            buf[OFF_COUNT..OFF_COUNT + 4].try_into().unwrap(),
        ) as usize;

        let id_list_end =
            HEADER_LEN
                .checked_add(count.checked_mul(ENTRY_LEN).ok_or(
                    StorageError::Corrupt("page entry count overflow"),
                )?)
                .ok_or(StorageError::Corrupt("page entry count overflow"))?;
        if buf.len() < id_list_end {
            return Err(StorageError::Corrupt("page id list truncated"));
        }
        let blob = &buf[id_list_end..];

        let mut records = BTreeMap::new();
        for i in 0..count {
            let at = HEADER_LEN + i * ENTRY_LEN;
            let raw = u128::from_le_bytes(buf[at..at + 16].try_into().unwrap());
            let offset =
                u32::from_le_bytes(buf[at + 16..at + 20].try_into().unwrap())
                    as usize;
            let size =
                u32::from_le_bytes(buf[at + 20..at + 24].try_into().unwrap())
                    as usize;
            let end = offset
                .checked_add(size)
                .ok_or(StorageError::Corrupt("page entry size overflow"))?;
            if end > blob.len() {
                return Err(StorageError::Corrupt("page entry past blob"));
            }
            records.insert(raw, blob[offset..end].to_vec());
        }

        Ok(Self {
            struct_hash,
            records,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::SlotPage;
    use crate::block::BLOCK_SIZE;
    use crate::error::StorageError;
    use wavedb_core::{Id, U48};

    fn id(key: u64) -> Id {
        Id::new(key, U48::from(1u32), false, key as u16)
    }

    #[test]
    fn upsert_get_remove() {
        let mut p = SlotPage::new(0xABCD);
        assert!(p.is_empty());
        p.upsert(id(1), vec![1, 2, 3]);
        p.upsert(id(2), vec![4, 5]);
        assert_eq!(p.len(), 2);
        assert_eq!(p.get(id(1)), Some(&[1, 2, 3][..]));
        // Overwrite.
        p.upsert(id(1), vec![9]);
        assert_eq!(p.get(id(1)), Some(&[9][..]));
        assert_eq!(p.remove(id(2)), Some(vec![4, 5]));
        assert_eq!(p.get(id(2)), None);
    }

    #[test]
    fn roundtrip_through_bytes() {
        let mut p = SlotPage::new(0x1122_3344_5566_7788);
        p.upsert(id(10), vec![0xAA; 5]);
        p.upsert(id(20), b"hello world".to_vec());
        p.upsert(id(30), vec![]); // empty record is legal
        let bytes = p.to_bytes();
        assert_eq!(bytes.len(), p.byte_len());
        let back = SlotPage::from_bytes(&bytes).unwrap();
        assert_eq!(back, p);
        assert_eq!(back.struct_hash(), 0x1122_3344_5566_7788);
    }

    #[test]
    fn iteration_is_in_id_order() {
        let mut p = SlotPage::new(1);
        p.upsert(id(30), vec![3]);
        p.upsert(id(10), vec![1]);
        p.upsert(id(20), vec![2]);
        let keys: Vec<u64> = p.iter().map(|(i, _)| i.key()).collect();
        assert_eq!(keys, vec![10, 20, 30]);
    }

    #[test]
    fn empty_page_roundtrips() {
        let p = SlotPage::new(7);
        let back = SlotPage::from_bytes(&p.to_bytes()).unwrap();
        assert!(back.is_empty());
        assert_eq!(back.struct_hash(), 7);
    }

    #[test]
    fn crc_mismatch_detected() {
        let mut p = SlotPage::new(1);
        p.upsert(id(1), vec![1, 2, 3]);
        let mut bytes = p.to_bytes();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF; // corrupt the blob
        assert!(matches!(
            SlotPage::from_bytes(&bytes),
            Err(StorageError::Corrupt(_))
        ));
    }

    #[test]
    fn truncated_buffer_detected() {
        assert!(matches!(
            SlotPage::from_bytes(&[0u8; 4]),
            Err(StorageError::Corrupt(_))
        ));
    }

    #[test]
    fn reads_from_padded_run() {
        // A page on disk occupies whole blocks; the tail past total_len is zero
        // padding. from_bytes must ignore it and still verify the crc.
        let mut p = SlotPage::new(5);
        p.upsert(id(1), vec![1, 2, 3]);
        p.upsert(id(2), b"abc".to_vec());
        let mut bytes = p.to_bytes();
        bytes.resize(BLOCK_SIZE * 2, 0);
        let back = SlotPage::from_bytes(&bytes).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn blocks_needed_rounds_up() {
        let mut p = SlotPage::new(1);
        // Small page fits in one block.
        p.upsert(id(1), vec![0u8; 10]);
        assert_eq!(p.blocks_needed(), 1);
        // Push it just past one block.
        p.upsert(id(2), vec![0u8; BLOCK_SIZE]);
        assert_eq!(p.blocks_needed(), 2);
    }
}
