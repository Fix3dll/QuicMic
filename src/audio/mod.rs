//! Audio subsystem: the lock-free ring buffer, incoming-packet decoding into the
//! ring, and the output device / resampler. Input DSP (noise gate and gain) runs
//! client-side in the AudioWorklet, so the server is a pure passthrough on the
//! receive hot path — it only decodes bytes and hands them to the output stage.
//!
//! Submodules keep each concern — and its tests — self-contained. The public
//! surface used by the rest of the crate is re-exported here.

mod output;
mod processor;
mod ring_buffer;

pub use output::{list_output_devices, spawn_output_supervisor};
pub use processor::{decode_into_ring, MAX_SAMPLES_PER_PACKET};
pub use ring_buffer::RingBuffer;

/// Maximum sample rate accepted from a client; anything higher is ignored.
pub const MAX_SAMPLE_RATE: u32 = 192_000;

/// Latency-recovery threshold range, in milliseconds (the web UI slider's ends).
/// Owned here because the output stage enforces it; the API and CLI normalize
/// through `normalize_latency_threshold`.
///
/// There is deliberately no "Off": a finite ring has to shed audio somewhere once
/// it is full, so "off" could only ever mean a hidden, rate-dependent ceiling
/// reached by dropping (the old behaviour). The maximum is the explicit form of
/// it, a clean skip only in extreme cases. The minimum stays above the steady
/// buffer depth (the 30ms prebuffer plus packet/callback jitter, ~30-45ms): a
/// lower threshold is crossed in normal operation and would trim a few ms every
/// cooldown, an audible periodic tick.
pub const LATENCY_THRESHOLD_MIN_MS: u32 = 50;
pub const LATENCY_THRESHOLD_MAX_MS: u32 = 500;

/// Map any stored/requested threshold onto the valid range. `0` is the legacy
/// "Off" (saved settings, `--latency-threshold 0` in scripts) and maps to the
/// maximum, its closest meaning (intervene as little as possible), not the
/// minimum. Anything else is clamped into range.
pub fn normalize_latency_threshold(ms: u32) -> u32 {
    match ms {
        0 => LATENCY_THRESHOLD_MAX_MS,
        t => t.clamp(LATENCY_THRESHOLD_MIN_MS, LATENCY_THRESHOLD_MAX_MS),
    }
}

/// Ring-buffer capacity, in milliseconds of audio at `MAX_SAMPLE_RATE`. The ring
/// is allocated once at startup and cannot be resized while the network task and
/// the audio thread share it, so it is sized for the worst case; every limit is
/// then applied in *time* (ms x the active source rate), and each one means the
/// same at 44.1k and at 192k.
///
/// It must be strictly larger than `LATENCY_THRESHOLD_MAX_MS`: recovery fires on
/// `len > limit`, so a ring that can only hold exactly the limit never triggers it
/// (the old ring, sized at 48 kHz, broke recovery this way at 96/192 kHz). The
/// extra 100ms absorbs a Wi-Fi burst, or growth during the skip cooldown, without
/// dropping audio. Cost: ~230 KB.
pub const RING_CAPACITY_MS: u32 = LATENCY_THRESHOLD_MAX_MS + 100;
pub const RING_CAPACITY_SAMPLES: usize =
    MAX_SAMPLE_RATE as usize * RING_CAPACITY_MS as usize / 1000;

// Guards the invariant above if either constant is ever edited independently.
const _: () = assert!(RING_CAPACITY_MS > LATENCY_THRESHOLD_MAX_MS);
