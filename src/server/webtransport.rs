//! WebTransport (QUIC/UDP) — the primary low-latency transport. Runs its own
//! QUIC endpoint (separate from the HTTP server) and feeds the shared state.

use std::sync::atomic::Ordering;
use std::time::Duration;

use tracing::{info, warn};
use wtransport::{Endpoint, Identity, ServerConfig};

use super::state::{acquire_connection_slot, ConnectionGuard, StreamState};
use crate::audio::{self, MAX_SAMPLES_PER_PACKET};

/// Expected datagram header size: 4 bytes sequence number.
const HEADER_SIZE: usize = 4;

/// Maximum expected datagram size: header + PCM data.
const MAX_DATAGRAM_SIZE: usize = HEADER_SIZE + MAX_SAMPLES_PER_PACKET * 2;

/// Extract the token (the first path segment, before any query string) from a
/// WebTransport session path such as `/<token>?sr=44100`.
fn token_from_path(path: &str) -> &str {
    path.split('?')
        .next()
        .unwrap_or(path)
        .trim_start_matches('/')
}

/// Parse an `sr=<rate>` query parameter from the session path. Returns
/// `Some(rate)` only when a well-formed rate within the accepted range is present.
fn parse_sample_rate(path: &str) -> Option<u32> {
    let query = path.split('?').nth(1)?;
    for param in query.split('&') {
        if let Some(sr_str) = param.strip_prefix("sr=") {
            if let Ok(sr) = sr_str.parse::<u32>() {
                if sr > 0 && sr <= audio::MAX_SAMPLE_RATE {
                    return Some(sr);
                }
            }
        }
    }
    None
}

/// Width of the reorder-tolerance window, in packets. A sequence number is only
/// declared lost once `max_seq` has advanced more than this far past it without
/// it arriving, so Wi-Fi reordering never produces a false loss. 64 fits a u64
/// received-bitmap.
const REORDER_WINDOW: u32 = 64;

/// Forward jumps larger than this re-baseline the tracker (a very long stall or a
/// misbehaving client) instead of fabricating a huge loss count or freezing
/// detection.
const MAX_FORWARD_GAP: u32 = 1024;

/// Reorder-tolerant packet-loss tracker for the unreliable datagram stream.
///
/// Received sequence numbers are tracked in a 64-packet sliding window. A seq is
/// only counted as lost once it is evicted from the window unseen, so a reordered
/// datagram (a higher seq arriving before a lower one) is absorbed instead of
/// being miscounted. u32 wrap-around is handled via wrapping arithmetic.
struct LossTracker {
    seen_any: bool,
    max_seq: u32,
    /// Bit `i` set means sequence number `max_seq - i` has been received. On the
    /// first packet the window is filled with ones so the (non-existent) packets
    /// before the first seq are never counted as lost.
    window: u64,
    /// Total confirmed losses this session.
    lost: u64,
}

impl LossTracker {
    fn new() -> Self {
        Self {
            seen_any: false,
            max_seq: 0,
            window: 0,
            lost: 0,
        }
    }

    /// Observe a received sequence number and return how many packets were newly
    /// *confirmed* lost (0 for the first packet, an in-order packet, a reorder, or
    /// a duplicate).
    fn observe(&mut self, seq: u32) -> u64 {
        if !self.seen_any {
            self.seen_any = true;
            self.max_seq = seq;
            self.window = !0; // warmup: treat pre-history as received
            return 0;
        }

        let forward = seq.wrapping_sub(self.max_seq);
        if forward == 0 {
            return 0; // duplicate of the newest seq
        }

        if forward < 0x8000_0000 {
            // `seq` is ahead of `max_seq` by `forward`.
            if forward > MAX_FORWARD_GAP {
                self.max_seq = seq;
                self.window = !0;
                return 0;
            }
            let newly_lost = if forward >= REORDER_WINDOW {
                // The whole window is evicted; count what it never received plus
                // the seqs beyond it that were never tracked.
                let evicted = u64::from(REORDER_WINDOW) - u64::from(self.window.count_ones());
                let beyond = u64::from(forward) - u64::from(REORDER_WINDOW);
                self.window = 1;
                evicted + beyond
            } else {
                // The top `forward` bits leave the window; unseen ones are lost.
                let leaving = self.window >> (REORDER_WINDOW - forward);
                let leaving_lost = u64::from(forward) - u64::from(leaving.count_ones());
                self.window = (self.window << forward) | 1;
                leaving_lost
            };
            self.max_seq = seq;
            self.lost += newly_lost;
            newly_lost
        } else {
            // `seq` is behind `max_seq`: a reordered or duplicate arrival.
            let back = self.max_seq.wrapping_sub(seq);
            if back < REORDER_WINDOW {
                self.window |= 1u64 << back;
            }
            0
        }
    }
}

