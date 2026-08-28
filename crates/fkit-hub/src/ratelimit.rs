//! Rate limiting.
//!
//! Argon2 makes a password guess expensive for *this* server, not for the
//! attacker: they can still ask as often as the network allows. Everything
//! cheap to request and expensive to answer — a login, a sign-up, an email
//! send — needs a ceiling that does not depend on the handler being slow.
//!
//! # Why a trait
//!
//! One hub process can keep counters in its own memory. Two cannot: a limit of
//! five per minute becomes ten the moment a second replica starts, and a
//! restart forgets everything an attacker has spent. So the backend is behind
//! [`RateLimiter`] and the handlers only ever see the trait — a Valkey-backed
//! implementation is a new file and one line in `main`, not a change to any
//! route.
//!
//! The trait is deliberately shaped so a networked backend can answer in a
//! single round trip: [`RateLimiter::check`] both counts the event and returns
//! the verdict. Valkey does that with `INCR` plus `EXPIRE` on first write,
//! which is exactly the fixed window [`MemoryLimiter`] keeps locally.
//!
//! The futures are boxed rather than `async fn` in the trait, because handlers
//! hold an `Arc<dyn RateLimiter>` and `async fn` in a trait is not dyn-safe.
//! That is the whole reason for the `BoxFuture` noise below.
//!
//! # Fixed windows
//!
//! A fixed window admits a burst across a boundary: five at 59s and five at
//! 61s is ten in two seconds. For "stop unlimited password guessing" that is
//! irrelevant — the attacker still gets 5/minute sustained — and it buys an
//! implementation that is one `INCR` on any backend. A sliding window or GCRA
//! is a swap behind the same trait if a limit ever needs to be exact.

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::HeaderMap;

/// Boxed future, so the trait stays usable as `dyn RateLimiter`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// At most `limit` events per `window`, per key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quota {
    pub limit: u32,
    pub window: Duration,
}

impl Quota {
    pub const fn per_minute(limit: u32) -> Self {
        Quota { limit, window: Duration::from_secs(60) }
    }
    pub const fn per_hour(limit: u32) -> Self {
        Quota { limit, window: Duration::from_secs(3600) }
    }
}

/// The limiter's verdict. `retry_after` is meaningless unless `allowed` is false.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decision {
    pub allowed: bool,
    pub retry_after: Duration,
}

impl Decision {
    pub const ALLOW: Decision = Decision { allowed: true, retry_after: Duration::ZERO };
}

pub trait RateLimiter: Send + Sync + 'static {
    /// Count one event against `key` and say whether it stayed within `quota`.
    fn check<'a>(&'a self, key: &'a str, quota: Quota) -> BoxFuture<'a, Decision>;

    /// Forget `key`.
    ///
    /// Called after a *successful* login so that failures counted against a
    /// username cannot be used to lock its owner out: an attacker guessing at
    /// someone else's account must not be able to deny them their own.
    fn reset<'a>(&'a self, key: &'a str) -> BoxFuture<'a, ()>;
}

/// Counters in this process's memory.
///
/// Correct for a single hub, which is the shipped topology. See the module
/// docs for what changes when there are two.
pub struct MemoryLimiter {
    windows: Mutex<HashMap<String, Window>>,
    /// Ceiling on distinct tracked keys. An attacker with a wide address range
    /// would otherwise turn a limiter into a memory leak. On overflow the map
    /// is swept of expired entries, and only if that frees nothing is it
    /// cleared — losing counts is survivable, unbounded growth is not.
    capacity: usize,
}

#[derive(Clone, Copy)]
struct Window {
    count: u32,
    resets_at: Instant,
}

impl Default for MemoryLimiter {
    fn default() -> Self {
        Self::new(100_000)
    }
}

impl MemoryLimiter {
    pub fn new(capacity: usize) -> Self {
        MemoryLimiter { windows: Mutex::new(HashMap::new()), capacity }
    }

    fn decide(&self, key: &str, quota: Quota, now: Instant) -> Decision {
        let mut map = self.windows.lock().unwrap_or_else(|e| e.into_inner());

        if map.len() >= self.capacity && !map.contains_key(key) {
            map.retain(|_, w| w.resets_at > now);
            if map.len() >= self.capacity {
                map.clear();
            }
        }

        let w = map
            .entry(key.to_owned())
            .and_modify(|w| {
                if w.resets_at <= now {
                    w.count = 0;
                    w.resets_at = now + quota.window;
                }
            })
            .or_insert(Window { count: 0, resets_at: now + quota.window });

        w.count = w.count.saturating_add(1);

        if w.count > quota.limit {
            Decision { allowed: false, retry_after: w.resets_at.saturating_duration_since(now) }
        } else {
            Decision::ALLOW
        }
    }
}

