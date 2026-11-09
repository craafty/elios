//! Voice links (SCO): a headset's microphone and earpiece during a call.
//!
//! Once the hands-free connection is up (`hfp.rs`), the stack asks the
//! controller for a synchronous connection to the headset, a link with
//! slots reserved every few milliseconds whichever way the radio is busy.
//! It carries 8 kHz mono voice; the controller turns that into CVSD on
//! the air, and over HCI it's plain 16-bit samples ("voice setting"
//! 0x0060). Over USB those go on the controller's second interface, whose
//! alternate settings each give its isochronous endpoints a packet size
//! for one, two or three links (the transport picks one when a link
//! opens).
//!
//! The samples don't pass through the stack's thread: they'd wake it
//! every 3 ms. The transport takes packets to send from the [`Adapter`]
//! at the USB frame clock (silence if nothing came to play), and hands it
//! those received, which go straight to card 0's capture stream
//! (`sound::remote`). What plays reaches the link from the stack's
//! [`play`](super::stack), resampled to 8 kHz mono.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use super::Adapter;
use crate::sound;

/// The voice setting: linear input, 2's complement, 16-bit, CVSD on the
/// air.
pub const SETTING: u16 = 0x0060;
/// The link's rate.
pub const RATE: u32 = 8000;
/// Samples in each packet sent: 3 ms, 48 bytes, which fills three of the
/// 17-byte USB packets in the alternate setting for one 16-bit link.
const PACKET_SAMPLES: usize = 24;
/// The most queued to send, in samples (100 ms).
const MOST_QUEUED: usize = RATE as usize / 10;

pub struct Link {
    handle: u16,
    /// What plays, at 8 kHz, waiting to be sent.
    out: VecDeque<i16>,
    down: Downsampler,
    received: u64,
    sent: u64,
    /// Packets sent as silence because nothing was playing.
    silent: u64,
}

impl Adapter {
    /// For the stack: a link is open on `handle` (`Some`), or none is.
    pub(super) fn set_voice(&self, handle: Option<u16>) {
        *self.voice.lock() = handle.map(|handle| Link {
            handle,
            out: VecDeque::new(),
            down: Downsampler::new(),
            received: 0,
            sent: 0,
            silent: 0,
        });
        sound::remote::record_from(handle.map(|_| RATE));
        (self.wake_transport)();
    }

    /// For the stack: what the card plays (stereo at `rate`), for the link.
    pub(super) fn play_voice(&self, rate: u32, samples: &[i16]) {
        let mut voice = self.voice.lock();
        let Some(link) = voice.as_mut() else { return };
        let Link { down, out, .. } = link;
        down.convert(rate, samples, out);
        if out.len() > MOST_QUEUED {
            let extra = out.len() - MOST_QUEUED;
            out.drain(..extra);
        }
    }

    /// For `/proc/bluetooth` and the log: packets received, sent, and sent
    /// as silence, if a link is open.
    pub(super) fn voice_counts(&self) -> Option<(u64, u64, u64)> {
        self.voice.lock().as_ref().map(|link| (link.received, link.sent, link.silent))
    }

    /// For the transport: whether a link is open (then its endpoints
    /// should be streaming).
    pub fn voice_open(&self) -> bool {
        self.voice.lock().is_some()
    }

    /// For the transport: the next SCO packet to send (handle, length,
    /// samples), or `None` when no link is open.
    pub fn voice_packet(&self) -> Option<Vec<u8>> {
        let mut voice = self.voice.lock();
        let link = voice.as_mut()?;
        let mut packet = Vec::with_capacity(3 + PACKET_SAMPLES * 2);
        packet.extend_from_slice(&link.handle.to_le_bytes());
        packet.push((PACKET_SAMPLES * 2) as u8);
        if link.out.len() < PACKET_SAMPLES {
            link.silent += 1;
        }
        for _ in 0..PACKET_SAMPLES {
            packet.extend_from_slice(&link.out.pop_front().unwrap_or(0).to_le_bytes());
        }
        link.sent += 1;
        Some(packet)
    }

    /// For the transport: a SCO packet from the controller.
    pub fn voice_received(&self, packet: &[u8]) {
        let Some(samples) = packet.get(3..) else { return };
        let handle = u16::from_le_bytes([packet[0], packet[1]]) & 0x0FFF;
        {
            let mut voice = self.voice.lock();
            let Some(link) = voice.as_mut().filter(|link| link.handle == handle) else { return };
            link.received += 1;
        }
        let samples: Vec<i16> = samples.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect();
        sound::remote::recorded(&samples);
    }
}

/// Stereo at any rate to 8 kHz mono, averaging the frames that make each
/// sample (which also keeps most of what's above 4 kHz out).
pub struct Downsampler {
    /// Progress towards the next sample, in units of the output rate.
    progress: u32,
    sum: i64,
    count: i64,
}

impl Downsampler {
    pub const fn new() -> Self {
        Self { progress: 0, sum: 0, count: 0 }
    }

    pub fn convert(&mut self, rate: u32, input: &[i16], out: &mut VecDeque<i16>) {
        if rate == 0 {
            return;
        }
        for frame in input.as_chunks::<2>().0 {
            self.sum += i64::from(frame[0]) + i64::from(frame[1]);
            self.count += 2;
            self.progress += RATE;
            if self.progress >= rate {
                self.progress -= rate;
                out.push_back((self.sum / self.count) as i16);
                self.sum = 0;
                self.count = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::ktest! {
        fn voice_downsampling_keeps_the_length_and_level() {
            let mut down = Downsampler::new();
            let mut out = VecDeque::new();
            let input: Vec<i16> = (0..48000).flat_map(|_| [3000, 1000]).collect();
            down.convert(48000, &input, &mut out);
            assert_eq!(out.len(), 8000);
            assert!(out.iter().all(|&s| s == 2000));
            out.clear();
            let input: Vec<i16> = (0..44100).flat_map(|_| [-500, -500]).collect();
            down.convert(44100, &input, &mut out);
            assert!((out.len() as i64 - 8000).abs() <= 1, "{}", out.len());
            assert!(out.iter().all(|&s| s == -500));
        }
    }
}
