//! Process-wide outbound proxy setting, shared by the REST/media clients and
//! the gateway socket. Kept global because the gateway and the many REST
//! helpers are constructed far from where settings live.

use std::sync::{OnceLock, RwLock};

fn cell() -> &'static RwLock<Option<String>> {
    static PROXY: OnceLock<RwLock<Option<String>>> = OnceLock::new();
    PROXY.get_or_init(|| RwLock::new(None))
}

/// Store the proxy URL; an empty string clears it.
pub(crate) fn set_proxy(url: &str) {
    let value = url.trim();
    let mut guard = cell().write().unwrap();
    *guard = if value.is_empty() { None } else { Some(value.to_string()) };
}

/// The configured proxy URL, if any.
pub(crate) fn proxy() -> Option<String> {
    cell().read().unwrap().clone()
}

/// Applies the configured proxy to a reqwest builder, if one is set. An invalid
/// URL is ignored rather than fatal: a typo shouldn't break every request.
pub(crate) fn with_proxy(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    match proxy().and_then(|url| reqwest::Proxy::all(&url).ok()) {
        Some(p) => builder.proxy(p),
        None => builder,
    }
}

/// Blocking-client variant of [`with_proxy`].
pub(crate) fn with_proxy_blocking(
    builder: reqwest::blocking::ClientBuilder,
) -> reqwest::blocking::ClientBuilder {
    match proxy().and_then(|url| reqwest::Proxy::all(&url).ok()) {
        Some(p) => builder.proxy(p),
        None => builder,
    }
}
