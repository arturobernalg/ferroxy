//! Admin endpoint server.
//!
//! Bound to `cfg.server.admin_listen` by the binary. Currently
//! responds to `/health` and `/ready` only.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use http::{Method, Request, Response, StatusCode};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use crate::metrics::MetricsHandle;

/// A thunk that renders the `/upstreams` endpoint body. The binary
/// supplies it because the admin crate cannot reach into
/// `conduit-lifecycle` without inverting the layer model.
pub type UpstreamsRenderer = Arc<dyn Fn() -> String + Send + Sync>;

/// Process start time, captured the first time the admin server is
/// asked to render `/metrics`. Stored as `Option<Instant>` behind a
/// `OnceLock` so unit tests that build responses without first
/// running the server still see a sensible uptime (`0`).
static PROCESS_START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

fn process_start() -> Instant {
    *PROCESS_START.get_or_init(Instant::now)
}

/// Errors surfaced from [`serve_admin`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AdminError {
    /// Could not bind the admin listener.
    #[error("admin bind failed for {addr}")]
    Bind {
        /// Address attempted.
        addr: SocketAddr,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

/// Run the admin HTTP/1.1 server until `cancel` flips. Each accepted
/// connection is handled on a tokio task; in-flight connections may
/// outlive the cancel signal but only until the request body / response
/// finishes — they are short by design.
///
/// The function returns when the listener stops accepting (cancel
/// observed). Returns `Ok(())` on clean shutdown, `Err(AdminError)`
/// only on bind failure (every per-connection error is logged and
/// otherwise discarded).
pub async fn serve_admin(
    addr: SocketAddr,
    cancel: Arc<AtomicBool>,
    metrics: Arc<MetricsHandle>,
    upstreams_renderer: Option<UpstreamsRenderer>,
) -> Result<(), AdminError> {
    // Anchor the uptime gauge from the moment the admin server binds.
    let _ = process_start();
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|source| AdminError::Bind { addr, source })?;
    tracing::info!(%addr, "admin listening");

    let poll = std::time::Duration::from_millis(100);
    loop {
        if cancel.load(Ordering::Acquire) {
            break;
        }
        match tokio::time::timeout(poll, listener.accept()).await {
            Ok(Ok((stream, peer))) => {
                let metrics = Arc::clone(&metrics);
                let renderer = upstreams_renderer.clone();
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let svc = service_fn(move |req: Request<Incoming>| {
                        let metrics = Arc::clone(&metrics);
                        let renderer = renderer.clone();
                        async move { Ok::<_, Infallible>(handle(&req, &metrics, renderer.as_ref())) }
                    });
                    if let Err(e) = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, svc)
                        .await
                    {
                        tracing::debug!(?peer, error = ?e, "admin connection ended with error");
                    }
                });
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "admin accept error");
            }
            Err(_elapsed) => {
                // fall through to next-iter cancel check
            }
        }
    }
    Ok(())
}

/// Per-request handler. Sync — every admin endpoint completes
/// without awaiting; the async wrapper at the call site keeps the
/// hyper `service_fn` signature happy.
fn handle(
    req: &Request<Incoming>,
    metrics: &MetricsHandle,
    upstreams: Option<&UpstreamsRenderer>,
) -> Response<Full<Bytes>> {
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/health") => plain_text(StatusCode::OK, "ok\n"),
        (&Method::GET, "/ready") => plain_text(StatusCode::OK, "ready\n"),
        (&Method::GET, "/metrics") => metrics_response(metrics),
        (&Method::GET, "/upstreams") => match upstreams {
            Some(r) => plain_text_owned(StatusCode::OK, r()),
            None => plain_text(StatusCode::NOT_FOUND, "not found\n"),
        },
        (&Method::GET, "/") => plain_text(
            StatusCode::OK,
            "conduit admin\nendpoints: /health /ready /metrics /upstreams\n",
        ),
        _ => plain_text(StatusCode::NOT_FOUND, "not found\n"),
    }
}

/// Like `plain_text` but for an owned string — the renderer produces
/// a fresh string every call.
fn plain_text_owned(status: StatusCode, body: String) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .expect("owned admin response builder")
}

/// Render the Prometheus 0.0.4 text-format exposition: synthetic
/// uptime + `build_info` gauges concatenated with the data-plane-fed
/// counter family from `MetricsHandle`.
fn metrics_response(metrics: &MetricsHandle) -> Response<Full<Bytes>> {
    let uptime = process_start().elapsed().as_secs_f64();
    let version = env!("CARGO_PKG_VERSION");
    let body = format!(
        "# HELP conduit_uptime_seconds Time since the admin server bound, in seconds.\n\
         # TYPE conduit_uptime_seconds gauge\n\
         conduit_uptime_seconds {uptime}\n\
         # HELP conduit_build_info Build information; value is always 1.\n\
         # TYPE conduit_build_info gauge\n\
         conduit_build_info{{version=\"{version}\"}} 1\n\
         {data_plane}",
        data_plane = metrics.render(),
    );
    Response::builder()
        .status(StatusCode::OK)
        // Prometheus text exposition content-type.
        .header("content-type", "text/plain; version=0.0.4; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .expect("build metrics response")
}

fn plain_text(status: StatusCode, body: &'static str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from_static(body.as_bytes())))
        .expect("static admin response builder")
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    #[tokio::test]
    async fn plain_text_builds() {
        let r = plain_text(StatusCode::OK, "x");
        assert_eq!(r.status(), StatusCode::OK);
        let b = r.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&b[..], b"x");
    }

    #[test]
    fn header_set_correctly() {
        let r = plain_text(StatusCode::NOT_FOUND, "nope\n");
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            r.headers()
                .get("content-type")
                .map(http::HeaderValue::as_bytes),
            Some(&b"text/plain; charset=utf-8"[..]),
        );
    }

    #[tokio::test]
    async fn metrics_renders_prometheus_exposition() {
        let m = MetricsHandle::new();
        m.observe_request_start();
        m.observe_status(200);
        let r = metrics_response(&m);
        assert_eq!(r.status(), StatusCode::OK);
        assert!(r
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("version=0.0.4")));
        let body = r.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("# TYPE conduit_uptime_seconds gauge"));
        assert!(text.contains("conduit_build_info{version=\""));
        // Counter family is concatenated after the gauges.
        assert!(text.contains("conduit_requests_total 1"));
        assert!(text.contains("conduit_responses_total{class=\"2xx\"} 1"));
        // Uptime line shape: `conduit_uptime_seconds <number>\n`.
        let line = text
            .lines()
            .find(|l| l.starts_with("conduit_uptime_seconds "))
            .expect("uptime line");
        let value = line.trim_start_matches("conduit_uptime_seconds ").trim();
        let parsed: f64 = value.parse().expect("uptime is a number");
        assert!(parsed >= 0.0);
    }
}
