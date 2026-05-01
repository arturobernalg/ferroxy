//! Route table + upstream map + dispatch.
//!
//! All three types share the same lifetime: they are constructed
//! once at config load and consumed (read-only) on the hot path.
//! The hot path performs:
//!   1. `RouteTable::find(&host, &path)`  — O(routes), linear today
//!   2. `UpstreamMap::get(name)`          — O(1) hash lookup
//!   3. `Upstream::forward(req)`          — async, real network I/O
//!
//! No `Arc<Mutex<…>>` or any other synchronisation primitive lives
//! on this path. The map and table are immutable after construction;
//! the binary holds them in an `Arc` so all worker tasks share one
//! copy without locking. Hot-reload (P10) replaces the `Arc<…>`
//! atomically via `arc-swap`.

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use http::header::HOST;
use hyper::body::Incoming;

use conduit_proto::{Body, Request, Response};
use conduit_upstream::{ForwardError, Upstream};

/// One route. Constructed from the config struct via [`Route::from_config`].
#[derive(Debug, Clone)]
pub struct Route {
    /// Optional virtual-host match. `None` matches any host.
    pub host: Option<String>,
    /// Path matcher.
    pub path: PathMatch,
    /// Name of the upstream to dispatch to. Resolved at config-load
    /// time against the [`UpstreamMap`]; if absent, the validator in
    /// `conduit-config` already rejected the document.
    pub upstream: String,
}

/// Path-matching kinds. Regex is parsed but not yet evaluated.
#[derive(Debug, Clone)]
pub enum PathMatch {
    /// Match any path.
    Any,
    /// `path_prefix = "/api"` — request path starts with this prefix.
    Prefix(String),
    /// `path_exact = "/healthz"` — request path equals this string.
    Exact(String),
}

impl Route {
    /// Build a route from the config struct. Picks the first non-empty
    /// path matcher in priority order: exact > prefix > regex (regex
    /// is currently swallowed; see deviations).
    pub fn from_config(cfg: &conduit_config::Route) -> Self {
        let path = if let Some(s) = cfg.match_.path_exact.as_ref() {
            PathMatch::Exact(s.clone())
        } else if let Some(s) = cfg.match_.path_prefix.as_ref() {
            PathMatch::Prefix(s.clone())
        } else {
            PathMatch::Any
        };
        Self {
            host: cfg.match_.host.clone(),
            path,
            upstream: cfg.upstream.clone(),
        }
    }

    fn matches(&self, host: Option<&str>, path: &str) -> bool {
        if let Some(want) = &self.host {
            match host {
                Some(got) if got.eq_ignore_ascii_case(want) => {}
                _ => return false,
            }
        }
        match &self.path {
            PathMatch::Any => true,
            PathMatch::Prefix(p) => path.starts_with(p.as_str()),
            PathMatch::Exact(p) => path == p.as_str(),
        }
    }
}

/// Ordered list of [`Route`]s scanned in declaration order. First
/// match wins.
#[derive(Debug, Clone)]
pub struct RouteTable {
    routes: Vec<Route>,
}

impl RouteTable {
    /// Build from the validated config document. Routes are scanned
    /// in the order they appear in `[[route]]` blocks — the same
    /// order operators see in their config file.
    pub fn from_config(cfg: &conduit_config::Config) -> Self {
        Self {
            routes: cfg.routes.iter().map(Route::from_config).collect(),
        }
    }

    /// Find the first route that matches `(host, path)`. Returns
    /// `None` if no route matches.
    pub fn find(&self, host: Option<&str>, path: &str) -> Option<&Route> {
        self.routes.iter().find(|r| r.matches(host, path))
    }

    /// Number of routes (for observability / tests).
    pub fn len(&self) -> usize {
        self.routes.len()
    }

    /// `true` if the table has no routes. Reject at config-validate
    /// time, but cheap to expose.
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }
}

/// Upstreams indexed by name.
#[derive(Clone, Default)]
pub struct UpstreamMap {
    by_name: HashMap<String, Upstream>,
}

impl std::fmt::Debug for UpstreamMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamMap")
            .field("len", &self.by_name.len())
            .finish_non_exhaustive()
    }
}

impl UpstreamMap {
    /// Build an empty map. Use [`UpstreamMap::insert`] to register
    /// upstreams as they are constructed.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an upstream under `name`. Returns the previous binding
    /// if one existed.
    pub fn insert(&mut self, name: impl Into<String>, upstream: Upstream) -> Option<Upstream> {
        self.by_name.insert(name.into(), upstream)
    }

    /// Look up an upstream by name. Cheap (`HashMap` get).
    pub fn get(&self, name: &str) -> Option<&Upstream> {
        self.by_name.get(name)
    }

    /// Build directly from a config document by constructing default
    /// [`Upstream`]s for every `[[upstream]]` entry. Retry policies
    /// from per-route blocks are *not* applied here — they get applied
    /// per request in [`Dispatch::handle`] (since the same upstream
    /// can be referenced by routes with different retry rules).
    pub fn from_config(cfg: &conduit_config::Config) -> Self {
        let mut map = Self::new();
        for u in &cfg.upstreams {
            // P5 ships with a default (no-retry) Upstream per name and
            // forwards to the upstream's first listed address — load
            // balancing across multiple addresses is P5.x.
            map.insert(
                u.name.clone(),
                Upstream::with_addrs(conduit_upstream::RetryPolicy::default(), u.addrs.clone()),
            );
        }
        map
    }
}

