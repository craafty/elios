//! A microphone somewhere else: while a Bluetooth headset's voice link is
//! up, card 0's capture stream records the headset's microphone instead
//! of the card's own inputs, so programs (Firefox, PulseAudio, `arecord`)
//! get it without knowing.
//!
//! The card keeps recording as usual, which keeps its clock and period
//! events: as the stream's position moves past newly recorded frames
//! ([`pcm::Pcm`]'s update, before any program can read them), they are
//! overwritten with the headset's samples, resampled from its rate (8 kHz
//! mono for a voice link) to the stream's. The two clocks differ a little,
//! so a small backlog is kept: playing starts once 30 ms have come, a
//! backlog past 150 ms is trimmed, and running dry gives silence until
//! the backlog builds up again.
//!
//! [`pcm::Pcm`]: super::pcm::Pcm

use alloc::collections::VecDeque;

use crate::sync::SpinLock;

/// Backlog to build before playing, and the most kept, in milliseconds.
const PRIME_MS: usize = 30;
const MOST_MS: usize = 150;

struct Remote {
    /// The source's rate, or 0 when card 0 records its own inputs.
    rate: u32,
    samples: VecDeque<i16>,
    primed: bool,
    /// Where between `last` and the next sample the stream is, in 1/2^32.
    position: u64,
    last: i16,
    /// Frames of silence given because the source ran dry.
    dry: u64,
}

static REMOTE: SpinLock<Remote> =
    SpinLock::new(Remote { rate: 0, samples: VecDeque::new(), primed: false, position: 0, last: 0, dry: 0 });

/// Card 0 records from a source of mono samples at `rate` (`Some`), or
/// from its own inputs again (`None`).
pub fn record_from(rate: Option<u32>) {
    let mut remote = REMOTE.lock();
    if remote.rate != 0 {
        // How often it ran dry says whether the two clocks got along.
        info!("sound: the headset's microphone ran dry for {} frames", remote.dry);
    }
    remote.rate = rate.unwrap_or(0);
    remote.samples.clear();
    remote.primed = false;
    remote.position = 0;
    remote.last = 0;
    remote.dry = 0;
    info!("sound: microphone: {}", if rate.is_some() { "a headset's" } else { "the card's own" });
}

/// Samples from the source.
pub fn recorded(samples: &[i16]) {
    let mut remote = REMOTE.lock();
    if remote.rate == 0 {
        return;
    }
    remote.samples.extend(samples);
    let most = remote.rate as usize * MOST_MS / 1000;
    if remote.samples.len() > most {
        let extra = remote.samples.len() - most;
        remote.samples.drain(..extra);
    }
}

/// Fills `out` (16-bit stereo frames at `rate`) from the source; returns
/// false, leaving it alone, when there is no source.
pub(super) fn fill(rate: u32, out: &mut [u8]) -> bool {
    let mut remote = REMOTE.lock();
    if remote.rate == 0 || rate == 0 {
        return false;
    }
    if !remote.primed && remote.samples.len() >= remote.rate as usize * PRIME_MS / 1000 {
        remote.primed = true;
    }
    let step = (u64::from(remote.rate) << 32) / u64::from(rate);
    for frame in out.as_chunks_mut::<4>().0 {
        let sample = match remote.samples.front() {
            Some(&next) if remote.primed => {
                let (a, b) = (i64::from(remote.last), i64::from(next));
                let sample = (a + (((b - a) * (remote.position >> 16) as i64) >> 16)) as i16;
                remote.position += step;
                while remote.position >= 1 << 32 {
                    remote.position -= 1 << 32;
                    remote.last = remote.samples.pop_front().unwrap_or(remote.last);
                }
                sample
            }
            _ => {
                remote.primed = false;
                remote.dry += 1;
                0
            }
        };
        let bytes = sample.to_le_bytes();
        *frame = [bytes[0], bytes[1], bytes[0], bytes[1]];
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    crate::ktest! {
        fn remote_microphone_is_resampled_after_a_backlog() {
            // Not while another test (or a headset) might use it.
            if REMOTE.lock().rate != 0 {
                return;
            }
            let mut out = vec![0x55u8; 4 * 48];
            assert!(!fill(48000, &mut out));
            record_from(Some(8000));
            // Too little to start: silence.
            recorded(&[1000; 100]);
            assert!(fill(48000, &mut out));
            assert!(out.iter().all(|&b| b == 0));
            // 30 ms: plays, six frames a sample, both channels.
            recorded(&[1000; 200]);
            let mut out = vec![0u8; 4 * 480];
            assert!(fill(48000, &mut out));
            let frames: vec::Vec<[i16; 2]> = out
                .as_chunks::<4>()
                .0
                .iter()
                .map(|f| [i16::from_le_bytes([f[0], f[1]]), i16::from_le_bytes([f[2], f[3]])])
                .collect();
            assert!(frames[12..].iter().all(|&f| f == [1000, 1000]), "{:?}", &frames[..16]);
            // 80 samples used (79 and nearly all of one more: the step
            // rounds down).
            let left = REMOTE.lock().samples.len();
            assert!(left == 220 || left == 221, "{left}");
            record_from(None);
            assert!(!fill(48000, &mut out));
        }
    }
}
