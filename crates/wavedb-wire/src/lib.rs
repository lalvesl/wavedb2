//! `wavedb-wire` — the standalone `WaveWire` (de)serialization format.
//!
//! Pure value ⇄ bytes. It knows **nothing** about `STRUCT_HASH`, the registry,
//! `Id`, permissions, or the engine — it is just a deterministic, platform-
//! independent codec. [`to_wire`] turns a value into a `Vec<u8>`; [`from_wire`]
//! turns bytes back into a value of a **caller-chosen** type. There is no type
//! tag on the bytes and no type validation on decode: `from_wire::<T>` simply
//! tries to read a `T` out of the buffer. The only thing that can go wrong is the
//! buffer not lining up with the type's layout — i.e. running short of bytes
//! ([`Error::UnexpectedEof`]) — plus the few intrinsic per-type checks (a `bool`
//! byte that isn't `0`/`1`, a non-scalar `char`, non-UTF-8 string bytes, an
//! out-of-range enum tag).
//!
//! A serialised value is two contiguous sections: a fixed-size **stack** section
//! (`T::STACK_SIZE` bytes, every offset a compile-time constant) followed by a
//! variable **heap** section. Dynamic fields keep a fixed slot in the stack
//! section (a `u32` length and/or flag byte) and put their payload in the heap.
//!
//! See `docs/wire_format.md` for the full specification. Two composition modes:
//!
//! - **Flattened** (struct fields, `Option`'s inner): stack slots are emitted
//!   inline into the parent stack; heap payloads append to the shared heap.
//! - **Unit** (each `Vec` element): the value is self-contained
//!   `[stack][heap]` back-to-back inside the parent's heap region.

// Pedantic/nursery lints that fight terse, byte-precise (de)serialization code.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_lossless,
    clippy::cast_sign_loss,
    clippy::missing_const_for_fn
)]

// The derive emits absolute `::wavedb_wire::` paths; this lets the crate use its
// own derive (e.g. in tests).
extern crate self as wavedb_wire;

use thiserror::Error;

/// Derive a [`WaveWire`] impl for a struct or enum. Re-exported from
/// [`wavedb-wire-derive`](wavedb_wire_derive); see its docs for the supported
/// shapes (structs: named/tuple/unit; enums: canonical tag form).
pub use wavedb_wire_derive::WaveWire;

/// A wire (de)serialization fault.
///
/// Every variant is a layout/shape mismatch between the bytes and the type the
/// caller asked to decode — never a "wrong type" error, because the wire bytes
/// carry no type identity. The dominant case is [`UnexpectedEof`](Error::UnexpectedEof):
/// the buffer didn't match the size the type needs.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum Error {
    /// A reader ran past the end of its buffer — the bytes didn't match the
    /// size the decoded type expected.
    #[error("unexpected end of wire buffer")]
    UnexpectedEof,
    /// A `String` field held bytes that were not valid UTF-8.
    #[error("invalid utf-8 in wire string")]
    Utf8,
    /// A `char` field held a `u32` that is not a Unicode scalar value.
    #[error("invalid char scalar {0:#x}")]
    InvalidChar(u32),
    /// An enum field held a tag outside the declared variant range.
    #[error("invalid enum tag {0}")]
    InvalidTag(u8),
    /// A `bool` field held a byte other than `0` or `1`.
    #[error("invalid bool byte {0}")]
    InvalidBool(u8),
    /// A [`from_wire_checked`] buffer's CRC32 prefix did not match its payload —
    /// the bytes were corrupted or truncated in a way that still parses.
    #[cfg(feature = "validation")]
    #[error("crc mismatch: stored {stored:#010x}, computed {computed:#010x}")]
    CrcMismatch {
        /// The CRC32 read from the buffer's 4-byte prefix.
        stored: u32,
        /// The CRC32 computed over the payload that followed it.
        computed: u32,
    },
}

/// Shorthand for a `Result` carrying the wire [`Error`].
pub type Result<T> = core::result::Result<T, Error>;

/// A forward-only byte cursor over a borrowed buffer, used for both the stack
/// and heap sections during decode.
pub struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    /// Wrap a byte slice.
    #[must_use]
    pub const fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Bytes consumed so far — how `Vec` decode finds where one unit ends.
    #[must_use]
    pub const fn pos(&self) -> usize {
        self.pos
    }

    /// Bytes left in the buffer.
    #[must_use]
    pub const fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Consume exactly `n` bytes, or [`Error::UnexpectedEof`].
    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(Error::UnexpectedEof);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    /// Read a little-endian `u8`.
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    /// Read a little-endian `u32` length slot.
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
}

