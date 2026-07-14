//! Diagnostic test: verify whether TimeBase::adjust eliminates or DOUBLES
//! the clock drift. This pinpoints the root cause of the periodic SRT stall.
//!
//! Setup: TimeBase::new(start) gives reference_time=start, reference_ts=MIN,
//! so instant_from(MIN) == start and timestamp_from(start) == MIN (trivially
//! calibrated at the point (start, MIN)).
//!
//! We simulate a peer whose timestamp stream is `drift` BEHIND our local now
//! (i.e. when we receive ts=T, our now = start + (T - MIN) + drift). The
//! measured drift is `timestamp_from(now) - T == drift`. A correct adjust
//! should drive the residual drift to 0. A sign-flipped adjust doubles it.
use srt_protocol::packet::{TimeStamp, TimeSpan};
use srt_protocol::protocol::time::TimeBase;
use std::time::{Duration, Instant};

#[test]
fn adjust_eliminates_drift_instead_of_doubling() {
    let start = Instant::now();
    let mut tb = TimeBase::new(start);

    // Sanity: trivially calibrated at (start, MIN).
    assert_eq!(tb.instant_from(TimeStamp::MIN), start);

    let drift = TimeSpan::from_micros(5_000); // 5ms
    // Peer timestamp we just "received".
    let ts = TimeStamp::MIN + Duration::from_micros(1_000_000); // 1s into the stream
    // Our local now is drift ahead of the calibrated expectation.
    let now = start + Duration::from_micros(1_000_000) + Duration::from_micros(5_000);

    let measured = tb.timestamp_from(now) - ts;
    assert_eq!(measured, drift, "measured drift should be 5ms");

    // Apply the adjustment the way SynchronizedRemoteClock does.
    tb.adjust(now, measured);

    // After adjust, timestamp_from(now) should equal the peer ts (residual 0).
    let residual = tb.timestamp_from(now) - ts;
    eprintln!(
        "after one adjust: residual drift = {} us (input drift = {} us)",
        residual.as_micros(),
        drift.as_micros(),
    );

    assert_eq!(
        residual,
        TimeSpan::ZERO,
        "adjust should eliminate drift, but residual is {} us (drift doubled to {} us)",
        residual.as_micros(),
        (2 * drift).as_micros(),
    );
}

/// Mirror of the vendored `time::base::adjust` proptest, to confirm the
/// updated assertions hold with the fix.
#[test]
fn adjust_proptest_assertions_hold() {
    let start = Instant::now();
    let drift = TimeSpan::from_micros(7_000);
    let clock_delta = TimeSpan::from_micros(123_000);
    let mut timebase = TimeBase::new(start);
    let original_ts = timebase.timestamp_from(start); // == MIN
    let now = start + clock_delta;

    timebase.adjust(now, drift);

    let original_time = timebase.instant_from(original_ts);
    // Fixed assertion: instant_from shifts by +drift.
    assert_eq!(start + drift - original_time, Duration::from_micros(0));

    let ts = timebase.timestamp_from(start);
    // Unchanged assertion: timestamp_from(start) == original_ts - drift.
    assert_eq!(ts, original_ts - drift);
}
