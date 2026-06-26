//! End-to-end M1 smoke: `#[wavedb]` structs + a `build.rs`-generated registry,
//! spliced in with `include!`. Proves the whole foundation chain links and
//! round-trips without any node, transport, or `Db` — just core + macros + build.

use wavedb_macros::wavedb;

// The generated `Object` enum (the dispatch seam). Items resolve regardless of
// order, so splicing this above the struct definitions is fine.
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
    use wavedb_core::traits::Shape;
    use wavedb_core::wire::to_wire;

    // The registry *is* the `Object` enum: every declared struct's `STRUCT_HASH`
    // routes to its variant by `match`, decodes, and round-trips — no descriptor
    // table, no stored names.
    #[test]
    fn object_dispatch_roundtrips_every_struct() {
        let about = AboutUser {
            name: "Ada".into(),
            city: "London".into(),
        };
        let note = Note {
            body: "hi".into(),
            pinned: true,
        };
        let invoice = Invoice { cents: 42 };

        let about_bytes = to_wire(&about);
        assert!(matches!(
            Object::from_wire(AboutUser::STRUCT_HASH, &about_bytes),
            Ok(Object::AboutUser(ref d)) if *d == about
        ));
        assert!(matches!(
            Object::from_wire(Note::STRUCT_HASH, &to_wire(&note)),
            Ok(Object::Note(ref d)) if *d == note
        ));
        assert!(matches!(
            Object::from_wire(Invoice::STRUCT_HASH, &to_wire(&invoice)),
            Ok(Object::Invoice(ref d)) if *d == invoice
        ));

        // Encode side + identity.
        let obj = Object::AboutUser(about);
        assert_eq!(obj.struct_hash(), AboutUser::STRUCT_HASH);
        assert_eq!(obj.to_wire(), about_bytes);
    }

    // Shape is a compile-time `const` on the type — reached directly, no runtime
    // lookup.
    #[test]
    fn shape_is_a_const_not_a_lookup() {
        assert_eq!(AboutUser::SHAPE, Shape::Unique);
        assert_eq!(Note::SHAPE, Shape::NonUnique);
        assert_eq!(Invoice::SHAPE, Shape::Unique);
    }

    // An unknown hash is refused, and the error carries the `STRUCT_HASH` for
    // diagnostics. `Object`'s Debug also prints the hash, not a name.
    #[test]
    fn unknown_hash_errors_with_the_hash() {
        let err = Object::from_wire(0x1234_5678, &[]).unwrap_err();
        assert_eq!(err, wavedb_core::Error::UnknownStructHash(0x1234_5678));

        let dbg = format!("{:?}", Object::AboutUser(AboutUser::default()));
        assert!(dbg.contains("struct_hash"), "Object Debug shows the hash: {dbg}");
    }
}