/// A type with a deterministic, platform-independent wire layout.
///
/// Implementors guarantee that [`encode_stack`](WaveWire::encode_stack) writes
/// **exactly** `STACK_SIZE` bytes and that [`decode`](WaveWire::decode) reads exactly
/// `STACK_SIZE` bytes from its stack cursor.
pub trait WaveWire: Sized {
    /// Fixed number of bytes this type occupies in the stack section.
    const STACK_SIZE: usize;

    /// Number of heap bytes this value contributes (so a writer can reserve the
    /// whole buffer in one allocation).
    fn heap_size(&self) -> usize;

    /// Append exactly `STACK_SIZE` bytes to the stack buffer.
    fn encode_stack(&self, stack: &mut Vec<u8>);

    /// Append this value's heap payload (depth-first) to the heap buffer.
    fn encode_heap(&self, heap: &mut Vec<u8>);

    /// Reconstruct a value, reading `STACK_SIZE` bytes from `stack` and any
    /// payload from `heap`.
    fn decode(stack: &mut Cursor, heap: &mut Cursor) -> Result<Self>;
}

/// Serialise a value to a single `[stack][heap]` byte vector in one allocation.
///
/// `STACK_SIZE` (a compile-time sum over every field, including nested `WaveWire`
/// types) plus the recursively-computed `heap_size()` give the exact final length
/// up front. `encode_stack` appends the `STACK_SIZE`-byte stack section, then
/// `encode_heap` appends the heap section to the same buffer — no second
/// allocation, no concat.
#[must_use]
pub fn to_wire<T: WaveWire>(value: &T) -> Vec<u8> {
    let mut buf = Vec::with_capacity(T::STACK_SIZE + value.heap_size());
    value.encode_stack(&mut buf);
    value.encode_heap(&mut buf);
    debug_assert_eq!(buf.len(), T::STACK_SIZE + value.heap_size());
    buf
}

/// Deserialise a value from a `[stack][heap]` byte slice.
///
/// No type tag is consulted — the caller picks `T`, and decode just tries to read
/// a `T`. A buffer shorter than `T::STACK_SIZE` is the size mismatch that fails as
/// [`Error::UnexpectedEof`].
pub fn from_wire<T: WaveWire>(bytes: &[u8]) -> Result<T> {
    if bytes.len() < T::STACK_SIZE {
        return Err(Error::UnexpectedEof);
    }
    let (stack, heap) = bytes.split_at(T::STACK_SIZE);
    let mut sc = Cursor::new(stack);
    let mut hc = Cursor::new(heap);
    T::decode(&mut sc, &mut hc)
}

// ---- checked encoding (crc32-framed) — feature `validation` -----------------

/// Framing bytes [`to_wire_checked`] prepends: one little-endian `u32` CRC32 of
/// the `[stack][heap]` payload that follows.
#[cfg(feature = "validation")]
pub const CRC_PREFIX_LEN: usize = 4;

/// Serialise like [`to_wire`], prefixed with a CRC32 of the payload:
/// `[crc32 (4 B LE)][stack][heap]`.
///
/// Still a single allocation: the CRC slot is reserved up front and patched
/// after the payload is written. Decode with [`from_wire_checked`] — plain
/// [`from_wire`] on this buffer would read the CRC bytes as the value's stack.
#[cfg(feature = "validation")]
#[must_use]
pub fn to_wire_checked<T: WaveWire>(value: &T) -> Vec<u8> {
    let mut buf =
        Vec::with_capacity(CRC_PREFIX_LEN + T::STACK_SIZE + value.heap_size());
    buf.extend_from_slice(&[0u8; CRC_PREFIX_LEN]); // slot, patched below
    value.encode_stack(&mut buf);
    value.encode_heap(&mut buf);
    let crc = crc32fast::hash(&buf[CRC_PREFIX_LEN..]);
    buf[..CRC_PREFIX_LEN].copy_from_slice(&crc.to_le_bytes());
    buf
}

/// Deserialise a [`to_wire_checked`] buffer: verify the 4-byte CRC32 prefix
/// against the payload, then decode like [`from_wire`].
///
/// A buffer shorter than the prefix fails as [`Error::UnexpectedEof`]; a prefix
/// that doesn't match the payload as [`Error::CrcMismatch`].
#[cfg(feature = "validation")]
pub fn from_wire_checked<T: WaveWire>(bytes: &[u8]) -> Result<T> {
    if bytes.len() < CRC_PREFIX_LEN {
        return Err(Error::UnexpectedEof);
    }
    let (prefix, payload) = bytes.split_at(CRC_PREFIX_LEN);
    let stored = u32::from_le_bytes(prefix.try_into().unwrap());
    let computed = crc32fast::hash(payload);
    if stored != computed {
        return Err(Error::CrcMismatch { stored, computed });
    }
    from_wire(payload)
}

