/**
 * AudioWorklet processor for low-latency microphone capture.
 *
 * Runs on the dedicated audio thread — no main-thread blocking. Accumulates
 * 128-sample frames from the Web Audio API into 480-sample packets (~10ms at
 * 48kHz) and sends them as Int16 PCM via the MessagePort.
 *
 * The noise gate runs here, on the audio thread, mirroring the server gate: only
 * packets that pass (or are within the hold window) are posted, so the radio
 * idles during silence. An onset look-ahead prepends the last gated packet when
 * the gate opens, so a word's soft attack isn't clipped. While gated, a throttled
 * `{ level }` message still drives the VU meter to zero. While muted, nothing is
 * processed or posted at all.
 *
 * Port protocol:
 *   main -> worklet: { type: 'gate', threshold }  (linear amplitude, 0 = off)
 *                    { type: 'gain', value }       (linear output multiplier)
 *                    { type: 'mute', muted }
 *   worklet -> main: { frame: ArrayBuffer, level, clip } (gate open)
 *                    { level }                            (gated, throttled)
 *
 * `level` is the post-gain VU level (so the meter follows the gain slider);
 * `clip` is true when the gain pushed at least one sample past full scale and it
 * had to be cut, which is what drives the red clipping flash on the mic ring.
 *
 * `frame` is the full wire packet: HEADER_BYTES of empty space (the main thread
 * stamps the sequence number there) followed by the Int16 PCM. Building it here
 * and transferring it lets the main thread send it as-is — no per-packet
 * allocation or copy on the main thread.
 */

// Fixed packet *size* (not duration): 480 samples = 964 bytes on the wire at any
// sample rate, which fits a single datagram's MTU. This must stay <= the server's
// MAX_SAMPLES_PER_PACKET (and WebTransport's MAX_DATAGRAM_SIZE), which are sized to
// match. It works out to ~10ms at 48kHz / ~11ms at 44.1kHz — the rates browsers
// actually capture at; a higher-rate source just yields more packets per second,
// not larger packets.
const SAMPLES_PER_PACKET = 480;
// Wire-packet header: 4 bytes for the u32 LE sequence number, written by the main
// thread. Must stay a multiple of 2 so the PCM `Int16Array` view that starts right
// after it is correctly aligned (an odd offset throws a RangeError).
const HEADER_BYTES = 4;
const PACKET_BYTES = HEADER_BYTES + SAMPLES_PER_PACKET * 2;
const HOLD_SECONDS = 0.25;         // keep the gate open this long after the last loud frame
const SILENCE_LEVEL_EVERY = 5;     // while gated, emit a VU level update every Nth packet (~50ms)
// While muted we skip all DSP, but still emit a sparse heartbeat (~1/s — the cadence
// of the main thread's health tick that consumes it). Without this, muting posts
// nothing at all and blinds the health check completely: a dead capture graph would
// go unnoticed until the user unmuted and found silence.
const MUTED_HEARTBEAT_EVERY = 100;

class MicProcessor extends AudioWorkletProcessor {
    constructor() {
        super();
        this.buffer = new Float32Array(SAMPLES_PER_PACKET);
        this.writePos = 0;

        // Noise gate (mirrors the server gate). `perSampleSq` is the per-sample
        // squared amplitude threshold; the gate opens when packet energy reaches
        // it and holds open for HOLD_SECONDS. 0 disables the gate (passthrough).
        this.perSampleSq = 0;
        this.lastActive = -1e9;
        this.wasOpen = false;
        this.silenceCount = 0;
        // Most recent gated packet (a full wire frame), retained for the onset look-ahead.
        this.prevFrame = null;
        this.prevLevel = 0;
        this.muted = false;
        this.mutedTicks = 0; // heartbeat counter while muted (see MUTED_HEARTBEAT_EVERY)
        this.gain = 1; // linear output gain, applied before Int16 conversion

        this.port.onmessage = (e) => {
            const d = e.data;
            if (!d) return;
            if (d.type === 'gate') {
                const linear = d.threshold || 0;
                this.perSampleSq = linear > 0 ? Math.pow(linear * 32768, 2) : 0;
            } else if (d.type === 'gain') {
                this.gain = typeof d.value === 'number' && d.value > 0 ? d.value : 1;
            } else if (d.type === 'mute') {
                this.muted = !!d.muted;
                // Reset gate state so neither muting nor unmuting leaks a stale
                // onset packet or hold window across the boundary.
                this.prevFrame = null;
                this.wasOpen = false;
                this.lastActive = -1e9;
            }
        };
    }

