//! Output device selection and the cpal playback stream with its
//! Catmull-Rom cubic resampler and latency-recovery logic.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, FromSample, Sample, SampleFormat, Stream, StreamConfig};
use tracing::{error, info, warn};

use super::normalize_latency_threshold;
use super::ring_buffer::RingBuffer;

/// Buffer depth, in source samples, above which latency recovery trims the ring.
///
/// The threshold is normalized again here, defensively (the API and CLI already
/// do it), so the output stage never sees an out-of-range or legacy-Off (0)
/// value. Always strictly below the ring's capacity, so `len > limit` stays
/// reachable at every source rate.
fn recovery_limit_samples(threshold_ms: u32, active_rate: usize) -> usize {
    normalize_latency_threshold(threshold_ms) as usize * active_rate / 1000
}

/// Target output-buffer depth, in milliseconds. Used both to end the initial
/// prebuffering phase and as the depth that the latency-recovery hard-skip trims
/// down to. It is converted to a sample count at runtime using the active source
/// sample rate, so the cushion stays ~constant in time across 44.1k–192k sources
/// (a fixed sample count would otherwise mean ~30ms at 48k but only ~7.5ms at
/// 192k).
const PREBUFFER_MS: u32 = 30;

/// Source samples pulled from the ring per batch. Reading in batches amortizes
/// the ring's per-sample atomic synchronization (a real win on weak-memory-model
/// CPUs like ARM/Apple Silicon; on x86 the atomics are plain loads/stores).
const RESAMPLE_CHUNK: usize = 64;

/// Minimum spacing between two latency-recovery skips, so a buffer hovering
/// around the limit is not trimmed over and over (audible stutter).
const SKIP_COOLDOWN: Duration = Duration::from_secs(3);

// ---------------------------------------------------------------------------
// Output device management
// ---------------------------------------------------------------------------

/// List all available audio output devices and return their names.
pub fn list_output_devices() -> Vec<String> {
    let host = cpal::default_host();
    let mut names = Vec::new();

    match host.output_devices() {
        Ok(devices) => {
            for device in devices {
                if let Ok(desc) = device.description() {
                    names.push(desc.name().to_string());
                }
            }
        }
        Err(e) => error!("Failed to enumerate audio devices: {}", e),
    }

    names
}

#[cfg(target_os = "windows")]
const DEFAULT_DEVICE: &str = "CABLE Input";

#[cfg(target_os = "macos")]
const DEFAULT_DEVICE: &str = "BlackHole";

#[cfg(target_os = "linux")]
const DEFAULT_DEVICE: &str = "VirtualQuicMic";

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
const DEFAULT_DEVICE: &str = "CABLE Input";

/// Find an output device by name substring (case-insensitive).
/// Falls back to the platform-specific default virtual device if no explicit name is given.
pub fn find_device(requested: Option<&str>) -> anyhow::Result<Device> {
    let host = cpal::default_host();
    let target = requested.unwrap_or(DEFAULT_DEVICE);

    let device = host
        .output_devices()?
        .find(|d| {
            d.description()
                .map(|desc| desc.name().to_lowercase().contains(&target.to_lowercase()))
                .unwrap_or(false)
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Audio device '{}' not found. Available devices:\n{}",
                target,
                list_output_devices().join("\n  - ")
            )
        })?;

    let device_name = device
        .description()
        .map(|desc| desc.name().to_string())
        .unwrap_or_else(|_| "Unknown".to_string());
    info!(device = %device_name, "Selected audio output device");
    Ok(device)
}

// ---------------------------------------------------------------------------
// Catmull-Rom cubic resampler
// ---------------------------------------------------------------------------

/// Persistent state for the Catmull-Rom resampler, carried across successive
/// cpal output callbacks.
struct ResamplerState {
    /// Fractional read position within the current source interval (between
    /// `s1` and `s2`).
    frac: f64,
    /// 4-tap interpolation window. Output is interpolated between `s1` and `s2`,
    /// with `s0`/`s3` as the surrounding Catmull-Rom control points (`s3` is the
    /// one-sample look-ahead).
    s0: i16,
    s1: i16,
    s2: i16,
    s3: i16,
    /// Whether we are refilling the buffer to the prebuffer low-water mark after
    /// startup or an underrun.
    is_prebuffering: bool,
    /// Small batch of source samples drained from the ring (see `RESAMPLE_CHUNK`).
    chunk: [i16; RESAMPLE_CHUNK],
    /// Number of valid samples currently in `chunk`.
    chunk_len: usize,
    /// Read cursor into `chunk`.
    chunk_pos: usize,
}

