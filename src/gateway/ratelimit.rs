//! Discord rate-limit bookkeeping: the numbers come from response headers, the
//! waiting from [`wait`].
//!
//! Discord answers every request with `X-RateLimit-*` headers and a 429 with
//! `retry_after`. Ignoring them (or retrying a 429 blindly) earns a temporary
//! IP ban, so every REST call asks here first and reports back afterwards.
//! Sleeping is a few lines of glue; the parsing and the bucket maths are pure
//! and tested (`gateway_tests.rs`, `rate_limit_tests`).

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use reqwest::header::HeaderMap;

/// What a response says about the limit for the route just called.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Limit {
    /// How long to wait before the next request on this route.
    pub(crate) wait: Duration,
    /// Discord throttles the whole token, not just this route.
    pub(crate) global: bool,
}

/// Header value as a number; Discord sends seconds like `"0.75"`, `"1"`.
fn number(value: &str) -> Option<f64> {
    let n: f64 = value.trim().parse().ok()?;
    n.is_finite().then_some(n)
}

impl Limit {
    /// The wait implied by `headers`, or `None` when the bucket still has room.
    pub(crate) fn from_headers(headers: &HeaderMap) -> Option<Self> {
        let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
        let global = get("x-ratelimit-global")
            .map(|v| v.trim().eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        // A 429 always carries `retry-after` and is final: don't wait for the
        // bucket's own reset, which can be shorter than what Discord demands.
        if let Some(wait) = get("retry-after").and_then(number).filter(|s| *s > 0.0) {
            return Some(Self {
                wait: Duration::from_secs_f64(wait),
                global,
            });
        }
        // A normal answer only matters when the bucket ran dry; otherwise the
        // next request is free and waiting would stall the client for nothing.
        if get("x-ratelimit-remaining").and_then(number)? > 0.0 {
            return None;
        }
        let wait = get("x-ratelimit-reset-after")
            .and_then(number)
            .filter(|s| *s > 0.0)?;
        Some(Self {
            wait: Duration::from_secs_f64(wait),
            global,
        })
    }
}

/// One "not before" per route. The client talks to a handful of routes at a
/// time, so a plain map is enough; expired entries are dropped on lookup.
fn routes() -> &'static Mutex<HashMap<String, Instant>> {
    static ROUTES: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    ROUTES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A global throttle covers every route, so it can't live in the map.
fn global_until() -> &'static Mutex<Option<Instant>> {
    static GLOBAL: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    GLOBAL.get_or_init(|| Mutex::new(None))
}

/// A panic on another thread must not disable rate limiting for good.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// How long `route` still has to wait; zero when it is free.
pub(crate) fn delay(route: &str) -> Duration {
    let now = Instant::now();
    if let Some(until) = *lock(global_until()) {
        if until > now {
            return until - now;
        }
    }
    let mut map = lock(routes());
    match map.get(route) {
        Some(until) if *until > now => *until - now,
        Some(_) => {
            map.remove(route);
            Duration::ZERO
        }
        None => Duration::ZERO,
    }
}

fn remember(route: &str, limit: Limit) {
    let until = Instant::now() + limit.wait;
    if limit.global {
        let mut global = lock(global_until());
        // Never shorten a longer wait that another route already set.
        if global.map_or(true, |old| old < until) {
            *global = Some(until);
        }
        return;
    }
    let mut map = lock(routes());
    let entry = map.entry(route.to_string()).or_insert(until);
    if *entry < until {
        *entry = until;
    }
}

/// Record what a response said and return the parsed limit, if any.
///
/// Returns `None` for an ordinary response with budget left: nothing to wait
/// for and nothing to remember.
pub(crate) fn note(route: &str, headers: &HeaderMap) -> Option<Limit> {
    let limit = Limit::from_headers(headers)?;
    remember(route, limit);
    Some(limit)
}

