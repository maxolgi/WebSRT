//! Pluggable session-acceptance policy for the gateway.
//!
//! Each incoming WebTransport session is shown to a [`SessionPolicy`], which
//! decides whether to accept or reject it. The library ships four built-in
//! policies that can be composed via [`chain`]:
//!
//! ```no_run
//! use websrt::hooks::{SessionPolicy, path_policy, origin_allowlist_policy, auth_token_policy, chain};
//!
//! fn my_policy() -> impl SessionPolicy {
//!     chain(
//!         path_policy("/wt".into()),
//!         chain(
//!             origin_allowlist_policy(vec!["https://example.com".into()]),
//!             auth_token_policy("s3cret".into()),
//!         ),
//!     )
//! }
//! ```
//!
//! For arbitrary logic (per-stream tokens, rate limits, JWT validation, etc.),
//! implement [`SessionPolicy`] directly on your own type.

use subtle::ConstantTimeEq;
use std::net::SocketAddr;

/// Information about an incoming WebTransport session request.
///
/// Built by the gateway from the `wtransport::SessionRequest` and passed to
/// [`SessionPolicy::decide`] for the accept/reject decision.
pub struct SessionRequest<'a> {
    /// Path portion of the URL (everything before `?`).
    pub path: &'a str,
    /// Query string (everything after `?`; empty if no `?`).
    pub query: &'a str,
    /// Origin header value if the client sent one.
    pub origin: Option<&'a str>,
    /// Authority (HTTP/3 `:authority` pseudo-header; comparable to Host).
    pub authority: &'a str,
    /// Remote peer socket address.
    pub remote_address: SocketAddr,
}

/// Decision returned by [`SessionPolicy::decide`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Accept the session and proceed with the WebTransport handshake.
    Accept,
    /// Reject the session. The gateway responds 404 and continues listening
    /// for the next incoming connection.
    Reject,
}

/// Pluggable session-acceptance policy.
///
/// Implementations must be `Send + Sync + 'static` because the gateway holds
/// the policy behind an `Arc` and consults it from the accept loop.
pub trait SessionPolicy: Send + Sync + 'static {
    /// Inspect `req` and return [`Decision::Accept`] or [`Decision::Reject`].
    fn decide(&self, req: &SessionRequest) -> Decision;
}

// Allow `Arc<dyn SessionPolicy>` (and `Arc<T: SessionPolicy>`) to satisfy
// `SessionPolicy` so type-erased policies can be chained at runtime.
impl<T: SessionPolicy + ?Sized> SessionPolicy for std::sync::Arc<T> {
    fn decide(&self, req: &SessionRequest) -> Decision {
        (**self).decide(req)
    }
}

// ---------------------------------------------------------------------------
// Built-in policies. Each is a small unit struct + SessionPolicy impl, exposed
// via a constructor function for ergonomics (`impl SessionPolicy` return type
// hides the concrete struct from the API).
// ---------------------------------------------------------------------------

/// Policy that accepts only if `req.path == expected`.
pub struct PathPolicy {
    expected: String,
}

impl SessionPolicy for PathPolicy {
    fn decide(&self, req: &SessionRequest) -> Decision {
        if req.path == self.expected {
            Decision::Accept
        } else {
            Decision::Reject
        }
    }
}

/// Constructor for the path-matching policy.
pub fn path_policy(expected: String) -> impl SessionPolicy {
    PathPolicy { expected }
}

/// Policy that accepts only if `req.origin` matches one of `allowed`.
/// Requests with no Origin header are rejected.
pub struct OriginAllowlistPolicy {
    allowed: Vec<String>,
}

impl SessionPolicy for OriginAllowlistPolicy {
    fn decide(&self, req: &SessionRequest) -> Decision {
        match req.origin {
            Some(o) if self.allowed.iter().any(|a| a == o) => Decision::Accept,
            _ => Decision::Reject,
        }
    }
}