impl ResamplerState {
    fn new() -> Self {
        Self {
            frac: 0.0,
            s0: 0,
            s1: 0,
            s2: 0,
            s3: 0,
            is_prebuffering: true,
            chunk: [0; RESAMPLE_CHUNK],
            chunk_len: 0,
            chunk_pos: 0,
        }
    }

    /// Zero the 4-tap window (used while prebuffering and on underrun).
    fn window_reset(&mut self) {
        self.s0 = 0;
        self.s1 = 0;
        self.s2 = 0;
        self.s3 = 0;
    }

    /// Pull the next source sample, refilling `chunk` from the ring in batches.
    /// Returns `None` on underrun (the ring is empty). Any samples left in
    /// `chunk` persist across callbacks, so none are ever dropped.
    fn next_source_sample(&mut self, ring: &RingBuffer) -> Option<i16> {
        if self.chunk_pos >= self.chunk_len {
            self.chunk_len = ring.pop(&mut self.chunk);
            self.chunk_pos = 0;
            if self.chunk_len == 0 {
                return None;
            }
        }
        let sample = self.chunk[self.chunk_pos];
        self.chunk_pos += 1;
        Some(sample)
    }
}

/// Catmull-Rom cubic interpolation between `p1` and `p2`, using `p0`/`p3` as the
/// surrounding control points, evaluated at `t` in `[0, 1]`. Higher quality than
/// linear interpolation (flatter passband, better image/alias rejection) for a
/// few extra multiplies per output sample. The result is clamped to the i16
/// range because a cubic can overshoot beyond the control points.
fn catmull_rom(p0: i16, p1: i16, p2: i16, p3: i16, t: f64) -> i16 {
    let (p0, p1, p2, p3) = (p0 as f64, p1 as f64, p2 as f64, p3 as f64);
    let a = -0.5 * p0 + 1.5 * p1 - 1.5 * p2 + 0.5 * p3;
    let b = p0 - 2.5 * p1 + 2.0 * p2 - 0.5 * p3;
    let c = -0.5 * p0 + 0.5 * p2;
    let d = p1;
    let v = ((a * t + b) * t + c) * t + d;
    v.clamp(i16::MIN as f64, i16::MAX as f64) as i16
}

/// Core resampler: reads from the ring buffer, applies Catmull-Rom cubic
/// interpolation to convert from the source sample rate to the output device's
/// native rate, and duplicates mono to all output channels.
///
/// `prebuffer_samples` is the low-water mark (in source samples) that gates the
/// start of playback and recovery from an underrun; the caller derives it from
/// `PREBUFFER_MS` and the active source rate so the cushion is rate-aware.
fn write_data<T>(
    data: &mut [T],
    ring: &RingBuffer,
    channels: usize,
    ratio: f64,
    prebuffer_samples: usize,
    state: &mut ResamplerState,
) where
    T: Sample + FromSample<i16>,
{
    // Fast path for the common idle / no-client case: while still prebuffering
    // with too little data, the whole output is silence — fill it in one shot
    // instead of running the per-frame interpolation loop (keeps idle CPU low).
    if state.is_prebuffering && ring.len() < prebuffer_samples {
        data.fill(T::from_sample(0i16));
        return;
    }

    for frame in data.chunks_mut(channels) {
        state.frac += ratio;

        while state.frac >= 1.0 {
            state.frac -= 1.0;

            if state.is_prebuffering {
                if ring.len() >= prebuffer_samples {
                    state.is_prebuffering = false;
                } else {
                    state.window_reset();
                    continue;
                }
            }

            // Advance the 4-tap window by one source sample.
            state.s0 = state.s1;
            state.s1 = state.s2;
            state.s2 = state.s3;
            match state.next_source_sample(ring) {
                Some(sample) => state.s3 = sample,
                None => {
                    // Buffer underflow — re-enter the prebuffering state.
                    state.is_prebuffering = true;
                    state.window_reset();
                }
            }
        }

        // Cubic interpolation across the window, between s1 and s2.
        let sample_t = T::from_sample(catmull_rom(
            state.s0, state.s1, state.s2, state.s3, state.frac,
        ));
        for ch in frame.iter_mut() {
            *ch = sample_t;
        }
    }
}