// ---- fixed-width scalars ----------------------------------------------------

macro_rules! wire_le {
    ($($t:ty),* $(,)?) => {$(
        impl WaveWire for $t {
            const STACK_SIZE: usize = size_of::<$t>();
            fn heap_size(&self) -> usize { 0 }
            fn encode_stack(&self, stack: &mut Vec<u8>) {
                stack.extend_from_slice(&self.to_le_bytes());
            }
            fn encode_heap(&self, _heap: &mut Vec<u8>) {}
            fn decode(stack: &mut Cursor, _heap: &mut Cursor) -> Result<Self> {
                Ok(<$t>::from_le_bytes(stack.take(size_of::<$t>())?.try_into().unwrap()))
            }
        }
    )*};
}

wire_le! {
    u8, u16, u32, u64, u128,
    i8, i16, i32, i64, i128,
    f32, f64,
}

impl WaveWire for bool {
    const STACK_SIZE: usize = 1;
    fn heap_size(&self) -> usize {
        0
    }
    fn encode_stack(&self, stack: &mut Vec<u8>) {
        stack.push(u8::from(*self));
    }
    fn encode_heap(&self, _heap: &mut Vec<u8>) {}
    fn decode(stack: &mut Cursor, _heap: &mut Cursor) -> Result<Self> {
        match stack.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(Error::InvalidBool(other)),
        }
    }
}

impl WaveWire for char {
    const STACK_SIZE: usize = size_of::<Self>();
    fn heap_size(&self) -> usize {
        0
    }
    fn encode_stack(&self, stack: &mut Vec<u8>) {
        stack.extend_from_slice(&(*self as u32).to_le_bytes());
    }
    fn encode_heap(&self, _heap: &mut Vec<u8>) {}
    fn decode(stack: &mut Cursor, _heap: &mut Cursor) -> Result<Self> {
        let scalar = stack.u32()?;
        Self::from_u32(scalar).ok_or(Error::InvalidChar(scalar))
    }
}

// ---- dynamic types ----------------------------------------------------------

impl WaveWire for String {
    const STACK_SIZE: usize = size_of::<u32>(); // byte-length
    fn heap_size(&self) -> usize {
        self.len()
    }
    fn encode_stack(&self, stack: &mut Vec<u8>) {
        stack.extend_from_slice(&(self.len() as u32).to_le_bytes());
    }
    fn encode_heap(&self, heap: &mut Vec<u8>) {
        heap.extend_from_slice(self.as_bytes());
    }
    fn decode(stack: &mut Cursor, heap: &mut Cursor) -> Result<Self> {
        let len = stack.u32()? as usize;
        let bytes = heap.take(len)?;
        Self::from_utf8(bytes.to_vec()).map_err(|_| Error::Utf8)
    }
}

impl<T: WaveWire> WaveWire for Vec<T> {
    const STACK_SIZE: usize = size_of::<u32>(); // byte-length
    fn heap_size(&self) -> usize {
        // Each element is a self-contained unit: its stack bytes then its heap.
        self.iter().map(|e| T::STACK_SIZE + e.heap_size()).sum()
    }
    fn encode_stack(&self, stack: &mut Vec<u8>) {
        stack.extend_from_slice(&(self.heap_size() as u32).to_le_bytes());
    }
    fn encode_heap(&self, heap: &mut Vec<u8>) {
        for e in self {
            e.encode_stack(heap);
            e.encode_heap(heap);
        }
    }
    fn decode(stack: &mut Cursor, heap: &mut Cursor) -> Result<Self> {
        let region_len = stack.u32()? as usize;
        let region = heap.take(region_len)?;
        let mut out = Self::new();
        let mut cur = 0;
        while cur < region.len() {
            // The element's stack is the next STACK_SIZE bytes; its heap is the
            // remainder of the region, of which it consumes only what it needs.
            if cur + T::STACK_SIZE > region.len() {
                return Err(Error::UnexpectedEof);
            }
            let mut es = Cursor::new(&region[cur..cur + T::STACK_SIZE]);
            let mut eh = Cursor::new(&region[cur + T::STACK_SIZE..]);
            out.push(T::decode(&mut es, &mut eh)?);
            cur += T::STACK_SIZE + eh.pos();
        }
        Ok(out)
    }
}

