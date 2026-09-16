/**
 * QuicMic Client — Streams microphone audio to the PC server.
 *
 * Transport priority:
 *   1. WebTransport (unreliable datagrams over QUIC/UDP)
 *   2. WebSocket   (reliable binary frames over TCP — fallback)
 *
 * Audio pipeline:
 *   getUserMedia -> AudioWorklet -> PCM Int16 -> Transport -> Server
 *
 * Disconnect handling is transport-agnostic. Any close the client did not
 * initiate funnels through `onTransportClosed`, which probes server liveness
 * exactly once and then either returns to the pairing screen (server gone) or
 * transparently reconnects (transient network drop).
 *
 * NOTE: The WebSocket/WebTransport close CODE is intentionally never inspected.
 * iOS Safari frequently reports 1006 (abnormal) — or rejects `transport.closed`
 * with an opaque error — for an otherwise graceful server shutdown, so the close
 * code is unreliable. Liveness is determined by probing the HTTP API instead,
 * which is deterministic across every browser.
 *
 * All diagnostic logs are tagged (e.g. "[transport]", "[reconnect]") so the
 * behaviour can be traced from the iOS Safari Web Inspector console.
 */

// ── State ─────────────────────────────────────────────────────────────
let serverInfo = null;
let sessionToken = null;
let transport = null;        // WebTransport instance (active)
let ws = null;               // WebSocket instance (active, fallback)
let datagramWriter = null;
let audioContext = null;
let micStream = null;
let micSource = null;        // MediaStreamSource feeding the worklet (rebuilt on mic recovery).
let workletNode = null;
let sequenceNumber = 0;
let isStreaming = false;     // The user intends to stream (mic is active).
let isMuted = false;
let isReconnecting = false;  // A reconnect cycle is currently in progress.
let isConnecting = false;    // A transport connect attempt is in progress.
let wasLongPressed = false;
let transportType = 'none';
let wtFallbackNotified = false;  // One-time WebSocket-fallback warning per stream.
let isPowerSaveActive = false;
let lastVuUpdateTime = 0;
let wakeLock = null;         // Screen Wake Lock sentinel (held during Eco Mode).
let ecoDimTimer = null;      // Timer that fades the Eco Mode controls to black.
let voiceTimeout = null;     // Debounce for the voice-activity glow on the mic ring.

// Auto-reconnect
let reconnectAttempts = 0;
const MAX_RECONNECT_ATTEMPTS = 5;

// The worklet posts a level update roughly every 50ms even while the noise gate is
// closed, so a multi-second gap means the capture graph has genuinely stopped — which
// is NOT the same thing as silence. Checked on the existing 1s stats tick, so it costs
// no timer of its own.
const AUDIO_STALL_MS = 4000;
// A momentary interruption (a notification chime, a quick app switch) usually heals by
// itself. Give it this long before we touch the microphone at all.
const UNHEALTHY_GRACE_MS = 5000;
// Hold the server-side warning this long: if recovery lands first, the operator gets a
// single "recovered" line instead of a WARN and its INFO milliseconds apart.
const INTERRUPTION_WARN_DELAY_MS = 700;

// Stats
let packetsSent = 0;
let startTime = 0;
let healthCheckTicks = 0;
let ecoHealthTicks = 0;     // Eco Mode low-frequency liveness counter.
let lastAudioOk = true;      // Last known audio-output-device health (from /api/stats).
let lastAudioFrameAt = 0;    // Last worklet message — ground truth that the graph is alive.
let audioFrameWaiter = null; // Resolver used to verify a recovery rung actually worked.
let audioRecovering = false; // Guards against overlapping recovery attempts.
let interruptionNotified = false; // One server warn per interruption episode (no spam).
let unhealthySince = 0;      // When the health check first saw trouble (grace period).

// ── DOM References ────────────────────────────────────────────────────
const pairScreen = document.getElementById('pair-screen');
const mainScreen = document.getElementById('main-screen');
const pinInput = document.getElementById('pin-input');
const pairBtn = document.getElementById('pair-btn');
const serverLost = document.getElementById('server-lost');
const reloadBtn = document.getElementById('reload-btn');
const micBtn = document.getElementById('mic-btn');
const micRing = document.getElementById('mic-ring');
const micIcon = document.getElementById('mic-icon');
const micHint = document.getElementById('mic-hint');
const statusBadge = document.getElementById('status-badge');
const statusText = document.getElementById('status-text');
const vuBar = document.getElementById('vu-bar');
const vuLevel = document.getElementById('vu-level');
const statTransport = document.getElementById('stat-transport');
const statPing = document.getElementById('stat-ping');
const statBuffer = document.getElementById('stat-buffer');
const statPackets = document.getElementById('stat-packets');
const statUptime = document.getElementById('stat-uptime');
const statLoss = document.getElementById('stat-loss');
const toast = document.getElementById('toast');
const updateBanner = document.getElementById('update-banner');
const updateText = document.getElementById('update-text');
const updateLink = document.getElementById('update-link');
const updateDismiss = document.getElementById('update-dismiss');
const powerSaveBtn = document.getElementById('power-save-btn');
const powerSaveOverlay = document.getElementById('power-save-overlay');
const exitPowerSaveBtn = document.getElementById('exit-power-save-btn');

// Settings UI
const settingsBtn = document.getElementById('settings-btn');
const settingsPanel = document.getElementById('settings-panel');
const ngSlider = document.getElementById('ng-slider');
const ngValue = document.getElementById('ng-value');
const ngReset = document.getElementById('ng-reset');
const gainSlider = document.getElementById('gain-slider');
const gainValue = document.getElementById('gain-value');
const gainReset = document.getElementById('gain-reset');
const lrSlider = document.getElementById('lr-slider');
const lrValue = document.getElementById('lr-value');
const lrReset = document.getElementById('lr-reset');

// ── Generic Helpers ───────────────────────────────────────────────────

function sleep(ms) {
    return new Promise((resolve) => setTimeout(resolve, ms));
}

/**
 * Fetch with a hard timeout. Prevents requests from hanging indefinitely when
 * the server is offline or unreachable on the LAN.
 */
async function fetchWithTimeout(url, options = {}, timeout = 1000) {
    const controller = new AbortController();
    const timeoutId = setTimeout(() => controller.abort(), timeout);
    try {
        return await fetch(url, { ...options, signal: controller.signal });
    } finally {
        clearTimeout(timeoutId);
    }
}

/**
 * Single, fast liveness probe. Returns true only if the server answers with a
 * 2xx response. A graceful shutdown makes the API reply 503, and an offline
 * server makes the fetch throw — both resolve to `false` (server gone).
 */
async function isServerAlive() {
    try {
        const resp = await fetchWithTimeout('/api/info', {}, 800);
        return resp.ok;
    } catch (e) {
        return false;
    }
}

// ── Initialization ────────────────────────────────────────────────────

// Number of attempts for the initial /api/info fetch. A flaky first load (Wi-Fi
// not fully associated yet, or the TLS warning only just dismissed) shouldn't
// strand the page until a manual reload; bounded so a genuinely-down server still
// fails clearly instead of polling forever.
const INFO_FETCH_ATTEMPTS = 3;