/// Open and start a cpal output stream for the named device (or the platform
/// default virtual device). The stream reads from `ring`, resamples to the
/// device's native rate, and duplicates mono to all channels.
///
/// `err_tx` is signalled from the stream's error callback when the device fails
/// (e.g. it is disabled or removed), so the supervisor can rebuild the stream.
/// The `source_sample_rate` atomic lets the resampler ratio follow the client's
/// reported capture rate. Returns the Stream handle — it must be kept alive for
/// playback to continue.
fn open_output_stream(
    device_name: Option<&str>,
    ring: &Arc<RingBuffer>,
    source_sample_rate: &Arc<AtomicU32>,
    latency_threshold: &Arc<AtomicU32>,
    err_tx: mpsc::Sender<()>,
) -> anyhow::Result<Stream> {
    let device = find_device(device_name)?;
    let default_config = device.default_output_config()?;
    let sample_format = default_config.sample_format();
    let config: StreamConfig = default_config.into();

    info!(
        sample_rate = config.sample_rate,
        channels = config.channels,
        format = ?sample_format,
        "Starting audio output stream (system default)"
    );

    // Guard against a (pathological) zero-channel device: chunks_mut(0) panics.
    let channels = (config.channels as usize).max(1);
    let target_rate = config.sample_rate as f64;

    // Macro to eliminate code duplication across sample format branches.
    // Each branch is identical except for the concrete sample type.
    macro_rules! build_stream {
        ($T:ty) => {{
            let ring = ring.clone();
            let source_rate = source_sample_rate.clone();
            let threshold = latency_threshold.clone();
            let err_tx = err_tx.clone();
            let mut resampler = ResamplerState::new();
            // No skip yet on this stream. The cooldown only spaces out *repeated*
            // skips; it must not delay the first one. Starting it at build time
            // would, after the supervisor rebuilds a lost device, play whatever
            // piled up in the ring during the outage (up to the ring capacity)
            // for a full cooldown before trimming it.
            let mut last_skip: Option<std::time::Instant> = None;
            device.build_output_stream(
                config,
                move |data: &mut [$T], _: &cpal::OutputCallbackInfo| {
                    let active_rate = source_rate.load(Ordering::Relaxed).max(1) as usize;
                    // Prebuffer / latency-recovery target depth in source samples,
                    // derived from PREBUFFER_MS so the cushion is ~constant in time
                    // regardless of the source sample rate.
                    let prebuffer_samples = (PREBUFFER_MS as usize * active_rate) / 1000;

                    // Latency Recovery (Hard Skip). Always armed; the threshold is a
                    // 50-500ms range with no "Off" (see LATENCY_THRESHOLD_MIN_MS).
                    let threshold_ms = threshold.load(Ordering::Relaxed);
                    {
                        let threshold_samples = recovery_limit_samples(threshold_ms, active_rate);
                        let current_len = ring.len();

                        // Only read the clock once the buffer has actually grown
                        // past the threshold (the rare case), not every callback.
                        if current_len > threshold_samples {
                            let now = std::time::Instant::now();
                            if last_skip.is_none_or(|t| now.duration_since(t) >= SKIP_COOLDOWN) {
                                let to_discard = current_len.saturating_sub(prebuffer_samples); // trim down to the prebuffer depth
                                if to_discard > 0 {
                                    let mut discard_buf = [0i16; 256];
                                    let mut discarded = 0;
                                    while discarded < to_discard {
                                        let chunk = (to_discard - discarded).min(discard_buf.len());
                                        let popped = ring.pop(&mut discard_buf[..chunk]);
                                        if popped == 0 {
                                            break;
                                        }
                                        discarded += popped;
                                    }
                                    if discarded > 0 {
                                        let before_ms = (current_len * 1000) / active_rate;
                                        let after_ms =
                                            ((current_len.saturating_sub(discarded)) * 1000) / active_rate;
                                        warn!(
                                            "Latency recovery: hard skipped {} samples ({}ms -> {}ms) to catch up (threshold: {}ms)",
                                            discarded, before_ms, after_ms,
                                            normalize_latency_threshold(threshold_ms)
                                        );
                                    }
                                }
                                last_skip = Some(now);
                            }
                        }
                    }

                    let ratio = active_rate as f64 / target_rate;
                    write_data(data, &ring, channels, ratio, prebuffer_samples, &mut resampler);
                },
                move |err| {
                    error!("Audio output stream error: {}", err);
                    // Signal the supervisor to rebuild; ignore if it has gone away.
                    let _ = err_tx.send(());
                },
                None,
            )?
        }};
    }

    let stream = match sample_format {
        SampleFormat::F32 => build_stream!(f32),
        SampleFormat::I16 => build_stream!(i16),
        SampleFormat::U16 => build_stream!(u16),
        _ => {
            return Err(anyhow::anyhow!(
                "Unsupported sample format: {:?}",
                sample_format
            ))
        }
    };

    stream.play()?;
    Ok(stream)
}

