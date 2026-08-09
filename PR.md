# Upstream PR Drafts for `russelltg/srt-rs`

Drafted from a deep diff of the `maxolgi/srt-rs` fork (base `d4c08ac`,
head `ac43332`) against upstream. Each PR is self-contained, ordered by
priority (unambiguous bugs first, features last).

**Base commit:** `d4c08ac` (v0.4.4 release)
**Fork head:** `ac4333228b1a3e19efc0d361119fda4f7c7693e5`

---

## PR 1 — Fix `Sub<TimeSpan> for Instant` (copy-paste of `Add`)

**Severity:** correctness bug, silent
**Fork patches:** 2 + 6 (coupled — must land together)
**Files:** `srt-protocol/src/packet/time.rs`, `srt-protocol/src/protocol/time/base.rs`

### Problem

`Sub<TimeSpan> for Instant` is byte-for-byte identical to `Add<TimeSpan> for Instant`.
Both implementations add the TimeSpan regardless of sign — a copy-paste error:

```rust
// UPSTREAM — Add<TimeSpan> for Instant
impl Add<TimeSpan> for Instant {
    fn add(self, rhs: TimeSpan) -> Self::Output {
        let micros = rhs.as_micros() as i64;
        if micros > 0 {
            self + Duration::from_micros(micros as u64)    // ADD
        } else {
            self.checked_sub(...).unwrap()                  // SUBTRACT
        }
    }
}

// UPSTREAM — Sub<TimeSpan> for Instant  ← IDENTICAL, should be the inverse
impl Sub<TimeSpan> for Instant {
    fn sub(self, rhs: TimeSpan) -> Self::Output {
        let micros = rhs.as_micros() as i64;
        if micros > 0 {
            self + Duration::from_micros(micros as u64)    // ADD ← WRONG, should subtract
        } else {
            self.checked_sub(...).unwrap()                  // SUBTRACT ← WRONG, should add
        }
    }
}
```

### Why it hasn't been caught

`TimeBase::adjust` is the primary consumer of `Sub<TimeSpan> on Instant`:

```rust
// UPSTREAM adjust
self.origin_time = self.origin_time - drift;    // calls Sub<TimeSpan>
self.reference_time = self.reference_time - drift;
```

Because `Sub` is broken (it adds), upstream `adjust` computes `+drift`.
This is **correct by accident** — the drift correction sign happens to be right.
The bug is masked because every call site gets the "right" result through a
double negative.

### Fix

Swap the branches in `Sub<TimeSpan>` so it actually subtracts, and change
`unwrap()` → `unwrap_or(self)` to avoid panics on Instant underflow (which
can happen during early connection setup when timestamps are near epoch).
Then update `adjust` to use `+drift` explicitly (since `Sub` now correctly
computes `-drift`, we need `+drift` to preserve the same behavior):

```rust
// FIXED Sub<TimeSpan> for Instant
impl Sub<TimeSpan> for Instant {
    fn sub(self, rhs: TimeSpan) -> Self::Output {
        let micros = rhs.as_micros() as i64;
        if micros > 0 {
            self.checked_sub(Duration::from_micros(micros as u64))
                .unwrap_or(self)
        } else {
            self + Duration::from_micros(micros.unsigned_abs())
        }
    }
}

// FIXED adjust — +drift instead of -drift
self.origin_time = self.origin_time + drift;
self.reference_time = self.reference_time + drift;
```

### Test

The existing `timebase` proptest in `base.rs` needs its assertion updated:

```rust
// UPSTREAM (relies on broken Sub):
assert_eq!(start - drift - original_time, Duration::from_micros(0));

// FIXED:
assert_eq!(start + drift - original_time, Duration::from_micros(0));
```

The `timestamp_from(start) == original_ts - drift` assertion remains correct.

---

## PR 2 — Fix sender buffer `SeqNumber` wrapping-subtraction panics

**Severity:** panic in production
**Fork patch:** 5
**Files:** `srt-protocol/src/protocol/sender/buffer.rs`

### Problem

Three places in `SendBuffer` compute `SeqNumber - SeqNumber` using wrapping
arithmetic without bounds checking. `SeqNumber` wraps at `u32::MAX`, so a
wrapping subtraction can produce a `u32::MAX`-sized index → panic or
nonsensical behavior.

