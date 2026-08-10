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
//! - Spec gap #10 (CongestionWarning/PeerError/unknown SRT control panics):
//!   the `congestion_warning_does_not_panic`, `peer_error_does_not_panic`,
//!   and `unhandled_srt_control_does_not_panic` tests below.
//! - Spec gap #11 (TSBPD wrap-period): the `instant_from_across_timestamp_wrap`
//!   test below.
//! - Spec gap #12 (HSREQ under-advertises TLPKTDROP/NAKREPORT): the
//!   `supported_flags_include_tlpktdrop_and_nakreport` test below.

use proptest::prelude::*;
use srt_protocol::packet::{TimeSpan, TimeStamp};
use srt_protocol::protocol::time::TimeBase;
use std::time::{Duration, Instant};

use srt_protocol::connection::{Connection, ConnectionSettings, DuplexConnection, Input};
use srt_protocol::options::{LiveBandwidthMode, PacketCount, PacketSize};
use srt_protocol::packet::{
    ControlPacket, ControlTypes, Packet, ReceivePacketResult, SeqNumber, SocketId,
    SrtControlPacket, SrtShakeFlags,
};
use srt_protocol::protocol::handshake::Handshake;
use std::net::SocketAddr;

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

// ---------------------------------------------------------------------------
// Spec gap #12: SrtShakeFlags::SUPPORTED must include TLPKTDROP and NAKREPORT.
//
// Both behaviors are unconditionally enabled in ConnInitSettings::default(),
// but SUPPORTED previously omitted the corresponding flags, under-advertising
// capabilities to the peer on the wire.
// ---------------------------------------------------------------------------

#[test]
fn supported_flags_include_tlpktdrop_and_nakreport() {
    assert!(
        SrtShakeFlags::SUPPORTED.contains(SrtShakeFlags::TLPKTDROP | SrtShakeFlags::NAKREPORT),
        "SUPPORTED must advertise TLPKTDROP and NAKREPORT since both are enabled by default"
    );
}

// ---------------------------------------------------------------------------
// Spec gap #11: TSBPD wrap-period — instant_from must work across a u32 wrap.
//
// The spec (§4.5.1) describes an explicit periodic TsbpdTimeBase adjustment.
// This implementation relies on Wrapping<u32> modular arithmetic. This test
// proves instant_from produces the correct result for a timestamp just past
// the wrap boundary when reference_ts has been kept current via adjust().
// ---------------------------------------------------------------------------

#[test]
fn instant_from_across_timestamp_wrap() {
    let start = Instant::now();
    let mut tb = TimeBase::new(start);

    // Advance reference_ts to ~1s before the wrap boundary (u32::MAX µs).
    let near_wrap = Duration::from_micros(u32::MAX as u64 - 1_000_000);
    let now = start + near_wrap;
    tb.adjust(now, TimeSpan::ZERO);

    // A packet arrives with a timestamp 0.5s past the wrap point.
    let wrapped_ts = TimeStamp::from_micros(500_000);
    let instant = tb.instant_from(wrapped_ts);

    // The packet is 1s-before-wrap + 0.5s-after-wrap = 1.5s after `now`.
    let diff = instant.saturating_duration_since(now);
    assert!(
        diff >= Duration::from_millis(1499) && diff <= Duration::from_millis(1501),
        "instant_from across wrap should be ~1500ms after now, got {:?}",
        diff
    );
}

// ---------------------------------------------------------------------------
// Spec gap #10: CongestionWarning, PeerError, and unknown SRT control packets
// must not panic. Previously these were `todo!()` / `unimplemented!()` which
// would crash the session if a peer sent them.
// ---------------------------------------------------------------------------

fn test_connection(now: Instant) -> DuplexConnection {
    let remote: SocketAddr = ([127, 0, 0, 1], 2223).into();
    DuplexConnection::new(Connection {
        settings: ConnectionSettings {
            remote,
            remote_sockid: SocketId(2),
            local_sockid: SocketId(2),
            socket_start_time: now,
            rtt: Duration::default(),
            init_seq_num: SeqNumber::new_truncate(0),
            max_packet_size: PacketSize(1316),
            max_flow_size: PacketCount(8192),
            send_tsbpd_latency: Duration::from_secs(1),
            recv_tsbpd_latency: Duration::from_secs(1),
            recv_buffer_size: PacketCount(1024),
            send_buffer_size: PacketCount(1024),
            cipher: None,
            stream_id: None,
            bandwidth: LiveBandwidthMode::Unlimited,
            statistics_interval: Duration::from_secs(10),
            peer_idle_timeout: Duration::from_secs(5),
            too_late_packet_drop: true,
        },
        handshake: Handshake::Connector,
    })
}

const REMOTE_ADDR: SocketAddr = SocketAddr::new(
    std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
    2223,
);

fn feed_control_packet(conn: &mut DuplexConnection, now: Instant, control_type: ControlTypes) {
    let pkt = Packet::Control(ControlPacket {
        timestamp: TimeStamp::from_micros(0),
        dest_sockid: SocketId(2),
        control_type,
    });
    let input: ReceivePacketResult = Ok((pkt, REMOTE_ADDR));
    let _ = conn.handle_input(now, Input::Packet(input));
}

#[test]
fn congestion_warning_does_not_panic() {
    let now = Instant::now();
    let mut conn = test_connection(now);
    feed_control_packet(&mut conn, now, ControlTypes::CongestionWarning);
}

#[test]
fn peer_error_does_not_panic() {
    let now = Instant::now();
    let mut conn = test_connection(now);
    feed_control_packet(&mut conn, now, ControlTypes::PeerError(4000));
}

#[test]
fn unhandled_srt_control_does_not_panic() {
    let now = Instant::now();
    let mut conn = test_connection(now);
    feed_control_packet(
        &mut conn,
        now,
        ControlTypes::Srt(SrtControlPacket::Congestion("live".to_string())),
    );
}
