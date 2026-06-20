//! `PermissionRef` — the per-record access rule stored inline in [`Metadata`].
//!
//! Tenant-only access is the `Metadata.permission` `Option` being `None`; this
//! enum covers the rest. The wire encoding is the canonical enum form: a `u8`
//! tag, a `u32` payload length, and the variant's fields packed as a unit in the
//! heap (see `docs/wire_format.md`).
//!
//! [`Metadata`]: crate::metadata::Metadata

use crate::error::{Error, Result};
use crate::u48::U48;
use crate::wire::{Cursor, Wire};

/// Access rule for a record. `None` (tenant-only) lives in the `Option` around it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionRef {
    /// World-readable.
    Public,
    /// Readable by a specific list of other tenants.
    Tenants(Vec<U48>),
    /// Reference to a shared permission group. _Group management is deferred;_
    /// only the reference is encoded here.
    Group(u64),
}

// Tag bytes — stable across client and server because both compile this crate.
const TAG_PUBLIC: u8 = 0;
const TAG_TENANTS: u8 = 1;
const TAG_GROUP: u8 = 2;

impl Wire for PermissionRef {
    // tag (u8) + payload length (u32); the variant fields are a unit in the heap.
    const STACK_SIZE: usize = 1 + 4;

    fn heap_size(&self) -> usize {
        match self {
            Self::Public => 0,
            // The unit for `Tenants(Vec<U48>)` is the `Vec`'s own encoding:
            // its stack slot (the u32 region length) plus its heap region.
            Self::Tenants(v) => <Vec<U48> as Wire>::STACK_SIZE + v.heap_size(),
            Self::Group(_) => <u64 as Wire>::STACK_SIZE,
        }
    }

    fn encode_stack(&self, stack: &mut Vec<u8>) {
        let tag = match self {
            Self::Public => TAG_PUBLIC,
            Self::Tenants(_) => TAG_TENANTS,
            Self::Group(_) => TAG_GROUP,
        };
        stack.push(tag);
        // The payload length is exactly this value's heap contribution.
        stack.extend_from_slice(&(self.heap_size() as u32).to_le_bytes());
    }

    fn encode_heap(&self, heap: &mut Vec<u8>) {
        match self {
            Self::Public => {}
            Self::Tenants(v) => {
                v.encode_stack(heap);
                v.encode_heap(heap);
            }
            Self::Group(g) => g.encode_stack(heap),
        }
    }

    fn decode(stack: &mut Cursor, heap: &mut Cursor) -> Result<Self> {
        let tag = stack.u8()?;
        let payload_len = stack.u32()? as usize;
        let payload = heap.take(payload_len)?;
        match tag {
            TAG_PUBLIC => Ok(Self::Public),
            TAG_TENANTS => Ok(Self::Tenants(decode_unit(
                payload,
                <Vec<U48> as Wire>::STACK_SIZE,
            )?)),
            TAG_GROUP => Ok(Self::Group(decode_unit(
                payload,
                <u64 as Wire>::STACK_SIZE,
            )?)),
            other => Err(Error::InvalidTag(other)),
        }
    }
}

/// Decode a single value from a self-contained `[stack][heap]` unit, where the
/// stack section is the first `stack_size` bytes.
fn decode_unit<T: Wire>(unit: &[u8], stack_size: usize) -> Result<T> {
    if unit.len() < stack_size {
        return Err(Error::UnexpectedEof);
    }
    let (stack, heap) = unit.split_at(stack_size);
    let mut sc = Cursor::new(stack);
    let mut hc = Cursor::new(heap);
    T::decode(&mut sc, &mut hc)
}

#[cfg(test)]
mod tests {
    use super::PermissionRef;
    use crate::u48::U48;
    use crate::wire::{Wire, from_wire, to_wire};

    fn roundtrip(value: &PermissionRef) {
        let bytes = to_wire(value);
        assert_eq!(bytes.len(), PermissionRef::STACK_SIZE + value.heap_size());
        assert_eq!(from_wire::<PermissionRef>(&bytes).expect("decode"), *value);
    }

    #[test]
    fn variants_roundtrip() {
        roundtrip(&PermissionRef::Public);
        roundtrip(&PermissionRef::Tenants(vec![]));
        roundtrip(&PermissionRef::Tenants(vec![
            U48::from(1u32),
            U48::from_truncated(0xABCD_1234_5678),
        ]));
        roundtrip(&PermissionRef::Group(0xDEAD_BEEF));
    }

    #[test]
    fn bad_tag_rejected() {
        // tag 9, zero payload length.
        let bytes = [9u8, 0, 0, 0, 0];
        assert!(from_wire::<PermissionRef>(&bytes).is_err());
    }
}