/// Start the WebTransport server that receives audio datagrams.
///
/// Binds to the given address on UDP and listens for incoming sessions. Each
/// accepted session's datagrams are decoded as PCM i16 and pushed into the shared
/// ring buffer. Input DSP (noise gate and gain) runs client-side, so the server
/// applies none on this path.
pub async fn run_webtransport_server(
    identity: Identity,
    port: u16,
    stream: StreamState,
) -> anyhow::Result<()> {
    let config = ServerConfig::builder()
        .with_bind_default(port)
        .with_identity(identity)
        .keep_alive_interval(Some(Duration::from_secs(3)))
        .max_idle_timeout(Some(Duration::from_secs(10)))
        .expect("valid idle timeout")
        .build();

    let endpoint = Endpoint::server(config)?;

    info!(port = port, "WebTransport server listening (UDP/QUIC)");

    loop {
        let incoming = endpoint.accept().await;

        // The WT accept loop bypasses the HTTP 503 middleware, so refuse new
        // sessions here once a shutdown is underway.
        if stream.is_shutdown.load(Ordering::SeqCst) {
            continue;
        }

        let stream = stream.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_session(incoming, stream).await {
                warn!(error = %e, "WebTransport session ended");
            }
        });
    }
}

/// Handle a single WebTransport session.
///
/// Validates the session path for the pairing token, then enters a tight
/// loop reading unreliable datagrams containing PCM audio.
async fn handle_session(
    incoming: wtransport::endpoint::IncomingSession,
    stream: StreamState,
) -> anyhow::Result<()> {
    let session_request = incoming.await?;

    // Validate the session path contains the correct token
    let path = session_request.path().to_string();

    let authorized = {
        let token_guard = stream.session_token.lock();
        match token_guard.as_ref() {
            // Exact-match the path's token segment (before the query).
            Some(expected) => {
                super::constant_time_eq(token_from_path(&path).as_bytes(), expected.as_bytes())
            }
            None => false,
        }
    };
    if !authorized {
        // Never log the path itself: its first segment is the attempted token, and
        // client-supplied strings are not echoed into the log. The server-observed
        // address says *who* tried, and the validated sample rate (a number, absent
        // if missing/invalid) hints *what* tried: a real (e.g. stale) QuicMic tab
        // always sends `sr`, a scanner typically does not.
        warn!(
            authority = %session_request.remote_address(),
            sample_rate = parse_sample_rate(&path),
            "Rejected WebTransport session: invalid or missing token"
        );
        // Reply 403 and finish the session, tearing the connection down immediately
        // instead of letting an unauthenticated session linger until the QUIC idle
        // timeout.
        session_request.forbidden().await;
        return Ok(());
    }

    // Token is valid: apply the optional sample rate (e.g. /TOKEN?sr=44100).
    // Done only after authentication so an unauthorized session can never mutate
    // the shared source sample rate.
    if let Some(sr) = parse_sample_rate(&path) {
        stream.source_sample_rate.store(sr, Ordering::Relaxed);
        info!(
            sample_rate = sr,
            "Client sample rate updated (WebTransport)"
        );
    }

    // Cancel any existing stream to initiate instant handover, then subscribe to
    // the cancellation channel *before* accepting. Subscribing after our own send
    // means we never receive our own cancel, while a newer connection that arrives
    // during the slot-acquire retries or the accept() handshake still cancels us
    // (its cancel lands after this subscribe, so it is not missed).
    let _ = stream.cancel_tx.send(());
    let mut cancel_rx = stream.cancel_tx.subscribe();

    // Enforce the single-connection limit (CAS with brief retries for handover).
    if !acquire_connection_slot(&stream.is_connected).await {
        warn!("Rejected WebTransport session: another client is already connected");
        // Reply 429 and tear the session down promptly.
        session_request.too_many_requests().await;
        return Ok(());
    }

    // Claim the RAII slot guard immediately, so the connection slot is released on
    // every exit path from here on — an accept() error or a panic included —
    // rather than relying on a manual reset in each early-return branch.
    let _guard = ConnectionGuard::new(stream.is_connected.clone());

    let connection = session_request.accept().await?;

    info!(
        authority = %connection.remote_address(),
        "WebTransport client connected"
    );

    // Stats are per session: reset them when this fresh connection starts so the
    // reported loss and counters reflect the current stream, not the process
    // lifetime.
    stream.packets_received.store(0, Ordering::Relaxed);
    stream.packets_lost.store(0, Ordering::Relaxed);
    let mut loss = LossTracker::new();
    let mut total_packets: u64 = 0;
    let mut pending_loss: u64 = 0;
    let mut last_loss_warn = std::time::Instant::now();

    loop {
        tokio::select! {
            cancel = cancel_rx.recv() => {
                // A newer connection took over (F5 handover) or the server is
                // shutting down. Dropping the connection closes the QUIC session;
                // clients detect a shutdown via the 503 API, not this close. A
                // `Lagged` error means several cancels coalesced — still a definite
                // cancel, so we end the session either way.
                if let Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) = cancel {
                    warn!(skipped = n, "Cancel channel lagged; ending session");
                }
                info!("WebTransport session ended (handover or shutdown)");
                break;
            }
            res = connection.receive_datagram() => {
                match res {
                    Ok(datagram) => {
                        // Use datagram directly as &[u8] (avoid .to_vec() allocation)
                        let data: &[u8] = &datagram;

                        if data.len() < HEADER_SIZE + 2 || data.len() > MAX_DATAGRAM_SIZE {
                            continue;
                        }

                        // Parse header: u32 LE sequence number
                        let seq = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);

                        // Reorder-tolerant loss tracking: a seq is only counted as
                        // lost once it leaves the reorder window unseen, so Wi-Fi
                        // reordering never produces a false positive.
                        let newly_lost = loss.observe(seq);
                        if newly_lost > 0 {
                            pending_loss += newly_lost;
                            stream.packets_lost.store(loss.lost, Ordering::Relaxed);
                        }
                        total_packets += 1;
                        stream.packets_received.fetch_add(1, Ordering::Relaxed);

                        // Real-time but rate-limited warning (at most once per
                        // second), so the user can react to deteriorating Wi-Fi
                        // without log spam or false positives.
                        if pending_loss > 0 {
                            let now = std::time::Instant::now();
                            if now.duration_since(last_loss_warn).as_secs() >= 1 {
                                warn!(
                                    lost = pending_loss,
                                    total = loss.lost,
                                    "Packet loss detected"
                                );
                                pending_loss = 0;
                                last_loss_warn = now;
                            }
                        }

                        audio::decode_into_ring(&data[HEADER_SIZE..], &stream.ring);
                    }
                    Err(e) => {
                        info!(error = %e, "Datagram stream ended");
                        break;
                    }
                }
            }
        }
    }

    info!(
        packets = total_packets,
        lost = loss.lost,
        "WebTransport datagram stream finished"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{parse_sample_rate, token_from_path, LossTracker, REORDER_WINDOW};

    #[test]
    fn token_from_path_strips_slash_and_query() {
        assert_eq!(token_from_path("/abc123"), "abc123");
        assert_eq!(token_from_path("/abc123?sr=48000"), "abc123");
        assert_eq!(token_from_path("abc123"), "abc123");
        assert_eq!(token_from_path("/"), "");
    }

    #[test]
    fn parse_sample_rate_accepts_valid_and_rejects_invalid() {
        assert_eq!(parse_sample_rate("/tok?sr=44100"), Some(44100));
        assert_eq!(parse_sample_rate("/tok?foo=1&sr=48000"), Some(48000));
        assert_eq!(parse_sample_rate("/tok"), None);
        assert_eq!(parse_sample_rate("/tok?sr=0"), None);
        // Above MAX_SAMPLE_RATE (192_000) is rejected.
        assert_eq!(parse_sample_rate("/tok?sr=999999"), None);
        assert_eq!(parse_sample_rate("/tok?sr=abc"), None);
    }

    #[test]
    fn loss_in_order_has_no_loss() {
        let mut t = LossTracker::new();
        for s in 0..200 {
            assert_eq!(t.observe(s), 0);
        }
        assert_eq!(t.lost, 0);
    }

    #[test]
    fn loss_reordering_is_not_counted() {
        let mut t = LossTracker::new();
        // 10, 12, 11, 13 — seq 11 is merely reordered, not lost.
        assert_eq!(t.observe(10), 0);
        assert_eq!(t.observe(12), 0);
        assert_eq!(t.observe(11), 0);
        assert_eq!(t.observe(13), 0);
        assert_eq!(t.lost, 0, "reordering must not count as loss");
    }

    #[test]
    fn loss_is_confirmed_after_window() {
        let mut t = LossTracker::new();
        t.observe(0);
        // Skip seq 1, then advance past the reorder window so it is evicted unseen.
        let mut confirmed = 0;
        for s in 2..=(2 + REORDER_WINDOW) {
            confirmed += t.observe(s);
        }
        assert_eq!(t.lost, 1, "seq 1 must be confirmed lost once evicted");
        assert_eq!(confirmed, 1);
    }

    #[test]
    fn loss_ignores_duplicates() {
        let mut t = LossTracker::new();
        t.observe(5);
        assert_eq!(t.observe(5), 0); // duplicate of newest
        t.observe(6);
        assert_eq!(t.observe(6), 0); // duplicate within window
        assert_eq!(t.lost, 0);
    }

    #[test]
    fn loss_handles_sequence_wraparound() {
        let mut t = LossTracker::new();
        t.observe(u32::MAX - 1);
        assert_eq!(t.observe(u32::MAX), 0);
        assert_eq!(t.observe(0), 0); // wraps forward by 1, not a loss
        assert_eq!(t.observe(1), 0);
        assert_eq!(t.lost, 0);
    }
}
