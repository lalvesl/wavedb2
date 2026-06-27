//! `PermissionRef` — the per-record access rule stored inline in [`Metadata`].
//!
//! Tenant-only access is the `Metadata.permission` `Option` being `None`; this
//! enum covers the rest. The wire encoding is the canonical enum form: a `u8`
//! tag, a `u32` payload length, and the variant's fields packed as a unit in the
//! heap (see `docs/wire_format.md`).
//!
//! [`Metadata`]: crate::metadata::Metadata

use crate::u48::U48;
use crate::wire::WaveWire;

/// Access rule for a record. `None` (tenant-only) lives in the `Option` around it.
///
/// `WaveWire` is derived. Because a variant carries fields, the encoding is the
/// canonical tag form — `tag (u8) + payload-length (u32)` in the stack, the
/// active variant's fields as a self-contained unit in the heap — with tags by
/// declaration order (`Public = 0`, `Tenants = 1`, `Group = 2`). Byte-identical
/// to the prior hand impl.
#[derive(Debug, Clone, PartialEq, Eq, WaveWire)]
pub enum PermissionRef {
    /// World-readable.
    Public,
    /// Readable by a specific list of other tenants.
    Tenants(Vec<U48>),
    /// Reference to a shared permission group. _Group management is deferred;_
    /// only the reference is encoded here.
    Group(u64),
}

#[cfg(test)]
mod tests {
    use super::PermissionRef;
    use crate::u48::U48;
    use crate::wire::{WaveWire, from_wire, to_wire};

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
