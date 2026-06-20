//! `Metadata` — the per-record header carried by every stored record: the
//! version chain, authorship, and the access rule.

use crate::error::Result;
use crate::local_id::LocalId;
use crate::permission::PermissionRef;
use crate::u48::U48;
use crate::wire::{Cursor, Wire};

/// Per-record metadata. Injected alongside the record body; serialised through
/// `Wire` like everything else.
///
/// Modification IDs and `pivot_id` are `Option<LocalId>`:
/// - `None` = no previous/next version, or a Unique record (no pivot).
/// - `LocalId` is 80-bit (`Id` with `TENANT` stripped) — the BpTree is
///   tenant-scoped so `TENANT` is derivable from context.
/// - `Option<T>` has `STACK_SIZE = 1` (flag only); the payload lands in the
///   heap section, so `None` costs exactly **1 byte** instead of 11.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Metadata {
    /// Previous version in the modification chain (`None` = first version).
    pub old_modification_id: Option<LocalId>,
    /// Next version (`None` = this is the live record).
    pub new_modification_id: Option<LocalId>,
    /// Owning Pivot back-link (`None` = Unique record).
    pub pivot_id: Option<LocalId>,
    /// Who wrote this version.
    pub user: U48,
    /// Which device produced it.
    pub device_created: u64,
    /// Access rule; `None` = tenant-only (the common case).
    pub permission: Option<PermissionRef>,
}

impl Wire for Metadata {
    const STACK_SIZE: usize = <Option<LocalId> as Wire>::STACK_SIZE        // old_modification_id  (1)
        + <Option<LocalId> as Wire>::STACK_SIZE        // new_modification_id  (1)
        + <Option<LocalId> as Wire>::STACK_SIZE        // pivot_id             (1)
        + <U48 as Wire>::STACK_SIZE                    // user                 (6)
        + <u64 as Wire>::STACK_SIZE                    // device_created       (8)
        + <Option<PermissionRef> as Wire>::STACK_SIZE; // permission           (1)
    // Stack total = 18 bytes. Heap grows only for fields that are Some.

    fn heap_size(&self) -> usize {
        self.old_modification_id.heap_size()
            + self.new_modification_id.heap_size()
            + self.pivot_id.heap_size()
            + self.permission.heap_size()
    }

    fn encode_stack(&self, stack: &mut Vec<u8>) {
        self.old_modification_id.encode_stack(stack);
        self.new_modification_id.encode_stack(stack);
        self.pivot_id.encode_stack(stack);
        self.user.encode_stack(stack);
        self.device_created.encode_stack(stack);
        self.permission.encode_stack(stack);
    }

    fn encode_heap(&self, heap: &mut Vec<u8>) {
        self.old_modification_id.encode_heap(heap);
        self.new_modification_id.encode_heap(heap);
        self.pivot_id.encode_heap(heap);
        self.user.encode_heap(heap);
        self.device_created.encode_heap(heap);
        self.permission.encode_heap(heap);
    }

    fn decode(stack: &mut Cursor, heap: &mut Cursor) -> Result<Self> {
        Ok(Self {
            old_modification_id: Option::<LocalId>::decode(stack, heap)?,
            new_modification_id: Option::<LocalId>::decode(stack, heap)?,
            pivot_id: Option::<LocalId>::decode(stack, heap)?,
            user: U48::decode(stack, heap)?,
            device_created: u64::decode(stack, heap)?,
            permission: Option::<PermissionRef>::decode(stack, heap)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Metadata;
    use crate::local_id::LocalId;
    use crate::permission::PermissionRef;
    use crate::u48::U48;
    use crate::wire::{Wire, from_wire, to_wire};

    fn roundtrip(m: Metadata) {
        let bytes = to_wire(&m);
        assert_eq!(bytes.len(), Metadata::STACK_SIZE + m.heap_size());
        assert_eq!(from_wire::<Metadata>(&bytes).expect("decode"), m);
    }

    #[test]
    fn default_is_tenant_only_first_version() {
        let m = Metadata::default();
        assert!(m.old_modification_id.is_none());
        assert!(m.new_modification_id.is_none());
        assert!(m.pivot_id.is_none());
        assert_eq!(m.user, U48::ZERO);
        assert_eq!(m.permission, None);
        roundtrip(m);
    }

    #[test]
    fn full_roundtrip() {
        roundtrip(Metadata {
            old_modification_id: Some(LocalId::new(7, false, 0)),
            new_modification_id: None,
            pivot_id: Some(LocalId::new(0xABCD, true, 3)),
            user: U48::from(42u32),
            device_created: 0xCAFE,
            permission: Some(PermissionRef::Tenants(vec![
                U48::from(1u32),
                U48::from(2u32),
            ])),
        });
        roundtrip(Metadata {
            old_modification_id: None,
            new_modification_id: Some(LocalId::new(99, false, 1)),
            pivot_id: None,
            user: U48::MAX,
            device_created: 1,
            permission: Some(PermissionRef::Public),
        });
    }

    #[test]
    fn unique_record_is_minimal() {
        // Unique: all Option fields None → stack=18, heap=0
        let m = Metadata::default();
        assert_eq!(Metadata::STACK_SIZE, 18);
        assert_eq!(m.heap_size(), 0);
        assert_eq!(to_wire(&m).len(), 18);
    }
}