**(a) `send_packet` — index underflow + panic:**

```rust
// UPSTREAM
fn send_packet(&mut self, ts_now: TimeStamp, seq_number: SeqNumber) -> Option<DataPacket> {
    let index = seq_number - self.front_packet()?;  // wraps if seq < front!
    let entry = self.buffer.get_mut(index as usize)?;  // huge index → panic
```

**(b) `number_of_unacked_packets` — wraps to huge number:**

```rust
// UPSTREAM
fn number_of_unacked_packets(&self) -> usize {
    self.buffer.front().map_or(0, |e| self.next_send - e.packet.seq_number) as usize
    //                                               ^^^^^^^^^^ wraps if next_send < front
}
```

**(c) `send_next_packet` — `next_send` can drift behind `front_packet`:**

No clamping; if `next_send < front_packet` (after ACKs advance the front),
the next `send_packet` call hits bug (a).

### Fix

```rust
// (a) bounds-checked index
fn send_packet(&mut self, ts_now: TimeStamp, seq_number: SeqNumber) -> Option<DataPacket> {
    let front = self.front_packet()?;
    let index = if seq_number.0 >= front.0 {
        seq_number.0 - front.0
    } else {
        return None;
    };
    let entry = self.buffer.get_mut(index as usize)?;
```

```rust
// (b) wrapping-safe unacked count
fn number_of_unacked_packets(&self) -> usize {
    self.buffer.front().map_or(0, |e| {
        let front = e.packet.seq_number.0;
        let next = self.next_send.0;
        if next > front { (next - front) as usize } else { 0 }
    })
}
```

```rust
// (c) clamp next_send to front
fn send_next_packet(&mut self, ts_now: TimeStamp) -> Option<DataPacket> {
    if let Some(front) = self.front_packet() {
        if self.next_send.0 < front.0 {
            self.next_send = front;
        }
    }
    let packet_to_send = self.send_packet(ts_now, self.next_send)?;
    self.next_send += 1;
    Some(packet_to_send)
}
```

---

## PR 3 — Prevent TSBPD `checked_sub` underflow panic

**Severity:** panic on connection start
**Fork patch:** 3
**Files:** `srt-protocol/src/protocol/receiver/buffer.rs`

### Problem

```rust
// UPSTREAM
let tsbpd_threshold = now - self.tsbpd_latency - self.tsbpd_tolerance;
```

When `now` is early in connection life (before `latency + tolerance` has
elapsed), this subtraction underflows. With `std::time::Instant` this panics
on debug builds and produces a nonsensical instant on release. With
`web_time::Instant` (WASM), the Performance API epoch can be near zero,
making this easy to hit on page load.

### Fix

```rust
let tsbpd_threshold = now.checked_sub(self.tsbpd_latency + self.tsbpd_tolerance);
let too_late_packets = data_packets.take_while(|packet| {
    packet.map_or(true, |(_, packet_time, _message_loc)| {
        tsbpd_threshold.map_or(false, |t| packet_time <= t)
            || !message_loc.contains(PacketLocation::FIRST)
    })
}).collect();
```

When `checked_sub` returns `None` (underflow), no packets are too-late —
correct behavior for a freshly started connection.

---

## PR 4 — Populate `rx_loss_data`, `rx_loss_bytes`, `rx_bandwidth` in statistics

**Severity:** broken stats (fields always report 0)
**Fork patch:** 4 (partial — stats population half)
**Files:** `srt-protocol/src/protocol/receiver/mod.rs`, `srt-protocol/src/connection/mod.rs`

### Problem

`SocketStatistics` declares `rx_loss_data`, `rx_loss_bytes`, and `rx_bandwidth`
but **never assigns them anywhere** in upstream — they are always 0. Any
application reading these for monitoring gets garbage.

### Fix

Track loss in the `ReceivedWithLoss` path:

```rust
// In ReceiverContext::next_data_packet, ReceivedWithLoss branch:
ReceivedWithLoss(loss_list) => {
    let lost = loss_list.iter_decompressed().count() as u64;
    self.stats.rx_loss_data += lost;
    self.stats.rx_loss_bytes += lost * bytes;
    self.output.send_control(now, Nak(loss_list));
}
```

