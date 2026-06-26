//! End-to-end M1 smoke: `#[wavedb]` structs + a `build.rs`-generated registry,
//! spliced in with `include!`. Proves the whole foundation chain links and
//! round-trips without any node, transport, or `Db` — just core + macros + build.

use wavedb_macros::wavedb;

// The generated `Object` enum + `REGISTRY`. Items resolve regardless of order, so
// splicing this above the struct definitions is fine.
include!(concat!(env!("OUT_DIR"), "/wavedb_registry.rs"));

/// Unique: one live record per tenant.
#[wavedb]
#[derive(Debug, PartialEq, Eq, Clone, Default)]
pub struct AboutUser {
    pub name: String,
    pub city: String,
}

/// NonUnique: many per tenant, with a secondary index on `pinned`.
#[wavedb(NonUnique)]
#[wavedb::pivot(pinned)]
#[derive(Debug, PartialEq, Eq, Clone, Default)]
pub struct Note {
    pub body: String,
    pub pinned: bool,
}

/// A struct in a submodule — exercises the scanner's module-path resolution.
pub mod billing {
    use wavedb_macros::wavedb;

    #[wavedb]
    #[derive(Debug, PartialEq, Eq, Clone, Default)]
    pub struct Invoice {
        pub cents: u64,
    }
}

#[cfg(test)]
mod tests {
    use super::billing::Invoice;
    use super::{AboutUser, Note, Object};
    use wavedb_core::registry::ObjectRegistry;
    use wavedb_core::traits::Shape;
    use wavedb_core::wire::to_wire;

    #[test]
    fn registry_resolves_every_declared_struct() {
        assert_eq!(
            super::REGISTRY.descriptor(AboutUser::STRUCT_HASH).unwrap().name,
            "AboutUser"
        );
        assert_eq!(
            super::REGISTRY.descriptor(Note::STRUCT_HASH).unwrap().shape,
            Shape::NonUnique
        );
        assert_eq!(
            super::REGISTRY.descriptor(Invoice::STRUCT_HASH).unwrap().name,
            "Invoice"
        );
        assert!(super::REGISTRY.descriptor(0xDEAD_BEEF).is_none());
    }

    #[test]
    fn object_dispatch_roundtrips() {
        let u = AboutUser {
            name: "Ada".into(),
            city: "London".into(),
        };
        let bytes = to_wire(&u);

        let obj = Object::from_wire(AboutUser::STRUCT_HASH, &bytes).unwrap();
        let Object::AboutUser(decoded) = obj else {
            panic!("wrong variant");
        };
        assert_eq!(decoded, u);

        let obj = Object::AboutUser(u);
        assert_eq!(obj.struct_hash(), AboutUser::STRUCT_HASH);
        assert_eq!(obj.to_wire(), bytes);
    }

    #[test]
    fn unknown_hash_is_an_error() {
        // `Object` is intentionally underived, so `matches!` rather than `unwrap_err`.
        assert!(matches!(
            Object::from_wire(0x1234_5678, &[]),
            Err(wavedb_core::Error::UnknownStructHash(0x1234_5678))
        ));
    }
}