impl<T: WaveWire> WaveWire for Option<T> {
    // 1 flag byte in stack; T's full wire representation (stack + heap) goes
    // into the parent's heap section only when Some. None costs exactly 1 byte.
    const STACK_SIZE: usize = 1;
    fn heap_size(&self) -> usize {
        self.as_ref().map_or(0, |v| T::STACK_SIZE + v.heap_size())
    }
    fn encode_stack(&self, stack: &mut Vec<u8>) {
        stack.push(u8::from(self.is_some()));
    }
    fn encode_heap(&self, heap: &mut Vec<u8>) {
        if let Some(v) = self {
            v.encode_stack(heap); // T's stack bytes land in parent heap
            v.encode_heap(heap);
        }
    }
    fn decode(stack: &mut Cursor, heap: &mut Cursor) -> Result<Self> {
        if stack.u8()? == 0 {
            return Ok(None);
        }
        let t_stack = heap.take(T::STACK_SIZE)?;
        let mut ts = Cursor::new(t_stack);
        Ok(Some(T::decode(&mut ts, heap)?))
    }
}

// Arrays and tuples compose **flattened**: each element's stack slots are emitted
// inline into the parent stack, heaps appended in order — so every offset stays a
// compile-time constant.

impl<T: WaveWire, const N: usize> WaveWire for [T; N] {
    const STACK_SIZE: usize = N * T::STACK_SIZE;
    fn heap_size(&self) -> usize {
        self.iter().map(WaveWire::heap_size).sum()
    }
    fn encode_stack(&self, stack: &mut Vec<u8>) {
        for e in self {
            e.encode_stack(stack);
        }
    }
    fn encode_heap(&self, heap: &mut Vec<u8>) {
        for e in self {
            e.encode_heap(heap);
        }
    }
    fn decode(stack: &mut Cursor, heap: &mut Cursor) -> Result<Self> {
        let mut v = Vec::with_capacity(N);
        for _ in 0..N {
            v.push(T::decode(stack, heap)?);
        }
        // `v` has exactly N elements, so the conversion never fails.
        v.try_into().map_err(|_| Error::UnexpectedEof)
    }
}

macro_rules! wire_tuple {
    ($($name:ident $idx:tt),+) => {
        impl<$($name: WaveWire),+> WaveWire for ($($name,)+) {
            const STACK_SIZE: usize = 0 $(+ $name::STACK_SIZE)+;
            fn heap_size(&self) -> usize { 0 $(+ self.$idx.heap_size())+ }
            fn encode_stack(&self, stack: &mut Vec<u8>) { $(self.$idx.encode_stack(stack);)+ }
            fn encode_heap(&self, heap: &mut Vec<u8>) { $(self.$idx.encode_heap(heap);)+ }
            fn decode(stack: &mut Cursor, heap: &mut Cursor) -> Result<Self> {
                Ok(( $($name::decode(stack, heap)?,)+ ))
            }
        }
    };
}

wire_tuple!(A 0, B 1);
wire_tuple!(A 0, B 1, C 2);
wire_tuple!(A 0, B 1, C 2, D 3);

#[cfg(test)]
mod tests {
    use super::{WaveWire, from_wire, to_wire};

    fn roundtrip<T: WaveWire + PartialEq + core::fmt::Debug>(value: &T) {
        let bytes = to_wire(value);
        assert_eq!(bytes.len(), T::STACK_SIZE + value.heap_size());
        let back: T = from_wire(&bytes).expect("decode");
        assert_eq!(*value, back);
    }

    #[test]
    fn scalars() {
        roundtrip(&0u8);
        roundtrip(&u128::MAX);
        roundtrip(&-7i32);
        roundtrip(&2.5f64);
        roundtrip(&true);
        roundtrip(&'🌊');
    }

    #[test]
    fn strings_and_vecs() {
        roundtrip(&String::new());
        roundtrip(&"wave".to_string());
        roundtrip(&vec![1u32, 2, 3]);
        roundtrip(&vec!["a".to_string(), String::new(), "ccc".to_string()]);
        roundtrip(&vec![vec![1u8], vec![], vec![2u8, 3]]);
    }

    #[test]
    fn options() {
        roundtrip(&Option::<u64>::None);
        roundtrip(&Some(42u64));
        roundtrip(&Some("x".to_string()));
        roundtrip(&vec![Some(1u8), None, Some(2)]);
    }

    #[test]
    fn arrays_and_tuples() {
        roundtrip(&[1u32, 2, 3]);
        roundtrip(&["a".to_string(), "bb".to_string()]); // array of heap-bearing elems
        roundtrip(&[Some(1u8), None]);
        roundtrip(&(1u8, "x".to_string(), 9u64));
        roundtrip(&(vec![1u16, 2], 'z', Option::<u32>::None, true));
    }

