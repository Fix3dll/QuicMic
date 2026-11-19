//! REST API: server info, pairing, token renewal, stats, settings, CA download.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::Instant;

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::state::AppState;

#[derive(Serialize)]
pub(super) struct ServerInfo {
    cert_hash: String,
    wt_port: u16,
    lan_ip: String,
    /// True when the startup update check found a strictly newer release.
    update_available: bool,
    /// The newer release tag (e.g. `v0.2.0`), present only when one was found.
    #[serde(skip_serializing_if = "Option::is_none")]
    latest_version: Option<String>,
    /// Releases page URL for the web UI's update banner link.
    releases_url: String,
}

#[derive(Deserialize)]
pub(super) struct PairRequest {
    pin: String,
}

#[derive(Serialize)]
pub(super) struct PairResponse {
    success: bool,
    token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct RenewRequest {
    token: String,
}

#[derive(Serialize)]
pub(super) struct RenewResponse {
    success: bool,
    token: Option<String>,
}

#[derive(Serialize)]
pub(super) struct StatsResponse {
    packets_received: u64,
    packets_lost: u64,
    loss_percent: f64,
    buffer_level: usize,
    /// Buffer depth in milliseconds, computed from the active source sample rate so
    /// it is accurate at any capture rate (the client used to assume 48 kHz).
    buffer_ms: u64,
    buffer_capacity: usize,
    connected: bool,
    /// False while the output device is lost and the audio supervisor is rebuilding.
    audio_device_ok: bool,
}

#[derive(Deserialize)]
pub(super) struct SettingsUpdate {
    token: Option<String>,
    noise_gate: Option<f32>,
    gain: Option<f32>,
    latency_threshold: Option<u32>,
}

#[derive(Serialize)]
pub(super) struct SettingsResponse {
    noise_gate: f32,
    gain: f32,
    latency_threshold: u32,
}

/// Generate a cryptographically random 64-char hex token.
fn generate_hex_token() -> String {
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes);
    let mut s = String::with_capacity(64);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

/// GET /api/info — Server metadata including the cert hash for WebTransport.
pub(super) async fn handle_info(State(state): State<AppState>) -> Json<ServerInfo> {
    let latest_version = state.update_status.lock().clone();
    Json(ServerInfo {
        cert_hash: state.tls_identity.cert_hash_base64.clone(),
        wt_port: state.wt_port,
        lan_ip: state.lan_ip.clone(),
        update_available: latest_version.is_some(),
        latest_version,
        releases_url: crate::update_check::releases_url(),
    })
}

/// POST /api/pair — Validate PIN and issue a session token.
/// Includes per-IP brute-force protection: 5 failed attempts triggers a 30s
/// lockout for that client only.
pub(super) async fn handle_pair(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<PairRequest>,
) -> Response {
    let ip = addr.ip();

    // All brute-force bookkeeping runs under a single short lock, so the lockout
    // check, the failed-attempt count, and the lockout deadline are updated as one
    // unit and can never drift apart under concurrent attempts. The critical
    // section is fully synchronous — no `.await` is held across the guard.
    {
        let now = Instant::now();
        let mut throttle = state.pairing_throttle.lock();

        // Still locked out from a previous burst of failures (this IP)?
        if let Some(remaining) = throttle.locked_remaining(ip, now) {
            drop(throttle);
            warn!("Pairing locked out for {}s", remaining);
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(PairResponse {
                    success: false,
                    token: None,
                    error: Some(format!("Too many attempts. Try again in {}s.", remaining)),
                }),
            )
                .into_response();
        }

        if !super::constant_time_eq(body.pin.trim().as_bytes(), state.pairing_pin.as_bytes()) {
            // The attempted PIN is deliberately not logged — it is a secret (and
            // often a near-miss of the real one).
            let (attempts, locked) = throttle.register_failure(ip, now);
            drop(throttle);

            warn!(attempts, "Pairing failed: incorrect PIN");
            if locked {
                warn!("Too many failed pairing attempts — locked out for 30s");
            }

            return Json(PairResponse {
                success: false,
                token: None,
                error: Some("Incorrect PIN".to_string()),
            })
            .into_response();
        }

        // Success: clear any accumulated failures for this IP.
        throttle.clear(ip);
    }