async function init() {
    for (let attempt = 1; ; attempt++) {
        try {
            const resp = await fetchWithTimeout('/api/info');
            serverInfo = await resp.json();
            break;
        } catch (e) {
            if (attempt >= INFO_FETCH_ATTEMPTS) {
                showToast('Cannot reach server');
                return;
            }
            // Surface the retry so the user can see it's still trying.
            showToast(`Cannot reach server (${attempt}/${INFO_FETCH_ATTEMPTS})`);
            await sleep(1000);
        }
    }

    // Auto-focus PIN input
    pinInput.focus();

    // Enter key to pair
    pinInput.addEventListener('keydown', (e) => {
        if (e.key === 'Enter') doPair();
        pinInput.classList.remove('error');
    });

    pairBtn.addEventListener('click', doPair);
    reloadBtn.addEventListener('click', () => location.reload());
    updateDismiss.addEventListener('click', () => {
        updateBanner.hidden = true;
        // Remember the dismissal per version so we don't nag again until a newer
        // release appears.
        if (serverInfo && serverInfo.latest_version) {
            localStorage.setItem('dismissedUpdate', serverInfo.latest_version);
        }
    });
    maybeShowUpdateBanner();
    micBtn.addEventListener('click', toggleMic);
    powerSaveBtn.addEventListener('click', togglePowerSave);
    exitPowerSaveBtn.addEventListener('click', togglePowerSave);
    // Tapping the black Eco Mode overlay brings the hint/controls back briefly.
    powerSaveOverlay.addEventListener('pointerdown', () => {
        if (isPowerSaveActive) revealEcoControls();
    });
    settingsBtn.addEventListener('click', toggleSettings);

    // Long-press mic button for mute toggle (500ms)
    let muteTimer = null;
    micBtn.addEventListener('pointerdown', () => {
        wasLongPressed = false;
        muteTimer = setTimeout(() => {
            if (isStreaming) {
                toggleMute();
                wasLongPressed = true;
                if (navigator.vibrate) {
                    navigator.vibrate(50);
                }
            }
            muteTimer = null;
        }, 500);
    });
    const cancelMuteTimer = () => {
        if (muteTimer) {
            clearTimeout(muteTimer);
            muteTimer = null;
        }
    };
    micBtn.addEventListener('pointerup', cancelMuteTimer);
    micBtn.addEventListener('pointerleave', cancelMuteTimer);
    micBtn.addEventListener('pointercancel', cancelMuteTimer);

    // Settings controls
    ngSlider.addEventListener('input', () => {
        const dbVal = parseInt(ngSlider.value);
        ngValue.textContent = dbVal === -100 ? 'Off' : `${dbVal} dB`;
    });
    ngSlider.addEventListener('change', updateServerSettings);
    ngReset.addEventListener('click', () => {
        ngSlider.value = -50;
        ngValue.textContent = '-50 dB';
        updateServerSettings();
    });

    gainSlider.addEventListener('input', () => {
        gainValue.textContent = parseFloat(gainSlider.value).toFixed(1) + 'x';
    });
    gainSlider.addEventListener('change', updateServerSettings);
    gainReset.addEventListener('click', () => {
        gainSlider.value = 1.0;
        gainValue.textContent = '1.0x';
        updateServerSettings();
    });

    lrSlider.addEventListener('input', () => {
        const val = parseInt(lrSlider.value);
        lrValue.textContent = val === 0 ? 'Off' : `${val} ms`;
    });
    lrSlider.addEventListener('change', updateServerSettings);
    lrReset.addEventListener('click', () => {
        lrSlider.value = 150;
        lrValue.textContent = '150 ms';
        updateServerSettings();
    });

    // Load saved settings from localStorage
    loadSettings();

    // Update stats display every second
    setInterval(updateStats, 1000);

    // QR code pairing: URL hash always takes priority (may contain PIN from QR scan)
    const hash = location.hash.slice(1);
    if (hash && hash.length >= 1) {
        // Clear any stale token from a previous server session
        localStorage.removeItem('sessionToken');
        sessionToken = null;
        pinInput.value = hash;
        // Clean up the hash so it doesn't show in the URL
        history.replaceState(null, '', location.pathname);
        // Auto-pair after a short delay (to let UI render)
        setTimeout(doPair, 300);
    } else {
        // No QR hash — check if we have a stored token from a previous pair
        const storedToken = localStorage.getItem('sessionToken');
        if (storedToken) {
            sessionToken = storedToken;
            pairScreen.classList.remove('active');
            mainScreen.classList.add('active');
            // Invalidate any stale zombie streams on the server immediately
            renewSessionToken();
        }
    }

    // Returning to the foreground is the one moment we may safely try to restore
    // capture (see the microphone-recovery section). Going *hidden* is deliberately
    // NOT treated as an event: the page being backgrounded does not reliably mean
    // the microphone was lost — often audio keeps flowing — so it must never raise
    // an alarm on its own. Only the OS's actual mic events do that.
    document.addEventListener('visibilitychange', () => {
        if (document.visibilityState === 'visible') onPageVisible();
    });
}

/**
 * Back in the foreground: make exactly ONE attempt to bring the capture graph back.
 * If it fails, stop cleanly rather than leaving the user in a "connected but
 * silent" limbo.
 */
async function onPageVisible() {
    // Screen Wake Locks are auto-released when the page is hidden; re-acquire.
    if (isPowerSaveActive && !wakeLock) acquireWakeLock();
    if (!isStreaming) return;

    // While we were hidden the OS may have taken the microphone — and JS was frozen, so
    // no event could reach us OR the server. This is the first moment we can look, and
    // the first moment we can TELL anyone. So route through handleAudioInterruption:
    // recovering *silently* here is exactly what left the terminal blind while the user
    // sat waiting, wondering what was happening.
    //
    // Check the flags first (cheap), then verify ACTUAL flow. The timestamp from before
    // the freeze proves nothing — and resetting it here (as an earlier version did) is
    // worse than useless: it masks a zombie graph, which passes every flag check. The
    // only honest question is "is the worklet posting right now?".
    const track = micStream && micStream.getAudioTracks()[0];
    const flagsOk = !!track && track.readyState === 'live' && !track.muted
        && !!audioContext && audioContext.state === 'running';

    // Returning to the page is an explicit signal the user is done with whatever took
    // the mic, so this is the one place we reclaim it even from an OS-muted track.
    // 1500ms, because a muted worklet only heartbeats about once a second.
    if (!flagsOk || !(await waitForAudioFrame(1500))) {
        await handleAudioInterruption('mic_interrupted');
        return;
    }

    // Detect a server that went away while we were suspended.
    if (!isReconnecting) fetchServerStats();
}

async function renewSessionToken() {
    if (!sessionToken) return;
    try {
        const renewed = await renewToken();
        if (renewed) {
            // Push client settings to the server to ensure consistency.
            updateServerSettings();
        } else {
            // renewToken() returns false for both a 503 (server gone) and an invalid
            // token (server alive). Probe once to tell them apart: gone -> lock+reload,
            // alive -> re-pair in place.
            const gone = !(await isServerAlive());
            returnToPairing(gone ? 'Server closed' : 'Session expired. Please pair again.', gone);
        }
    } catch (e) {
        console.warn('[init] failed to renew session token:', e);
        returnToPairing('Server closed', true);
    }
}

