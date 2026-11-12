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

/// Per-bucket state. Discord groups several routes into one bucket and names it
/// with `X-RateLimit-Bucket`; until that header is seen, a route is tracked by
/// its own name (`r:`), afterwards by the shared bucket plus its major
/// parameter (`b:<hash>:<channel|guild|webhook>`).
#[derive(Default)]
struct Limits {
    /// "not before" per key; expired entries are dropped on lookup.
    waits: HashMap<String, Instant>,
    /// Route name → bucket hash learned from the response headers.
    buckets: HashMap<String, String>,
}

fn state() -> &'static Mutex<Limits> {
    static STATE: OnceLock<Mutex<Limits>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(Limits::default()))
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

/// The major parameter of a route: Discord counts limits per `channel_id`,
/// `guild_id` and `webhook_id`. Routes are named `kind:<major>`, so the part
/// after the first colon is the major parameter; routes without one (`dm`,
/// `relationships`) share a single limit. Splitting on the first colon keeps
/// any later colon inside the parameter instead of silently changing the key.
fn major_param(route: &str) -> &str {
    route.split_once(':').map_or("", |(_, major)| major)
}

/// The wait key for a bucket, qualified by the major parameter. The
/// `X-RateLimit-Bucket` hash is explicitly non-inclusive of major parameters,
/// so keying by the hash alone would make a 429 in one channel stall another.
fn bucket_key(bucket: &str, route: &str) -> String {
    format!("b:{bucket}:{}", major_param(route))
}

fn route_key(route: &str) -> String {
    format!("r:{route}")
}

/// How long `route` still has to wait; zero when it is free.
pub(crate) fn delay(route: &str) -> Duration {
    let now = Instant::now();
    if let Some(until) = *lock(global_until()) {
        if until > now {
            return until - now;
        }
    }
    let mut st = lock(state());
    let key = match st.buckets.get(route).cloned() {
        Some(bucket) => bucket_key(&bucket, route),
        None => route_key(route),
    };
    match st.waits.get(&key).copied() {
        Some(until) if until > now => until - now,
        Some(_) => {
            st.waits.remove(&key);
            Duration::ZERO
        }
        None => Duration::ZERO,
    }
}

fn remember(route: &str, bucket: Option<&str>, limit: Limit) {
    let until = Instant::now() + limit.wait;
    if limit.global {
        let mut global = lock(global_until());
        // Never shorten a longer wait that another route already set.
        if global.map_or(true, |old| old < until) {
            *global = Some(until);
        }
        return;
    }
    let mut st = lock(state());
    let key = match bucket {
        Some(bucket) => bucket_key(bucket, route),
        None => match st.buckets.get(route).cloned() {
            Some(bucket) => bucket_key(&bucket, route),
            None => route_key(route),
        },
    };
    let entry = st.waits.entry(key).or_insert(until);
    // Never shorten a longer wait that another route in the same bucket set.
    if *entry < until {
        *entry = until;
    }
}