/// Sleep until `route` is allowed again; cheap when the bucket is free.
pub(crate) async fn wait(route: &str) {
    let d = delay(route);
    if !d.is_zero() {
        tokio::time::sleep(d).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};
    use std::sync::{Mutex, MutexGuard};

    /// Limits are process-wide, so the tests below take turns and start from a
    /// clean slate: a global 429 from one test must not block another.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn exclusive() -> MutexGuard<'static, ()> {
        let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        *lock(global_until()) = None;
        lock(routes()).clear();
        guard
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    /// A 429's `retry-after` is what Discord demands, in fractional seconds.
    #[test]
    fn retry_after_becomes_the_wait() {
        let limit = Limit::from_headers(&headers(&[("retry-after", "0.75")])).unwrap();
        assert_eq!(limit.wait, Duration::from_millis(750), "retry-after не разобран");
        assert!(!limit.global, "обычный лимит не должен считаться глобальным");
        assert_eq!(
            Limit::from_headers(&headers(&[("retry-after", "2")])).unwrap().wait,
            Duration::from_secs(2),
            "целые секунды тоже встречаются"
        );
    }

    /// `X-RateLimit-Global: true` throttles the whole token.
    #[test]
    fn global_flag_is_read() {
        let limit = Limit::from_headers(&headers(&[
            ("retry-after", "1"),
            ("x-ratelimit-global", "true"),
        ]))
        .unwrap();
        assert!(limit.global, "глобальный флаг потерян");
        let plain = Limit::from_headers(&headers(&[
            ("retry-after", "1"),
            ("x-ratelimit-global", "false"),
        ]))
        .unwrap();
        assert!(!plain.global, "ложный флаг не должен блокировать всё");
    }

    /// Budget left means no wait; a dry bucket waits for its reset.
    #[test]
    fn remaining_decides_whether_to_wait() {
        let roomy = headers(&[
            ("x-ratelimit-remaining", "4"),
            ("x-ratelimit-reset-after", "10"),
        ]);
        assert!(
            Limit::from_headers(&roomy).is_none(),
            "запас есть — ждать нечего"
        );
        let dry = headers(&[
            ("x-ratelimit-remaining", "0"),
            ("x-ratelimit-reset-after", "1.5"),
        ]);
        assert_eq!(
            Limit::from_headers(&dry).unwrap().wait,
            Duration::from_millis(1500),
            "пустой бакет должен ждать своего сброса"
        );
    }

    /// Missing or unparsable headers never invent a wait.
    #[test]
    fn garbage_headers_are_ignored() {
        assert!(Limit::from_headers(&HeaderMap::new()).is_none(), "нет заголовков — нет лимита");
        assert!(
            Limit::from_headers(&headers(&[("retry-after", "soon")])).is_none(),
            "нечисловой retry-after не должен вешать клиента"
        );
        assert!(
            Limit::from_headers(&headers(&[("x-ratelimit-remaining", "0")])).is_none(),
            "без reset-after ждать нечего"
        );
    }

    /// A note pushes the route back, and only that route.
    #[test]
    fn note_blocks_its_own_route_only() {
        let _guard = exclusive();
        let route = "test-note-own-route";
        assert_eq!(delay(route), Duration::ZERO, "до лимита маршрут свободен");
        let limit = note(route, &headers(&[("retry-after", "0.3")])).unwrap();
        assert_eq!(limit.wait, Duration::from_millis(300), "лимит не разобран");
        assert!(delay(route) > Duration::ZERO, "маршрут должен ждать");
        assert!(delay(route) <= Duration::from_millis(300), "ждать дольше нельзя");
        assert_eq!(delay("test-note-other-route"), Duration::ZERO, "соседний маршрут не задет");
    }

    /// A global note also covers routes that were never seen.
    #[test]
    fn global_note_covers_every_route() {
        let _guard = exclusive();
        let limit = note(
            "test-global-note",
            &headers(&[("retry-after", "0.1"), ("x-ratelimit-global", "true")]),
        )
        .unwrap();
        assert!(limit.global, "флаг должен дойти до вызывающего");
        assert!(
            delay("test-global-never-noted") > Duration::ZERO,
            "глобальный лимит должен держать все маршруты"
        );
        // Leave the shared state clean for the tests running in parallel.
        *lock(global_until()) = None;
    }

    /// A second, shorter note never shortens a wait already in place.
    #[test]
    fn shorter_note_does_not_shorten_the_wait() {
        let _guard = exclusive();
        let route = "test-keep-longer-wait";
        note(route, &headers(&[("retry-after", "5")])).unwrap();
        note(route, &headers(&[("retry-after", "0.1")])).unwrap();
        assert!(delay(route) > Duration::from_secs(1), "короткий лимит срезал длинный");
    }
}
