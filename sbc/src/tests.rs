//! The encoder's frames are checked by decoding them with ffmpeg's SBC
//! decoder (an independent implementation) and comparing what comes back
//! with what went in. Without ffmpeg those tests say so and pass.

extern crate std;

use std::process::Command;
use std::vec::Vec;
use std::{eprintln, format};

use super::*;

/// A tone in each channel (different ones, to catch mixed-up channels),
/// plus a little of the other in each so joint stereo has work to do.
fn tones(rate: u32, samples: usize) -> Vec<i16> {
    let mut pcm = Vec::with_capacity(samples * 2);
    for n in 0..samples {
        let t = n as f64 / f64::from(rate);
        let a = (2.0 * core::f64::consts::PI * 440.0 * t).sin();
        let b = (2.0 * core::f64::consts::PI * 1250.0 * t).sin();
        pcm.push((12000.0 * a + 3000.0 * b) as i16);
        pcm.push((12000.0 * b + 3000.0 * a) as i16);
    }
    pcm
}

fn encode(config: Config, pcm: &[i16]) -> Vec<u8> {
    let mut encoder = Encoder::new(config);
    let mut out = Vec::new();
    for frame in pcm.chunks_exact(config.frame_samples() * config.mode.channels()) {
        encoder.encode(frame, &mut out);
    }
    out
}

/// ffmpeg's decoding of `sbc`, as interleaved stereo; `None` without ffmpeg.
fn decode(sbc: &[u8], name: &str) -> Option<Vec<i16>> {
    let directory = std::env::temp_dir().join(format!("sbc-test-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let input = directory.join("in.sbc");
    let output = directory.join("out.raw");
    std::fs::write(&input, sbc).unwrap();
    let status = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "sbc", "-i"])
        .arg(&input)
        .args(["-f", "s16le", "-ac", "2"])
        .arg(&output)
        .status();
    let Ok(status) = status else {
        eprintln!("ffmpeg isn't installed; skipping the decoding check");
        return None;
    };
    assert!(status.success(), "ffmpeg couldn't decode the frames");
    let bytes = std::fs::read(&output).unwrap();
    std::fs::remove_dir_all(&directory).ok();
    Some(bytes.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect())
}

/// The signal-to-noise ratio of `decoded` against `original`, in dB, at
/// the best delay (the filter banks delay the signal) and gain.
fn snr(original: &[i16], decoded: &[i16]) -> f64 {
    let frames = original.len() / 2;
    let mut best = f64::MIN;
    for delay in 0..300 {
        let (mut signal, mut cross, mut decoded_power) = (0.0, 0.0, 0.0);
        // Skip the start, where the filters are filling.
        for n in 1000..frames.min(decoded.len() / 2 - delay) {
            for channel in 0..2 {
                let a = f64::from(original[2 * n + channel]);
                let b = f64::from(decoded[2 * (n + delay) + channel]);
                signal += a * a;
                cross += a * b;
                decoded_power += b * b;
            }
        }
        if decoded_power == 0.0 {
            continue;
        }
        // The best gain g makes g * decoded closest to the original.
        let gain = cross / decoded_power;
        let noise = signal - 2.0 * gain * cross + gain * gain * decoded_power;
        let ratio = 10.0 * (signal / noise.max(1e-9)).log10();
        if ratio > best {
            best = ratio;
            if delay > 0 && gain.abs() > 0.0 {
                // Gain should be 1: the decoder gives back the level.
                assert!((gain - 1.0).abs() < 0.05 || ratio < 20.0, "decoded at {gain} times the level");
            }
        }
    }
    best
}

fn config(mode: Mode, bitpool: u8) -> Config {
    Config { rate: Rate::Hz44100, blocks: 16, mode, allocation: Allocation::Loudness, bitpool }
}

#[test]
fn frame_lengths() {
    // A2DP's usual high-quality setting: 119 bytes a frame.
    assert_eq!(config(Mode::JointStereo, 53).frame_len(), 119);
    assert_eq!(config(Mode::Stereo, 53).frame_len(), 118);
    assert_eq!(config(Mode::Mono, 31).frame_len(), 4 + 4 + 62);
    let pcm = tones(44100, 128 * 3);
    assert_eq!(encode(config(Mode::JointStereo, 53), &pcm).len(), 3 * 119);
}

#[test]
fn crc_matches_the_specification() {
    // CRC-8 with polynomial 0x1D from 0x0F, most significant bit first,
    // as a byte-at-a-time table would compute it.
    let mut crc = Crc::new();
    crc.byte(0xBD);
    crc.byte(0x35);
    let mut expected: u8 = 0x0F;
    for byte in [0xBDu8, 0x35] {
        expected ^= byte;
        for _ in 0..8 {
            expected = if expected & 0x80 != 0 { expected << 1 ^ 0x1D } else { expected << 1 };
        }
    }
    assert_eq!(crc.finish(), expected);
}

#[test]
fn ffmpeg_decodes_what_we_encode() {
    let pcm = tones(44100, 44100);
    for (mode, bitpool, name, at_least) in [
        (Mode::JointStereo, 53, "joint", 30.0),
        (Mode::Stereo, 53, "stereo", 30.0),
        (Mode::DualChannel, 32, "dual", 30.0),
        (Mode::JointStereo, 20, "low", 15.0),
    ] {
        let sbc = encode(config(mode, bitpool), &pcm);
        let Some(decoded) = decode(&sbc, name) else { return };
        let ratio = snr(&pcm, &decoded);
        eprintln!("{name} (bitpool {bitpool}): {ratio:.1} dB");
        assert!(ratio > at_least, "{name}: only {ratio:.1} dB");
    }
}

#[test]
fn snr_allocation_and_other_rates_decode() {
    let pcm = tones(48000, 24000);
    let config =
        Config { rate: Rate::Hz48000, blocks: 8, mode: Mode::JointStereo, allocation: Allocation::Snr, bitpool: 35 };
    let sbc = encode(config, &pcm);
    let Some(decoded) = decode(&sbc, "snr") else { return };
    let ratio = snr(&pcm, &decoded);
    eprintln!("SNR allocation, 48 kHz, 8 blocks: {ratio:.1} dB");
    assert!(ratio > 25.0, "only {ratio:.1} dB");
}