impl RateLimiter for MemoryLimiter {
    fn check<'a>(&'a self, key: &'a str, quota: Quota) -> BoxFuture<'a, Decision> {
        let d = self.decide(key, quota, Instant::now());
        Box::pin(async move { d })
    }

    fn reset<'a>(&'a self, key: &'a str) -> BoxFuture<'a, ()> {
        self.windows.lock().unwrap_or_else(|e| e.into_inner()).remove(key);
        Box::pin(async {})
    }
}

/// Who to charge a request to.
///
/// Behind a reverse proxy every request arrives from the proxy, so the peer
/// address is the same for everyone and an IP limit would be an outage. The
/// forwarded header is the real client — but it is a *request header*, so
/// trusting it on a directly-exposed server lets anyone mint a fresh identity
/// per request and skip the limit entirely. Hence the flag: it must be turned
/// on deliberately, by someone who knows a proxy is in front.
///
/// With `trust_proxy` the **rightmost** entry is used, not the leftmost. Each
/// hop appends what it saw, so a client sending its own `X-Forwarded-For` gets
/// that value pushed left when the real proxy appends the address it observed.
/// The last entry is the only one written by something we trust. This assumes
/// exactly one proxy in front; with two, the last entry is the inner proxy and
/// the count needs to change with it.
pub fn client_ip(headers: &HeaderMap, peer: Option<SocketAddr>, trust_proxy: bool) -> String {
    if trust_proxy
        && let Some(fwd) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok())
        && let Some(last) = fwd.rsplit(',').map(str::trim).find(|s| !s.is_empty())
    {
        return last.to_owned();
    }
    peer.map(|a| a.ip().to_string()).unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    #[test]
    fn allows_up_to_the_limit_then_refuses() {
        let l = MemoryLimiter::default();
        let t = Instant::now();
        let q = Quota { limit: 3, window: Duration::from_secs(60) };
        for i in 1..=3 {
            assert!(l.decide("ip", q, t).allowed, "request {i} should be allowed");
        }
        let d = l.decide("ip", q, t);
        assert!(!d.allowed);
        assert!(d.retry_after > Duration::ZERO, "a refusal must say when to come back");
    }

    #[test]
    fn the_window_rolls_over() {
        let l = MemoryLimiter::default();
        let t = Instant::now();
        let q = Quota { limit: 1, window: Duration::from_secs(60) };
        assert!(l.decide("ip", q, t).allowed);
        assert!(!l.decide("ip", q, at(t, 30)).allowed);
        assert!(l.decide("ip", q, at(t, 61)).allowed, "a new window starts clean");
    }

    #[test]
    fn keys_are_counted_separately() {
        let l = MemoryLimiter::default();
        let t = Instant::now();
        let q = Quota { limit: 1, window: Duration::from_secs(60) };
        assert!(l.decide("a", q, t).allowed);
        assert!(!l.decide("a", q, t).allowed);
        assert!(l.decide("b", q, t).allowed, "one key's exhaustion is not another's");
    }

    #[test]
    fn reset_clears_a_key() {
        let l = MemoryLimiter::default();
        let t = Instant::now();
        let q = Quota { limit: 1, window: Duration::from_secs(60) };
        assert!(l.decide("user", q, t).allowed);
        assert!(!l.decide("user", q, t).allowed);
        l.windows.lock().unwrap().remove("user");
        assert!(l.decide("user", q, t).allowed, "a success must un-penalise the account");
    }

    #[test]
    fn tracked_keys_stay_bounded() {
        let l = MemoryLimiter::new(8);
        let t = Instant::now();
        let q = Quota { limit: 1, window: Duration::from_secs(60) };
        for i in 0..200 {
            l.decide(&format!("ip-{i}"), q, t);
        }
        assert!(
            l.windows.lock().unwrap().len() <= 8,
            "a wide address range must not grow the map without bound"
        );
    }

    #[test]
    fn forwarded_header_is_ignored_unless_a_proxy_is_trusted() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "1.2.3.4".parse().unwrap());
        let peer: SocketAddr = "10.0.0.9:5000".parse().unwrap();
        assert_eq!(client_ip(&h, Some(peer), false), "10.0.0.9");
        assert_eq!(client_ip(&h, Some(peer), true), "1.2.3.4");
    }

    #[test]
    fn a_spoofed_forwarded_entry_does_not_win() {
        // The client claimed 9.9.9.9; our proxy appended what it actually saw.
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "9.9.9.9, 1.2.3.4".parse().unwrap());
        let peer: SocketAddr = "10.0.0.9:5000".parse().unwrap();
        assert_eq!(client_ip(&h, Some(peer), true), "1.2.3.4");
    }
}
