# Developer & Agent Guide: QuicMic

This document explains the architecture, design choices, key implementation details, and critical guardrails of the **QuicMic** project. 

---

## 🎙️ Project Overview

**QuicMic** turns a mobile or desktop device's web browser (iOS Safari, Android Chrome, desktop browsers, etc.) into a low-latency wireless PC microphone over a Local Area Network (LAN). 

It consists of:
1. **Frontend (Client):** An HTML5/JS web page that captures mic input via `getUserMedia`, processes it in an `AudioWorklet`, and streams it.
2. **Backend (Server):** A Rust server (using Axum and WTransport) that hosts the web assets, validates client pairing, receives raw audio packets, and plays them into a virtual audio device (such as VB-Cable).

---

## 🛠️ System Architecture & Audio Pipeline

```text
+-------------------------------------------------------------------------+
|                         CLIENT (Mobile / Desktop Browser)               |
|                                                                         |
|    [ Microphone ] ---> ( AudioWorklet ) ---> [ Network Writer ]         |
|     (mono, ~48kHz)       (worklet.js)             (app.js)              |
+-------------------------------------+-----------------------------------+
                                      |
                                      v
+-------------------------------------+-----------------------------------+
|                            LOCAL NETWORK (LAN)                          |
|                                                                         |
|        /--- [ WebTransport ] (UDP/QUIC - Primary Low-Latency) ---\      |
|       /                                                           \     |
|      /---- [ WebSocket ]   (TCP - Secondary Fallback) -------------\    |
+-----+---------------------------------------------------------------+---+
      |                                                               |
      v                                                               v
+-----+---------------------------------------------------------------+---+
|                    SERVER (Windows / macOS / Linux)                     |
|                                                                         |
|    [ wtransport Connection ]              [ axum WebSocket Connection ] |
|               \                                      /                  |
|                v                                    v                   |
|              +----------------------------------------+                 |
|              |   Decode i16 PCM into Ring Buffer      |                 |
|              +--------------------+-------------------+                 |
|                                   |                                     |
|                                   v                                     |
|              +----------------------------------------+                 |
|              |    Ring Buffer (audio/ring_buffer)     |                 |
|              +--------------------+-------------------+                 |
|                                   |                                     |
|                                   v                                     |
|              +----------------------------------------+                 |
|              |           CPAL Output Thread           |                 |
|              +--------------------+-------------------+                 |
|                                   |                                     |
|                                   v                                     |
|              +----------------------------------------+                 |
|              | Catmull-Rom Cubic Resampler (Rate)     |                 |
|              +--------------------+-------------------+                 |
|                                   |                                     |
|                                   v                                     |
|              +----------------------------------------+                 |
|              |    Channel Duplication (Mono -> Multi) |                 |
|              +--------------------+-------------------+                 |
|                                   |                                     |
|                                   v                                     |
|                      [ Virtual Device / Speakers ]                      |
+-------------------------------------------------------------------------+
```

