//! Gateway-level resource limits and connection tracking.
//!
//! [`GatewayLimits`] configures per-IP session caps, global session caps, and
//! timeout values. [`ConnectionTracker`] enforces those limits at runtime using
//! RAII guards that decrement counters on drop.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Configuration for gateway-level resource limits.
///
/// All limits are **on by default** with safe values. Set fields to `None`
/// to disable a specific limit.
#[derive(Debug, Clone)]
pub struct GatewayLimits {
    /// Maximum concurrent sessions from a single IP address.
    /// Default: `Some(10)`.
    pub max_sessions_per_ip: Option<usize>,

    /// Maximum total concurrent sessions across all IPs.
    /// Default: `Some(1000)`.
    pub max_sessions: Option<usize>,

    /// WebTransport idle timeout. If no datagrams are received within this
    /// duration, the connection is closed.
    /// Default: `10s`.
    pub max_idle_timeout: Duration,

    /// Maximum time to complete the WebTransport handshake after a connect.
    /// Default: `10s`.
    pub handshake_timeout: Duration,
}

impl Default for GatewayLimits {
    fn default() -> Self {
        Self {
            max_sessions_per_ip: Some(10),
            max_sessions: Some(1000),
            max_idle_timeout: Duration::from_secs(10),
            handshake_timeout: Duration::from_secs(10),
        }
    }
}

impl GatewayLimits {
    /// Create a builder for customizing limits.
    pub fn builder() -> GatewayLimitsBuilder {
        GatewayLimitsBuilder::default()
    }
}

/// Builder for [`GatewayLimits`].
#[derive(Debug, Clone)]
pub struct GatewayLimitsBuilder {
    limits: GatewayLimits,
}

impl Default for GatewayLimitsBuilder {
    fn default() -> Self {
        Self {
            limits: GatewayLimits::default(),
        }
    }
}

impl GatewayLimitsBuilder {
    pub fn max_sessions_per_ip(mut self, n: impl Into<Option<usize>>) -> Self {
        self.limits.max_sessions_per_ip = n.into();
        self
    }

    pub fn max_sessions(mut self, n: impl Into<Option<usize>>) -> Self {
        self.limits.max_sessions = n.into();
        self
    }

    pub fn max_idle_timeout(mut self, d: Duration) -> Self {
        self.limits.max_idle_timeout = d;
        self
    }

    pub fn handshake_timeout(mut self, d: Duration) -> Self {
        self.limits.handshake_timeout = d;
        self
    }

    pub fn build(self) -> GatewayLimits {
        self.limits
    }
}

/// RAII guard for a tracked session. Decrements both the per-IP and global
/// counters when dropped.
///
/// Created by [`ConnectionTracker::try_acquire`]. Store one per active session
/// — when the session ends (task completes or aborts), the guard drops and
/// releases the slot.
pub struct SessionGuard {
    tracker: Arc<ConnectionTracker>,
    ip: IpAddr,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.tracker.release(&self.ip);
    }
}

impl std::fmt::Debug for SessionGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionGuard")
            .field("ip", &self.ip)
            .finish()
    }
}

/// Tracks active sessions per-IP and globally. Thread-safe via interior
/// mutability.
///
/// Created once at startup from [`GatewayLimits`] and shared via `Arc` across
/// the accept loop and session tasks. Use [`try_acquire`](Self::try_acquire)
/// before accepting a session; the returned [`SessionGuard`] automatically
/// releases the slot on drop.
pub struct ConnectionTracker {
    limits: GatewayLimits,
    per_ip: Mutex<HashMap<IpAddr, u32>>,
    total: AtomicUsize,
}

impl ConnectionTracker {
    /// Create a new tracker from the given limits.
    pub fn new(limits: GatewayLimits) -> Self {
        Self {
            limits,
            per_ip: Mutex::new(HashMap::new()),
            total: AtomicUsize::new(0),
        }
    }

    /// Current total active session count.
    pub fn total(&self) -> usize {
        self.total.load(Ordering::Relaxed)
    }

    /// Current session count for a specific IP.
    pub fn per_ip(&self, ip: &IpAddr) -> u32 {
        self.per_ip.lock().unwrap().get(ip).copied().unwrap_or(0)
    }

    /// Try to acquire a session slot. Returns `Some(guard)` if within limits,
    /// or `None` if the per-IP or global cap would be exceeded.
    pub fn try_acquire(self: &Arc<Self>, ip: IpAddr) -> Option<SessionGuard> {
        let mut per_ip = self.per_ip.lock().unwrap();
        let current_ip = per_ip.get(&ip).copied().unwrap_or(0);
        let current_total = self.total.load(Ordering::Relaxed);

        if let Some(max_per_ip) = self.limits.max_sessions_per_ip {
            if current_ip as usize >= max_per_ip {
                return None;
            }
        }
        if let Some(max_total) = self.limits.max_sessions {
            if current_total >= max_total {
                return None;
            }
        }

        per_ip.insert(ip, current_ip + 1);
        self.total.fetch_add(1, Ordering::Relaxed);

        Some(SessionGuard {
            tracker: Arc::clone(self),
            ip,
        })
    }

