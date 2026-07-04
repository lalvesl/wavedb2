//! Decode a node [`Reply`] into the shape a typed call expects.
//!
//! Each helper accepts exactly the reply its command produces and treats any
//! other arm as an [`Error::UnexpectedReply`] — a protocol mismatch, distinct
//! from a decode fault or a node refusal.
//!
//! (Items are `pub` in this private module — crate-visible, never exported.)

use wavedb_core::Id;
use wavedb_core::expose::Reply;
use wavedb_core::wire::{WaveWire, from_wire};

use crate::error::{Error, Result};

/// A `Get`'s reply → an optional decoded value (consumes the reply to move
/// the body bytes out without a copy).
pub fn value<T: WaveWire>(reply: Reply) -> Result<Option<T>> {
    match reply {
        Reply::Value(Some(bytes)) => {
            let value =
                from_wire::<T>(&bytes).map_err(wavedb_core::Error::from)?;
            Ok(Some(value))
        }
        Reply::Value(None) => Ok(None),
        _ => Err(Error::UnexpectedReply),
    }
}

/// A `Save`/`Update`'s reply → unit.
pub const fn done(reply: &Reply) -> Result<()> {
    match reply {
        Reply::Done => Ok(()),
        _ => Err(Error::UnexpectedReply),
    }
}

/// An `Insert`'s reply → the minted record `Id`.
pub const fn inserted(reply: &Reply) -> Result<Id> {
    match reply {
        Reply::Inserted(id) => Ok(*id),
        _ => Err(Error::UnexpectedReply),
    }
}

/// A `Remove`'s reply → whether the record was in the living set.
pub const fn removed(reply: &Reply) -> Result<bool> {
    match reply {
        Reply::Removed(was_live) => Ok(*was_live),
        _ => Err(Error::UnexpectedReply),
    }
}