Compute bandwidth via byte-delta in `DuplexConnection::update_statistics`:

```rust
let dt = (now - self.prev_stats_time).as_secs_f64();
if dt > 0.0 {
    let delta = self.stats.rx_bytes.saturating_sub(self.prev_rx_bytes);
    self.stats.rx_bandwidth = (delta as f64 * 8.0 / dt) as u64;
}
self.prev_rx_bytes = self.stats.rx_bytes;
self.prev_stats_time = now;
```

(Requires adding `prev_rx_bytes: u64` and `prev_stats_time: Instant` fields
to `DuplexConnection`, initialized in `new`.)

---

## PR 5 — Populate `tx_average_rtt` in statistics

**Severity:** broken stats (field always reports 0)
**Fork patch:** 11
**Files:** `srt-protocol/src/protocol/sender/mod.rs`, `srt-protocol/src/connection/mod.rs`

### Problem

`SocketStatistics.tx_average_rtt` is declared but **never assigned** — only
`rx_average_rtt` was populated. Publisher-side stats show RTT=0 because the
receiver half is idle.

### Fix

Add `rtt()` accessor to `Sender` / `SendBuffer`:

```rust
// SendBuffer
pub fn rtt(&self) -> Duration {
    self.rtt.mean_as_duration()
}

// Sender
pub fn rtt(&self) -> Duration {
    self.send_buffer.rtt()
}
```

Then assign in `DuplexConnection::update_statistics`:

```rust
self.stats.tx_average_rtt = self.sender.rtt();
```

(While there: also populate `rx_average_rtt` from `self.receiver.rtt()`,
which requires the same accessor on `Receiver`/`ARQ` — currently done
ad-hoc in the fork.)

---

## PR 6 — Handle `CongestionWarning`, `PeerError`, and unknown SRT control without panicking

**Severity:** panic on valid peer messages
**Fork patch:** 12
**Files:** `srt-protocol/src/connection/mod.rs`, `srt-protocol/src/packet/control/srt.rs`

### Problem

Upstream has `todo!()` for control types that any SRT peer can send:

```rust
// UPSTREAM
CongestionWarning => todo!(),
PeerError(_) => todo!(),
```

And `unimplemented!()` for unrecognized SRT extension packets:

```rust
// UPSTREAM (in handle_srt_control_packet)
_ => unimplemented!("{:?}", pack),
```

These are valid messages. A peer implementing the full SRT spec (e.g.,
libsrt-based OBS) will send them.

### Fix

```rust
CongestionWarning => {}
PeerError(_) => {}
```

```rust
_ => self.warn(now, "unhandled srt control", &pack),
```

Also: advertise `TLPKTDROP` and `NAKREPORT` in `SrtShakeFlags::SUPPORTED`:

```rust
const SUPPORTED = Self::TSBPDSND.bits()
    | Self::TSBPDRCV.bits()
    | Self::HAICRYPT.bits()
    | Self::REXMITFLG.bits()
    | Self::TLPKTDROP.bits()
    | Self::NAKREPORT.bits();
```

Both features are implemented in srt-rs but were not advertised to the peer,
so a compliant peer would assume they are unsupported.

---

## PR 7 — Seed RTT from `ConnInitSettings.initial_rtt`

**Severity:** correctness (bad retransmit timing / congestion window on cold start)
**Fork patch:** 9
**Files:** `srt-protocol/src/settings/connection.rs`, `srt-protocol/src/protocol/time/rtt.rs`,
`srt-protocol/src/protocol/receiver/arq.rs`, `srt-protocol/src/protocol/sender/buffer.rs`,
`srt-protocol/src/protocol/pending_connection/hsv5.rs`

### Problem

Upstream populates `ConnectionSettings.rtt` during the handshake but **never
feeds it to `SendBuffer` or `ARQ`** — both initialize with `Rtt::default()`
(mean ~100ms, variance ~25ms). For connections where the actual RTT differs
significantly (e.g., QUIC/WebTransport where the transport layer already knows
the smoothed RTT), this produces bad initial retransmit timers and congestion
window estimates.

### Fix

1. Add `initial_rtt: Option<Duration>` to `ConnInitSettings`.
2. Add `Rtt::from_mean_duration(Duration)` constructor (variance = mean/4,
   matching RFC 6298).