    /// Release a session slot. Called automatically by [`SessionGuard::drop`].
    fn release(&self, ip: &IpAddr) {
        let mut per_ip = self.per_ip.lock().unwrap();
        if let Some(count) = per_ip.get_mut(ip) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                per_ip.remove(ip);
            }
        }
        let _ = self.total.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
            Some(v.saturating_sub(1))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddr};

    fn ip(n: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, n))
    }

    #[test]
    fn default_limits_have_sensible_values() {
        let l = GatewayLimits::default();
        assert_eq!(l.max_sessions_per_ip, Some(10));
        assert_eq!(l.max_sessions, Some(1000));
        assert_eq!(l.max_idle_timeout, Duration::from_secs(10));
        assert_eq!(l.handshake_timeout, Duration::from_secs(10));
    }

    #[test]
    fn builder_customizes_limits() {
        let l = GatewayLimits::builder()
            .max_sessions_per_ip(5)
            .max_sessions(100)
            .max_idle_timeout(Duration::from_secs(30))
            .handshake_timeout(Duration::from_secs(5))
            .build();
        assert_eq!(l.max_sessions_per_ip, Some(5));
        assert_eq!(l.max_sessions, Some(100));
    }

    #[test]
    fn builder_disables_limit_with_none() {
        let l = GatewayLimits::builder()
            .max_sessions_per_ip(None)
            .build();
        assert_eq!(l.max_sessions_per_ip, None);
    }

    #[test]
    fn try_acquire_succeeds_under_limits() {
        let tracker = Arc::new(ConnectionTracker::new(GatewayLimits::default()));
        let g = tracker.try_acquire(ip(1));
        assert!(g.is_some());
        assert_eq!(tracker.total(), 1);
        assert_eq!(tracker.per_ip(&ip(1)), 1);
    }

    #[test]
    fn guard_drop_releases_slot() {
        let tracker = Arc::new(ConnectionTracker::new(GatewayLimits::default()));
        {
            let _g = tracker.try_acquire(ip(1)).unwrap();
            assert_eq!(tracker.total(), 1);
            assert_eq!(tracker.per_ip(&ip(1)), 1);
        }
        assert_eq!(tracker.total(), 0);
        assert_eq!(tracker.per_ip(&ip(1)), 0);
    }

    #[test]
    fn per_ip_cap_enforced() {
        let limits = GatewayLimits::builder().max_sessions_per_ip(2).build();
        let tracker = Arc::new(ConnectionTracker::new(limits));

        let g1 = tracker.try_acquire(ip(1));
        let g2 = tracker.try_acquire(ip(1));
        let g3 = tracker.try_acquire(ip(1));

        assert!(g1.is_some());
        assert!(g2.is_some());
        assert!(g3.is_none(), "third session from same IP should be rejected");
    }

    #[test]
    fn per_ip_cap_does_not_affect_other_ips() {
        let limits = GatewayLimits::builder().max_sessions_per_ip(1).build();
        let tracker = Arc::new(ConnectionTracker::new(limits));

        let g1 = tracker.try_acquire(ip(1)).unwrap();
        let g2 = tracker.try_acquire(ip(2));

        assert!(g2.is_some(), "different IP should not be blocked");
        drop(g1);
        drop(g2);
    }

    #[test]
    fn global_cap_enforced() {
        let limits = GatewayLimits::builder()
            .max_sessions(2)
            .max_sessions_per_ip(None)
            .build();
        let tracker = Arc::new(ConnectionTracker::new(limits));

        let g1 = tracker.try_acquire(ip(1));
        let g2 = tracker.try_acquire(ip(2));
        let g3 = tracker.try_acquire(ip(3));

        assert!(g1.is_some());
        assert!(g2.is_some());
        assert!(g3.is_none(), "third session globally should be rejected");
    }

    #[test]
    fn none_limits_allow_everything() {
        let limits = GatewayLimits::builder()
            .max_sessions(None)
            .max_sessions_per_ip(None)
            .build();
        let tracker = Arc::new(ConnectionTracker::new(limits));

        let mut guards = Vec::new();
        for _ in 0..100 {
            guards.push(tracker.try_acquire(ip(1)).unwrap());
        }
        assert_eq!(tracker.total(), 100);
    }

    #[test]
    fn release_cleans_up_ip_entry() {
        let tracker = Arc::new(ConnectionTracker::new(GatewayLimits::default()));
        {
            let _g = tracker.try_acquire(ip(1)).unwrap();
        }
        // After drop, the IP entry should be removed from the map.
        assert!(tracker.per_ip.lock().unwrap().is_empty());
    }

    #[test]
    fn total_never_underflows() {
        let tracker = Arc::new(ConnectionTracker::new(GatewayLimits::default()));
        let g = tracker.try_acquire(ip(1)).unwrap();
        drop(g);
        // Manually call release again (simulates a bug/double-release).
        tracker.release(&ip(1));
        assert_eq!(tracker.total(), 0, "total should not underflow");
    }
}
