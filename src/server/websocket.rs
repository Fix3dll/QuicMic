//! WebSocket fallback transport (TCP). Rides on the HTTP server via the
//! `/ws` upgrade route; carries the same PCM packets as WebTransport.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use tracing::{info, warn};

use super::state::{acquire_connection_slot, AppState, ConnectionGuard};
use crate::audio::{self, MAX_SAMPLE_RATE};

#[derive(Deserialize)]
pub(super) struct WsQuery {
    token: Option<String>,
    // Kept as a string and parsed leniently in the handler, so a malformed `sr`
    // never fails the upgrade — mirroring the WebTransport path's tolerant parsing.
    sr: Option<String>,
}

/// GET /ws?token=...&sr=... — WebSocket upgrade for fallback transport.
pub(super) async fn handle_ws_upgrade(
    ws: WebSocketUpgrade,
    Query(query): Query<WsQuery>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<AppState>,
) -> Response {
    // Parsed leniently: a non-numeric or out-of-range `sr` is ignored rather than
    // rejecting the whole upgrade (mirrors webtransport::parse_sample_rate).
    let sample_rate = query
        .sr
        .as_deref()
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|&sr| sr > 0 && sr <= MAX_SAMPLE_RATE);

    // Validate token (a missing one is rejected exactly like a wrong one).
    let authorized = {
        let guard = state.stream.session_token.lock();
        matches!(
            (guard.as_ref(), query.token.as_deref()),
            (Some(expected), Some(provided))
                if super::constant_time_eq(expected.as_bytes(), provided.as_bytes())
        )
    };
    if !authorized {
        // Same shape as the WebTransport rejection: the server-observed address
        // and the validated sample rate (absent if missing/invalid), never the
        // attempted token.
        warn!(
            authority = %addr,
            sample_rate,
            "WebSocket rejected: invalid or missing token"
        );
        return (StatusCode::UNAUTHORIZED, "Invalid or missing token").into_response();
    }

    // Only an authenticated client may change the shared source sample rate.
    if let Some(sr) = sample_rate {
        state.stream.source_sample_rate.store(sr, Ordering::Relaxed);
        info!(sample_rate = sr, "Client sample rate updated (WebSocket)");
    }

    // Cancel any existing stream to initiate instant handover, then subscribe to
    // the cancellation channel *before* acquiring the slot and upgrading. Doing it
    // here (rather than inside the connection task, which only starts once the
    // upgrade completes) mirrors the WebTransport path: a newer connection that
    // races in during slot acquisition or the upgrade handshake still cancels us,
    // because its cancel lands after this subscribe instead of in a blind window.
    let _ = state.stream.cancel_tx.send(());
    let cancel_rx = state.stream.cancel_tx.subscribe();

    // Enforce the single-connection limit (CAS with brief retries for handover).
    if !acquire_connection_slot(&state.stream.is_connected).await {
        return (StatusCode::CONFLICT, "Another client is already connected").into_response();
    }

    // Build the RAII guard now, before the upgrade. The `on_upgrade` callback
    // future can be dropped without ever running (e.g. the upgrade never
    // completes); moving the guard into it still releases the connection slot on
    // drop. Otherwise the slot would leak and lock out every future client until
    // the server restarts.
    let guard = ConnectionGuard::new(state.stream.is_connected.clone());
    ws.on_upgrade(move |socket| handle_ws_connection(socket, state, guard, cancel_rx))
}

/// Process an authenticated WebSocket connection.
///
/// `guard` is the RAII connection-slot guard acquired in `handle_ws_upgrade`.
/// Holding it for the session keeps the single-connection slot claimed and
/// releases it on any exit (completion, handover, or panic). `cancel_rx` is the
/// handover/shutdown subscription, also created in `handle_ws_upgrade` so no
/// cancellation can slip through between slot acquisition and the upgrade.
async fn handle_ws_connection(
    mut socket: WebSocket,
    state: AppState,
    guard: ConnectionGuard,
    mut cancel_rx: tokio::sync::broadcast::Receiver<()>,
) {
    // is_connected is already true (set in handle_ws_upgrade).
    info!("WebSocket client connected (fallback mode)");

    let _guard = guard;

    // Stats are per session: reset so the reported counters reflect this stream.
    // The WebSocket transport is reliable TCP, so it never reports loss.
    state.stream.packets_received.store(0, Ordering::Relaxed);
    state.stream.packets_lost.store(0, Ordering::Relaxed);

    loop {
        tokio::select! {
            cancel = cancel_rx.recv() => {
                // A newer connection took over (F5 handover) or the server is
                // shutting down. Either way, end this session — dropping the
                // socket closes it; clients detect a shutdown via the 503 API. A
                // `Lagged` error means several cancels coalesced — still a definite
                // cancel, so we end the session either way.
                if let Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) = cancel {
                    warn!(skipped = n, "Cancel channel lagged; ending session");
                }
                info!("WebSocket session ended (handover or shutdown)");
                break;
            }
            msg = socket.recv() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    _ => break,
                };

                match msg {
                    Message::Binary(data) => {
                        // Header is 4 bytes (seq) + at least one i16 sample. The
                        // socket is authenticated, so frames only come from the
                        // paired client; `decode_into_ring` caps the sample count
                        // defensively regardless.
                        if data.len() < 6 {
                            continue;
                        }
                        state.stream.packets_received.fetch_add(1, Ordering::Relaxed);
                        audio::decode_into_ring(&data[4..], &state.stream.ring);
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }
    }

    info!("WebSocket client disconnected");
}