function returnToPairing(reason, serverGone = false) {
    console.warn('[pairing] returning to pairing screen:', reason, serverGone ? '(server gone)' : '');
    if (isStreaming) {
        stopStreaming();
    }
    if (isPowerSaveActive) {
        isPowerSaveActive = false;
        if (ecoDimTimer) {
            clearTimeout(ecoDimTimer);
            ecoDimTimer = null;
        }
        powerSaveOverlay.classList.remove('active');
        powerSaveOverlay.classList.remove('dimmed');
        releaseWakeLock();
    }
    localStorage.removeItem('sessionToken');
    sessionToken = null;
    mainScreen.classList.remove('active');
    pairScreen.classList.add('active');

    if (serverGone) {
        // The server is gone. If it comes back it will have a NEW self-signed
        // certificate (regenerated on every start), so this page's pinned hash and
        // already-accepted cert are stale: in-page re-pairing would silently fail on
        // the cert mismatch, and we can't even probe for its return (the mismatch
        // fails the fetch). A full reload is the only reliable way back — so lock the
        // PIN entry and prompt a refresh.
        pinInput.value = '';
        pinInput.disabled = true;
        pairBtn.disabled = true;
        serverLost.hidden = false;
    } else {
        // Server still reachable (e.g. the session was taken over): let the user
        // re-pair in place — the cert is unchanged, so it works without a reload.
        pinInput.disabled = false;
        pairBtn.disabled = false;
        serverLost.hidden = true;
        pinInput.value = '';
        pinInput.focus();
    }

    if (reason) {
        showToast(reason);
    }
}

// ── Eco Mode (Power Save) ─────────────────────────────────────────────

// After this delay with no interaction, the Eco Mode hint/controls fade out so
// the screen becomes fully black and static, maximising OLED-off time.
const ECO_DIM_DELAY = 4000;

function scheduleEcoDim() {
    if (ecoDimTimer) clearTimeout(ecoDimTimer);
    ecoDimTimer = setTimeout(() => {
        powerSaveOverlay.classList.add('dimmed');
        ecoDimTimer = null;
    }, ECO_DIM_DELAY);
}

function revealEcoControls() {
    powerSaveOverlay.classList.remove('dimmed');
    scheduleEcoDim();
}

function togglePowerSave() {
    isPowerSaveActive = !isPowerSaveActive;
    if (isPowerSaveActive) {
        powerSaveOverlay.classList.remove('dimmed');
        powerSaveOverlay.classList.add('active');
        mainScreen.classList.remove('active');
        // Keep the page awake so JS and the audio/transport sockets stay live
        // behind the black overlay (the screen stays on but OLED pixels are off).
        acquireWakeLock();
        // Auto-fade the hint/controls so the screen goes fully black.
        scheduleEcoDim();
    } else {
        if (ecoDimTimer) {
            clearTimeout(ecoDimTimer);
            ecoDimTimer = null;
        }
        powerSaveOverlay.classList.remove('active');
        powerSaveOverlay.classList.remove('dimmed');
        mainScreen.classList.add('active');
        vuBar.style.width = '0%';
        vuLevel.textContent = '0%';
        releaseWakeLock();
    }
}

async function acquireWakeLock() {
    if (!('wakeLock' in navigator)) {
        console.log('[wakelock] Screen Wake Lock API not supported');
        return;
    }
    try {
        wakeLock = await navigator.wakeLock.request('screen');
        console.log('[wakelock] acquired');
        wakeLock.addEventListener('release', () => {
            console.log('[wakelock] released by system');
        });
    } catch (e) {
        console.warn('[wakelock] request failed:', e);
        wakeLock = null;
    }
}

async function releaseWakeLock() {
    if (!wakeLock) return;
    try {
        await wakeLock.release();
    } catch (e) {
        // Ignore — the lock may already be gone.
    }
    wakeLock = null;
    console.log('[wakelock] released');
}

function toggleSettings() {
    settingsPanel.classList.toggle('active');
}

function toggleMute() {
    isMuted = !isMuted;
    if (isMuted) {
        micIcon.textContent = '🔇';
        statusText.textContent = 'Muted';
        statusBadge.className = 'status-badge muted';
        micBtn.classList.add('muted');
        micRing.classList.add('muted');
        micRing.classList.remove('voice');
        micHint.textContent = 'Long press to unmute';
        vuBar.style.width = '0%';
        vuLevel.textContent = '0%';
    } else {
        micIcon.textContent = '⏹';
        statusText.textContent = 'Streaming';
        statusBadge.className = 'status-badge connected';
        micBtn.classList.remove('muted');
        micRing.classList.remove('muted');
        micHint.textContent = 'Long press to mute';
    }
    sendMuteToWorklet(); // tell the worklet to stop/resume processing
}

// ── Settings ──────────────────────────────────────────────────────────

// The noise gate is stored/sent as a linear amplitude (0.0 = off) but shown on
// the slider in dB. These two helpers are the single source of that mapping.
function noiseGateToDb(linear) {
    return linear > 0 ? Math.round(20 * Math.log10(linear)) : -100;
}
function dbToNoiseGate(db) {
    return db === -100 ? 0.0 : Math.pow(10, db / 20);
}

/** Apply a { noise_gate, gain, latency_threshold } object to the settings UI. */
function applySettingsToUI(s) {
    if (s.noise_gate !== undefined) {
        // Clamp to the slider's range so an out-of-range server value (e.g. a tiny
        // linear noise_gate set via the API) can't leave the thumb and the label
        // disagreeing. Anything at or below the floor reads as "Off".
        const db = Math.max(-100, Math.min(0, noiseGateToDb(parseFloat(s.noise_gate))));
        ngSlider.value = db;
        ngValue.textContent = db <= -100 ? 'Off' : `${db} dB`;
    }
    if (s.gain !== undefined) {
        gainSlider.value = s.gain;
        gainValue.textContent = parseFloat(s.gain).toFixed(1) + 'x';
    }
    if (s.latency_threshold !== undefined) {
        const lt = parseInt(s.latency_threshold);
        lrSlider.value = lt;
        lrValue.textContent = lt === 0 ? 'Off' : `${lt} ms`;
    }
}

function loadSettings() {
    const saved = localStorage.getItem('quicmic_settings');
    if (saved) {
        // The client (localStorage) is the source of truth, so a user's saved
        // settings survive a server restart: apply them and let the pair/renew sync
        // push them to the server. We deliberately do NOT fetch and overwrite with
        // the server's values here — a freshly restarted server would otherwise
        // clobber them with its CLI defaults.
        try {
            applySettingsToUI(JSON.parse(saved));
            return;
        } catch (e) { /* corrupt entry — fall through to the server defaults */ }
    }
    // First run (nothing saved yet): adopt whatever the server currently has.
    fetchSettings();
}

async function fetchSettings() {
    try {
        const resp = await fetchWithTimeout('/api/settings');
        applySettingsToUI(await resp.json());
    } catch (e) { /* server may not be reachable yet */ }
}

async function updateServerSettings() {
    const settings = {
        noise_gate: dbToNoiseGate(parseInt(ngSlider.value)),
        gain: parseFloat(gainSlider.value),
        latency_threshold: parseInt(lrSlider.value),
    };

    localStorage.setItem('quicmic_settings', JSON.stringify(settings));
    sendGateToWorklet(); // keep the worklet's client-side gate in sync
    sendGainToWorklet(); // and the gain

    try {
        await fetchWithTimeout('/api/settings', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ ...settings, token: sessionToken }),
        });
    } catch (e) {
        showToast('Settings update failed');
    }
}

/**
 * Push the current noise-gate threshold (linear amplitude) to the worklet's
 * client-side gate. A threshold of 0 disables the gate (passthrough).
 */
function sendGateToWorklet() {
    if (!workletNode) return;
    workletNode.port.postMessage({
        type: 'gate',
        threshold: dbToNoiseGate(parseInt(ngSlider.value)),
    });
}

