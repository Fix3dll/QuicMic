//! Incoming-packet decoding: parse little-endian i16 PCM and push it to the ring.
//!
//! Audio *input* processing (noise gate and gain) runs client-side in the
//! AudioWorklet (`web/worklet.js`), so the server is a pure passthrough on the
//! hot path — it just decodes the bytes and hands them to the output stage.

use super::ring_buffer::RingBuffer;

/// Maximum samples per packet (480 = 10ms at 48kHz).
pub const MAX_SAMPLES_PER_PACKET: usize = 480;

/// Decode little-endian i16 PCM from `pcm_bytes` and push it into `ring`.
///
/// `MAX_SAMPLES_PER_PACKET` caps how much a single frame can contribute, so a
/// malformed or oversize frame can never write more than one packet's worth (the
/// WebTransport path also rejects oversize datagrams up front). A well-formed
/// frame is always within the cap. A trailing odd byte, if any, is ignored by
/// `as_chunks`.
pub fn decode_into_ring(pcm_bytes: &[u8], ring: &RingBuffer) {
    let mut samples = [0i16; MAX_SAMPLES_PER_PACKET];
    let mut count = 0;
    let (chunks, _) = pcm_bytes.as_chunks::<2>();
    for chunk in chunks.iter().take(MAX_SAMPLES_PER_PACKET) {
        samples[count] = i16::from_le_bytes(*chunk);
        count += 1;
    }
    if count > 0 {
        ring.push(&samples[..count]);
    }
}

#[cfg(test)]
mod tests {
    use super::{decode_into_ring, MAX_SAMPLES_PER_PACKET};
    use crate::audio::RingBuffer;

    #[test]
    fn decodes_le_i16_into_ring() {
        let ring = RingBuffer::new(64);
        // Two little-endian i16 samples: 1 and -1.
        let bytes = [0x01, 0x00, 0xff, 0xff];
        decode_into_ring(&bytes, &ring);
        assert_eq!(ring.len(), 2);
        let mut out = [0i16; 2];
        ring.pop(&mut out);
        assert_eq!(out, [1, -1]);
    }

    #[test]
    fn caps_at_max_samples_per_packet() {
        let ring = RingBuffer::new(4096);
        let bytes = vec![0u8; (MAX_SAMPLES_PER_PACKET + 50) * 2];
        decode_into_ring(&bytes, &ring);
        assert_eq!(ring.len(), MAX_SAMPLES_PER_PACKET);
    }
}
