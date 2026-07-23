//! Directory **chain blocks** — a type's page-address list persisted in
//! `data.bin` as linked 4 KiB blocks, copy-on-write.
//!
//! The journal's `Commit` frame carries only the 8-byte **root address**
//! per type; the addresses themselves live here. A commit writes a fresh
//! chain for a type whose directory changed (never in place — blocks the
//! last durable commit references are protected in the allocator) and
//! simply repeats the old root for an untouched type.
//!
//! ## Block layout
//!
//! ```text
//! [ len (u32 LE) ][ to_wire_checked(ChainNode) ]   (zero-padded to 4 KiB)
//! ChainNode { next: u64, prev: u64, addresses: Vec<u64> }
//! ```
//!
//! `next`/`prev` are block indices; `0` = none (block 0 is the write-once
//! superblock, so it can never be a chain node). `addresses` are the raw
//! `BlockDescriptor` words of the directory's buckets, in bucket order,
//! split across as many nodes as needed.

use wavedb_core::wire::{WaveWire, from_wire_checked, to_wire_checked};

use crate::alloc::BlockAllocator;
use crate::block::{BLOCK_SIZE, Run};
use crate::block_file::BlockFile;
use crate::error::{StorageError, StorageResult};

/// Per-block prefix: `payload_len (u32)`.
const LEN_PREFIX: usize = 4;

/// One linked block of a directory chain.
#[derive(Debug, Clone, PartialEq, Eq, WaveWire)]
struct ChainNode {
    next: u64,
    prev: u64,
    addresses: Vec<u64>,
}

/// Raw descriptor words one 4 KiB node can carry: the block minus the
/// framing (`len` + crc) and the node's fixed head (`next` + `prev` + the
/// vec length header).
const fn node_capacity() -> usize {
    // len(4) + crc(4) + next(8) + prev(8) + Vec stack header (len+offset).
    let overhead = LEN_PREFIX + 4 + 8 + 8 + 16;
    (BLOCK_SIZE - overhead) / 8
}

/// Write `addresses` as a fresh chain (copy-on-write: only newly allocated
/// blocks), returning the root block index. An empty list writes a single
/// empty node — the root still names the type as "settled, zero buckets"
/// (which `Directory` forbids anyway; callers pass ≥1 bucket).
///
/// # Errors
/// A write fault.
pub fn write_chain(
    file: &BlockFile,
    alloc: &mut BlockAllocator,
    addresses: &[u64],
) -> StorageResult<u64> {
    let per_node = node_capacity();
    let chunks: Vec<&[u64]> = if addresses.is_empty() {
        vec![&[]]
    } else {
        addresses.chunks(per_node).collect()
    };
    // Allocate every node up front so links can be written in one pass.
    let blocks: Vec<u64> =
        chunks.iter().map(|_| alloc.alloc(1).start).collect();
    for (i, chunk) in chunks.iter().enumerate() {
        let node = ChainNode {
            next: if i + 1 < blocks.len() {
                blocks[i + 1]
            } else {
                0
            },
            prev: if i > 0 { blocks[i - 1] } else { 0 },
            addresses: chunk.to_vec(),
        };
        let payload = to_wire_checked(&node);
        let mut bytes = Vec::with_capacity(LEN_PREFIX + payload.len());
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&payload);
        file.write_run(Run::new(blocks[i], 1), &bytes)?;
    }
    Ok(blocks[0])
}

/// Read a chain back from its root: the flattened addresses and the chain's
/// own block indices (the caller marks both used in the allocator).
///
/// # Errors
/// [`StorageError::Corrupt`] on an unreadable node or a cycle.
pub fn read_chain(
    file: &BlockFile,
    root: u64,
) -> StorageResult<(Vec<u64>, Vec<u64>)> {
    let mut addresses = Vec::new();
    let mut blocks = Vec::new();
    let mut at = root;
    while at != 0 {
        if blocks.contains(&at) {
            return Err(StorageError::Corrupt("directory chain cycle"));
        }
        blocks.push(at);
        let bytes = file.read_run(Run::new(at, 1))?;
        let node = decode_node(&bytes)?;
        addresses.extend_from_slice(&node.addresses);
        at = node.next;
    }
    Ok((addresses, blocks))
}

/// Parse one (zero-padded) chain block.
fn decode_node(buf: &[u8]) -> StorageResult<ChainNode> {
    let len_bytes: [u8; LEN_PREFIX] = buf
        .get(..LEN_PREFIX)
        .and_then(|s| s.try_into().ok())
        .ok_or(StorageError::Corrupt("chain node length truncated"))?;
    let len = u32::from_le_bytes(len_bytes) as usize;
    let end = LEN_PREFIX
        .checked_add(len)
        .filter(|&end| end <= buf.len())
        .ok_or(StorageError::Corrupt("chain node length out of range"))?;
    from_wire_checked(&buf[LEN_PREFIX..end])
        .map_err(|_| StorageError::Corrupt("chain node body"))
}

#[cfg(test)]
mod tests {
    use super::{node_capacity, read_chain, write_chain};
    use crate::alloc::BlockAllocator;
    use crate::block_file::{BlockFile, RESERVED_BLOCKS};

    fn backed() -> (tempfile::TempDir, BlockFile, BlockAllocator) {
        let d = tempfile::tempdir().unwrap();
        let bf = BlockFile::open(d.path().join("data.bin")).unwrap();
        let mut alloc = BlockAllocator::new();
        alloc.alloc(RESERVED_BLOCKS);
        (d, bf, alloc)
    }

    #[test]
    fn single_block_roundtrip() {
        let (_d, bf, mut alloc) = backed();
        let addrs: Vec<u64> = (100..150).collect();
        let root = write_chain(&bf, &mut alloc, &addrs).unwrap();
        assert_ne!(root, 0, "block 0 is the superblock");
        let (got, blocks) = read_chain(&bf, root).unwrap();
        assert_eq!(got, addrs);
        assert_eq!(blocks, vec![root], "fits one node");
    }

    #[test]
    fn multi_block_chain_links_and_roundtrips() {
        let (_d, bf, mut alloc) = backed();
        let n = node_capacity() * 2 + 7; // forces three nodes
        let addrs: Vec<u64> = (0..n as u64).map(|i| i * 3 + 1).collect();
        let root = write_chain(&bf, &mut alloc, &addrs).unwrap();
        let (got, blocks) = read_chain(&bf, root).unwrap();
        assert_eq!(got, addrs, "order preserved across nodes");
        assert_eq!(blocks.len(), 3);
    }

    #[test]
    fn cow_rewrite_leaves_the_old_chain_readable() {
        let (_d, bf, mut alloc) = backed();
        let v1: Vec<u64> = (1..40).collect();
        let root1 = write_chain(&bf, &mut alloc, &v1).unwrap();
        // A rewrite allocates fresh blocks — the old root must survive
        // (the last durable commit may still point at it).
        let v2: Vec<u64> = (100..160).collect();
        let root2 = write_chain(&bf, &mut alloc, &v2).unwrap();
        assert_ne!(root1, root2);
        assert_eq!(read_chain(&bf, root1).unwrap().0, v1);
        assert_eq!(read_chain(&bf, root2).unwrap().0, v2);
    }
}
