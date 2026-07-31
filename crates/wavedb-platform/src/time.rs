//! The wall clock — `SystemTime` natively, `Date.now()` in the browser.
//!
//! wasm32-unknown-unknown has no system clock: `SystemTime::now()` there
//! panics at runtime, so every timestamp in the workspace routes through
//! here. Browser precision is one millisecond; id minting therefore uses
//! [`key_nanos`], which fuses a process-wide counter into the dead
//! sub-millisecond digits so same-instant mints never share a key.

/// Unix seconds now — what token TTLs run on.
#[must_use]
pub fn unix_secs() -> u64 {
    unix_nanos() / 1_000_000_000
}

/// Unix nanoseconds now — the id-minting key (`CREATED_AT`). In the
/// browser the low six digits are always zero (millisecond clock).
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub fn unix_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        // Truncation is theoretical: u64 nanoseconds overflow in 2554.
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

/// Unix nanoseconds now — the id-minting key (`CREATED_AT`). In the
/// browser the low six digits are always zero (millisecond clock).
#[cfg(target_arch = "wasm32")]
#[must_use]
pub fn unix_nanos() -> u64 {
    // `Date.now()` is finite non-negative milliseconds since the epoch;
    // the product stays far under `u64::MAX` until the year 2554.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let millis = js_sys::Date::now().max(0.0) as u64;
    millis * 1_000_000
}

/// The id-minting key: unix nanoseconds whose six sub-millisecond digits
/// are a process-wide counter instead of clock digits.
///
/// Both targets use the same formula, so native- and browser-minted keys
/// are structurally identical: chronological across milliseconds (the
/// clock part), mint-ordered and collision-free within one (the counter
/// part) — which a browser's millisecond clock cannot promise on its own.
#[must_use]
pub fn key_nanos() -> u64 {
    use core::sync::atomic::{AtomicU32, Ordering};
    static MINT: AtomicU32 = AtomicU32::new(0);
    let count = u64::from(MINT.fetch_add(1, Ordering::Relaxed)) % 1_000_000;
    (unix_nanos() / 1_000_000) * 1_000_000 + count
}

/// Sleep for `duration` — the timer the connection manager's poll loop
/// runs on (tokio's timer natively, `setTimeout` in the browser — no
/// tokio in wasm).
#[cfg(not(target_arch = "wasm32"))]
pub async fn sleep(duration: core::time::Duration) {
    tokio::time::sleep(duration).await;
}

/// Sleep for `duration` — the timer the connection manager's poll loop
/// runs on (tokio's timer natively, `setTimeout` in the browser — no
/// tokio in wasm).
#[cfg(target_arch = "wasm32")]
pub async fn sleep(duration: core::time::Duration) {
    let millis = i32::try_from(duration.as_millis()).unwrap_or(i32::MAX);
    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
        if let Some(window) = web_sys::window() {
            // A refused timer leaves the promise pending — the caller's
            // task idles rather than spinning; nothing to surface.
            let _ = window
                .set_timeout_with_callback_and_timeout_and_arguments_0(
                    &resolve, millis,
                );
        }
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::{key_nanos, unix_nanos, unix_secs};

    #[test]
    fn clock_is_past_2023_and_units_agree() {
        let secs = unix_secs();
        let nanos = unix_nanos();
        assert!(secs > 1_700_000_000, "clock reads before 2023: {secs}");
        // Taken microseconds apart; agree to within a second.
        assert!((nanos / 1_000_000_000).abs_diff(secs) <= 1);
    }

    #[test]
    fn key_nanos_never_collides_within_a_millisecond() {
        // Far more mints than one millisecond can hold clock-wise; every
        // key must still be distinct (the fused counter) and the sequence
        // strictly ordered (clock digits rise, counter digits rise).
        let keys: Vec<u64> = (0..10_000).map(|_| key_nanos()).collect();
        for pair in keys.windows(2) {
            assert!(pair[0] < pair[1], "{} !< {}", pair[0], pair[1]);
        }
        // The key stays wall-clock shaped: its millisecond half agrees
        // with the clock even though the low digits are counter digits.
        let key_ms = key_nanos() / 1_000_000;
        let clock_ms = unix_nanos() / 1_000_000;
        assert!(key_ms.abs_diff(clock_ms) < 1_000);
    }
}