/// Bucket hash from `X-RateLimit-Bucket`, if the response carried one.
fn bucket_of(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-ratelimit-bucket")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Record what a response said and return the parsed limit, if any.
///
/// The bucket name is recorded even when there is budget left: a later 429 on
/// another route in the same bucket must also apply here. Returns `None` for an
/// ordinary response with budget left — nothing to wait for.
pub(crate) fn note(route: &str, headers: &HeaderMap) -> Option<Limit> {
    let bucket = bucket_of(headers);
    if let Some(bucket) = &bucket {
        let mut st = lock(state());
        let known = st.buckets.insert(route.to_string(), bucket.clone()).as_deref()
            == Some(bucket.as_str());
        if !known {
            // A wait remembered under the route name before the bucket was
            // known belongs to the bucket now; move it so both routes see it.
            if let Some(until) = st.waits.remove(&route_key(route)) {
                let entry = st.waits.entry(bucket_key(bucket, route)).or_insert(until);
                if *entry < until {
                    *entry = until;
                }
            }
        }
        drop(st);
    }
    let limit = Limit::from_headers(headers)?;
    remember(route, bucket.as_deref(), limit);
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
        let mut st = lock(state());
        st.waits.clear();
        st.buckets.clear();
        drop(st);
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

    /// Two routes Discord put in one bucket share the wait, even though only
    /// one of them hit the limit. Same major parameter: same budget.
    #[test]
    fn routes_in_one_bucket_share_the_wait() {
        let _guard = exclusive();
        // Both learn the bucket while it still has room: that alone is enough
        // for a later 429 on either of them to apply to both.
        for route in ["messages:42", "edit:42"] {
            note(
                route,
                &headers(&[
                    ("x-ratelimit-bucket", "shared-abc"),
                    ("x-ratelimit-remaining", "5"),
                ]),
            );
        }
        note(
            "messages:42",
            &headers(&[
                ("x-ratelimit-bucket", "shared-abc"),
                ("x-ratelimit-remaining", "0"),
                ("x-ratelimit-reset-after", "0.4"),
            ]),
        );
        assert!(
            delay("edit:42") > Duration::ZERO,
            "маршрут из того же бакета должен ждать"
        );
        assert!(
            delay("edit:42") <= Duration::from_millis(400),
            "ждать дольше сброса нельзя"
        );
    }

    /// The bucket hash excludes the major parameter, so two channels share the
    /// hash but not the budget: a 429 in one must not stall the other.
    #[test]
    fn same_bucket_hash_in_other_channel_is_independent() {
        let _guard = exclusive();
        for route in ["messages:1", "messages:2"] {
            note(
                route,
                &headers(&[
                    ("x-ratelimit-bucket", "shared-abc"),
                    ("x-ratelimit-remaining", "5"),
                ]),
            );
        }
        note(
            "messages:1",
            &headers(&[
                ("x-ratelimit-bucket", "shared-abc"),
                ("x-ratelimit-remaining", "0"),
                ("x-ratelimit-reset-after", "0.4"),
            ]),
        );
        assert!(delay("messages:1") > Duration::ZERO, "свой канал должен ждать");
        assert_eq!(
            delay("messages:2"),
            Duration::ZERO,
            "другой канал с тем же хешем ждать не должен"
        );
    }

    /// The major parameter is the part after the first colon: a route without
    /// one shares a single limit, and a later colon never changes the key.
    #[test]
    fn major_param_splits_on_the_first_colon() {
        assert_eq!(super::major_param("messages:42"), "42");
        assert_eq!(super::major_param("dm"), "");
        assert_eq!(super::major_param("messages:42:7"), "42:7");
    }

    /// A wait recorded before the bucket was known moves into the bucket when
    /// the next response names it, instead of being lost with the old key.
    #[test]
    fn bucket_learned_later_keeps_the_earlier_wait() {
        let _guard = exclusive();
        let route = "test-bucket-late";
        note(route, &headers(&[("retry-after", "2")])).unwrap();
        note(
            route,
            &headers(&[
                ("x-ratelimit-bucket", "late-b7"),
                ("x-ratelimit-remaining", "9"),
            ]),
        );
        assert!(
            delay(route) > Duration::from_secs(1),
            "ожидание потерялось при смене ключа на бакет"
        );
    }

    /// Different buckets stay independent.
    #[test]
    fn different_buckets_are_independent() {
        let _guard = exclusive();
        note(
            "test-bucket-a",
            &headers(&[
                ("x-ratelimit-bucket", "one"),
                ("x-ratelimit-remaining", "0"),
                ("x-ratelimit-reset-after", "0.4"),
            ]),
        );
        note(
            "test-bucket-b",
            &headers(&[
                ("x-ratelimit-bucket", "two"),
                ("x-ratelimit-remaining", "5"),
            ]),
        );
        assert!(delay("test-bucket-a") > Duration::ZERO, "свой бакет должен ждать");
        assert_eq!(delay("test-bucket-b"), Duration::ZERO, "чужой бакет ждать не должен");
    }
}