    let token = generate_hex_token();
    {
        let mut guard = state.stream.session_token.lock();
        *guard = Some(token.clone());
    }

    info!("Device paired successfully");

    Json(PairResponse {
        success: true,
        token: Some(token),
        error: None,
    })
    .into_response()
}

/// POST /api/renew — Validate existing token and issue a new one.
pub(super) async fn handle_renew(
    State(state): State<AppState>,
    Json(body): Json<RenewRequest>,
) -> Json<RenewResponse> {
    let mut new_token = None;

    {
        let mut token_guard = state.stream.session_token.lock();
        if let Some(expected) = token_guard.as_ref() {
            if super::constant_time_eq(expected.as_bytes(), body.token.as_bytes()) {
                let token = generate_hex_token();
                *token_guard = Some(token.clone());
                new_token = Some(token);
            }
        }
    }

    if let Some(token) = new_token {
        info!("Session token renewed (invalidating old connections)");
        let _ = state.stream.cancel_tx.send(());
        // Give the old session ~50ms to receive the cancellation and release the
        // connection slot before the client opens its new stream (smooths handover).
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        Json(RenewResponse {
            success: true,
            token: Some(token),
        })
    } else {
        Json(RenewResponse {
            success: false,
            token: None,
        })
    }
}

/// GET /api/stats — Connection and audio stream statistics.
///
/// Requires the session token (sent as the `X-Session-Token` header): stats are
/// only polled by an already-paired client, so this keeps connection/buffer
/// telemetry off open access. The shutdown `503` still takes precedence — the
/// `reject_during_shutdown` middleware runs before this handler, so a
/// shutting-down server answers `503` regardless of the token and the client's
/// liveness detection is unaffected.
pub(super) async fn handle_stats(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let authorized = {
        let provided = headers.get("x-session-token").and_then(|v| v.to_str().ok());
        let guard = state.stream.session_token.lock();
        matches!(
            (guard.as_ref(), provided),
            (Some(expected), Some(p)) if super::constant_time_eq(expected.as_bytes(), p.as_bytes())
        )
    };
    if !authorized {
        return (StatusCode::UNAUTHORIZED, "Invalid or missing token").into_response();
    }

    let received = state.stream.packets_received.load(Ordering::Relaxed);
    let lost = state.stream.packets_lost.load(Ordering::Relaxed);
    let loss_pct = if received + lost > 0 {
        (lost as f64) / ((received + lost) as f64) * 100.0
    } else {
        0.0
    };

    let buffer_level = state.stream.ring.len();
    // Convert the buffer depth to milliseconds using the client's actual capture
    // rate, so the figure is correct for non-48 kHz sources too.
    let sample_rate = state
        .stream
        .source_sample_rate
        .load(Ordering::Relaxed)
        .max(1);
    let buffer_ms = (buffer_level as u64 * 1000) / sample_rate as u64;

    Json(StatsResponse {
        packets_received: received,
        packets_lost: lost,
        loss_percent: (loss_pct * 1000.0).round() / 1000.0, // 3 decimal places
        buffer_level,
        buffer_ms,
        buffer_capacity: state.stream.ring.capacity(),
        connected: state.stream.is_connected.load(Ordering::Relaxed),
        audio_device_ok: state.stream.device_ok.load(Ordering::Relaxed),
    })
    .into_response()
}

/// GET /api/settings — Current audio processing settings.
pub(super) async fn handle_get_settings(State(state): State<AppState>) -> Json<SettingsResponse> {
    Json(SettingsResponse {
        noise_gate: f32::from_bits(state.stream.noise_gate.load(Ordering::Relaxed)),
        gain: f32::from_bits(state.stream.gain.load(Ordering::Relaxed)),
        latency_threshold: state.stream.latency_threshold.load(Ordering::Relaxed),
    })
}