/// Constructor for the origin-allowlist policy.
pub fn origin_allowlist_policy(allowed: Vec<String>) -> impl SessionPolicy {
    OriginAllowlistPolicy { allowed }
}

/// Policy that accepts only if `req.query` carries `?token=<expected>`.
/// The comparison uses constant-time equality via `subtle::ConstantTimeEq` to
/// avoid a timing side-channel on auth-token validation.
pub struct AuthTokenPolicy {
    expected: String,
}

impl SessionPolicy for AuthTokenPolicy {
    fn decide(&self, req: &SessionRequest) -> Decision {
        let token_valid = req
            .query
            .split('&')
            .find_map(|kv| {
                let mut parts = kv.splitn(2, '=');
                if parts.next()? == "token" {
                    Some(parts.next().unwrap_or(""))
                } else {
                    None
                }
            })
            .map(|t| {
                let decoded = percent_encoding::percent_decode_str(t).decode_utf8_lossy();
                let matched: bool = decoded.as_bytes().ct_eq(self.expected.as_bytes()).into();
                matched
            })
            .unwrap_or(false);
        if token_valid {
            Decision::Accept
        } else {
            Decision::Reject
        }
    }
}

/// Constructor for the auth-token policy.
pub fn auth_token_policy(expected: String) -> impl SessionPolicy {
    AuthTokenPolicy { expected }
}

/// Policy that accepts only if BOTH `a` and `b` accept. Short-circuits on
/// the first `Reject`. Useful for composing the built-in policies.
pub struct Chain<A, B> {
    pub first: A,
    pub second: B,
}

impl<A: SessionPolicy, B: SessionPolicy> SessionPolicy for Chain<A, B> {
    fn decide(&self, req: &SessionRequest) -> Decision {
        match self.first.decide(req) {
            Decision::Accept => self.second.decide(req),
            Decision::Reject => Decision::Reject,
        }
    }
}

/// Compose two policies: accepts only if both accept.
pub fn chain<A: SessionPolicy, B: SessionPolicy>(a: A, b: B) -> impl SessionPolicy {
    Chain { first: a, second: b }
}

/// Always-accept policy. Useful as a default or for testing.
pub struct AcceptAll;

