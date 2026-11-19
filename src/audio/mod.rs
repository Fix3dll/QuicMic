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

/// Largest latency-recovery threshold, in milliseconds (the web UI slider's top).
/// It is also the ceiling the output stage enforces when recovery is set to Off
/// (0), so the buffer can never grow unbounded. Owned here because the output
/// stage enforces it; the API/CLI clamp uses it as `server::LATENCY_THRESHOLD_MAX_MS`.
pub const LATENCY_THRESHOLD_MAX_MS: u32 = 500;

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