/** Push the current output gain (linear multiplier) to the worklet. */
function sendGainToWorklet() {
    if (!workletNode) return;
    workletNode.port.postMessage({ type: 'gain', value: parseFloat(gainSlider.value) });
}

/** Tell the worklet whether we are muted, so it can skip all work while muted. */
function sendMuteToWorklet() {
    if (!workletNode) return;
    workletNode.port.postMessage({ type: 'mute', muted: isMuted });
}

// ── Pairing & Session Tokens ──────────────────────────────────────────

/**
 * Renew the session token, invalidating any stale connection on the server.
 * Returns true on success. Returns false if the server rejected the token or
 * is shutting down (non-2xx). Throws on a network error (server unreachable).
 */
async function renewToken() {
    const resp = await fetchWithTimeout('/api/renew', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ token: sessionToken }),
    });
    if (!resp.ok) return false;
    const result = await resp.json();
    if (result.success && result.token) {
        sessionToken = result.token;
        localStorage.setItem('sessionToken', sessionToken);
        return true;
    }
    return false;
}

async function doPair() {
    const pin = pinInput.value.trim();
    // The pairing PIN is always exactly 6 digits, so reject anything else before
    // making a doomed round-trip to the server.
    if (!/^\d{6}$/.test(pin)) {
        pinInput.classList.add('error');
        showToast('Enter the 6-digit PIN');
        return;
    }

    pairBtn.disabled = true;
    pairBtn.textContent = 'Connecting...';

    try {
        const resp = await fetchWithTimeout('/api/pair', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ pin }),
        });

        const result = await resp.json();

        if (result.success) {
            sessionToken = result.token;
            localStorage.setItem('sessionToken', sessionToken);
            pairScreen.classList.remove('active');
            mainScreen.classList.add('active');
            // Push settings to server after pairing
            updateServerSettings();
        } else {
            pinInput.classList.add('error');
            showToast(result.error || 'Incorrect PIN');
        }
    } catch (e) {
        showToast('Connection error');
    } finally {
        pairBtn.disabled = false;
        pairBtn.textContent = 'Connect';
    }
}

// ── Microphone Toggle ─────────────────────────────────────────────────

async function toggleMic() {
    if (wasLongPressed) {
        wasLongPressed = false;
        return;
    }
    if (isStreaming) {
        stopStreaming();
    } else {
        await startStreaming();
    }
}

async function startStreaming() {
    try {
        // Renew the token first to clear any old/zombie session on the server.
        if (sessionToken) {
            const renewed = await renewToken();
            if (!renewed) {
                // Distinguish a gone server (lock + reload) from a merely stale
                // session on a live server (re-pair in place).
                const gone = !(await isServerAlive());
                returnToPairing(gone ? 'Server closed' : 'Session expired. Please pair again.', gone);
                return;
            }
        }

        // Build the whole capture graph. Extracted so the recovery ladder can rebuild
        // it from scratch when it turns out to be a zombie (see setupAudioGraph).
        await setupAudioGraph();

        // Mark streaming intent BEFORE connecting so the close handler treats
        // any subsequent drop as a real disconnect (not initial-connect noise).
        isStreaming = true;
        isMuted = false;
        isReconnecting = false;
        sequenceNumber = 0;
        packetsSent = 0;
        startTime = Date.now();
        reconnectAttempts = 0;
        wtFallbackNotified = false;

        // Establish transport connection.
        try {
            await connectTransport();
        } catch (e) {
            isStreaming = false;
            throw e;
        }

        // Update UI
        micBtn.classList.add('active');
        micRing.classList.add('active');
        micIcon.textContent = '⏹';
        micHint.textContent = 'Long press to mute';
        setConnected(true);

    } catch (e) {
        console.error('[stream] failed to start streaming:', e);

        let errMsg = e.message;
        const isNetworkError = e instanceof TypeError ||
            (errMsg && (errMsg.includes('fetch') || errMsg.includes('NetworkError') || errMsg.includes('Failed to fetch')));

        stopStreaming();

        if (isNetworkError) {
            returnToPairing('Server closed', true);
        } else {
            if (errMsg === 'WebSocket connection failed') {
                errMsg = 'Connection rejected. Another device may be active. Try again in 5s.';
            }
            showToast(errMsg || 'Connection failed');
        }
    }
}

function stopStreaming() {
    // Clearing the streaming intent makes every pending close handler a no-op,
    // so tearing the transport down here never triggers a reconnect.
    isStreaming = false;
    isMuted = false;
    isReconnecting = false;

    teardownTransport();

    // Stop audio
    teardownAudioGraph();
    interruptionNotified = false;
    unhealthySince = 0;
    lastAudioFrameAt = 0;

    // Update UI
    micBtn.classList.remove('active');
    micRing.classList.remove('active');
    micBtn.classList.remove('muted');
    micRing.classList.remove('muted');
    micRing.classList.remove('voice');
    clearTimeout(voiceTimeout);
    micIcon.textContent = '🎙';
    micHint.textContent = 'Tap to start streaming';
    setConnected(false);
    transportType = 'none';
    if (isPowerSaveActive) {
        togglePowerSave();
    }
    vuBar.style.width = '0%';
}

// ── Microphone Interruption & Recovery ────────────────────────────────
//
// The OS — not us — owns the microphone. An incoming call, another app grabbing
// it, or the page being backgrounded all kill capture, and the browser does not
// hand it back on its own. A dead capture graph is invisible on the wire: the
// client-side noise gate legitimately sends nothing during silence, so "no
// packets" is NOT evidence of a fault. Hence everything below keys off the OS's
// own events — no polling, no timers, no retry loops.
//
// Guardrail: we only ever ASK for the microphone again while the page is in the
// foreground (or on an explicit user tap). We never re-request it in the
// background, so another app is never fought for the mic.

/** Subscribe to the OS's microphone-track lifecycle events. */
function attachMicTrackHandlers() {
    const track = micStream && micStream.getAudioTracks()[0];
    if (!track) return;

    // Permanently revoked (another app took it). Only a fresh getUserMedia can
    // recover, and only with the page in the foreground.
    // Permanently revoked (another app took it). Nothing brings it back on its own, so
    // escalate straight away.
    track.onended = () => {
        console.warn('[audio] microphone track ended (taken by another app)');
        handleAudioInterruption('mic_lost');
    };

    // Temporary OS interruption (e.g. an incoming call). Diagnostic only:
    // `checkAudioHealth` is the single decision point, and it deliberately WAITS on an
    // OS-muted track rather than fighting the call for the microphone.
    track.onmute = () => console.warn('[audio] microphone muted by the OS (interruption)');

    track.onunmute = async () => {
        console.log('[audio] microphone unmuted by the OS');
        if (!isStreaming) return;
        // The OS handed the microphone back, so resuming *our own* context takes nothing
        // from anyone — do it even while backgrounded. Gating this on `visible` (as an
        // earlier version did) is why audio used to stay dead after a call until the user
        // reopened the page: the mic was already back, we just never un-parked the
        // context. Re-acquiring (getUserMedia) is the thing that must stay foreground-only.
        if (audioContext && audioContext.state !== 'running') {
            try { await audioContext.resume(); } catch (e) { /* may need a user gesture */ }
        }
    };
}

/** The mic constraints, shared by the initial setup and the recovery ladder. */
const MIC_CONSTRAINTS = {
    audio: {
        channelCount: 1,
        echoCancellation: false,
        noiseSuppression: false,
        autoGainControl: false,
    },
};

