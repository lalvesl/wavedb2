//! M1 smoke: what the `#[wavedb]` derive alone guarantees, proven end-to-end
//! without any node, transport, or `Db` — `STRUCT_HASH` identity, `WaveWire`
//! round-trips, shape consts, and the generated NonUnique collection types.
//!
//! The former `build.rs` + `include!` registry (`wavedb-build`) is removed;
//! wire reachability will come from the explicit `expose_server!` /
//! `expose_client!` declarations once those land.

use wavedb_macros::wavedb;

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

/// A struct in a submodule — items are named by path, not found by a scanner.
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
    use super::{AboutUser, Note, NotePivot, NotePivotId};
    use wavedb_core::traits::Shape;
    use wavedb_core::wire::{from_wire, to_wire};
    use wavedb_core::{LocalId, WaveDbStruct};

    // Every declared struct round-trips through its derive-emitted WaveWire
    // impl, and its STRUCT_HASH is a distinct compile-time const.
    #[test]
    fn derived_structs_roundtrip_and_hashes_differ() {
        let about = AboutUser {
            name: "Ada".into(),
            city: "London".into(),
        };
        let note = Note {
            body: "hi".into(),
            pinned: true,
        };
        let invoice = Invoice { cents: 42 };

        assert_eq!(from_wire::<AboutUser>(&to_wire(&about)), Ok(about));
        assert_eq!(from_wire::<Note>(&to_wire(&note)), Ok(note));
        assert_eq!(from_wire::<Invoice>(&to_wire(&invoice)), Ok(invoice));

        assert_ne!(AboutUser::STRUCT_HASH, Note::STRUCT_HASH);
        assert_ne!(AboutUser::STRUCT_HASH, Invoice::STRUCT_HASH);
        assert_ne!(Note::STRUCT_HASH, Invoice::STRUCT_HASH);
    }

    // Shape is a compile-time `const` on the type — no runtime lookup.
    #[test]
    fn shape_is_a_const_not_a_lookup() {
        assert_eq!(AboutUser::SHAPE, Shape::Unique);
        assert_eq!(Note::SHAPE, Shape::NonUnique);
        assert_eq!(Invoice::SHAPE, Shape::Unique);
    }

    // The NonUnique derive emits the collection machinery: a typed PivotId
    // handle and a Pivot with current/dead roots plus one secondary slot per
    // `#[wavedb::pivot(...)]`.
    #[test]
    fn nonunique_generates_pivot_types() {
        let pivot = NotePivot {
            current: LocalId::new(10, false, 1),
            dead: LocalId::new(20, false, 2),
            ..NotePivot::default()
        };
        assert_eq!(pivot.secondaries.len(), 1, "one #[wavedb::pivot(...)]");
        assert_eq!(from_wire::<NotePivot>(&to_wire(&pivot)), Ok(pivot));

        // The typed handle is what a holder stores to reference the collection.
        let handle: <Note as WaveDbStruct>::PivotId =
            NotePivotId::new(LocalId::new(7, false, 0));
        assert_eq!(handle.local_id(), LocalId::new(7, false, 0));
    }
}
