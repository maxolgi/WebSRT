//! Regression guards for the critical forked `srt-protocol` patches.
//!
//! These tests exist so that a future rebase onto upstream `srt-protocol`
//! that silently drops one of the WebSRT patches is caught by `cargo test
//! -p websrt`. They exercise the patched code paths directly through the
//! `srt_protocol` crate.
//!
//! Coverage map:
//! - Patch 2 (`TimeBase::adjust` sign flip): `adjust_eliminates_drift_*` and
//!   the `adjust_shifts_instant_from_by_plus_drift` proptest below.
//! - Patch 6 (`Sub<TimeSpan>`/`Add<TimeSpan>` for `Instant`): the
//!   `instant_sub_timespan_*` tests below.
//! - Patch 3 (TLPKTL `checked_sub` in `protocol/receiver/buffer.rs`): lives
//!   deep inside the receiver buffer and only fires in the first instants of
//!   page life when `now < tsbpd_latency + tsbpd_tolerance`. It cannot be
//!   triggered without driving a full SRT connection, so it is covered
//!   indirectly by the `skip_induction` integration tests (and the E2E test
//!   when it lands) rather than by a dedicated unit test here.

use proptest::prelude::*;
use srt_protocol::packet::{TimeSpan, TimeStamp};
use srt_protocol::protocol::time::TimeBase;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Patch 2: TimeBase::adjust must ELIMINATE drift, not double it.
//
// Upstream applies `-drift` to the reference points, which flips the sign and
// doubles the TSBPD clock error on every sync. The fork applies `+drift`.
// ---------------------------------------------------------------------------

/// Mirror of `crates/websrt-gateway/tests/timebase_drift.rs`, kept in the
/// library crate so it runs under `cargo test -p websrt`.
#[test]
fn adjust_eliminates_drift_not_doubles() {
    let start = Instant::now();
    let mut tb = TimeBase::new(start);

    let drift = TimeSpan::from_micros(5_000); // 5ms
    let ts = TimeStamp::MIN + Duration::from_micros(1_000_000); // 1s into the stream
    // Our local `now` is `drift` ahead of the calibrated expectation for `ts`.
    let now = start + Duration::from_micros(1_000_000) + Duration::from_micros(5_000);

    let measured = tb.timestamp_from(now) - ts;
    assert_eq!(measured, drift, "measured drift should be 5ms");

    tb.adjust(now, measured);

    let residual = tb.timestamp_from(now) - ts;
    assert_eq!(
        residual,
        TimeSpan::ZERO,
        "adjust should eliminate drift, not double it (residual = {} us)",
        residual.as_micros(),
    );
}

// Property: after `adjust(now, drift)`, `instant_from` of the pre-adjust
// timestamp shifts by exactly `+drift`.
//
// With the patched `+drift` the shift is `+drift`; with the upstream `-drift`
// it becomes `-drift`, so `start + drift - original_time` evaluates to
// `2 * drift` (non-zero) and the property fails.
proptest! {
    #[test]
    fn adjust_shifts_instant_from_by_plus_drift(
        drift_us in -50_000i32..50_000i32,
        clock_delta_us in 0u64..60_000_000u64,
    ) {
        let start = Instant::now();
        let drift = TimeSpan::from_micros(drift_us);
        // Keep `now` strictly in the future so the clock is well-defined.
        let now = start + Duration::from_micros(clock_delta_us);
        let mut tb = TimeBase::new(start);
        let original_ts = tb.timestamp_from(start);

        tb.adjust(now, drift);

        let original_time = tb.instant_from(original_ts);
        // Patched: original_time == start + drift  =>  diff == 0.
        // Buggy:   original_time == start - drift  =>  diff == 2*drift.
        prop_assert_eq!(start + drift - original_time, Duration::ZERO);
    }
}

// ---------------------------------------------------------------------------
// Patch 6: `Sub<TimeSpan>` / `Add<TimeSpan>` for `Instant`.
//
// NOTE: the actual forked change (commit 5070eb7, "patch 6") differs from the
// high-level description in AGENTS.md. The real edit in `packet/time.rs`:
//   1. Fixed the inverted sign logic in `Sub<TimeSpan> for Instant` (upstream
//      ADDED when it should have SUBTRACTED for a positive span).
//   2. Replaced the panicking `.unwrap()` with `.unwrap_or(self)` in both the
//      `Add<TimeSpan>` and `Sub<TimeSpan>` impls, so an underflow below the
//      `Instant` epoch no longer crashes the page.
// The dominant, deterministic regression signal is the sign fix: subtracting
// a positive `TimeSpan` from an `Instant` must move the instant EARLIER. A
// revert restores the inverted sign and these tests fail.
// ---------------------------------------------------------------------------

/// Subtracting a positive `TimeSpan` from an `Instant` must yield an earlier
/// instant. Upstream's `Sub<TimeSpan> for Instant` had the branches inverted
/// and returned a LATER instant.
#[test]
fn instant_sub_timespan_positive_moves_earlier() {
    let base = Instant::now() + Duration::from_secs(10);
    let span = TimeSpan::from_micros(5_000);

    let result = base - span;

    assert!(
        result < base,
        "base - positive TimeSpan should move earlier, not later",
    );
    // And by exactly the requested magnitude.
    assert_eq!(base - result, Duration::from_micros(5_000));
}

/// Adding a negative `TimeSpan` to an `Instant` must also move earlier (the
/// mirror of subtraction). With the patched `Add<TimeSpan> for Instant` the
/// `micros <= 0` branch subtracts the absolute value; an inverted impl would
/// move later.
#[test]
fn instant_add_negative_timespan_moves_earlier() {
    let base = Instant::now() + Duration::from_secs(10);
    let span = TimeSpan::from_micros(-5_000);

    let result = base + span;

    assert!(
        result < base,
        "base + negative TimeSpan should move earlier, not later",
    );
    assert_eq!(base - result, Duration::from_micros(5_000));
}
