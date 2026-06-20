//! The `Wire` serialization format — no serde, no `repr(C)`.
//!
//! A value serialises to two contiguous sections: a fixed-size **stack** section
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

use crate::error::{Error, Result};

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
/// Implementors guarantee that [`encode_stack`](Wire::encode_stack) writes
/// **exactly** `STACK_SIZE` bytes and that [`decode`](Wire::decode) reads exactly
/// `STACK_SIZE` bytes from its stack cursor.
pub trait Wire: Sized {
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

/// Serialise a value to a single `[stack][heap]` byte vector in **one
/// allocation**: `STACK_SIZE` (a compile-time sum over every field, including
/// nested `Wire` types like `Metadata`) plus the recursively-computed
/// `heap_size()` give the exact final length up front. `encode_stack` appends the
/// `STACK_SIZE`-byte stack section, then `encode_heap` appends the heap section to
/// the same buffer — no second allocation, no concat.
#[must_use]
pub fn to_wire<T: Wire>(value: &T) -> Vec<u8> {
    let mut buf = Vec::with_capacity(T::STACK_SIZE + value.heap_size());
    value.encode_stack(&mut buf);
    value.encode_heap(&mut buf);
    debug_assert_eq!(buf.len(), T::STACK_SIZE + value.heap_size());
    buf
}

/// Deserialise a value from a `[stack][heap]` byte slice.
pub fn from_wire<T: Wire>(bytes: &[u8]) -> Result<T> {
    if bytes.len() < T::STACK_SIZE {
        return Err(Error::UnexpectedEof);
    }
    let (stack, heap) = bytes.split_at(T::STACK_SIZE);
    let mut sc = Cursor::new(stack);
    let mut hc = Cursor::new(heap);
    T::decode(&mut sc, &mut hc)
}

// ---- fixed-width scalars ----------------------------------------------------

macro_rules! wire_le {
    ($($t:ty => $n:literal),* $(,)?) => {$(
        impl Wire for $t {
            const STACK_SIZE: usize = $n;
            fn heap_size(&self) -> usize { 0 }
            fn encode_stack(&self, stack: &mut Vec<u8>) {
                stack.extend_from_slice(&self.to_le_bytes());
            }
            fn encode_heap(&self, _heap: &mut Vec<u8>) {}
            fn decode(stack: &mut Cursor, _heap: &mut Cursor) -> Result<Self> {
                Ok(<$t>::from_le_bytes(stack.take($n)?.try_into().unwrap()))
            }
        }
    )*};
}

wire_le! {
    u8 => 1, u16 => 2, u32 => 4, u64 => 8, u128 => 16,
    i8 => 1, i16 => 2, i32 => 4, i64 => 8, i128 => 16,
    f32 => 4, f64 => 8,
}

impl Wire for bool {
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

impl Wire for char {
    const STACK_SIZE: usize = 4;
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

impl Wire for String {
    const STACK_SIZE: usize = 4; // u32 byte-length
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

impl<T: Wire> Wire for Vec<T> {
    const STACK_SIZE: usize = 4; // u32 region byte-length
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

impl<T: Wire> Wire for Option<T> {
    // 1 flag byte in stack; T's full wire representation (stack + heap) goes
    // into the parent's heap section only when Some. None costs exactly 1 byte.
    const STACK_SIZE: usize = 1;
    fn heap_size(&self) -> usize {
        self.as_ref().map_or(0, |v| T::STACK_SIZE + v.heap_size())
    }
    fn encode_stack(&self, stack: &mut Vec<u8>) {
        stack.push(if self.is_some() { 1 } else { 0 });
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

impl<T: Wire, const N: usize> Wire for [T; N] {
    const STACK_SIZE: usize = N * T::STACK_SIZE;
    fn heap_size(&self) -> usize {
        self.iter().map(Wire::heap_size).sum()
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
        impl<$($name: Wire),+> Wire for ($($name,)+) {
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
    use super::{Wire, from_wire, to_wire};

    fn roundtrip<T: Wire + PartialEq + core::fmt::Debug>(value: T) {
        let bytes = to_wire(&value);
        assert_eq!(bytes.len(), T::STACK_SIZE + value.heap_size());
        let back: T = from_wire(&bytes).expect("decode");
        assert_eq!(value, back);
    }

    #[test]
    fn scalars() {
        roundtrip(0u8);
        roundtrip(u128::MAX);
        roundtrip(-7i32);
        roundtrip(2.5f64);
        roundtrip(true);
        roundtrip('🌊');
    }

    #[test]
    fn strings_and_vecs() {
        roundtrip(String::new());
        roundtrip("wave".to_string());
        roundtrip(vec![1u32, 2, 3]);
        roundtrip(vec!["a".to_string(), String::new(), "ccc".to_string()]);
        roundtrip(vec![vec![1u8], vec![], vec![2u8, 3]]);
    }

    #[test]
    fn options() {
        roundtrip(Option::<u64>::None);
        roundtrip(Some(42u64));
        roundtrip(Some("x".to_string()));
        roundtrip(vec![Some(1u8), None, Some(2)]);
    }

    #[test]
    fn arrays_and_tuples() {
        roundtrip([1u32, 2, 3]);
        roundtrip(["a".to_string(), "bb".to_string()]); // array of heap-bearing elems
        roundtrip([Some(1u8), None]);
        roundtrip((1u8, "x".to_string(), 9u64));
        roundtrip((vec![1u16, 2], 'z', Option::<u32>::None, true));
    }
}