/// POST /api/settings — Update noise gate, gain and/or latency threshold dynamically.
pub(super) async fn handle_update_settings(
    State(state): State<AppState>,
    Json(body): Json<SettingsUpdate>,
) -> Response {
    // Changing settings requires a valid session token (GET stays open).
    let authorized = {
        let guard = state.stream.session_token.lock();
        match (guard.as_ref(), body.token.as_ref()) {
            (Some(expected), Some(provided)) => {
                super::constant_time_eq(expected.as_bytes(), provided.as_bytes())
            }
            _ => false,
        }
    };
    if !authorized {
        return (StatusCode::UNAUTHORIZED, "Invalid or missing token").into_response();
    }

    if let Some(ng) = body.noise_gate {
        let clamped = ng.clamp(super::NOISE_GATE_MIN, super::NOISE_GATE_MAX);
        state
            .stream
            .noise_gate
            .store(clamped.to_bits(), Ordering::Relaxed);
        info!(noise_gate = clamped, "Noise gate updated");
    }
    if let Some(g) = body.gain {
        let clamped = g.clamp(super::GAIN_MIN, super::GAIN_MAX);
        state
            .stream
            .gain
            .store(clamped.to_bits(), Ordering::Relaxed);
        info!(gain = clamped, "Gain updated");
    }
    if let Some(lt) = body.latency_threshold {
        // Legacy 0 ("Off") maps to the maximum; anything else is clamped into range.
        let clamped = crate::audio::normalize_latency_threshold(lt);
        state
            .stream
            .latency_threshold
            .store(clamped, Ordering::Relaxed);
        info!(latency_threshold = clamped, "Latency threshold updated");
    }

    Json(SettingsResponse {
        noise_gate: f32::from_bits(state.stream.noise_gate.load(Ordering::Relaxed)),
        gain: f32::from_bits(state.stream.gain.load(Ordering::Relaxed)),
        latency_threshold: state.stream.latency_threshold.load(Ordering::Relaxed),
    })
    .into_response()
}

/// Why a client's audio went silent. Sent by the web UI so the terminal shows a
/// cause instead of unexplained silence.
#[derive(Deserialize)]
pub(super) struct ClientStateRequest {
    token: String,
    reason: String,
}

/// POST /api/client-state — the client reports that it actually lost the microphone.
///
/// Only *real* microphone events reach this endpoint: the OS ended or muted the
/// capture track. The page merely being **backgrounded is deliberately not reported**
/// — it does not reliably mean capture stopped (audio often keeps flowing), so it
/// must never raise an alarm on its own.
///
/// This matters because a lost microphone is otherwise indistinguishable from
/// silence on the wire: the client-side noise gate legitimately sends nothing during
/// silence, so the operator would just see unexplained quiet. Sent with `sendBeacon`
/// if the page is already hidden, otherwise a normal fetch. Purely informational —
/// this only logs.
pub(super) async fn handle_client_state(
    State(state): State<AppState>,
    Json(body): Json<ClientStateRequest>,
) -> StatusCode {
    let authorized = {
        let guard = state.stream.session_token.lock();
        matches!(guard.as_ref(), Some(expected)
            if super::constant_time_eq(expected.as_bytes(), body.token.as_bytes()))
    };
    if !authorized {
        return StatusCode::UNAUTHORIZED;
    }

    match body.reason.as_str() {
        "mic_lost" => warn!(
            "Client lost microphone access (taken by another app) — recovering; no audio until it succeeds"
        ),
        "mic_interrupted" => warn!(
            "Client microphone interrupted (call or another app) — recovering; no audio until it succeeds"
        ),
        // Progress notes while an announced interruption is being worked on, so a slow
        // recovery (typically after a phone call) narrates itself instead of leaving the
        // operator staring at silence. Only sent once the warning above has gone out — a
        // recovery that lands faster than that stays a single quiet line.
        "recovery_resume" => info!("Client recovery: resuming the audio context"),
        "recovery_reacquire" => info!("Client recovery: re-acquiring the microphone"),
        "recovery_rebuild" => info!("Client recovery: rebuilding the audio graph"),
        // Closes the episode in the log, so the operator sees the interruption *end*
        // rather than the terminal just going quiet again. Worded to stand on its own,
        // because a fast recovery beats the warning and arrives without one.
        "audio_resumed" => info!("Client microphone recovered after an interruption"),
        // Never echo an unrecognised, client-supplied string into the log.
        _ => warn!("Client reported an unspecified audio interruption"),
    }

    StatusCode::NO_CONTENT
}

/// GET /ca — Download the CA certificate in DER format.
pub(super) async fn handle_ca_download(State(state): State<AppState>) -> impl IntoResponse {
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-x509-ca-cert"),
    );
    headers.insert(
        http::header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=\"quicmic-ca.cer\""),
    );

    (headers, state.tls_identity.cert_der.clone())
}