impl SessionPolicy for AcceptAll {
    fn decide(&self, _req: &SessionRequest) -> Decision {
        Decision::Accept
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req<'a>(path: &'a str, query: &'a str, origin: Option<&'a str>) -> SessionRequest<'a> {
        SessionRequest {
            path,
            query,
            origin,
            authority: "localhost:4433",
            remote_address: "127.0.0.1:0".parse().unwrap(),
        }
    }

    // PathPolicy
    #[test]
    fn path_policy_matches_exact_path() {
        let p = path_policy("/wt".into());
        assert_eq!(p.decide(&req("/wt", "", None)), Decision::Accept);
    }

    #[test]
    fn path_policy_rejects_different_path() {
        let p = path_policy("/wt".into());
        assert_eq!(p.decide(&req("/other", "", None)), Decision::Reject);
    }

    #[test]
    fn path_policy_does_not_match_query_string() {
        let p = path_policy("/wt".into());
        // path is /wt; query is stream=foo — must still match
        assert_eq!(p.decide(&req("/wt", "stream=foo", None)), Decision::Accept);
    }

    // OriginAllowlistPolicy
    #[test]
    fn origin_allowlist_matches_allowed_origin() {
        let p = origin_allowlist_policy(vec!["https://example.com".into()]);
        assert_eq!(
            p.decide(&req("/wt", "", Some("https://example.com"))),
            Decision::Accept
        );
    }

    #[test]
    fn origin_allowlist_rejects_unlisted_origin() {
        let p = origin_allowlist_policy(vec!["https://example.com".into()]);
        assert_eq!(
            p.decide(&req("/wt", "", Some("https://evil.test"))),
            Decision::Reject
        );
    }

    #[test]
    fn origin_allowlist_rejects_missing_origin() {
        let p = origin_allowlist_policy(vec!["https://example.com".into()]);
        assert_eq!(p.decide(&req("/wt", "", None)), Decision::Reject);
    }

    #[test]
    fn origin_allowlist_empty_list_rejects_everything() {
        let p = origin_allowlist_policy(vec![]);
        assert_eq!(p.decide(&req("/wt", "", Some("https://example.com"))), Decision::Reject);
    }

    // AuthTokenPolicy
    #[test]
    fn auth_token_accepts_correct_token() {
        let p = auth_token_policy("s3cret".into());
        assert_eq!(p.decide(&req("/wt", "token=s3cret", None)), Decision::Accept);
    }

    #[test]
    fn auth_token_accepts_with_other_params() {
        let p = auth_token_policy("s3cret".into());
        assert_eq!(
            p.decide(&req("/wt", "stream=foo&token=s3cret&other=bar", None)),
            Decision::Accept
        );
    }

    #[test]
    fn auth_token_rejects_wrong_token() {
        let p = auth_token_policy("s3cret".into());
        assert_eq!(p.decide(&req("/wt", "token=wrong", None)), Decision::Reject);
    }

    #[test]
    fn auth_token_rejects_missing_token() {
        let p = auth_token_policy("s3cret".into());
        assert_eq!(p.decide(&req("/wt", "stream=foo", None)), Decision::Reject);
    }

    #[test]
    fn auth_token_rejects_empty_query() {
        let p = auth_token_policy("s3cret".into());
        assert_eq!(p.decide(&req("/wt", "", None)), Decision::Reject);
    }

    #[test]
    fn auth_token_percent_decodes_value() {
        let p = auth_token_policy("s3cret pass".into());
        assert_eq!(p.decide(&req("/wt", "token=s3cret%20pass", None)), Decision::Accept);
    }

    // Chain
    #[test]
    fn chain_accepts_when_both_accept() {
        let p = chain(path_policy("/wt".into()), auth_token_policy("t".into()));
        assert_eq!(p.decide(&req("/wt", "token=t", None)), Decision::Accept);
    }

    #[test]
    fn chain_rejects_when_first_rejects() {
        let p = chain(path_policy("/wt".into()), auth_token_policy("t".into()));
        assert_eq!(p.decide(&req("/other", "token=t", None)), Decision::Reject);
    }

    #[test]
    fn chain_rejects_when_second_rejects() {
        let p = chain(path_policy("/wt".into()), auth_token_policy("t".into()));
        assert_eq!(p.decide(&req("/wt", "token=wrong", None)), Decision::Reject);
    }

    #[test]
    fn chain_short_circuits_on_first_reject() {
        // Triple-chain to verify associativity.
        let p = chain(
            path_policy("/wt".into()),
            chain(
                origin_allowlist_policy(vec!["https://x.test".into()]),
                auth_token_policy("t".into()),
            ),
        );
        // Wrong path → reject without checking origin/token.
        assert_eq!(p.decide(&req("/other", "token=t", Some("https://x.test"))), Decision::Reject);
    }

    // AcceptAll
    #[test]
    fn accept_all_always_accepts() {
        let p = AcceptAll;
        assert_eq!(p.decide(&req("/anything", "", None)), Decision::Accept);
    }

    // Arc<dyn SessionPolicy> blanket impl
    #[test]
    fn arc_dyn_policy_satisfies_trait() {
        // Verify Arc<dyn SessionPolicy> can be wrapped in another policy.
        let inner: std::sync::Arc<dyn SessionPolicy> = std::sync::Arc::new(path_policy("/wt".into()));
        let outer = chain(inner, AcceptAll);
        assert_eq!(outer.decide(&req("/wt", "", None)), Decision::Accept);
        assert_eq!(outer.decide(&req("/other", "", None)), Decision::Reject);
    }
}
