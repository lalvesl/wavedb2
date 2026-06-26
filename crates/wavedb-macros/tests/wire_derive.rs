//! `#[derive(WaveWire)]` round-trips through the core `Wire` format.

use wavedb_core::wire::{Wire, from_wire, to_wire};
use wavedb_macros::WaveWire;

#[derive(WaveWire, Debug, PartialEq, Clone)]
struct Named {
    a: u32,
    b: String,
    c: Vec<u16>,
    d: Option<u64>,
}

#[derive(WaveWire, Debug, PartialEq, Clone)]
struct Tuple(u8, String, bool);

#[derive(WaveWire, Debug, PartialEq, Clone)]
struct Unit;

fn roundtrip<T: Wire + PartialEq + std::fmt::Debug>(value: &T) {
    let bytes = to_wire(value);
    assert_eq!(bytes.len(), T::STACK_SIZE + value.heap_size());
    let back: T = from_wire(&bytes).expect("decode");
    assert_eq!(&back, value);
}

#[test]
fn named_struct_roundtrips() {
    roundtrip(&Named {
        a: 0xDEAD_BEEF,
        b: "wavedb".into(),
        c: vec![1, 2, 3],
        d: Some(42),
    });
    roundtrip(&Named {
        a: 0,
        b: String::new(),
        c: vec![],
        d: None,
    });
}

#[test]
fn tuple_struct_roundtrips() {
    roundtrip(&Tuple(7, "x".into(), true));
}

#[test]
fn unit_struct_roundtrips() {
    assert_eq!(Unit::STACK_SIZE, 0);
    roundtrip(&Unit);
}

#[test]
fn named_stack_size_is_field_sum() {
    // u32(4) + String(4) + Vec(4) + Option(1) = 13
    assert_eq!(Named::STACK_SIZE, 13);
}