/**
 * Build the capture graph from scratch: getUserMedia -> AudioContext -> worklet.
 * `startStreaming` and the ladder's full-rebuild rung share this, so a rebuilt graph is
 * identical to a freshly started one — including re-pushing gate/gain/mute into the new
 * worklet, which would otherwise come up at its defaults.
 */
async function setupAudioGraph() {
    micStream = await navigator.mediaDevices.getUserMedia(MIC_CONSTRAINTS);

    audioContext = new AudioContext({ latencyHint: 'interactive' });
    // Diagnostic only. We deliberately do NOT recover from here: state changes fire in
    // bursts (running -> suspended -> interrupted), and acting on each one spammed the
    // server with duplicate warnings. `checkAudioHealth` is the single decision point.
    audioContext.onstatechange = () =>
        console.warn('[audio] AudioContext state ->', audioContext && audioContext.state);

    try {
        // No cache-buster needed: the server sends `Cache-Control: no-cache` + ETag, so
        // the browser revalidates and picks up an edited worklet on the next start.
        await audioContext.audioWorklet.addModule('worklet.js');
    } catch (e) {
        throw new Error('Audio processor failed to load. Check browser compatibility.');
    }

    attachMicTrackHandlers();

    micSource = audioContext.createMediaStreamSource(micStream);
    workletNode = new AudioWorkletNode(audioContext, 'mic-processor');

    workletNode.port.onmessage = (event) => {
        // GROUND TRUTH that the graph is pumping: the worklet posts a level update even
        // while the gate is closed, and a heartbeat even while muted. This timestamp is
        // the only thing that can tell "silent" apart from "dead" — the state flags
        // cannot, and trusting them is what produced "Reconnected!" over silence.
        lastAudioFrameAt = Date.now();
        if (audioFrameWaiter) {
            const resolve = audioFrameWaiter;
            audioFrameWaiter = null;
            resolve(true);
        }
        if (isMuted) return; // Muted: nothing to send; meter stays at 0.
        const d = event.data;
        updateVu(d.level);
        if (d.frame) {
            micRing.classList.add('voice');
            clearTimeout(voiceTimeout);
            voiceTimeout = setTimeout(() => micRing.classList.remove('voice'), 200);
            sendAudioPacket(d.frame);
        }
    };

    // A fresh worklet starts at its defaults, so re-push the live settings.
    sendGateToWorklet();
    sendGainToWorklet();
    sendMuteToWorklet();

    micSource.connect(workletNode);
    // Don't connect to destination — we don't want local playback.
    lastAudioFrameAt = Date.now();
}

/** Release every audio object. No UI changes — the caller decides what to show. */
function teardownAudioGraph() {
    if (micSource) {
        micSource.disconnect();
        micSource = null;
    }
    if (workletNode) {
        workletNode.disconnect();
        workletNode = null;
    }
    if (audioContext) {
        audioContext.onstatechange = null;
        audioContext.close().catch(() => { /* already closing */ });
        audioContext = null;
    }
    if (micStream) {
        micStream.getTracks().forEach((t) => t.stop());
        micStream = null;
    }
}

/** Is the graph actually pumping? The one signal that cannot lie. */
function isAudioFlowing() {
    return lastAudioFrameAt > 0 && Date.now() - lastAudioFrameAt < AUDIO_STALL_MS;
}

/**
 * Full health = the graph is pumping AND the OS is really giving us signal. BOTH terms
 * are required, because each catches exactly what the other misses:
 *   - flow alone is not enough: an OS-muted track still pumps (silence)
 *   - flags alone are not enough: a zombie graph reports 'live' + 'running' while dead
 */
function isAudioHealthy() {
    const track = micStream && micStream.getAudioTracks()[0];
    return isAudioFlowing()
        && !!track && track.readyState === 'live' && !track.muted
        && !!audioContext && audioContext.state === 'running';
}

/** Resolves as soon as the worklet posts again — the only proof a recovery rung worked. */
function waitForAudioFrame(timeoutMs = 800) {
    return new Promise((resolve) => {
        const timer = setTimeout(() => {
            audioFrameWaiter = null;
            resolve(false);
        }, timeoutMs);
        audioFrameWaiter = (ok) => {
            clearTimeout(timer);
            resolve(ok);
        };
    });
}

/**
 * Escalating recovery ladder — one pass, foreground only, never in the background.
 * Every rung is verified by the worklet actually posting again; the state flags are
 * never allowed to declare success, because that is precisely how a zombie graph
 * slipped through before.
 */
async function recoverAudio() {
    if (document.visibilityState !== 'visible') return false;

    // An unmuted worklet posts every ~50ms; a muted one only heartbeats about once a
    // second. Spending the muted budget on every rung is what made recovery take
    // seconds after a phone call.
    const verifyMs = isMuted ? 1500 : 400;

    // A rung only counts as verified when the worklet posts again AND the OS is really
    // giving us signal. Flow alone is not proof: a still-muted track happily pumps
    // silence through the graph, which would let a rung declare victory over no audio.
    const verified = async () => (await waitForAudioFrame(verifyMs)) && isAudioHealthy();

    // Narrate each rung to the operator's terminal — but only once the interruption has
    // actually been announced. A recovery that lands before the warning fires stays a
    // single quiet line, instead of a burst of progress notes about a problem nobody saw.
    const rung = (label, reason) => {
        console.log(`[audio] ${label}`);
        if (interruptionNotified) notifyServerState(reason);
    };

    const track = micStream && micStream.getAudioTracks()[0];
    const trackDead = !track || track.readyState !== 'live';

    // 1. A suspended/interrupted context just needs a resume (cheapest rung) — but only
    //    if the source is still alive. Resuming a context whose track is dead cannot
    //    produce anything, so skip the wasted wait and go straight to re-acquiring.
    if (!trackDead && audioContext && audioContext.state !== 'running') {
        rung('recovery 1/3: resume context', 'recovery_resume');
        try { await audioContext.resume(); } catch (e) { /* may need a user gesture */ }
        if (await verified()) return true;
    }

    // 2. The track is dead or the OS still holds it: get a fresh one and re-wire it into
    //    the existing worklet.
    rung('recovery 2/3: re-acquire microphone', 'recovery_reacquire');
    try {
        if (micStream) micStream.getTracks().forEach((t) => t.stop());
        micStream = await navigator.mediaDevices.getUserMedia(MIC_CONSTRAINTS);
        attachMicTrackHandlers();
        if (micSource) micSource.disconnect();
        micSource = audioContext.createMediaStreamSource(micStream);
        micSource.connect(workletNode);
        if (await verified()) return true;
    } catch (e) {
        console.warn('[audio] re-acquire failed:', e);
    }

    // 3. The context/worklet is itself a zombie — the usual outcome after a long freeze:
    //    every flag reads healthy, yet nothing is ever pumped. Only a full rebuild fixes
    //    it, and this rung is what the old code was missing.
    rung('recovery 3/3: full graph rebuild', 'recovery_rebuild');
    try {
        teardownAudioGraph();
        await setupAudioGraph();
        if (await verified()) return true;
    } catch (e) {
        console.warn('[audio] rebuild failed:', e);
    }

    return false;
}

/**
 * Something stopped capture. Warn the server ONCE per episode, then run the ladder
 * (foreground only) and stop cleanly if every rung fails.
 */
