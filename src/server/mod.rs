//! Server-side networking: shared state, the HTTPS/REST API, the WebSocket and
//! WebTransport audio transports, and static asset serving.
//!
//! Submodules keep each concern self-contained; the public surface used by the
//! rest of the crate is re-exported here.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;

use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use tracing::info;

mod api;
mod assets;
mod state;
mod websocket;
mod webtransport;

pub use state::{AppState, PairingThrottle, StreamState};
pub use webtransport::run_webtransport_server;

use api::{
    handle_ca_download, handle_client_state, handle_get_settings, handle_info, handle_pair,
    handle_renew, handle_stats, handle_update_settings,
};
use assets::handle_static_assets;
use websocket::handle_ws_upgrade;

/// Audio-setting bounds shared by the REST API clamp (`POST /api/settings`) and
/// the CLI flags, so the server, the CLI, and the web UI all agree on the valid
/// ranges. These mirror the web UI slider limits.
pub(crate) const NOISE_GATE_MIN: f32 = 0.0;
pub(crate) const NOISE_GATE_MAX: f32 = 1.0;
/// Linear gain bounds. The web UI shows gain as a perceived-loudness percentage
/// (25%–300%, `amplitude = (percent/100)^1.66`), i.e. 0.1x (-20 dB) to ~6.2x
/// (+16 dB); `GAIN_MAX` leaves a little headroom above the slider's top.
pub(crate) const GAIN_MIN: f32 = 0.1;
pub(crate) const GAIN_MAX: f32 = 6.3;
// The latency-recovery range (50-500ms) is owned by the audio output stage, which
// enforces it: see `audio::LATENCY_THRESHOLD_{MIN,MAX}_MS` and
// `audio::normalize_latency_threshold`, used by the API and the CLI alike.

/// Constant-time byte-slice equality, so response timing does not reveal how many
/// leading bytes of a PIN or session token matched. Returns `false` immediately
/// on a length mismatch (the lengths are not secret here); otherwise it compares
/// every byte without short-circuiting.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Build the axum Router with all routes.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/api/info", get(handle_info))
        .route("/api/pair", post(handle_pair))
        .route("/api/renew", post(handle_renew))
        .route(
            "/api/settings",
            get(handle_get_settings).post(handle_update_settings),
        )
        .route("/api/stats", get(handle_stats))
        .route("/api/client-state", post(handle_client_state))
        .route("/ws", get(handle_ws_upgrade))
        .route("/ca", get(handle_ca_download))
        .fallback(handle_static_assets)
        .layer(middleware::from_fn(reject_cross_origin))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            reject_during_shutdown,
        ))
        .layer(middleware::from_fn(apply_no_store))
        .with_state(state)
}