    #[test]
    fn short_buffer_is_eof() {
        // The defining failure: bytes that don't match the type's size.
        use super::Error;
        assert_eq!(from_wire::<u64>(&[0u8; 4]), Err(Error::UnexpectedEof));
        assert_eq!(from_wire::<u32>(&[]), Err(Error::UnexpectedEof));
    }

    // ---- #[derive(WaveWire)] ------------------------------------------------
    // `WaveWire` (trait + derive macro, same name) is imported at the top of the
    // module.

    #[derive(WaveWire, PartialEq, Debug)]
    struct Named {
        a: u64,
        b: String,
        c: Vec<u16>,
    }

    #[derive(WaveWire, PartialEq, Debug)]
    struct Tuple(u32, String);

    #[derive(WaveWire, PartialEq, Debug)]
    struct UnitS;

    #[derive(WaveWire, PartialEq, Debug)]
    enum FieldlessEnum {
        A,
        B,
        C,
    }

    #[derive(WaveWire, PartialEq, Debug)]
    enum MixedEnum {
        Nothing,
        One(u64),
        Many(u32, String),
        Struct { x: u8, label: String },
    }

    #[test]
    fn derive_structs() {
        roundtrip(&Named {
            a: 7,
            b: "wave".to_string(),
            c: vec![1, 2, 3],
        });
        roundtrip(&Tuple(42, String::new()));
        roundtrip(&UnitS);
    }

    #[test]
    fn derive_fieldless_enum_is_one_byte() {
        assert_eq!(FieldlessEnum::STACK_SIZE, 1);
        assert_eq!(to_wire(&FieldlessEnum::A), vec![0]);
        assert_eq!(to_wire(&FieldlessEnum::C), vec![2]);
        roundtrip(&FieldlessEnum::B);
    }

    #[test]
    fn derive_enum_with_fields() {
        assert_eq!(MixedEnum::STACK_SIZE, 1 + 4); // tag + payload length
        roundtrip(&MixedEnum::Nothing);
        roundtrip(&MixedEnum::One(0xDEAD_BEEF));
        roundtrip(&MixedEnum::Many(9, "x".to_string()));
        roundtrip(&MixedEnum::Struct {
            x: 5,
            label: "hello".to_string(),
        });
    }

    #[test]
    fn derive_enum_bad_tag_is_invalid_tag() {
        // tag 9, zero payload length — no such variant.
        let bytes = [9u8, 0, 0, 0, 0];
        assert_eq!(
            from_wire::<MixedEnum>(&bytes),
            Err(super::Error::InvalidTag(9))
        );
    }

    // ---- feature `validation`: crc32-framed encoding -------------------------

    #[cfg(feature = "validation")]
    mod checked {
        use super::Named;
        use crate::{
            CRC_PREFIX_LEN, Error, from_wire_checked, to_wire_checked,
        };

        #[test]
        fn roundtrips_with_crc_prefix() {
            let v = Named {
                a: 7,
                b: "wave".to_string(),
                c: vec![1, 2, 3],
            };
            let bytes = to_wire_checked(&v);
            assert_eq!(
                bytes.len(),
                CRC_PREFIX_LEN + crate::to_wire(&v).len(),
                "checked buffer = crc prefix + plain encoding"
            );
            assert_eq!(&bytes[CRC_PREFIX_LEN..], crate::to_wire(&v));
            assert_eq!(from_wire_checked::<Named>(&bytes).unwrap(), v);
        }

        #[test]
        fn tampered_payload_is_crc_mismatch() {
            let mut bytes = to_wire_checked(&42u64);
            *bytes.last_mut().unwrap() ^= 0xFF;
            assert!(matches!(
                from_wire_checked::<u64>(&bytes),
                Err(Error::CrcMismatch { .. })
            ));
        }

        #[test]
        fn tampered_prefix_is_crc_mismatch() {
            let mut bytes = to_wire_checked(&42u64);
            bytes[0] ^= 0xFF;
            assert!(matches!(
                from_wire_checked::<u64>(&bytes),
                Err(Error::CrcMismatch { .. })
            ));
        }

        #[test]
        fn short_buffer_is_eof() {
            assert_eq!(
                from_wire_checked::<u64>(&[0u8; 3]),
                Err(Error::UnexpectedEof)
            );
            // Prefix intact but payload truncated: crc fails first.
            let bytes = to_wire_checked(&42u64);
            assert!(matches!(
                from_wire_checked::<u64>(&bytes[..bytes.len() - 1]),
                Err(Error::CrcMismatch { .. })
            ));
        }
    }
}
