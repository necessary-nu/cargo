//! Utilities for network proxies.

use crate::util::context::CargoHttpConfig;
use crate::util::context::GlobalContext;

/// Proxy environment variables that are picked up by libcurl.
const LIBCURL_HTTP_PROXY_ENVS: [&str; 4] =
    ["http_proxy", "HTTP_PROXY", "https_proxy", "HTTPS_PROXY"];

/// Finds an explicit HTTP proxy if one is available.
///
/// Favor [Cargo's `http.proxy`], then [Git's `http.proxy`].
/// Proxies specified via environment variables are picked up by libcurl.
/// See [`LIBCURL_HTTP_PROXY_ENVS`].
///
/// [Cargo's `http.proxy`]: https://doc.rust-lang.org/nightly/cargo/reference/config.html#httpproxy
/// [Git's `http.proxy`]: https://git-scm.com/docs/git-config#Documentation/git-config.txt-httpproxy
pub fn http_proxy(http: &CargoHttpConfig) -> Option<String> {
    if let Some(s) = &http.proxy {
        return Some(s.into());
    }
    // Try to discover a repo and read http.proxy from its config (includes global config)
    if let Ok(repo) = gix::discover(".") {
        if let Some(proxy) = repo.config_snapshot().string("http.proxy") {
            return Some(proxy.to_string());
        }
    }
    // Fall back to global config if not in a repo
    gix::config::File::from_globals()
        .ok()
        .and_then(|cfg| cfg.string("http.proxy").map(|s| s.to_string()))
}

/// Determine if an http proxy exists.
///
/// Checks the following for existence, in order:
///
/// * Cargo's `http.proxy`
/// * Git's `http.proxy`
/// * `http_proxy` env var
/// * `HTTP_PROXY` env var
/// * `https_proxy` env var
/// * `HTTPS_PROXY` env var
pub fn http_proxy_exists(http: &CargoHttpConfig, gctx: &GlobalContext) -> bool {
    http_proxy(http).is_some()
        || LIBCURL_HTTP_PROXY_ENVS
            .iter()
            .any(|v| gctx.get_env(v).is_ok())
}