async function handleAudioInterruption(reason) {
    if (!isStreaming || audioRecovering) return;
    audioRecovering = true;
    let warnTimer = null;
    try {
        // Don't cry wolf. A recovery that lands in a few hundred ms would otherwise log
        // its WARN and the "recovered" INFO back-to-back, milliseconds apart — noise the
        // operator can do nothing with. So hold the warning briefly: if the interruption
        // actually drags on (a phone call, a zombie graph needing a full rebuild), it
        // goes out, and they learn why the audio stopped and that we are on it.
        // Several detectors can fire for one interruption, so this is also the single
        // dedupe point — without it each of them sent its own WARN.
        // Backgrounded. Capture keeps running in the background as long as nothing else
        // takes the microphone — so getting here means something really did take it.
        // We will NOT try to reclaim it (that would fight whatever app is using it), and
        // since no recovery can therefore "beat" the warning, send it straight away.
        // (Deferring it here was a bug: the `finally` cleanup cancelled the timer on this
        // very path, so a mic lost in the background was never reported at all.)
        if (document.visibilityState !== 'visible') {
            if (!interruptionNotified) {
                interruptionNotified = true;
                notifyServerState(reason);
            }
            return;
        }

        if (!interruptionNotified) {
            warnTimer = setTimeout(() => {
                warnTimer = null;
                interruptionNotified = true;
                notifyServerState(reason);
            }, INTERRUPTION_WARN_DELAY_MS);
        }

        const recovered = await recoverAudio();
        if (warnTimer) {
            clearTimeout(warnTimer);
            warnTimer = null;
        }

        if (recovered) {
            console.log('[audio] recovered');
            // Close the episode in the operator's log, so the terminal shows the
            // interruption *ending* instead of just going quiet again. Standalone when
            // the recovery beat the warning; the closing half of the pair when it didn't.
            notifyServerState('audio_resumed');
            interruptionNotified = false;
            unhealthySince = 0;
            return;
        }

        // Failed outright: the operator must hear about it even if the delay hadn't run.
        if (!interruptionNotified) {
            interruptionNotified = true;
            notifyServerState(reason);
        }
        if (isStreaming) stopForAudioLoss('Microphone was interrupted — streaming stopped.');
    } finally {
        if (warnTimer) clearTimeout(warnTimer);
        audioRecovering = false;
    }
}

/**
 * The single health decision point, on the 1s tick that already exists — no new timer.
 *
 * An OS-muted track is handled specially: it means a call or another app currently owns
 * the microphone, and the OS gives it back via `onunmute` however long that takes. We
 * WAIT instead of reclaiming it — recovering here would mean fighting an active phone
 * call for the mic. Everything else gets a short grace period (so a momentary blip heals
 * itself untouched) and then the ladder.
 */
function checkAudioHealth() {
    // Runs while backgrounded too. Capture keeps working in the background as long as
    // nothing else takes the microphone, so trouble seen here is real and worth
    // reporting — and reporting is all that happens while hidden, because
    // handleAudioInterruption is the one that refuses to *reclaim* the mic there.
    if (!isStreaming || isReconnecting || audioRecovering) return;

    const track = micStream && micStream.getAudioTracks()[0];
    if (track && track.readyState === 'live' && track.muted) {
        if (!interruptionNotified) {
            interruptionNotified = true;
            notifyServerState('mic_interrupted');
        }
        return; // the OS owns it; wait for onunmute rather than fight for it
    }

    if (isAudioHealthy()) {
        unhealthySince = 0;
        // Close the episode. This is the path that fires when the OS simply hands the mic
        // back on its own (a call ending) — no rung of the ladder runs, so without this
        // the terminal showed the interruption but never its end.
        if (interruptionNotified) {
            interruptionNotified = false;
            notifyServerState('audio_resumed');
        }
        return;
    }
    if (unhealthySince === 0) {
        unhealthySince = Date.now();
        return; // give it a chance to fix itself before we touch anything
    }
    if (Date.now() - unhealthySince < UNHEALTHY_GRACE_MS) return;

    console.warn('[audio] unhealthy past the grace period — running recovery');
    unhealthySince = 0;
    handleAudioInterruption('mic_interrupted');
}

/** Stop cleanly with an honest message, instead of a "connected but silent" limbo. */
function stopForAudioLoss(reason) {
    stopStreaming();
    showToast(reason);
}

/**
 * Best-effort note to the server explaining why the audio went quiet, so the
 * terminal shows a cause instead of unexplained silence. While the page is being
 * backgrounded we use `sendBeacon` (the browser flushes it even as JS freezes); in
 * the foreground a normal fetch is fully reliable.
 */
function notifyServerState(reason) {
    if (!sessionToken) return;
    const payload = JSON.stringify({ token: sessionToken, reason });
    if (document.visibilityState === 'hidden' && navigator.sendBeacon) {
        navigator.sendBeacon('/api/client-state', new Blob([payload], { type: 'application/json' }));
        return;
    }
    fetchWithTimeout('/api/client-state', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: payload,
    }).catch(() => { /* best-effort; never surfaced to the user */ });
}

/** Close and discard the active transport objects without touching audio. */
function teardownTransport() {
    if (transport) {
        try { transport.close(); } catch (e) { /* already closing */ }
        transport = null;
        datagramWriter = null;
    }
    if (ws) {
        try { ws.close(); } catch (e) { /* already closing */ }
        ws = null;
    }
}

// ── Transport Connection ──────────────────────────────────────────────

async function connectTransport() {
    // The actual sample rate the browser settled on (may differ from 48kHz).
    const actualSampleRate = audioContext ? audioContext.sampleRate : 48000;

    // While connecting, close events are resolved by this function's own
    // success/failure path rather than by `onTransportClosed`, so a failed
    // attempt never spuriously triggers the reconnect machinery.
    isConnecting = true;
    try {
        // Try WebTransport first (low-latency UDP/QUIC).
        if ('WebTransport' in window) {
            try {
                await connectWebTransport(actualSampleRate);
                return;
            } catch (e) {
                console.warn('[transport] WebTransport failed, falling back to WebSocket:', e);
                teardownTransport();
            }
        }

        // Fallback to WebSocket (reliable TCP).
        await connectWebSocket(actualSampleRate);
    } finally {
        isConnecting = false;
    }
}

async function connectWebTransport(sampleRate) {
    // Bracket an IPv6 literal for the URL authority (RFC 3986); IPv4 is unchanged.
    // The server brackets the same way in its printed/QR URL (`url_host`).
    const host = serverInfo.lan_ip.includes(':') ? `[${serverInfo.lan_ip}]` : serverInfo.lan_ip;
    const url = `https://${host}:${serverInfo.wt_port}/${sessionToken}?sr=${sampleRate}`;

    // Decode the base64 cert hash into bytes for certificate pinning.
    const hashBytes = Uint8Array.from(atob(serverInfo.cert_hash), (c) => c.charCodeAt(0));

    transport = new WebTransport(url, {
        serverCertificateHashes: [{ algorithm: 'sha-256', value: hashBytes.buffer }],
        allowPooling: false,
    });
    const activeTransport = transport;

    // Register the close handler immediately — before awaiting `ready` — so that if
    // the connection never establishes (e.g. UDP blocked), the rejected `closed`
    // promise is still handled instead of surfacing as an unhandled rejection. Both
    // the clean (.then) and errored (.catch) paths, for any close code, funnel into
    // the single transport-agnostic handler, which ignores events while still
    // connecting (isConnecting guard).
    transport.closed
        .then((info) => onTransportClosed('WebTransport', activeTransport, {
            clean: true,
            code: info && info.closeCode,
            reason: info && info.reason,
        }))
        .catch((err) => onTransportClosed('WebTransport', activeTransport, {
            clean: false,
            code: err && err.closeCode,
            reason: (err && err.message) || String(err),
        }));

    const timeoutPromise = new Promise((_, reject) =>
        setTimeout(() => reject(new Error('WebTransport connection timeout')), 1000)
    );
    await Promise.race([transport.ready, timeoutPromise]);

    // Datagram writer acquisition differs by engine (MDN BCD):
    // - Modern `createWritable()`: Safari 26.4+, Firefox 155+
    // - Legacy `writable`: Chromium 97+, Firefox 114+
    // Feature-detect so each engine takes the appropriate path.
    const datagramStream = transport.datagrams;
    const writable = typeof datagramStream.createWritable === 'function'
        ? datagramStream.createWritable()
        : datagramStream.writable;
    datagramWriter = writable.getWriter();
    transportType = 'WebTransport';
    console.log('[transport] connected via WebTransport (QUIC/UDP)');
}

