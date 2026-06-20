//! Static type description for the build-time registry.
//!
//! Core declares the *shape* of a type's static description — [`ObjectDescriptor`]
//! and the [`ObjectRegistry`] lookup. The concrete registry (the generated
//! `Object` enum and its `STRUCT_HASH → variant` table) is emitted in `build.rs`
//! by the schema crate, not here; it implements [`ObjectRegistry`] so the engine
//! can locate a type's layout without `dyn`.

use crate::traits::Shape;

/// Static description of one heap-bearing field — enough to locate its bytes in a
/// wire-encoded record without decoding the whole value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldDescriptor {
    /// Declared field name (the registry key for field lookups).
    pub name: &'static str,
    /// Byte offset of this field's slot within the stack section.
    pub stack_offset: usize,
    /// `true` if the field carries a heap payload (String, Vec, nested dynamic).
    pub heapable: bool,
}

/// The static shape of a `#[wavedb]` type.
///
/// Identity, cardinality, stack size, and its field table. Emitted by the
/// macro/`build.rs`; `'static` so it costs no heap and `wasm-opt` can
/// dictionary-compress it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectDescriptor {
    /// Compile-time identity (`= T::STRUCT_HASH`).
    pub struct_hash: u64,
    /// Declared struct name.
    pub name: &'static str,
    /// Cardinality discipline.
    pub shape: Shape,
    /// Fixed stack-section size (`= T::STACK_SIZE`).
    pub stack_size: usize,
    /// The heap-bearing fields, in declaration order.
    pub fields: &'static [FieldDescriptor],
}

impl ObjectDescriptor {
    /// Look up a field's descriptor by name.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&FieldDescriptor> {
        self.fields.iter().find(|f| f.name == name)
    }
}

/// Resolve a type's [`ObjectDescriptor`] from its `STRUCT_HASH`.
///
/// The generated registry implements this over a static `match`; an unknown hash
/// returns `None` (a record written under a schema this build doesn't know).
pub trait ObjectRegistry {
    /// The descriptor for `struct_hash`, or `None` if this build doesn't declare it.
    fn descriptor(&self, struct_hash: u64)
    -> Option<&'static ObjectDescriptor>;
}

#[cfg(test)]
mod tests {
    use super::{FieldDescriptor, ObjectDescriptor, ObjectRegistry};
    use crate::traits::Shape;

    static NAME: FieldDescriptor = FieldDescriptor {
        name: "name",
        stack_offset: 0,
        heapable: true,
    };
    static ABOUT_USER: ObjectDescriptor = ObjectDescriptor {
        struct_hash: 0xABCD,
        name: "AboutUser",
        shape: Shape::Unique,
        stack_size: 4,
        fields: &[NAME],
    };

    struct StaticRegistry;
    impl ObjectRegistry for StaticRegistry {
        fn descriptor(
            &self,
            struct_hash: u64,
        ) -> Option<&'static ObjectDescriptor> {
            match struct_hash {
                0xABCD => Some(&ABOUT_USER),
                _ => None,
            }
        }
    }

    #[test]
    fn lookup_by_hash_and_field() {
        let reg = StaticRegistry;
        let d = reg.descriptor(0xABCD).expect("known hash");
        assert_eq!(d.name, "AboutUser");
        assert!(d.shape.is_unique());
        assert_eq!(d.field("name"), Some(&NAME));
        assert!(d.field("missing").is_none());
        assert!(reg.descriptor(0x1).is_none());
    }
}