    process(inputs) {
        const input = inputs[0];
        if (!input || !input[0]) return true;

        const channelData = input[0]; // Mono channel

        for (let i = 0; i < channelData.length; i++) {
            this.buffer[this.writePos++] = channelData[i];

            if (this.writePos >= SAMPLES_PER_PACKET) {
                this.writePos = 0;
                if (this.muted) {
                    // Muted: skip all DSP (that is the whole point), but keep a sparse
                    // heartbeat so the main thread can still tell "alive but muted"
                    // apart from "the capture graph has died".
                    if (++this.mutedTicks >= MUTED_HEARTBEAT_EVERY) {
                        this.mutedTicks = 0;
                        this.port.postMessage({ level: 0 });
                    }
                    continue;
                }

                // Build the full wire frame: a leading HEADER_BYTES gap (filled with
                // the sequence number on the main thread) followed by the PCM, so the
                // main thread can send it without re-allocating or copying. The PCM
                // view starts right after the header.
                const frame = new ArrayBuffer(PACKET_BYTES);
                const pcm = new Int16Array(frame, HEADER_BYTES, SAMPLES_PER_PACKET);
                // Convert Float32 [-1, 1] to Int16 and accumulate energy in one pass.
                let sumSq = 0;
                let outSumSq = 0;
                let clipped = false;
                for (let j = 0; j < SAMPLES_PER_PACKET; j++) {
                    const raw = Math.max(-1, Math.min(1, this.buffer[j]));
                    // The gate decides on the raw (pre-gain) energy, matching the old
                    // server-side semantics, so its threshold is independent of gain.
                    const rawInt = raw < 0 ? raw * 0x8000 : raw * 0x7FFF;
                    sumSq += rawInt * rawInt;
                    // The output carries the gain, applied in the float domain
                    // (no double quantization) and clamped to [-1, 1]. A sample that
                    // needs the clamp is a hard clip: audible distortion.
                    const amplified = raw * this.gain;
                    if (amplified > 1 || amplified < -1) clipped = true;
                    const g = Math.max(-1, Math.min(1, amplified));
                    outSumSq += g * g;
                    pcm[j] = g < 0 ? g * 0x8000 : g * 0x7FFF;
                }

                // Linear VU percentage of what is actually sent (post-gain), so the
                // meter visibly follows the gain slider. Same scale as before.
                const rms = Math.sqrt(outSumSq / SAMPLES_PER_PACKET);
                const level = Math.max(0, Math.min(100, rms * 300));

                // Gate decision: a threshold of 0 means the gate is off.
                const gateOff = this.perSampleSq <= 0;
                if (gateOff || sumSq >= this.perSampleSq * SAMPLES_PER_PACKET) {
                    this.lastActive = currentTime;
                }
                const open = gateOff || (currentTime - this.lastActive) < HOLD_SECONDS;

                if (open) {
                    // Onset look-ahead: on a closed -> open edge, prepend the last
                    // gated packet so a word's soft attack is not clipped.
                    if (!this.wasOpen && this.prevFrame) {
                        this.port.postMessage(
                            { frame: this.prevFrame, level: this.prevLevel },
                            [this.prevFrame]
                        );
                        this.prevFrame = null;
                    }
                    this.silenceCount = 0;
                    this.port.postMessage({ frame, level, clip: clipped }, [frame]);
                } else {
                    // Gated: retain this packet as the potential onset for the next
                    // open edge, and drip a throttled level so the VU falls to zero.
                    this.prevFrame = frame;
                    this.prevLevel = level;
                    if (++this.silenceCount >= SILENCE_LEVEL_EVERY) {
                        this.silenceCount = 0;
                        this.port.postMessage({ level });
                    }
                }
                this.wasOpen = open;
            }
        }

        return true;
    }
}

registerProcessor('mic-processor', MicProcessor);