3. Make `SendBuffer::new` and `ARQ::new` use `settings.rtt` instead of
   `Rtt::default()`:
   ```rust
   // SendBuffer::new
   rtt: Rtt::from_mean_duration(settings.rtt),
   // ARQ::new (now takes initial_rtt parameter)
   rtt: Rtt::from_mean_duration(initial_rtt),
   ```
4. In `hsv5.rs`, use `settings.initial_rtt.unwrap_or(measured_rtt)` for both
   the listener and initiator paths, so callers can override the
   handshake-measured RTT.

### Acceptance

If `initial_rtt` is `None`, behavior is identical to upstream. If set, the
EWMA starts at the provided value instead of the default.

---

## PR 8 — Add stats accessor methods on ARQ / Receiver / Sender

**Severity:** ergonomics / observability
**Fork patch:** 4 (partial — accessor half)
**Files:** `srt-protocol/src/protocol/receiver/arq.rs`, `srt-protocol/src/protocol/receiver/mod.rs`,
`srt-protocol/src/protocol/sender/mod.rs`

### Problem

Internal state needed for monitoring (RTT, bandwidth, buffer occupancy) has
no public accessors. Applications must either read `SocketStatistics` (which
doesn't expose everything) or reach into private fields.

### Fix

Add thin accessors:

```rust
// ARQ
pub fn rtt(&self) -> Duration { self.rtt.mean_as_duration() }
pub fn bandwidth_bps(&self) -> u64 { ... }
pub fn buffered_packets(&self) -> usize { self.receive_buffer.len() }
pub fn buffer_available_packets(&self) -> usize { self.receive_buffer.buffer_available() }

// Receiver — delegate to ARQ
pub fn rtt(&self) -> Duration { self.arq.rtt() }
pub fn bandwidth_bps(&self) -> u64 { self.arq.bandwidth_bps() }
pub fn buffered_packets(&self) -> usize { self.arq.buffered_packets() }
pub fn buffer_available_packets(&self) -> usize { self.arq.buffer_available_packets() }

// Sender — delegate to SendBuffer
pub fn rtt(&self) -> Duration { self.send_buffer.rtt() }

// ReceiveBuffer
pub fn len(&self) -> usize { self.buffer.len() }
```

---

## Not included in these PRs (WebSRT-specific features)

These patches are architecturally specific to WebSRT and would need careful
consideration for upstream (they could be merged as opt-in features, but
are less universally applicable):

| Fork patch | What it does | Why not upstream yet |
|---|---|---|
| 1 (`web_time`) | `std::time::Instant` → `web_time::Instant` | Large mechanical diff (26 files). Best handled as a separate PR with `cfg` gates or a crate-level type alias. |
| 7+8 (skip-induction) | `Listen::allow_skip_induction` + `Connect::new_skip_induction` | SRT-over-WebTransport specific. Upstream might prefer it as a feature flag. Clean API, well-tested in WebSRT. |
| 10 (CC-aware retransmit skip) | Skip NAK retransmits predicted to miss TSBPD deadline | Optimization, not a bug fix. Correct (packet is popped from lost list before the check), but bandwidth-saving only. |

---

## Summary

| PR | Patches | Type | Priority |
|----|---------|------|----------|
| 1 | 2+6 | Correctness: broken `Sub` operator | **Critical** — silent math error |
| 2 | 5 | Panic: `SeqNumber` underflow ×3 | **Critical** — panics in production |
| 3 | 3 | Panic: TSBPD threshold underflow | **High** — panics on connection start |
| 4 | 4 (half) | Broken stats: `rx_loss` always 0 | **High** — monitoring is blind |
| 5 | 11 | Broken stats: `tx_rtt` always 0 | **High** — monitoring is blind |
| 6 | 12 | Panic: `todo!()` on valid peer messages | **High** — panics in production |
| 7 | 9 | Correctness: RTT not seeded | **Medium** — bad cold-start timing |
| 8 | 4 (half) | Ergonomics: stats accessors | **Low** — convenience |

PRs 1–6 are unambiguous upstream bugs that affect all srt-rs users, not just
WebSRT. PRs 7–8 are improvements that benefit any embedding application.