async function connectWebSocket(sampleRate) {
    return new Promise((resolve, reject) => {
        const url = `wss://${location.host}/ws?token=${sessionToken}&sr=${sampleRate}`;
        ws = new WebSocket(url);
        ws.binaryType = 'arraybuffer';
        const activeWs = ws;

        const timeoutId = setTimeout(() => {
            if (ws.readyState !== WebSocket.OPEN) {
                try { ws.close(); } catch (e) { /* ignore */ }
                reject(new Error('WebSocket connection timeout'));
            }
        }, 1000);

        ws.onopen = () => {
            clearTimeout(timeoutId);
            transportType = 'WebSocket';
            console.log('[transport] connected via WebSocket (TCP — fallback)');
            // If the browser supports WebTransport yet we still landed on WebSocket,
            // the low-latency UDP/QUIC path failed. On a LAN the overwhelming cause is
            // a blocked UDP port (a healthy QUIC handshake is sub-second), so we give
            // the direct, actionable hint. Warn once per stream (the stat panel only
            // shows the transport name); browsers without WebTransport skip this (WS
            // is the expected transport there).
            if (('WebTransport' in window) && !wtFallbackNotified) {
                wtFallbackNotified = true;
                showToast('Using WebSocket fallback — WebTransport (UDP) unavailable. Allow UDP on port 8443 in your firewall for lower latency.');
            }
            resolve();
        };

        ws.onerror = () => {
            clearTimeout(timeoutId);
            reject(new Error('WebSocket connection failed'));
        };

        ws.onclose = (event) => {
            clearTimeout(timeoutId);
            onTransportClosed('WebSocket', activeWs, {
                clean: event.wasClean,
                code: event.code,
                reason: event.reason,
            });
        };
    });
}

// ── Disconnect Handling & Auto-Reconnect ──────────────────────────────

/**
 * Single entry point for every transport close event (WebSocket or
 * WebTransport, clean or errored). Stale events from a transport we already
 * replaced, and closes we initiated ourselves, are ignored.
 */
function onTransportClosed(kind, instance, detail) {
    console.warn(`[transport] ${kind} closed:`, detail);

    // Ignore events from a transport instance we have already replaced/closed.
    const current = kind === 'WebTransport' ? transport : ws;
    if (instance !== current) {
        console.log(`[transport] ignoring stale ${kind} close event`);
        return;
    }
    // Only an established-stream drop is a real disconnect. Ignore closes while
    // still connecting (handled by connectTransport), after the user stopped
    // (isStreaming=false), or during an in-progress reconnect handover.
    if (isConnecting || !isStreaming || isReconnecting) {
        return;
    }
    handleUnexpectedDisconnect();
}

/**
 * Decide what an unexpected disconnect means with a single liveness probe:
 *   - server gone (offline or shutting down)  -> return to the pairing screen
 *   - server alive (transient network drop)   -> reconnect transparently
 */
async function handleUnexpectedDisconnect() {
    if (isReconnecting || !isStreaming) return;
    isReconnecting = true;

    teardownTransport();
    statusText.textContent = 'Reconnecting...';
    statusBadge.className = 'status-badge disconnected';

    console.log('[disconnect] probing server liveness...');
    const alive = await isServerAlive();

    if (!isStreaming) { // The user stopped while we were probing.
        isReconnecting = false;
        return;
    }
    if (!alive) {
        console.log('[disconnect] server is gone -> returning to pairing');
        isReconnecting = false;
        returnToPairing('Server closed', true);
        return;
    }

    console.log('[disconnect] server is alive -> reconnecting');
    showToast('Connection lost. Reconnecting...');
    await reconnectLoop();
}

async function reconnectLoop() {
    reconnectAttempts = 0;
    while (reconnectAttempts < MAX_RECONNECT_ATTEMPTS && isStreaming) {
        const delay = reconnectAttempts === 0 ? 0 : Math.min(1000 * reconnectAttempts, 4000);
        if (delay > 0) await sleep(delay);
        reconnectAttempts++;
        if (!isStreaming) break;

        try {
            if (!(await renewToken())) {
                // Server is reachable but rejected the session — pairing is stale.
                break;
            }
            await connectTransport();

            // The transport being back says nothing about the microphone: an OS
            // interruption can leave capture dead, in which case we would be
            // "connected" but permanently silent. Verify before claiming success —
            // never toast "Reconnected!" over a dead mic. Route through
            // handleAudioInterruption so the server is TOLD, rather than recovering
            // silently and leaving the terminal none the wiser.
            if (!isAudioHealthy()) {
                await handleAudioInterruption('mic_interrupted');
                if (!isStreaming) return; // every rung failed; it stopped us cleanly
            }

            // Reconnected successfully.
            reconnectAttempts = 0;
            isReconnecting = false;
            setConnected(true);
            showToast('Reconnected!');
            updateServerSettings();
            return;
        } catch (e) {
            console.warn(`[reconnect] attempt ${reconnectAttempts} failed:`, e);
            // Bail out early once the server is confirmed gone.
            if (!(await isServerAlive())) break;
        }
    }

    isReconnecting = false;
    if (isStreaming) {
        // Decide the terminal state with a single probe: a reachable server means
        // the session is just stale (re-pair in place); an unreachable one means a
        // reload is needed (the cert changes on restart).
        const gone = !(await isServerAlive());
        returnToPairing(gone ? 'Server closed' : 'Session expired. Please pair again.', gone);
    }
}

// ── Audio Packet Sending ──────────────────────────────────────────────

function sendAudioPacket(packet) {
    if (!isStreaming || isMuted) return;

    // `packet` is the full wire frame from the worklet: [4 bytes seq (u32 LE)]
    // [PCM i16 LE samples], with the header bytes left empty. Stamp the sequence
    // number (wraps at 2^32) into the header and send the buffer as-is — no extra
    // allocation or copy.
    new DataView(packet).setUint32(0, sequenceNumber++, true);

    // Send via the active transport.
    try {
        if (datagramWriter) {
            const writer = datagramWriter;
            // WebTransport: unreliable datagram.
            writer.write(new Uint8Array(packet)).then(() => {
                packetsSent++;
            }).catch((err) => {
                // The transport is going away. Drop the writer so the worklet
                // stops flooding doomed writes, and recover once.
                if (datagramWriter === writer) {
                    console.warn('[transport] datagram write failed:', err);
                    datagramWriter = null;
                    if (isStreaming && !isReconnecting) handleUnexpectedDisconnect();
                }
            });
        } else if (ws && ws.readyState === WebSocket.OPEN) {
            // WebSocket: reliable binary frame.
            ws.send(packet);
            packetsSent++;
        }
    } catch (e) {
        // Silently drop — acceptable for real-time audio.
    }
}