### 1. The Audio Path
- **Client Side:** Capture is mono at the browser's native sample rate (typically 48kHz; the actual rate is reported to the server via the `sr` parameter so the output resampler can match it). The `AudioWorklet` (`web/worklet.js`) buffers samples cleanly and sends them to the main thread.
- **Network Packet Format:** Each packet has a 4-byte little-endian sequence number followed by raw `i16` PCM samples (up to 480 samples = 10ms at 48kHz).
- **Server-Side Audio Handling (`audio::decode_into_ring`):** Both transports just parse the little-endian i16 samples and push them into the ring — a pure passthrough on the hot path, with **no per-sample DSP**. The **noise gate** (250ms hold) and **gain** now run client-side in the AudioWorklet (`web/worklet.js`): the gate decides on the raw signal and gain is applied in the float domain before Int16 conversion (no double quantization). So the server never even receives gated-out silence (the phone's radio idles instead). The `noise_gate` / `gain` settings are still stored server-side, only for persistence and cross-device sync (and `latency_threshold`, which is genuinely server-side, still drives the output stage).
- **Ring Buffer (`src/audio/ring_buffer.rs`):** A lock-free, SPSC (Single Producer Single Consumer) ring buffer using `UnsafeCell<i16>` for interior mutability and batch `ptr::copy_nonoverlapping` for efficient memcpy. Uses `Acquire`/`Release` atomic ordering on head/tail pointers for cross-core memory visibility (safe on ARM and x86).
- **Resampling & Channel Mapping:** The `cpal` audio output thread reads from the ring buffer. The output thread:
  - Performs **Catmull-Rom cubic** fractional resampling on the fly to match the target device's native rate (upgraded from linear interpolation, which itself replaced nearest-neighbor/zero-order-hold). The cubic kernel gives a flatter passband and better image/alias rejection on non-48k devices for a few extra multiplies per output sample; it carries a 2-sample group delay from its one-sample look-ahead window.
  - Supports **dynamic source sample rate** via `Arc<AtomicU32>`: the client reports its actual capture rate, and the resampler adjusts the ratio in real-time.
  - Performs **Latency Recovery (Hard Skip)**: If the buffer size exceeds the configurable threshold (converted to samples based on the active client sample rate), the consumer discards the oldest samples down to the prebuffer low-water mark (`PREBUFFER_MS`, ~30ms) to reset the lag instantly. Both the prebuffer mark and this trim target are derived from `PREBUFFER_MS` and the active sample rate, so they stay ~30ms in real time at any source rate (44.1k–192k). A 3-second cooldown prevents stuttering.
  - Duplicates the mono audio channel to all available channels of the virtual audio device.
  - Drains source samples from the ring in small batches (`RESAMPLE_CHUNK`, 64) carried in `ResamplerState`, amortizing the per-sample atomic synchronization. This is a measurable win on weak-memory-model CPUs (ARM / Apple Silicon); on x86 the Acquire/Release loads are already plain `mov`s, so it is behaviour-neutral there. Leftover batch samples persist across callbacks, so none are dropped.
- **Output Supervisor (`spawn_output_supervisor`):** The cpal stream is `!Send`, so a dedicated thread owns its whole lifecycle. If the device fails mid-stream (e.g. the virtual cable is disabled — on Windows this surfaces as WASAPI `AUDCLNT_E_DEVICE_INVALIDATED`), the stream's error callback signals the supervisor over an `mpsc` channel; the supervisor drops the dead stream and rebuilds it, retrying once per second until the device is back, so playback resumes **with no restart**. While the device is down, `StreamState.device_ok` is cleared and reported via `/api/stats` (`audio_device_ok`) so the web UI can warn. The initial build result is reported back to `main`, so a bad `--device` name stays a fatal startup error exactly as before.

---

## 🔐 TLS & Trust Bypassing on LAN

WebTransport requires a secure HTTPS/QUIC connection. Normally, local IP addresses (like `192.168.1.X`) cannot easily get signed SSL certificates from public authorities (like Let's Encrypt).

To solve this:
1. The server generates a self-signed ECDSA P-256 certificate on-the-fly (`src/tls.rs`) using `wtransport`'s internal builder in memory.
2. The server computes the **SHA-256 hash** of the DER-encoded certificate using `ring`.
3. The client fetches the hash via `/api/info` and configures its `WebTransport` connection with the `serverCertificateHashes` option:
   ```javascript
   new WebTransport(url, {
       serverCertificateHashes: [{ algorithm: 'sha-256', value: hashBytes }],
   });
   ```
4. This allows modern web browsers to establish a secure UDP connection to the local IP without throwing certificate trust errors.
5. Certificate lifetime is 14 days (WebTransport spec maximum for `serverCertificateHashes`).
6. All operations are performed 100% in-memory with zero disk-writes by default. Use `--dump-certs` to export PEM and DER files to `certs/` for debugging.

---

## 🔄 Concurrency & F5 Refresh Handover (Critical Guardrail)

To prevent resource exhaustion and conflicting streams, the server enforces a **single-connection limit** via `AtomicBool` CAS (Compare-And-Swap). Only one active stream is permitted.

### The Handover Challenge
When a user refreshes the browser page (F5):
1. The browser initiates page unloading, and audio datagrams stop.
2. The server's socket loop (`receive_datagram` or `socket.recv()`) is blocked, waiting for network data or a timeout (which can take seconds to detect).
3. The client immediately tries to establish a *new* connection.
4. If the server does not immediately clear the old session, the new connection is rejected with `"another client is already connected"`.

### The Solution Implementation
We resolved this using a token renewal workflow, CAS-based connection guard, and an event-driven broadcast cancellation channel:

- **Token Renewal (`/api/renew`):** Before initiating a new stream, the client hits `/api/renew`. The server generates a new token, stores it in `session_token`, broadcasts a cancellation signal over `cancel_tx`, and sleeps for 50ms.
- **Broadcast Cancellation (`tokio::select!`):** The session loops listen to `cancel_rx.recv()`. When a cancellation is broadcasted, the old connection instantly breaks its loop and terminates. This eliminates high-frequency timer-wheel allocations on the hot path (which occurred when using a 50ms polling loop) and operates at **0% CPU** during active streaming.
  ```rust
  tokio::select! {
      _ = cancel_rx.recv() => {
          // Terminate session immediately!
      }
      res = receive_datagram() => { ... }
  }
  ```
- **Graceful Shutdown (Ctrl+C):** Deliberately minimal. Clients detect a shutdown by polling the HTTP API — *not* by listening for a transport close frame, because that event is unreliable and arrives seconds late on iOS Safari (verified on-device; see Disconnect Detection below). So the server does not bother sending `1001` close frames or awaiting any close handshake. The main thread simply:
  1. Sets `is_shutdown` to `true`. A small Axum middleware (`reject_during_shutdown`) then replies `503 Service Unavailable` to every new HTTP request — the actual, deterministic "server is leaving" signal.
  2. Broadcasts over `cancel_tx`, so the active session loop breaks and drops its connection (which closes the QUIC/TCP socket on its own — no explicit close call needed).
  3. If a client was connected, keeps the API up for `SHUTDOWN_GRACE_PERIOD` (1200ms) so that client's ~1s liveness poll reliably lands on a `503` before the process exits.
  4. Drains the HTTP server via `axum_handle.graceful_shutdown` and exits.

  An earlier version sent `1001` close frames over both transports and waited on a `shutdown_notify` handshake; that machinery was removed once on-device testing showed iOS ignores the close anyway and detection happens entirely over HTTP.
- **CAS Connection Guard:** New connections use `compare_exchange(false, true, SeqCst, SeqCst)` with retries (10 attempts × 20ms) to atomically acquire the connection slot, eliminating TOCTOU races.
- **RAII ConnectionGuard:** A `Drop`-implementing struct resets `is_connected` to `false` when the active connection task terminates (on completion, cancellation, or panic), freeing the single-connection slot.

---

## 🔋 Client UI & CPU Optimization (Eco Mode)

Mobile screens (especially during persistent real-time streaming) can heat up or experience battery drain during persistent WebGL/DOM updates.
- **Eco Mode:** A solid black fullscreen overlay shuts off OLED pixels to prevent screen burn-in and save power.
- **Screen Wake Lock:** On entering Eco Mode the client acquires a `navigator.wakeLock('screen')` (and re-acquires it on `visibilitychange`, since the lock is auto-released when the page hides). This keeps JavaScript and the audio/transport sockets alive behind the black overlay — the screen stays on but its OLED pixels are off. Without it, iOS could auto-lock, suspend JS, and never deliver the shutdown signal.
- **Throttled VU Updates:** DOM updates for the VU level and VU bar style are throttled to execute at most once every 100ms (instead of every frame) in `web/app.js`.
- **Suspended Stats UI + Low-Frequency Liveness Check:** When Eco Mode is active, the full per-second stats UI (`updateStats()` body) is suspended — no JSON parsing, DOM updates, or layout while the screen is black. A lightweight liveness check still runs roughly every 3 seconds so a server shutdown is detected (and the user is returned to the pairing screen) without keeping the screen lit. This is the deliberate trade-off that fixed Eco Mode being unresponsive to shutdowns.

---

## 🎛️ Dynamic Audio Settings (Noise Gate, Gain & Latency Recovery)

Noise gate, gain, and latency recovery threshold can be adjusted at runtime via the web UI or CLI:

- **CLI Defaults:** `--noise-gate -50 --gain 1.0 --latency-threshold 150` sets initial values at startup. `--noise-gate` is specified in **dB** (-100 = Off, 0 = max), matching the web UI slider; `main::noise_gate_db_to_linear` converts it to the linear amplitude the audio thread and HTTP API use (so the stored/synced value stays linear).
- **Web UI:** The ⚙️ Settings panel provides:
  - A single decibel-based slider for the noise gate (range -100 dB [Off] to 0 dB [Muted], default -50 dB).
  - A slider for gain (range 0.2x–3.0x, default 1.0x).
  - A slider for Latency Recovery (range 0 ms [Off] to 500 ms, default 150 ms).
  - Individual reset buttons (🔄) next to each value to restore their factory defaults.
- **API Endpoints:**
  - `GET /api/settings` — returns current `{ noise_gate, gain, latency_threshold }` as JSON (open; read-only).
  - `POST /api/settings` — updates one or more values (all fields optional). **Requires the session token** in the body (constant-time compared) or returns `401`. Incoming values are clamped to the shared bounds in `server/mod.rs` (`NOISE_GATE_*`, `GAIN_*`, `LATENCY_THRESHOLD_MAX_MS`) — the same linear ranges the `--gain` / `--latency-threshold` CLI flags are clamped to at startup (the `--noise-gate` flag is given in dB and converted to a linear value inside these bounds).
- **Storage:** Float values (noise_gate, gain) are stored as `Arc<AtomicU32>` (using `f32::to_bits()` / `f32::from_bits()`). Latency threshold is stored as `Arc<AtomicU32>` directly representing milliseconds.
- **Persistence & Auto-Sync:** The client (`localStorage`) is the **source of truth**. On page load it restores its saved settings to the UI and pushes them to the server on successful pairing, token renewal, or reconnection — so they survive a server restart. `loadSettings` only adopts the server's values on a true first run (nothing saved yet); when `localStorage` already has settings it does **not** fetch-and-overwrite them, otherwise a freshly restarted server (CLI defaults) would clobber the user's choices.

---

## 📱 QR Code Pairing

At startup, the server prints a QR code to the terminal containing the URL with the pairing PIN in the hash fragment:
```
https://192.168.1.42:8443#123456
```
- The PIN is in the URL **hash** (not a query parameter), so it is never sent to the server in HTTP requests.
- The client reads `location.hash`, auto-fills the PIN input, and initiates pairing automatically.
- The `qr2term` crate renders the QR code directly in the terminal.
- **IPv6:** server URLs bracket IPv6 literals per RFC 3986 (`https://[fe80::1]:8443#…`) via `url_host` on the server and a matching check in the client's WebTransport URL builder; the WebSocket fallback already gets this right through `location.host`. WebTransport binds dual-stack (`[::]`), and the self-signed cert registers the IP literal as an IP SAN, so a global/ULA IPv6 client works. `--ip` accepts either the bare or the bracketed form (`fe80::1` or `[fe80::1]`) via `parse_ip_arg`. Link-local addresses (with a `%zone` suffix) are **out of scope** — they don't round-trip through `IpAddr` or browser URLs.
- **Proxy-TUN fake-ip filtering (`detect_lan_ip` in `main.rs`):** `local_ip_address::local_ip()` returns the first IPv4 unicast address among the adapters that own a *default route*, which is wrong when a proxy TUN in fake-ip mode (mihomo/Clash, sing-box, Surge, etc. — e.g. `device: sekai` holding `198.18.0.1`) owns a default route with a lower metric: the TUN's fake address wins even though nothing on the LAN can route to it. So the default-route pick is trusted only when it is a **usable LAN address** (`is_unusable_lan_addr` rejects `0.0.0.0/8`, loopback, link-local, multicast/reserved `240/4`, and the RFC 2544 benchmark range `198.18.0.0/15` — the de-facto fake-ip default for mihomo/Clash, sing-box and Surge); otherwise `list_afinet_netifas()` is scanned and the best-ranked candidate is picked (`pick_lan_ip` + `lan_addr_rank`: RFC 1918 (2) > CGNAT `100.64/10` and IPv6 (1) > public IPv4 (0), IPv4 over IPv6 on rank ties). Same-rank same-family ties (e.g. Wi-Fi `192.168.x` vs a Hyper-V/VPN `172.x`) break in favour of physical adapters — virtual ones (`vEthernet`/`wsl`/`docker`/`vmnet`/`tun` etc., see `is_virtual_adapter`) are deprioritized, and any remaining tie resolves by OS enumeration order (`max_by_key`'s last maximum); `--ip` is the manual override. The pure functions are unit-tested.

---

## 🔁 Disconnect Detection & Auto-Reconnect

Disconnect handling is **transport-agnostic and does not inspect the close code.** This is a deliberate response to how iOS Safari behaves:

- On iOS Safari (the primary mobile target, using WebTransport), `transport.closed` surfaces **seconds late** and carries no useful code — it resolved as `{clean: true, code: 0}` even back when the server still sent an explicit `1001`. Datagram `write()` errors are similarly delayed. So neither the transport close event nor its code is a reliable, timely signal there. (Earlier the client branched on `1000`/`1001`/`0` and string-matched error messages, and the server sent close frames; both were removed in favour of the HTTP signal below.)
- The reliable, fast signal is instead the **HTTP layer**: the client already polls `/api/stats` every second while streaming, and the server replies `503` while shutting down. A `503` (or an unreachable server) is treated as a definitive "server gone".

How it works now:
1. **Unified close handler:** Both `ws.onclose` and `transport.closed` (resolve *and* reject paths, any code) funnel into a single handler. It ignores events that are stale (from a transport already replaced) or self-initiated (user stopped, or a reconnect handover is in progress), then runs one fast liveness probe (`/api/info`).
2. **Probe decides intent:** probe fails / `503` → server is gone → return to the pairing screen. Probe succeeds → it was a transient drop → reconnect. A `401` from `/api/stats` is handled separately: the server is alive but our session was taken over, so we re-pair **in place** (no reload).
3. **Server-gone locks the pairing screen (reload required):** the self-signed certificate is **regenerated on every server start**, so a server that went away and came back has a *new* cert hash — the loaded page's pinned hash and already-accepted cert are stale, in-page re-pairing would silently fail on the cert mismatch, and the stale page cannot even probe for the server's return (the mismatch fails the fetch). So on a confirmed "server gone" the client **locks the PIN input and shows a Reload button** (`location.reload()`); a full reload is the only reliable way back. Each "server gone" verdict is confirmed first (a `503` is definitive; a network error or a single idle-check failure triggers one confirming `/api/info` probe), so a transient blip doesn't needlessly force a reload.
4. **Stats poll is the fast path on iOS:** a streaming `/api/stats` poll that returns `503` triggers an immediate return to pairing. This catches a shutdown within ~1s without waiting on the late transport event. In Eco Mode, the lighter ~3s liveness check plays the same role.
5. **Reconnect (transient drops):** the first attempt is immediate (0s), then exponential backoff (1s → 2s → 3s → 4s, max 5 attempts). Each attempt renews the session token via `/api/renew` first; the loop bails out early once a probe confirms the server is gone.
6. The AudioContext and Worklet stay alive during reconnection to minimize resume latency.

All of these paths are idempotent: the `isStreaming` / `isReconnecting` / `isConnecting` flags ensure a shutdown observed by several detectors at once still results in a single, clean transition.

---

## 🎤 Microphone Interruption & Recovery (Critical Guardrail)

The **OS owns the microphone, not us.** An incoming call or another app grabbing the mic kills capture, and the browser does not necessarily hand it back on its own.

> ⚠️ **Backgrounding the page does NOT, by itself, stop capture.** Verified on-device: streaming keeps working with the browser in the background — the main thread keeps running, events keep firing, packets keep flowing. **The only thing that actually kills capture is another app taking the microphone.** Do **not** "optimise" by treating a hidden page as a dead page: that assumption caused two separate shipped bugs — a mic lost in the background was never reported to the server at all, and audio stayed dead after a phone call because the AudioContext was only un-parked when the page became visible again. The rule is narrower than it looks: **never call `getUserMedia` while hidden** (that would fight whatever app took the mic), but *do* keep detecting, reporting, and resuming our own context there.

This previously surfaced as the worst kind of bug: the transport reconnected fine, the client toasted **"Reconnected!"**, and no audio ever reached the server — because the reconnect path only rebuilt the *transport*, and nothing ever checked whether the *microphone* was still alive.

- **Health = FLOW ∧ FLAGS. Never infer it from the flags alone.** This is the central lesson, learned the hard way through three separate shipped bugs. `isAudioHealthy()` requires **both**:
  - **flow** — `isAudioFlowing()`: did the worklet post within `AUDIO_STALL_MS` (4s)?
  - **flags** — `track.readyState === 'live'`, `!track.muted`, `audioContext.state === 'running'`.

  Each term catches exactly what the other misses. **Flow alone is insufficient:** an OS-muted track still pumps (silence). **Flags alone are insufficient:** after a long freeze iOS leaves a **zombie graph** — every flag reads healthy, the context says `'running'`, the track says `'live'`, and yet *nothing is ever pumped*. Trusting the flags is precisely what made the client toast **"Reconnected!"** over dead silence. If you add a new health signal, add it as a *term*, never as a replacement for flow.
- **The worklet heartbeats even while muted.** `MUTED_HEARTBEAT_EVERY` (~1/s) makes it post a bare `{ level: 0 }` while muted — it still skips all DSP, so the battery win is intact. Without this, muting posted **nothing**, which blinded the health check completely: a graph that died while muted stayed undetected until the user unmuted and found silence. **Never make the muted path fully silent again.**
- **One decision point: `checkAudioHealth()`**, on the 1s tick that **already exists** — no new timer, and it now runs while muted too. Events (`onended`, `onmute`, `onstatechange`) are **diagnostic**; they do not each act. Letting them act independently produced duplicate `WARN` spam on the server (several detectors firing for one interruption).
- **An OS-muted track means WAIT, not recover.** `track.muted` means a call or another app owns the mic *right now*, and the OS returns it via `onunmute` however long that takes. `checkAudioHealth` deliberately does nothing there — recovering would mean **fighting an active phone call for the microphone**. The one exception is `visibilitychange → visible`: the user returning to the page is an explicit signal they are done with the other app, so that path *does* reclaim.
- **Never watch packets.** A watchdog on packet flow would false-positive on every silence, because the noise gate deliberately sends nothing then. The worklet's message stream — not the packet stream — is the only safe liveness signal.
- **Recovery is an escalating ladder, and every rung is VERIFIED** by `waitForAudioFrame()` (the worklet actually posting again), never by the flags: `resume()` → re-acquire the mic → **full graph rebuild** (`teardownAudioGraph()` + `setupAudioGraph()`). The rebuild rung is what fixes the zombie case and is the one the earlier code was missing. One pass, foreground only.
- **One WARN per interruption episode** (`interruptionNotified`), cleared only once audio is flowing again.
- **Being backgrounded is NOT a microphone event.** `visibilitychange → hidden` only means the page lost focus — capture very often keeps running, and whether it stops varies by browser/OS. Treating "hidden" as "mic lost" produced false `WARN`s on the server while audio was flowing perfectly. Only the OS's actual track events (`onended`/`onmute`) may raise an alarm.
- **A dead capture graph is invisible on the wire.** The client-side noise gate legitimately sends nothing during silence, so **"no packets" is NOT evidence of a fault** — never build a watchdog on packet flow. `track.readyState`, `track.muted`, and `audioContext.state` are the only trustworthy signals.
- **`onmute` → do nothing.** The track survives and the OS usually hands it back itself (`onunmute`), so the incoming-call case self-heals with zero intervention. iOS can still leave the `AudioContext` parked in **`'interrupted'`** — a Safari-only state that is *not* `'suspended'`, which is why the old `state === 'suspended'` check missed it — so `onunmute` nudges `resume()`.
- **Never grab the mic in the background.** `recoverAudio()` returns immediately unless the page is visible, and makes exactly **one** attempt — never a loop. We only ask the OS for the microphone while the user is actually looking at the page (or on an explicit tap), so another app is never fought for it. Do not "fix" this by retrying harder.
- **Fail honestly, never hang.** If the single recovery attempt fails, `stopForAudioLoss()` performs a clean full stop (transport + audio torn down, server slot released) with a clear message — instead of leaving the user in a "connected but silent" limbo.
- **"Transport connected" ≠ "microphone live".** `reconnectLoop` verifies `isAudioLive()` (and tries one recovery) before it is allowed to toast "Reconnected!".
- **`POST /api/client-state`** tells the server *why* the audio went quiet, so the terminal shows a cause instead of unexplained silence. **Only real microphone events are reported** — `track.onended` → `mic_lost`, `track.onmute` → `mic_interrupted`. Sent with `navigator.sendBeacon` when the page is already hidden (the browser flushes it even as JS freezes), otherwise a normal `fetch` (fully reliable in the foreground). The server only logs it:
  ```
  WARN Client lost microphone access (taken by another app) — no audio until the user resumes
  WARN Client microphone interrupted (call or another app) — audio paused until the OS releases it
  ```

---

## 🔇 Mute

- **Activation:** Long-press (500ms) on the mic button toggles mute (with support for 50ms vibration haptic feedback). An accidental connection closure guard ensures that releasing the button after a long-press does not toggle the stream off.
- **Behavior:** When muted, the client stops sending audio packets (the AudioWorklet continues running to keep the AudioContext alive). The ring buffer naturally underruns, producing silence.
- **UI:** The status badge shows "Muted" in yellow, the VU meter resets to 0%, the mic button and ring turn yellow with a matching glow, and the helper text updates to "Long press to unmute".

---

## 📊 Statistics & Monitoring

- **Client-side stats:** Packets sent, uptime, transport type, network RTT (Ping), and server buffer depth are displayed in real-time.
  - **RTT (Ping):** Calculated on-the-fly by measuring the client-side request round-trip time of the `/api/stats` polling request (reusing HTTP persistent keep-alive connections).
  - **Buffer Depth:** The server reports `buffer_ms` directly in `/api/stats`, computed from `buffer_level` and the active source sample rate (`buffer_level * 1000 / source_sample_rate`), so it is accurate for non-48 kHz sources. (The client previously derived this as `buffer_level / 48`, which only held at 48 kHz.)
- **Server-side stats:** `GET /api/stats` **requires the session token** in the `X-Session-Token` header (only the already-paired client polls it, so this keeps connection/buffer telemetry off open access) and returns:
  ```json
  {
    "packets_received": 12345,
    "packets_lost": 2,
    "loss_percent": 0.016,
    "buffer_level": 480,
    "buffer_ms": 10,
    "buffer_capacity": 24000,
    "connected": true,
    "audio_device_ok": true
  }
  ```
  The shutdown `503` still takes precedence over this token gate — the `reject_during_shutdown` middleware runs **before** the handler, so a shutting-down server answers `503` regardless of the token and the liveness/shutdown detection is unaffected (a `401` instead means the session was taken over while the server is still alive). `audio_device_ok` is `false` while the output device is lost and the supervisor is rebuilding (see the Output Supervisor).
- **Packet loss detection:** The WebTransport handler tracks loss with a reorder-tolerant sliding window (`LossTracker`): received sequence numbers fill a 64-packet (`REORDER_WINDOW`) received-bitmap, and a seq is only counted as lost once it is **evicted from the window unseen**. So Wi-Fi reordering (a higher seq arriving before a lower one) is absorbed instead of miscounted — the earlier forward-gap approach emitted a false `warn!` every time a reordered packet arrived late, which this design eliminates. Duplicates are ignored, u32 sequence wrap-around is handled via wrapping arithmetic, and an anomalous forward jump (> `MAX_FORWARD_GAP`) re-baselines the tracker rather than fabricating a huge count. Confirmed losses surface live via `/api/stats` (`packets_lost`) and through a **rate-limited** `warn!("Packet loss detected")` — at most once per second, aggregating that interval's losses — so the user can react to deteriorating Wi-Fi in real time without log spam or false positives. A per-session summary is also logged on disconnect. `LossTracker` has its own `#[cfg(test)]` unit tests (reordering, confirmed-after-window, duplicates, wrap-around).
- **Per-session counters:** `packets_received` and `packets_lost` are reset when a new connection is acquired (both transports), so `/api/stats` reflects the **current** stream rather than the whole process lifetime. The WebSocket fallback runs over reliable TCP, so it only counts received packets (never loss).
- `buffer_capacity` is reported directly from the ring buffer (`ring.capacity()`) rather than a hard-coded constant, so it stays in sync if the buffer size changes.
- The client polls `/api/stats` every second during streaming, updating the loss percentage, RTT, and buffer depth with zero additional server overhead. This same poll doubles as the **primary shutdown detector** (see Disconnect Detection): a `503` response returns the user to pairing immediately, which is why it is the fast path on iOS Safari.

---

## 🛡️ Brute-Force Protection

The `/api/pair` endpoint includes **per-IP** rate limiting:
- After **5 consecutive failed PIN attempts from the same client IP**, the endpoint returns `429 Too Many Requests` and locks out **that IP** for **30 seconds**. Keying by IP means one misbehaving host cannot lock everyone else out (an earlier version used a single global counter that did).
- State lives in a single `Arc<parking_lot::Mutex<PairingThrottle>>`, where `PairingThrottle` holds a `HashMap<IpAddr, ThrottleEntry>`. Each IP's failure count and lockout deadline are read and updated together (the whole `handle_pair` bookkeeping runs in one synchronous critical section — no guard held across an `.await`). The attempted PIN is never logged.
- The client IP comes from `ConnectInfo<SocketAddr>` (the HTTPS server is served with `into_make_service_with_connect_info`). A completed pair request requires a real TCP+TLS handshake, so the key cannot be spoofed to flood the map.
- Idle entries are **pruned** on every check: an entry is dropped once it is neither actively locked out nor seen within the `FAILURE_DECAY` window (60s), so the map stays bounded and a legitimate user's stray mistype decays away. Successful pairing clears that IP's entry. `PairingThrottle` has `#[cfg(test)]` unit tests (per-IP isolation, lockout, idle pruning).

---

## 🔔 Update Check

A best-effort, opt-out check for a newer GitHub release (`src/update_check.rs`):
- Runs once at startup on a background `tokio::spawn`, **never blocks startup**, and is **silent on any failure** (offline, DNS/TLS error, unknown repo, malformed response). It only ever reports a *strictly newer* version (`parse_version` tuple comparison against `CARGO_PKG_VERSION`), so it can't produce a false positive.
- Opt out with `--no-update-check` or `QUICMIC_NO_UPDATE_CHECK`.
- **No HTTP-client crate.** It reuses the existing TLS stack (`rustls` + the process-default `ring` provider + `tokio-rustls`, all already in the tree) and the OS trust store (`rustls-native-certs`) to validate GitHub's public cert, and hits the `releases/latest` **redirect** — reading only the `Location` header (no response body or JSON to parse). The repo is `REPO` in the module. `tokio-rustls` is pinned `default-features = false` (+`ring`) so it never drags `aws-lc-rs` back in.
- The result is surfaced two ways: a one-line `info!` in the terminal, and `update_available` / `latest_version` / `releases_url` on `/api/info`. The web UI turns that into a small **dismissible top banner** linking to the releases page; the dismissal is remembered per version in `localStorage`. The browser never contacts GitHub (the server does), so the strict CSP (`connect-src 'self'`) is unchanged — an external `<a href>` navigation is not subject to CSP.

---

## 📂 Hybrid Static File Serving

The server implements a hybrid asset-serving strategy to allow easy customization without sacrificing standalone portability:
1. **Embedded Assets (`rust-embed`):** During compilation, all files in the `web/` directory are embedded directly into the binary.
2. **Local Disk Overrides:** The custom fallback handler checks if a requested file exists under a local `web/` directory, in precedence order: **(1) next to the executable** (the documented customization location), then **(2) the current working directory** (a fallback that also covers `cargo run`, where the binary lives under `target/`). Only the directory *paths* are cached (`override_dirs`, via `OnceLock` — the executable's location is fixed for the process); file existence and contents are re-checked on **every request**, so a `web/` directory or file added/edited while the server is running is served **immediately, with no restart**.
   - If found on disk, the local file is read and served (allowing users to customize the HTML/CSS/JS without recompiling the executable).
   - If not found on disk, the server falls back to serving the embedded file from binary memory.
   - **Path-traversal guard (`is_safe_asset_path`):** the request path is validated before any filesystem access — any path containing a `..`/parent or absolute component is rejected with `404`, so a raw client cannot escape `web/` (e.g. `GET /../Cargo.toml`). Browsers normalise `..` away, but a hand-crafted request would not, so this guard is required.
3. **MIME Guessing (`mime-guess`):** Correct `Content-Type` headers are dynamically resolved for both local and embedded files.
4. **Cache Validation (`ETag` + `Cache-Control: no-cache`):** Every asset is served with an `ETag` and `no-cache`, so the browser revalidates on each load (`If-None-Match` → `304 Not Modified` when unchanged) and picks up a rebuilt/edited asset — HTML, favicon, JS — **immediately** instead of serving a stale copy. The ETag source differs by origin: **disk** overrides use a **content-derived** hash recomputed per request, so even a same-length edit is reliably detected for live editing; **embedded** assets use rust-embed's **compile-time SHA-256**, so there is no per-request hashing and the body is never materialized on a `304`. Previously no cache/validation headers were sent at all, so browsers (especially iOS Safari, for the HTML and favicons) cached assets heuristically and never learned they had changed.
5. **Security & cache headers:** Every served document carries a strict **Content-Security-Policy** (`default-src 'self'`; `img-src 'self' data:` for the inline favicon; `frame-ancestors 'none'`), plus `X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff`, and `Referrer-Policy: no-referrer`. `style-src` is plain `'self'` with **no** `'unsafe-inline'`: all styling lives in `style.css` (there are no inline `<style>` blocks or `style="..."` attributes), and the JS-driven styling — e.g. the VU bar width — goes through the CSSOM (`element.style`), which CSP does not gate — the app is fully self-contained, so anything cross-origin is blocked. (If you ever load an external resource, or the WebTransport/WebSocket streams break on iOS Safari, widen `connect-src`/the relevant directive.) Separately, a router-wide middleware (`apply_no_store`) adds `Cache-Control: no-store` to any response that did **not** set its own caching policy, so dynamic API / `/ca` responses are never heuristically cached — the static assets keep their `no-cache` + ETag revalidation untouched (the same heuristic-caching lesson as item 4, applied to the dynamic responses the liveness/shutdown detection depends on).

---

## 🏗️ Shared State Architecture

State is split into two layers to keep the transport layer decoupled from HTTP concerns:

```rust
StreamState {          // Shared between WebTransport + WebSocket + API
    ring,              // Arc<RingBuffer>
    is_connected,      // Arc<AtomicBool>
    session_token,     // Arc<parking_lot::Mutex<Option<String>>>
    noise_gate,        // Arc<AtomicU32> (f32 bits)
    gain,              // Arc<AtomicU32> (f32 bits)
    latency_threshold, // Arc<AtomicU32> (milliseconds)
    packets_received,  // Arc<AtomicU64>
    packets_lost,      // Arc<AtomicU64>
    source_sample_rate,// Arc<AtomicU32>
    cancel_tx,         // tokio::sync::broadcast::Sender<()> (F5 handover + shutdown)
    is_shutdown,       // Arc<AtomicBool> (makes the HTTP API reply 503 while shutting down)
    device_ok,         // Arc<AtomicBool> (cleared while the output device is lost / rebuilding)
}

AppState {             // HTTP-layer only (extends StreamState)
    stream: StreamState,
    tls_identity,
    pairing_pin,
    wt_port, lan_ip,
    pairing_throttle,  // Arc<Mutex<PairingThrottle>> (per-IP failed-attempt count + lockout deadline, auto-pruned)
}
```

- `parking_lot::Mutex` is used instead of `std::sync::Mutex` to avoid mutex poisoning panics and provide faster lock acquisition.
- All hot-path state (noise_gate, gain, stats counters) uses lock-free atomics.

---

## ⚠️ Notes for Future Agents

1. **Compilation Environment (w64devkit / gnu).** The local toolchain is `x86_64-pc-windows-gnu` (w64devkit-style, not MSVC). Recent w64devkit builds (GCC 16.x) merge the unwinder into `libgcc.a` and no longer ship a separate `libgcc_eh.a`, yet Rust's `x86_64-pc-windows-gnu` target still passes `-lgcc_eh` to the linker — so a stock `cargo build` fails with `cannot find -lgcc_eh`. The fix is **local to the toolchain and intentionally not committed**: drop an *empty* `libgcc_eh.a` archive into one of the toolchain's own linker search directories (alongside the real `libgcc.a`; run `gcc -print-libgcc-file-name` to locate it). An empty archive — the 8-byte `!<arch>\n` header, e.g. `ar rcs "$(dirname "$(gcc -print-libgcc-file-name)")/libgcc_eh.a"` — is sufficient, because the actual unwinder symbols already resolve from `libgcc.a`; the stub exists only to satisfy the `-lgcc_eh` lookup and has **zero effect on the produced binary** (an empty archive cannot mask a genuinely missing symbol — the final link would still fail with `undefined reference`). Because the fix lives in the toolchain, the repository carries no machine-specific linker paths: there is deliberately **no in-repo `lib/` stub and no `.cargo/config.toml` rustflags block**, so the MSVC (Windows CI + release), Linux, and macOS targets never see any of this. Background: <https://github.com/skeeto/w64devkit/issues/52>. The README's "Build from source" section has the contributor-facing version. **Crypto backend:** the whole tree is pinned to the **`ring`** provider — `rustls` is pulled with `default-features = false, features = ["ring", ...]` and `axum-server` with `tls-rustls-no-provider` (we install the ring provider ourselves in `run`). This deliberately keeps `aws-lc-rs`/`aws-lc-sys` (which need a C compiler + NASM) out of the dependency graph, which matters most for this gnu toolchain. Do **not** re-add a default-featured `rustls`/`axum-server` (`tls-rustls`) — it would silently pull aws-lc-sys back in and reintroduce the C/NASM build burden. The trade-off is no post-quantum key exchange, which `ring` does not implement and a LAN self-signed setup does not need.
2. **Comment Style:** All code comments and documentation inside source code files (`.rs`, `.js`, `.toml`) must be written in **English**.
3. **Mutex Across Await Points:** In Rust async code, never hold a `std::sync::MutexGuard` (or `parking_lot::MutexGuard`) across an `.await` boundary. It makes the returned Future `!Send` and causes Axum's `Handler` compiler errors (`E0277`). Always drop the guard or run lock-dependent operations inside independent blocks.
4. **RingBuffer Safety:** The SPSC ring buffer uses `UnsafeCell` with `Acquire`/`Release` ordering. Only one producer and one consumer are allowed. Adding a second producer or consumer will cause data races. The `unsafe impl Send + Sync` is only valid under the SPSC contract.
5. **Atomic f32 Pattern:** Noise gate and gain are stored as `AtomicU32` using `f32::to_bits()` / `f32::from_bits()`. This avoids the need for `AtomicF32` (which doesn't exist in std). Always use this pattern for lock-free float sharing.
6. **Fetch Timeout Guard:** All client HTTP fetch requests use the `fetchWithTimeout` helper with a default 1000ms timeout. This prevents connection attempts from remaining "pending" indefinitely when the server is offline or unreachable on LAN.
7. **Liveness Detection Cadences:** The client watches for a vanished server at three rates, all driven by the 1s `updateStats` interval (see Disconnect Detection): every second while streaming (full `/api/stats` poll), every ~3 seconds while paired-but-idle on the main screen (lightweight `/api/stats` health check), and every ~3 seconds while in Eco Mode (lightweight check behind the black overlay). Any of them returns the user to the pairing screen when the server is gone. Do **not** rely on the WebTransport/WebSocket close event for this on iOS Safari — it arrives seconds late.
8. **Audio Module (`src/audio/`):** The audio subsystem is split by concern — `ring_buffer.rs` (lock-free SPSC ring), `processor.rs` (packet decode into the ring via `decode_into_ring`; the noise gate and gain now live client-side), `output.rs` (device selection + resampler + cpal stream + the device-loss supervisor `spawn_output_supervisor`, which owns the `!Send` stream on its own thread and rebuilds it if the device drops), and `mod.rs` (re-exports + `MAX_SAMPLE_RATE`). Each file keeps its own `#[cfg(test)]` unit tests next to the code (the idiomatic Rust layout): ring buffer (push/pop, wrap-around, overflow, underrun), the PCM decoder, and the resampler (unity-ratio passthrough, downsampling at a non-unity ratio, prebuffering, channel duplication). The resampler tests are an exact-behaviour safety net — keep them green when touching `write_data`/`ResamplerState`. `cargo test` runs them; CI gates on `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test`, so keep all three green. CI (`ci.yml`) **skips docs-only changes** via `paths-ignore` (markdown, `assets/`, issue templates, `.gitignore`) — a commit that also touches code still runs, and `web/` is intentionally not ignored (it's embedded). A `concurrency` group cancels superseded in-progress runs on the same ref.
9. **Server Module (`src/server/`):** All server-side networking lives here, split by concern — `state.rs` (`StreamState`, `AppState`, and the single-connection lifecycle: `acquire_connection_slot` + `ConnectionGuard`), `api.rs` (REST handlers + their request/response types), `websocket.rs` (the WebSocket fallback, which rides on the HTTP server), `webtransport.rs` (the WebTransport QUIC server — a *separate* server, not routed through Axum), `assets.rs` (hybrid static serving), and `mod.rs` (router, the 503-on-shutdown / same-origin / `no-store` middleware, HTTPS server, and re-exports). Both transports share `state.rs`. Only `StreamState`, `AppState`, `build_router`, `run_https_server`, and `run_webtransport_server` are re-exported (public); everything else is `pub(super)`/private to the module. **Auth surfaces:** `/api/pair` (PIN), then the session token on `/api/renew`, `/api/settings` POST, `/api/client-state`, `/ws`, and `/api/stats` (`/api/stats` reads it from the `X-Session-Token` header; the others from the body/query — `/api/client-state` *must* take it in the body, because `navigator.sendBeacon` cannot set headers), plus the WebTransport session path. A WebTransport session with an invalid/missing token is **explicitly rejected** (`session_request.forbidden()` → 403) and a slot-busy one with `too_many_requests()` → 429, tearing the connection down immediately instead of letting it linger to the QUIC idle timeout — there is intentionally **no per-IP accept rate-limiting** (the LAN trust model + QUIC address validation + the single-connection CAS are deemed sufficient).
10. **Startup Errors & the `run`/`main` Split:** `main` is a thin wrapper that calls `run()`; on any `Err` it prints the full error chain and, **if the app was launched by double-clicking** (`launched_by_double_click`, detected on Windows via `GetConsoleProcessList`), waits for Enter before exiting — so a freshly created console window doesn't vanish before the error can be read. Server startup failures (e.g. the HTTPS/WebTransport port already being in use) are propagated out of the `tokio::select!` as fatal errors rather than only logged, so they reach this handler. CLI inputs are validated/clamped early: `--pin` must be exactly 6 digits, `--noise-gate` is taken in dB (clamped to [-100, 0], -100 = Off) and converted to a linear amplitude, and `--gain` / `--latency-threshold` are clamped to the shared `server::*` bounds.
11. **Client-side Noise Gate (`web/worklet.js`):** The noise gate **and gain** run **client-side** on the audio thread (the AudioWorklet); the server applies no per-sample DSP (it just decodes packets into the ring via `decode_into_ring`). The worklet's gate (same per-sample² threshold, 250ms hold) and only `postMessage`s packets that pass it, so during silence the client sends **nothing** (the phone's Wi-Fi radio idles — a real battery/heat win) and the main thread isn't woken; a throttled `{ level }` message still drives the VU meter to zero. Crucially, the sequence number is **only incremented for sent packets**, so the server's `LossTracker` sees a contiguous stream and never reports false loss. The threshold is pushed to the worklet via a `{ type: 'gate', threshold }` port message on stream start and whenever the dB slider changes (`sendGateToWorklet`). The server keeps no gate at all; it just decodes packets into the ring. An **onset look-ahead** prepends the last gated packet on a closed→open edge so a word's soft attack isn't clipped (the prepended packet takes the next sequence number, keeping the stream contiguous). The main thread also signals the worklet on mute (`{ type: 'mute' }`) so it skips all processing while muted, and drives a `.voice` glow on the mic ring from the packet flow as a gate-activity indicator. The structured port protocol (`gate` / `gain` / `mute` / `frame+level` / `level`) is the extension point for any future audio-thread feature. Note: the worklet emits the **full wire frame** — a 4-byte header gap (`HEADER_BYTES`, kept 2-byte aligned for the PCM `Int16Array` view) followed by the Int16 PCM — and transfers it; the main thread only stamps the sequence number into the gap and sends the buffer as-is, so there is **no per-packet allocation or copy on the main thread** (the one unavoidable allocation is the worklet's frame buffer, which transfer detaches).
12. **Release pipeline (`cargo-dist`).** Multi-platform release builds are managed by **`dist`** (cargo-dist). `.github/workflows/release.yml` is **generated** — do **not** hand-edit it; change `dist-workspace.toml` and run `dist generate` (it will be clobbered otherwise). Config highlights: `installers = []` (we ship plain archives, no install scripts), `targets` covers **x86_64 + arm64 for all three OSes** (Linux, Windows, macOS) — each built natively (cargo-dist picks current runners: `macos-15-intel` for x86_64-apple-darwin, `ubuntu-22.04-arm` for arm64 Linux, and a `[dist.github-custom-runners]` override pins arm64 Windows to `windows-11-arm` so it builds natively instead of cross-compiling from Linux via xwin), so there's no dead-runner problem to chase, `[dist.dependencies.apt] libasound2-dev` provides cpal's ALSA headers on Linux, and releases trigger on **pushing a `vX.Y.Z` git tag** (plus a `pull_request` plan-only check; no `workflow_dispatch`). The third-party license file is wired as a `[[dist.extra-artifacts]]` that runs `cargo about generate` (so it's attached to every release). Releasing: bump `version` in `Cargo.toml`, commit/push, then push the matching tag (`git tag v0.2.0 && git push origin v0.2.0`) — the tag, not the Cargo.toml commit, is what fires the release. It uses the built-in `GITHUB_TOKEN` (no PAT). The separate `ci.yml` still gates `fmt`/`clippy`/`test` on push/PR. `[profile.dist]` in `Cargo.toml` (added by `dist init`, `lto = "thin"`) is the release build profile cargo-dist uses. **crates.io:** dist has **no built-in crates.io publisher** (it ships binaries, not source packages), so the crate is published by a **custom publish job** — `.github/workflows/publish-crates.yml`, wired in via `publish-jobs = ["./publish-crates"]`. dist runs it at the publish stage with `needs: [plan, host]`, i.e. **after** the binaries are built and the GitHub Release exists, so a failed build never burns an immutable crates.io version (a published version can only be yanked, never replaced). It needs a `CARGO_REGISTRY_TOKEN` repo secret; dist passes `secrets: inherit` automatically. Adding or changing a publish job means re-running `dist generate`.
13. **Client WebTransport datagrams API (`web/app.js`):** Safari/iOS 26.4+ only implements the modern `transport.datagrams.createWritable()`; the legacy `writable` property is **never exposed** there and accessing it throws a `TypeError`, which made WebTransport silently fall back to WebSocket ~4ms after connecting (server log: `WebTransport client connected` → `connection closed by peer: 0`). Always feature-detect when acquiring the datagram writer — `createWritable()` first, legacy `writable` fallback (MDN BCD: `createWritable()` Safari 26.4+/Firefox 155+, legacy `writable` Chromium 97+/Firefox 114+).