/// Per-worker dispatcher: an [`Arc`]-shared bundle of route table +
/// upstream map. Cloning is a refcount bump.
#[derive(Clone)]
pub struct Dispatch {
    inner: Arc<DispatchInner>,
}

struct DispatchInner {
    routes: RouteTable,
    upstreams: UpstreamMap,
}

impl std::fmt::Debug for Dispatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dispatch")
            .field("routes", &self.inner.routes)
            .field("upstreams", &self.inner.upstreams)
            .finish()
    }
}

impl Dispatch {
    /// Build from a route table and an upstream map.
    pub fn new(routes: RouteTable, upstreams: UpstreamMap) -> Self {
        Self {
            inner: Arc::new(DispatchInner { routes, upstreams }),
        }
    }

    /// Build from a validated config document end-to-end.
    pub fn from_config(cfg: &conduit_config::Config) -> Self {
        Self::new(RouteTable::from_config(cfg), UpstreamMap::from_config(cfg))
    }

    /// Look up the route for the supplied request and dispatch it to
    /// the named upstream's forward path. Returns the upstream
    /// response on success, or a [`DispatchError`] on routing /
    /// forwarding failures.
    pub async fn handle<B>(&self, req: Request<B>) -> Result<Response<Incoming>, DispatchError>
    where
        B: Body<Data = Bytes> + Send + 'static,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let host = req
            .headers()
            .get(HOST)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let path = req.uri().path().to_owned();

        let route = self
            .inner
            .routes
            .find(host.as_deref(), &path)
            .cloned()
            .ok_or(DispatchError::NoRoute)?;
        let upstream = self
            .inner
            .upstreams
            .get(&route.upstream)
            .cloned()
            .ok_or_else(|| DispatchError::UpstreamNotRegistered {
                name: route.upstream.clone(),
            })?;

        upstream.forward(req).await.map_err(DispatchError::Upstream)
    }
}

/// Errors returned by [`Dispatch::handle`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DispatchError {
    /// No route in the table matched the request's host + path.
    #[error("no route matched the request")]
    NoRoute,

    /// The route resolved to an upstream name that isn't registered.
    /// `conduit-config`'s validation rejects this at load time, so
    /// the only way to trigger it in practice is constructing
    /// `RouteTable` and `UpstreamMap` independently.
    #[error("route references upstream `{name}` which is not registered")]
    UpstreamNotRegistered {
        /// The unresolved upstream name.
        name: String,
    },

    /// The upstream client returned an error.
    #[error("upstream forward failed")]
    Upstream(#[source] ForwardError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(host: Option<&str>, path: PathMatch, upstream: &str) -> Route {
        Route {
            host: host.map(str::to_owned),
            path,
            upstream: upstream.to_owned(),
        }
    }

    fn table(routes: Vec<Route>) -> RouteTable {
        RouteTable { routes }
    }

    #[test]
    fn first_matching_route_wins() {
        let t = table(vec![
            route(
                Some("api.example.com"),
                PathMatch::Prefix("/v1/".into()),
                "api-v1",
            ),
            route(Some("api.example.com"), PathMatch::Any, "api-default"),
        ]);
        let r = t.find(Some("api.example.com"), "/v1/items").unwrap();
        assert_eq!(r.upstream, "api-v1");
        let r = t.find(Some("api.example.com"), "/health").unwrap();
        assert_eq!(r.upstream, "api-default");
    }

    #[test]
    fn host_match_is_case_insensitive() {
        let t = table(vec![route(Some("Example.COM"), PathMatch::Any, "u")]);
        assert!(t.find(Some("example.com"), "/").is_some());
        assert!(t.find(Some("EXAMPLE.COM"), "/").is_some());
    }

    #[test]
    fn host_none_matches_only_no_host_routes() {
        let t = table(vec![
            route(Some("a.example.com"), PathMatch::Any, "a"),
            route(None, PathMatch::Any, "fallback"),
        ]);
        assert_eq!(t.find(None, "/").unwrap().upstream, "fallback");
        assert_eq!(t.find(Some("a.example.com"), "/").unwrap().upstream, "a");
        assert_eq!(
            t.find(Some("b.example.com"), "/").unwrap().upstream,
            "fallback"
        );
    }

    #[test]
    fn path_exact_does_not_match_prefix() {
        let t = table(vec![route(None, PathMatch::Exact("/healthz".into()), "h")]);
        assert!(t.find(None, "/healthz").is_some());
        assert!(t.find(None, "/healthz/").is_none());
        assert!(t.find(None, "/healthzy").is_none());
    }

    #[test]
    fn path_prefix_match() {
        let t = table(vec![route(None, PathMatch::Prefix("/api/".into()), "u")]);
        assert!(t.find(None, "/api/").is_some());
        assert!(t.find(None, "/api/users").is_some());
        assert!(t.find(None, "/api").is_none()); // no trailing slash
    }

    #[test]
    fn no_match_returns_none() {
        let t = table(vec![route(Some("a.com"), PathMatch::Any, "a")]);
        assert!(t.find(Some("b.com"), "/").is_none());
        assert!(t.find(None, "/").is_none());
    }

    #[test]
    fn upstream_map_round_trip() {
        let mut m = UpstreamMap::new();
        let u = Upstream::new(conduit_upstream::RetryPolicy::default());
        let prev = m.insert("api", u.clone());
        assert!(prev.is_none());
        assert!(m.get("api").is_some());
        assert!(m.get("missing").is_none());
    }
}