/// Add `Cache-Control: no-store` to any response that doesn't already set a caching
/// policy. Static assets set their own `no-cache` (with ETag revalidation) and are
/// left untouched; dynamic API / `/ca` responses get `no-store`, so the client's
/// liveness/shutdown detection always reaches the server instead of a
/// heuristically-cached copy (a real risk on iOS Safari, which is why the static
/// assets already opt out of caching).
async fn apply_no_store(req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    resp.headers_mut()
        .entry(header::CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    resp
}

/// Reject any request whose `Origin` header is present and does not match the
/// request's own `Host` — i.e. a cross-site request. Browsers attach `Origin` to
/// state-changing requests (POST, WebSocket upgrades) and cross-origin fetches but
/// omit it on same-origin top-level navigations, so the legitimate page and its
/// own API calls are never blocked.
///
/// Defense-in-depth: the self-signed certificate and the JSON content-type already
/// make remote cross-origin abuse hard, but this also covers the WebSocket upgrade
/// (which browsers exempt from CORS preflight) and stays correct even if the CA is
/// later trusted via `/ca`. It pins nothing to the (dynamic) LAN IP — it only
/// requires a request to originate from the same authority it targets.
async fn reject_cross_origin(req: Request, next: Next) -> Response {
    if let Some(origin) = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    {
        // The Origin's authority (host:port, after the scheme).
        let origin_authority = origin.split_once("://").map(|(_, rest)| rest);
        // The request's own authority, from the HTTP/2 `:authority` pseudo-header
        // (surfaced on the URI) or the HTTP/1.1 `Host` header. Over h2 — which
        // browsers negotiate for HTTPS — there is no `Host` header at all, so the
        // URI is the only source.
        let target_authority = req.uri().authority().map(|a| a.as_str()).or_else(|| {
            req.headers()
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
        });
        // Reject only a genuine cross-origin request: both authorities present and
        // different. If the target authority can't be determined we don't block —
        // this is defense-in-depth, not the primary auth (the session token is).
        if let (Some(origin_authority), Some(target_authority)) =
            (origin_authority, target_authority)
        {
            if origin_authority != target_authority {
                return (StatusCode::FORBIDDEN, "Cross-origin request rejected").into_response();
            }
        }
    }
    next.run(req).await
}

/// Reject every HTTP request with 503 once a graceful shutdown is underway.
///
/// This lets a client probing `/api/info` (or attempting to reconnect via
/// `/api/renew` / `/ws`) learn immediately and deterministically that the server
/// is going away, instead of having to infer it from an unreliable transport
/// close code. The already-upgraded streaming connection is unaffected, since
/// this layer only runs for newly received HTTP requests.
async fn reject_during_shutdown(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    if state.stream.is_shutdown.load(Ordering::SeqCst) {
        return (StatusCode::SERVICE_UNAVAILABLE, "Server is shutting down").into_response();
    }
    next.run(req).await
}

/// Start the HTTPS server (axum) on the given address.
pub async fn run_https_server(
    addr: SocketAddr,
    router: Router,
    tls_config: axum_server::tls_rustls::RustlsConfig,
    handle: axum_server::Handle<SocketAddr>,
) -> anyhow::Result<()> {
    info!(addr = %addr, "HTTPS server listening (TCP)");

    axum_server::bind_rustls(addr, tls_config)
        .handle(handle)
        .serve(router.into_make_service_with_connect_info::<SocketAddr>())
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{build_router, AppState, PairingThrottle, StreamState, GAIN_MAX, NOISE_GATE_MAX};
    use crate::audio::{RingBuffer, LATENCY_THRESHOLD_MAX_MS, LATENCY_THRESHOLD_MIN_MS};
    use crate::tls::TlsIdentity;
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{header, Request, StatusCode};
    use axum::response::Response;
    use serde_json::json;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
    use std::sync::Arc;
    use tower::ServiceExt;

    /// Stand-in client address injected into every test request, since driving the
    /// router with `oneshot` bypasses the `ConnectInfo` the real make-service adds.
    /// `handle_pair` needs it for per-IP throttling.
    fn test_conn_info() -> ConnectInfo<SocketAddr> {
        ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 50000)))
    }

    /// Build an `AppState` for tests without touching any audio device or socket.
    fn test_state() -> AppState {
        let (cancel_tx, _) = tokio::sync::broadcast::channel(16);
        let stream = StreamState {
            ring: Arc::new(RingBuffer::new(48_000)),
            is_connected: Arc::new(AtomicBool::new(false)),
            session_token: Arc::new(parking_lot::Mutex::new(None)),
            noise_gate: Arc::new(AtomicU32::new(0.003f32.to_bits())),
            gain: Arc::new(AtomicU32::new(1.0f32.to_bits())),
            latency_threshold: Arc::new(AtomicU32::new(150)),
            packets_received: Arc::new(AtomicU64::new(0)),
            packets_lost: Arc::new(AtomicU64::new(0)),
            source_sample_rate: Arc::new(AtomicU32::new(48_000)),
            cancel_tx,
            is_shutdown: Arc::new(AtomicBool::new(false)),
            device_ok: Arc::new(AtomicBool::new(true)),
        };
        AppState {
            stream,
            tls_identity: TlsIdentity {
                cert_pem: String::new(),
                key_pem: String::new(),
                cert_der: vec![1, 2, 3],
                cert_hash_base64: "TESTHASH".to_string(),
            },
            pairing_pin: "123456".to_string(),
            wt_port: 8443,
            lan_ip: "192.168.1.42".to_string(),
            pairing_throttle: Arc::new(parking_lot::Mutex::new(PairingThrottle::default())),
            update_status: Arc::new(parking_lot::Mutex::new(None)),
        }
    }

    fn get(uri: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .extension(test_conn_info())
            .body(Body::empty())
            .unwrap()
    }

    fn post(uri: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .extension(test_conn_info())
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    async fn body_json(resp: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn constant_time_eq_matches_only_identical_slices() {
        use super::constant_time_eq;
        assert!(constant_time_eq(b"abc123", b"abc123"));
        assert!(constant_time_eq(b"", b""));
        // A single differing byte fails.
        assert!(!constant_time_eq(b"abc123", b"abc124"));
        // A length mismatch fails (lengths are not secret here).
        assert!(!constant_time_eq(b"abc", b"abc123"));
        assert!(!constant_time_eq(b"abc123", b"abc"));
    }

    #[tokio::test]
    async fn info_returns_metadata() {
        let resp = build_router(test_state())
            .oneshot(get("/api/info"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert_eq!(json["cert_hash"], "TESTHASH");
        assert_eq!(json["wt_port"], 8443);
        assert_eq!(json["lan_ip"], "192.168.1.42");
        // No update check has run in tests, so no update is advertised.
        assert_eq!(json["update_available"], false);
        assert!(json.get("latest_version").is_none());
    }

    #[tokio::test]
    async fn pair_rejects_wrong_pin_and_accepts_correct() {
        let app = build_router(test_state());
        let resp = app
            .clone()
            .oneshot(post("/api/pair", json!({ "pin": "000000" })))
            .await
            .unwrap();
        assert_eq!(body_json(resp).await["success"], false);

        let resp = app
            .oneshot(post("/api/pair", json!({ "pin": "123456" })))
            .await
            .unwrap();
        let json = body_json(resp).await;
        assert_eq!(json["success"], true);
        assert!(json["token"].is_string());
    }

    #[tokio::test]
    async fn pair_locks_out_after_five_failures() {
        let app = build_router(test_state());
        for _ in 0..5 {
            let resp = app
                .clone()
                .oneshot(post("/api/pair", json!({ "pin": "000000" })))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }
        let resp = app
            .oneshot(post("/api/pair", json!({ "pin": "000000" })))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn renew_validates_token() {
        let state = test_state();
        *state.stream.session_token.lock() = Some("tok-abc".to_string());
        let app = build_router(state);

        let resp = app
            .clone()
            .oneshot(post("/api/renew", json!({ "token": "wrong" })))
            .await
            .unwrap();
        assert_eq!(body_json(resp).await["success"], false);

        let resp = app
            .oneshot(post("/api/renew", json!({ "token": "tok-abc" })))
            .await
            .unwrap();
        let json = body_json(resp).await;
        assert_eq!(json["success"], true);
        assert!(json["token"].is_string());
    }

    #[tokio::test]
    async fn settings_clamp_to_bounds() {
        let state = test_state();
        *state.stream.session_token.lock() = Some("tok".to_string());
        let resp = build_router(state)
            .oneshot(post(
                "/api/settings",
                json!({ "token": "tok", "gain": 9.0, "latency_threshold": 5000, "noise_gate": 2.0 }),
            ))
            .await
            .unwrap();
        let json = body_json(resp).await;
        // The settings are f32 on the server and serde_json prints an f32 in its
        // shortest form (6.3, not 6.300000190734863), so compare in f32 — widening
        // GAIN_MAX to f64 would only match bounds that are exact in binary.
        assert_eq!(json["gain"].as_f64().unwrap() as f32, GAIN_MAX);
        assert_eq!(
            json["latency_threshold"].as_u64().unwrap(),
            LATENCY_THRESHOLD_MAX_MS as u64
        );
        assert_eq!(json["noise_gate"].as_f64().unwrap() as f32, NOISE_GATE_MAX);
    }

    #[tokio::test]
    async fn latency_threshold_maps_legacy_off_and_raises_low_values() {
        let state = test_state();
        *state.stream.session_token.lock() = Some("tok".to_string());
        let app = build_router(state);
        // (sent, stored): legacy "Off" (0) is the maximum, not the minimum; values
        // the old slider allowed below the new minimum are raised to it.
        for (sent, stored) in [
            (0, LATENCY_THRESHOLD_MAX_MS),
            (10, LATENCY_THRESHOLD_MIN_MS),
            (150, 150),
        ] {
            let resp = app
                .clone()
                .oneshot(post(
                    "/api/settings",
                    json!({ "token": "tok", "latency_threshold": sent }),
                ))
                .await
                .unwrap();
            assert_eq!(
                body_json(resp).await["latency_threshold"].as_u64().unwrap(),
                stored as u64,
                "latency_threshold {sent} must be stored as {stored}"
            );
        }
    }

    #[tokio::test]
    async fn settings_update_requires_token() {
        let state = test_state();
        *state.stream.session_token.lock() = Some("tok-abc".to_string());
        let app = build_router(state);

        // Missing token -> 401.
        let resp = app
            .clone()
            .oneshot(post("/api/settings", json!({ "gain": 2.0 })))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // Wrong token -> 401.
        let resp = app
            .clone()
            .oneshot(post(
                "/api/settings",
                json!({ "gain": 2.0, "token": "wrong" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // Correct token -> applied.
        let resp = app
            .oneshot(post(
                "/api/settings",
                json!({ "gain": 2.0, "token": "tok-abc" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_json(resp).await["gain"].as_f64().unwrap(), 2.0);
    }

    #[tokio::test]
    async fn shutdown_middleware_returns_503() {
        let state = test_state();
        state.stream.is_shutdown.store(true, Ordering::SeqCst);
        let resp = build_router(state).oneshot(get("/api/info")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn stats_requires_token() {
        let state = test_state();
        *state.stream.session_token.lock() = Some("tok".to_string());
        let app = build_router(state);

        // No token -> 401.
        let resp = app.clone().oneshot(get("/api/stats")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // Valid token in the X-Session-Token header -> 200.
        let req = Request::builder()
            .uri("/api/stats")
            .header("x-session-token", "tok")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn stats_shutdown_503_precedes_auth() {
        // The shutdown 503 must win over the token gate: the client's liveness
        // detection relies on a stats poll still seeing the 503 even though the
        // server is going away.
        let state = test_state();
        *state.stream.session_token.lock() = Some("tok".to_string());
        state.stream.is_shutdown.store(true, Ordering::SeqCst);
        let resp = build_router(state)
            .oneshot(get("/api/stats"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn static_asset_serves_etag_then_304() {
        let app = build_router(test_state());
        let resp = app.clone().oneshot(get("/index.html")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let etag = resp
            .headers()
            .get(header::ETAG)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        let req = Request::builder()
            .uri("/index.html")
            .header(header::IF_NONE_MATCH, &etag)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn static_assets_reject_path_traversal() {
        let app = build_router(test_state());
        for uri in [
            "/../Cargo.toml",
            "/../../src/main.rs",
            "/web/../../Cargo.toml",
        ] {
            let resp = app.clone().oneshot(get(uri)).await.unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "traversal path must be rejected: {uri}"
            );
        }
    }

    #[tokio::test]
    async fn rejects_cross_origin_but_allows_same_origin() {
        let app = build_router(test_state());

        // Cross-origin (Origin authority != Host) is rejected.
        let req = Request::builder()
            .method("POST")
            .uri("/api/pair")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::HOST, "192.168.1.42:8443")
            .header(header::ORIGIN, "https://evil.example")
            .extension(test_conn_info())
            .body(Body::from(json!({ "pin": "123456" }).to_string()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Same-origin (Origin authority == Host) passes the guard.
        let req = Request::builder()
            .method("POST")
            .uri("/api/pair")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::HOST, "192.168.1.42:8443")
            .header(header::ORIGIN, "https://192.168.1.42:8443")
            .extension(test_conn_info())
            .body(Body::from(json!({ "pin": "123456" }).to_string()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_ne!(resp.status(), StatusCode::FORBIDDEN);

        // HTTP/2 same-origin: no `Host` header, the authority lives on the URI
        // (the `:authority` pseudo-header). This must NOT be treated as cross-origin
        // — browsers negotiate h2 for HTTPS, so this is the real client's request.
        let req = Request::builder()
            .method("POST")
            .uri("https://192.168.1.42:8443/api/pair")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ORIGIN, "https://192.168.1.42:8443")
            .extension(test_conn_info())
            .body(Body::from(json!({ "pin": "123456" }).to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_ne!(resp.status(), StatusCode::FORBIDDEN);
    }
}