/// Spawn the audio output supervisor.
///
/// It owns the cpal stream — which is `!Send`, so it must live on a single thread
/// — and rebuilds it whenever the device fails, retrying until the device is back.
/// A virtual device that is disabled or removed mid-session, then restored, thus
/// recovers automatically with no restart. Blocks until the initial stream is
/// built (or fails), so a bad `--device` name remains a fatal startup error.
pub fn spawn_output_supervisor(
    device_name: Option<String>,
    ring: Arc<RingBuffer>,
    source_sample_rate: Arc<AtomicU32>,
    latency_threshold: Arc<AtomicU32>,
    device_ok: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let (init_tx, init_rx) = mpsc::channel::<anyhow::Result<()>>();

    std::thread::Builder::new()
        .name("audio-output".into())
        .spawn(move || {
            // The cpal error callback signals here when the active stream dies.
            let (err_tx, err_rx) = mpsc::channel::<()>();

            // Initial build: report success/failure so a bad device name stays a
            // fatal startup error (as before).
            let mut stream = match open_output_stream(
                device_name.as_deref(),
                &ring,
                &source_sample_rate,
                &latency_threshold,
                err_tx.clone(),
            ) {
                Ok(stream) => {
                    device_ok.store(true, Ordering::SeqCst);
                    let _ = init_tx.send(Ok(()));
                    stream
                }
                Err(e) => {
                    let _ = init_tx.send(Err(e));
                    return;
                }
            };

            // Supervise: rebuild on any device failure.
            loop {
                // Block until the error callback reports the stream is dead.
                if err_rx.recv().is_err() {
                    return; // all senders gone — should not happen; exit quietly.
                }
                device_ok.store(false, Ordering::SeqCst);
                warn!("Audio output device lost; rebuilding the output stream...");
                drop(stream);
                while err_rx.try_recv().is_ok() {} // coalesce repeated signals

                let mut attempts: u32 = 0;
                stream = loop {
                    std::thread::sleep(Duration::from_secs(1));
                    attempts += 1;
                    match open_output_stream(
                        device_name.as_deref(),
                        &ring,
                        &source_sample_rate,
                        &latency_threshold,
                        err_tx.clone(),
                    ) {
                        Ok(stream) => break stream,
                        // Don't log every second. The loss was already warned once
                        // above; emit only a sparse heartbeat (~every 30s) so a long
                        // outage still shows it is being retried, without spamming.
                        Err(_) => {
                            if attempts.is_multiple_of(30) {
                                warn!(attempts, "Audio device still unavailable; retrying");
                            }
                        }
                    }
                };
                while err_rx.try_recv().is_ok() {} // drop signals from the rebuild
                device_ok.store(true, Ordering::SeqCst);
                info!(attempts, "Audio output stream rebuilt; playback resumed");
            }
        })?;

    // Wait for the initial build so a startup failure is fatal, as before.
    match init_rx.recv() {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!(
            "Audio output thread exited before initialization"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{recovery_limit_samples, write_data, ResamplerState, PREBUFFER_MS};
    use crate::audio::{
        normalize_latency_threshold, RingBuffer, LATENCY_THRESHOLD_MAX_MS,
        LATENCY_THRESHOLD_MIN_MS, MAX_SAMPLE_RATE, RING_CAPACITY_SAMPLES,
    };

    /// The bug this guards against: the ring used to be sized at 48 kHz, so at
    /// 96/192 kHz a threshold could exceed what the ring can hold and recovery
    /// silently never fired. For every accepted rate and every possible stored
    /// value (legacy 0 included), the limit must be strictly below the capacity,
    /// i.e. the `len > limit` trigger must stay reachable.
    #[test]
    fn recovery_limit_is_reachable_at_every_rate_and_threshold() {
        let ring = RingBuffer::new(RING_CAPACITY_SAMPLES);
        for rate in [
            8_000,
            44_100,
            48_000,
            88_200,
            96_000,
            176_400,
            MAX_SAMPLE_RATE as usize,
        ] {
            for ms in (0..=LATENCY_THRESHOLD_MAX_MS).step_by(10) {
                let limit = recovery_limit_samples(ms, rate);
                assert!(
                    limit < ring.capacity(),
                    "unreachable recovery: {ms}ms at {rate} Hz -> limit {limit} >= capacity {}",
                    ring.capacity()
                );
            }
        }
    }

    #[test]
    fn recovery_limit_is_time_based() {
        // The same setting means the same time at any rate.
        assert_eq!(recovery_limit_samples(150, 48_000), 7_200);
        assert_eq!(recovery_limit_samples(150, 192_000), 28_800);
        assert_eq!(recovery_limit_samples(150, 44_100), 6_615);
    }

    #[test]
    fn threshold_normalization_maps_legacy_off_and_clamps() {
        // Legacy "Off" means "intervene as little as possible": the maximum.
        assert_eq!(normalize_latency_threshold(0), LATENCY_THRESHOLD_MAX_MS);
        // Below the minimum (the old slider allowed 10-40ms) is raised to it.
        assert_eq!(normalize_latency_threshold(10), LATENCY_THRESHOLD_MIN_MS);
        assert_eq!(normalize_latency_threshold(49), LATENCY_THRESHOLD_MIN_MS);
        // In range passes through; above the maximum is clamped to it.
        assert_eq!(normalize_latency_threshold(150), 150);
        assert_eq!(
            normalize_latency_threshold(10_000),
            LATENCY_THRESHOLD_MAX_MS
        );
    }

    #[test]
    fn skip_target_is_below_the_minimum_limit() {
        // A skip trims down to the prebuffer depth. That target must sit below even
        // the smallest limit at every rate, or a skip could not lower latency (and
        // the limit would be crossed in steady state, ticking every cooldown).
        for rate in [44_100, 48_000, 96_000, MAX_SAMPLE_RATE as usize] {
            let prebuffer = PREBUFFER_MS as usize * rate / 1000;
            assert!(prebuffer < recovery_limit_samples(LATENCY_THRESHOLD_MIN_MS, rate));
        }
    }

    #[test]
    fn resampler_stays_silent_while_prebuffering() {
        let ring = RingBuffer::new(4096);
        // Below PREBUFFER_SAMPLES: playback must not start yet.
        ring.push(&[100i16; 500]);

        let mut state = ResamplerState::new();
        let mut out = [0i16; 480];
        write_data(&mut out, &ring, 1, 1.0, 1440, &mut state);

        assert!(
            out.iter().all(|&s| s == 0),
            "must output silence until prebuffered"
        );
        assert!(state.is_prebuffering);
    }

    #[test]
    fn resampler_unity_ratio_passthrough_with_delay() {
        let ring = RingBuffer::new(4096);
        // Above the prebuffer low-water mark so playback starts on this callback.
        let input: Vec<i16> = (0..2000).map(|i| (i % 97) as i16).collect();
        ring.push(&input);

        let mut state = ResamplerState::new();
        let mut out = [0i16; 480];
        write_data(&mut out, &ring, 1, 1.0, 1440, &mut state);

        // At ratio 1.0 the cubic interpolation evaluates at t=0 every frame, so it
        // degenerates to a pure passthrough with a 2-sample group delay (from the
        // one-sample look-ahead window): out[0]=out[1]=0, then out[n]=input[n-2].
        assert_eq!(out[0], 0);
        assert_eq!(out[1], 0);
        for n in 2..out.len() {
            assert_eq!(out[n], input[n - 2], "mismatch at output frame {n}");
        }
        assert!(!state.is_prebuffering);
    }

    #[test]
    fn resampler_downsamples_at_ratio_two() {
        let ring = RingBuffer::new(4096);
        let input: Vec<i16> = (0..2000).map(|i| (i % 97) as i16).collect();
        ring.push(&input);

        let mut state = ResamplerState::new();
        let mut out = [0i16; 480];
        // ratio 2.0: the source is consumed twice as fast as the output is
        // produced (e.g. a 96k source feeding a 48k device), so each output frame
        // advances the window by two source samples.
        write_data(&mut out, &ring, 1, 2.0, 1440, &mut state);

        // At t=0 every frame the cubic degenerates to s1. After the 2-sample
        // startup delay, out[n] is the source decimated by two: out[n] = input[2n-1].
        assert_eq!(out[0], 0);
        for n in 1..out.len() {
            assert_eq!(out[n], input[2 * n - 1], "mismatch at output frame {n}");
        }
        assert!(!state.is_prebuffering);
    }

    #[test]
    fn resampler_duplicates_mono_to_all_channels() {
        let ring = RingBuffer::new(4096);
        ring.push(&(0..2000).map(|i| (i % 50) as i16).collect::<Vec<_>>());

        let mut state = ResamplerState::new();
        let channels = 2;
        let mut out = [0i16; 480 * 2];
        write_data(&mut out, &ring, channels, 1.0, 1440, &mut state);

        // Every stereo frame must carry identical samples on both channels.
        for frame in out.chunks_exact(channels) {
            assert_eq!(frame[0], frame[1]);
        }
    }
}
