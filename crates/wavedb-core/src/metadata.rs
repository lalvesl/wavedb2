//! `Metadata` — the per-record header carried by every stored record: the
//! version chain, authorship, and the access rule.

use crate::error::Result;
use crate::permission::PermissionRef;
use crate::u48::U48;
use crate::wire::{Cursor, Wire};

/// Per-record metadata. Injected alongside the record body; serialised through
/// `Wire` like everything else.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Metadata {
    /// Previous version in the modification chain (`0` = first).
    pub old_modification_id: u128,
    /// Next version (`0` = this is the live record).
    pub new_modification_id: u128,
    /// Who wrote this version.
    pub user: U48,
    /// Which device produced it.
    pub device_created: u64,
    /// Access rule; `None` = tenant-only (the common case).
    pub permission: Option<PermissionRef>,
}

impl Wire for Metadata {
    const STACK_SIZE: usize = <u128 as Wire>::STACK_SIZE        // old_modification_id
        + <u128 as Wire>::STACK_SIZE                            // new_modification_id
        + <U48 as Wire>::STACK_SIZE                             // user
        + <u64 as Wire>::STACK_SIZE                             // device_created
        + <Option<PermissionRef> as Wire>::STACK_SIZE;         // permission

    fn heap_size(&self) -> usize {
        // Only the permission field can carry heap bytes.
        self.permission.heap_size()
    }

    fn encode_stack(&self, stack: &mut Vec<u8>) {
        self.old_modification_id.encode_stack(stack);
        self.new_modification_id.encode_stack(stack);
        self.user.encode_stack(stack);
        self.device_created.encode_stack(stack);
        self.permission.encode_stack(stack);
    }

    fn encode_heap(&self, heap: &mut Vec<u8>) {
        self.old_modification_id.encode_heap(heap);
        self.new_modification_id.encode_heap(heap);
        self.user.encode_heap(heap);
        self.device_created.encode_heap(heap);
        self.permission.encode_heap(heap);
    }

    fn decode(stack: &mut Cursor, heap: &mut Cursor) -> Result<Self> {
        Ok(Self {
            old_modification_id: u128::decode(stack, heap)?,
            new_modification_id: u128::decode(stack, heap)?,
            user: U48::decode(stack, heap)?,
            device_created: u64::decode(stack, heap)?,
            permission: Option::<PermissionRef>::decode(stack, heap)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Metadata;
    use crate::permission::PermissionRef;
    use crate::u48::U48;
    use crate::wire::{from_wire, to_wire, Wire};

    fn roundtrip(m: Metadata) {
        let bytes = to_wire(&m);
        assert_eq!(bytes.len(), Metadata::STACK_SIZE + m.heap_size());
        assert_eq!(from_wire::<Metadata>(&bytes).expect("decode"), m);
    }

    #[test]
    fn default_is_tenant_only_first_version() {
        let m = Metadata::default();
        assert_eq!(m.old_modification_id, 0);
        assert_eq!(m.new_modification_id, 0);
        assert_eq!(m.user, U48::ZERO);
        assert_eq!(m.permission, None);
        roundtrip(m);
    }

    #[test]
    fn full_roundtrip() {
        roundtrip(Metadata {
            old_modification_id: 7,
            new_modification_id: 0,
            user: U48::from(42u32),
            device_created: 0xCAFE,
            permission: Some(PermissionRef::Tenants(vec![U48::from(1u32), U48::from(2u32)])),
        });
        roundtrip(Metadata {
            old_modification_id: 0,
            new_modification_id: 99,
            user: U48::MAX,
            device_created: 1,
            permission: Some(PermissionRef::Public),
        });
    }
}
