//! `#[wavedb]` produces a complete WaveDB object: identity, shape, `Wire`, and the
//! generated collection types for NonUnique.

// These tests deliberately assert on compile-time consts to document the
// generated surface; that is the point, not a mistake.
#![allow(clippy::assertions_on_constants)]

use wavedb_core::index::Pivot as _;
use wavedb_core::traits::{Shape, WaveDbStruct};
use wavedb_core::wire::{Wire, from_wire, to_wire};
use wavedb_macros::wavedb;

#[wavedb]
#[derive(Debug, PartialEq, Eq, Clone, Default)]
pub struct AboutUser {
    pub name: String,
    pub surname: String,
    pub phone: String,
}

#[wavedb(NonUnique)]
#[derive(Debug, PartialEq, Eq, Clone, Default)]
pub struct Note {
    pub body: String,
    pub pinned: bool,
}

#[wavedb(NonUnique)]
#[wavedb::pivot(amount)]
#[wavedb::pivot((customer, date))]
#[derive(Debug, PartialEq, Eq, Clone, Default)]
pub struct Order {
    pub amount: u64,
    pub customer: u64,
    pub date: u64,
}

#[test]
fn unique_identity_and_shape() {
    assert_eq!(AboutUser::SHAPE, Shape::Unique);
    assert_eq!(<AboutUser as WaveDbStruct>::SHAPE, Shape::Unique);
    assert_ne!(AboutUser::STRUCT_HASH, 0);
    assert_eq!(
        AboutUser::STRUCT_HASH,
        <AboutUser as WaveDbStruct>::STRUCT_HASH
    );
    assert!(!AboutUser::HAS_VALIDATE);
    assert!(!AboutUser::HAS_PREPROCESS);
}

#[test]
fn unique_pivot_id_is_unit() {
    // Unique types have no collection handle.
    let _: <AboutUser as WaveDbStruct>::PivotId = ();
}

#[test]
fn unique_wire_roundtrips() {
    let u = AboutUser {
        name: "Ada".into(),
        surname: "Lovelace".into(),
        phone: "555".into(),
    };
    let bytes = to_wire(&u);
    assert_eq!(from_wire::<AboutUser>(&bytes).unwrap(), u);
}

#[test]
fn nonunique_shape_and_generated_types() {
    assert_eq!(Note::SHAPE, Shape::NonUnique);

    // Generated handle + roots holder exist and the PivotId assoc type points at it.
    let handle = NotePivotId::new(wavedb_core::LocalId::new(7, false, 3));
    assert_eq!(handle.local_id().key(), 7);
    let _: <Note as WaveDbStruct>::PivotId = handle;

    let pivot = NotePivot::default();
    assert_eq!(pivot.current(), wavedb_core::LocalId::ZERO);
    assert_eq!(pivot.dead(), wavedb_core::LocalId::ZERO);
    assert_eq!(pivot.secondaries().len(), 0); // no #[wavedb::pivot]
}

#[test]
fn secondary_indexes_size_the_pivot() {
    // Order declares two secondary indexes → two extra roots.
    let pivot = OrderPivot::default();
    assert_eq!(pivot.secondaries().len(), 2);
}

#[test]
fn pivot_id_roundtrips() {
    let h = OrderPivotId::new(wavedb_core::LocalId::new(99, true, 1));
    let bytes = to_wire(&h);
    assert_eq!(bytes.len(), OrderPivotId::STACK_SIZE);
    assert_eq!(from_wire::<OrderPivotId>(&bytes).unwrap(), h);
}

#[test]
fn distinct_structs_have_distinct_hashes() {
    assert_ne!(AboutUser::STRUCT_HASH, Note::STRUCT_HASH);
    assert_ne!(Note::STRUCT_HASH, Order::STRUCT_HASH);
}

// A struct that references another collection by storing its PivotId (nesting).
#[wavedb]
#[derive(Debug, PartialEq, Eq, Clone, Default)]
pub struct Profile {
    pub username: String,
    pub notes: <Note as WaveDbStruct>::PivotId,
}

#[test]
fn nesting_via_pivot_id_field() {
    let p = Profile {
        username: "ada".into(),
        notes: NotePivotId::new(wavedb_core::LocalId::new(5, false, 0)),
    };
    let bytes = to_wire(&p);
    assert_eq!(from_wire::<Profile>(&bytes).unwrap(), p);
}