// ── UI Helpers ────────────────────────────────────────────────────────

/** Update the VU meter (throttled to ~10 FPS, skipped in Eco Mode). Level is 0..100. */
function updateVu(level) {
    if (isPowerSaveActive) return;
    const now = Date.now();
    if (now - lastVuUpdateTime > 100) {
        vuBar.style.width = `${level}%`;
        vuLevel.textContent = `${Math.round(level)}%`;
        lastVuUpdateTime = now;
    }
}

function setConnected(connected) {
    if (isMuted) {
        statusBadge.className = 'status-badge muted';
        statusText.textContent = 'Muted';
    } else {
        statusBadge.className = `status-badge ${connected ? 'connected' : 'disconnected'}`;
        statusText.textContent = connected ? 'Streaming' : 'Idle';
    }
}

function updateStats() {
    // The single audio-health decision point. Rides on this existing tick, so it costs
    // no extra timer — and it now runs even while muted, because the worklet heartbeats.
    checkAudioHealth();

    // Eco Mode: suspend the full stats UI/polling to save power, but keep a
    // low-frequency liveness check (~every 3s) so a server shutdown is still
    // detected behind the black overlay. On iOS Safari the WebTransport close
    // event surfaces seconds late, so this HTTP check is the reliable signal.
    if (isPowerSaveActive) {
        if (isStreaming || sessionToken) {
            ecoHealthTicks++;
            if (ecoHealthTicks >= 3) {
                ecoHealthTicks = 0;
                checkServerHealth();
            }
        }
        return;
    }
    ecoHealthTicks = 0;

    statTransport.textContent = transportType;
    statPackets.textContent = packetsSent.toLocaleString();

    if (isStreaming && startTime > 0) {
        const elapsed = Math.floor((Date.now() - startTime) / 1000);
        const hours = Math.floor(elapsed / 3600);
        const min = Math.floor((elapsed % 3600) / 60);
        const sec = elapsed % 60;
        const pad = (n) => String(n).padStart(2, '0');

        statUptime.textContent = hours > 0
            ? `${pad(hours)}:${pad(min)}:${pad(sec)}`
            : `${pad(min)}:${pad(sec)}`;
    } else {
        statUptime.textContent = '—';
        statPing.textContent = '—';
        statBuffer.textContent = '—';
        statLoss.textContent = '—';
    }

    // Fetch server stats for packet loss, ping, and buffer depth display.
    if (isStreaming) {
        fetchServerStats();
    } else if (sessionToken) {
        // Idle on the main screen: check the server is still alive every 3s.
        healthCheckTicks++;
        if (healthCheckTicks >= 3) {
            healthCheckTicks = 0;
            checkServerHealth();
        }
    }
}

async function fetchServerStats() {
    if (isReconnecting) return;

    let resp;
    try {
        const fetchStartTime = performance.now();
        resp = await fetchWithTimeout('/api/stats', { headers: { 'X-Session-Token': sessionToken } });
        statPing.textContent = `${Math.round(performance.now() - fetchStartTime)} ms`;
    } catch (e) {
        // The HTTP poll is the fastest, most reliable shutdown signal on iOS
        // Safari (the WebTransport close surfaces seconds late). A network error
        // means the server is unreachable — confirm with one probe and leave
        // immediately if it is gone; tolerate an isolated transient hiccup.
        statPing.textContent = '—';
        statBuffer.textContent = '—';
        statLoss.textContent = '—';
        // If the probe succeeds the server is up and the stream is still fine,
        // so an isolated failure is silently tolerated.
        if (isStreaming && !isReconnecting && !(await isServerAlive())) {
            console.warn('[stats] server unreachable -> returning to pairing');
            returnToPairing('Server closed', true);
        }
        return;
    }

    // 401 = our session was invalidated (e.g. taken over by another device) while
    // the server itself is alive: re-pair in place (the cert is unchanged).
    if (resp.status === 401) {
        console.warn('[stats] session no longer valid -> returning to pairing');
        returnToPairing('Session expired. Please pair again.', false);
        return;
    }
    // Any other non-2xx (503) means the server is shutting down — definitive.
    if (!resp.ok) {
        console.warn('[stats] server is shutting down (HTTP', resp.status + ') -> returning to pairing');
        returnToPairing('Server closed', true);
        return;
    }

    const data = await resp.json();
    statLoss.textContent = data.loss_percent.toFixed(2) + '%';
    // buffer_ms is computed server-side from the actual capture rate (accurate for
    // non-48 kHz sources too).
    statBuffer.textContent = `${data.buffer_ms} ms`;

    // Surface audio-output-device health: the server keeps decoding into the ring
    // buffer while a lost device's stream rebuilds, so there would otherwise be
    // silence with no explanation. Toast on the transitions only (not every poll).
    if (data.audio_device_ok === false) {
        if (lastAudioOk !== false) showToast('Audio device lost — recovering…');
        lastAudioOk = false;
    } else if (data.audio_device_ok === true) {
        if (lastAudioOk === false) showToast('Audio device recovered');
        lastAudioOk = true;
    }

    // The server is alive but reports no active connection: our stream dropped
    // server-side. Recover it transparently (reconnect). Skip while a connection
    // attempt is still in progress (`isConnecting`) — during the initial connect
    // (or a slow WebTransport handshake before the WebSocket fallback) the server
    // legitimately reports `connected: false`, and triggering recovery here would
    // renew the session token mid-connect and invalidate the in-flight transport.
    if (isStreaming && !isReconnecting && !isConnecting && !data.connected) {
        console.warn('[stats] server reports no active connection -> recovering');
        handleUnexpectedDisconnect();
    }
}

async function checkServerHealth() {
    try {
        const resp = await fetchWithTimeout('/api/stats', { headers: { 'X-Session-Token': sessionToken } });
        // Session taken over (server alive): re-pair in place, no reload.
        if (resp.status === 401) {
            returnToPairing('Session expired. Please pair again.', false);
            return;
        }
        // 503 = shutting down — definitive.
        if (!resp.ok) {
            returnToPairing('Server closed', true);
        }
    } catch (e) {
        // One confirming probe before concluding the server is gone, so a single
        // transient idle blip doesn't force a reload (parity with the streaming path).
        if (!(await isServerAlive())) {
            console.warn('[health] idle health check failed and server is unreachable');
            returnToPairing('Server closed', true);
        }
    }
}

/**
 * Show the small update bar if the server's startup check reported a newer
 * release and the user hasn't already dismissed that exact version. The check
 * itself runs server-side, so the browser never contacts GitHub here.
 */
function maybeShowUpdateBanner() {
    if (!serverInfo || !serverInfo.update_available || !serverInfo.latest_version) return;
    if (localStorage.getItem('dismissedUpdate') === serverInfo.latest_version) return;
    updateText.textContent = `New version ${serverInfo.latest_version} available`;
    updateLink.href = serverInfo.releases_url || '#';
    updateBanner.hidden = false;
}

function showToast(message) {
    toast.textContent = message;
    toast.classList.add('show');
    setTimeout(() => toast.classList.remove('show'), 3000);
}

// ── Start ─────────────────────────────────────────────────────────────
init();
