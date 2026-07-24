//! Entropy — `RandomState` hasher keys natively, `crypto.getRandomValues`
//! in the browser.
//!
//! The native source needs no rand dependency: `RandomState`'s keys are
//! randomly seeded from OS entropy per value, so hashing nothing through a
//! fresh one yields 8 unpredictable bytes at a time.

use crate::error::Result;

/// Fill `buf` with unpredictable bytes.
///
/// # Errors
/// Never natively; in the browser, `Error::Js` when the
/// crypto API is unreachable (no `window`, e.g. a worker context).
#[cfg(not(target_arch = "wasm32"))]
pub fn fill(buf: &mut [u8]) -> Result<()> {
    use std::hash::{BuildHasher, Hasher};
    for chunk in buf.chunks_mut(8) {
        let word = std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish();
        let n = chunk.len();
        chunk.copy_from_slice(&word.to_le_bytes()[..n]);
    }
    Ok(())
}

/// Fill `buf` with unpredictable bytes.
///
/// # Errors
/// Never natively; in the browser, `Error::Js` when the
/// crypto API is unreachable (no `window`, e.g. a worker context).
#[cfg(target_arch = "wasm32")]
pub fn fill(buf: &mut [u8]) -> Result<()> {
    use crate::error::{Error, js};
    let window = web_sys::window()
        .ok_or_else(|| Error::Js(String::from("no window")))?;
    let crypto = window.crypto().map_err(|e| js("crypto", &e))?;
    // `getRandomValues` throws past 65536 bytes per call.
    for chunk in buf.chunks_mut(65_536) {
        crypto
            .get_random_values_with_u8_array(chunk)
            .map_err(|e| js("getRandomValues", &e))?;
    }
    Ok(())
}

/// A fresh 32-byte secret — the node's HMAC-key shape.
///
/// # Errors
/// As [`fill`].
pub fn secret32() -> Result<[u8; 32]> {
    let mut secret = [0u8; 32];
    fill(&mut secret)?;
    Ok(secret)
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::{fill, secret32};

    #[test]
    fn secrets_differ_between_draws() {
        assert_ne!(secret32().unwrap(), secret32().unwrap());
    }

    #[test]
    fn odd_length_buffer_fills_to_the_end() {
        let mut buf = [0u8; 13];
        fill(&mut buf).unwrap();
        // 2^-40 flake odds: the 5-byte tail past the last full word.
        assert_ne!(&buf[8..], &[0u8; 5]);
    }
}
