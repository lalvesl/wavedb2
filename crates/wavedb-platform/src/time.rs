//! The wall clock — `SystemTime` natively, `Date.now()` in the browser.
//!
//! wasm32-unknown-unknown has no system clock: `SystemTime::now()` there
//! panics at runtime, so every timestamp in the workspace routes through
//! here. Browser precision is one millisecond; id minting stays collision
//! free the same way a same-nanosecond native mint does — the caller's
//! per-process salt counter distinguishes same-instant ids.

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

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::{unix_nanos, unix_secs};

    #[test]
    fn clock_is_past_2023_and_units_agree() {
        let secs = unix_secs();
        let nanos = unix_nanos();
        assert!(secs > 1_700_000_000, "clock reads before 2023: {secs}");
        // Taken microseconds apart; agree to within a second.
        assert!((nanos / 1_000_000_000).abs_diff(secs) <= 1);
    }
}
