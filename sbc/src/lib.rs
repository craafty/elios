//! SBC (low-complexity subband coding), the audio codec every Bluetooth
//! headset understands: A2DP makes it mandatory, so it's what eliOS sends
//! to Bluetooth headphones. This is the encoder; the specification is the
//! A2DP specification's appendix B.
//!
//! A frame codes `blocks` x 8 samples per channel. Each block of 8 samples
//! goes through an analysis filter bank (an 80-tap window, then a cosine
//! matrix) that splits it into 8 frequency bands. Per band and channel the
//! frame has a scale factor (the band's peak, as a power of two); the
//! bitpool, a budget of bits per block, is shared out among the bands by
//! how loud each is (bands we hear less get less), and each sample is
//! quantized to its band's share. With joint stereo, bands where the two
//! channels are alike are sent as their sum and difference instead.
//!
//! Everything is integer arithmetic: the kernel runs without floating
//! point (SSE belongs to user programs), and the encoder runs there.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rate {
    Hz16000,
    Hz32000,
    Hz44100,
    Hz48000,
}

impl Rate {
    pub fn hz(self) -> u32 {
        match self {
            Rate::Hz16000 => 16_000,
            Rate::Hz32000 => 32_000,
            Rate::Hz44100 => 44_100,
            Rate::Hz48000 => 48_000,
        }
    }

    pub fn from_hz(hz: u32) -> Option<Rate> {
        [Rate::Hz16000, Rate::Hz32000, Rate::Hz44100, Rate::Hz48000].into_iter().find(|rate| rate.hz() == hz)
    }

    fn code(self) -> u8 {
        self as u8
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Mono,
    DualChannel,
    Stereo,
    JointStereo,
}

impl Mode {
    pub fn channels(self) -> usize {
        if self == Mode::Mono { 1 } else { 2 }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Allocation {
    /// Bits by loudness, weighted by what the ear hears (the usual one).
    Loudness,
    /// Bits by signal-to-noise ratio alone.
    Snr,
}

/// A stream's settings. There are always 8 subbands (A2DP headsets must
/// take 4 or 8; 8 sounds better).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    pub rate: Rate,
    /// 4, 8, 12 or 16.
    pub blocks: usize,
    pub mode: Mode,
    pub allocation: Allocation,
    /// Bits per block to share among the bands: 2 to 250 (A2DP headsets
    /// take up to 53 or so; 53 at 44.1 kHz joint stereo is 328 kbit/s).
    pub bitpool: u8,
}

pub const SUBBANDS: usize = 8;

impl Config {
    /// Samples per channel in a frame.
    pub fn frame_samples(&self) -> usize {
        self.blocks * SUBBANDS
    }

    /// Bytes in a frame.
    pub fn frame_len(&self) -> usize {
        let channels = self.mode.channels();
        let bitpool = usize::from(self.bitpool);
        let audio_bits = match self.mode {
            Mode::Mono | Mode::DualChannel => self.blocks * channels * bitpool,
            Mode::Stereo => self.blocks * bitpool,
            Mode::JointStereo => SUBBANDS + self.blocks * bitpool,
        };
        4 + 4 * SUBBANDS * channels / 8 + audio_bits.div_ceil(8)
    }
}

/// The prototype filter's first half (taps 0 to 40; the rest mirror it),
/// from the specification's table of the analysis window. Taps 16 to 31
/// appear there with their signs flipped, as below; see `window`.
const PROTOTYPE: [f64; 41] = [
    0.00000000E+00,
    1.56575398E-04,
    3.43256425E-04,
    5.54620202E-04,
    8.23919506E-04,
    1.13992507E-03,
    1.47640169E-03,
    1.78371725E-03,
    2.01182542E-03,
    2.10371989E-03,
    1.99454554E-03,
    1.61656283E-03,
    9.02154502E-04,
    -1.78805361E-04,
    -1.64973098E-03,
    -3.49717454E-03,
    5.65949473E-03,
    8.02941163E-03,
    1.04584443E-02,
    1.27472335E-02,
    1.46525263E-02,
    1.59045603E-02,
    1.62208471E-02,
    1.53184106E-02,
    1.29371806E-02,
    8.85757540E-03,
    2.92408442E-03,
    -4.91578024E-03,
    -1.46404076E-02,
    -2.61098752E-02,
    -3.90751381E-02,
    -5.31873032E-02,
    6.79989431E-02,
    8.29847578E-02,
    9.75753918E-02,
    1.11196689E-01,
    1.23264548E-01,
    1.33264415E-01,
    1.40753505E-01,
    1.45389847E-01,
    1.46955068E-01,
];

/// Fractional bits of the window taps and of the cosine matrix.
const WINDOW_BITS: u32 = 15;
const MATRIX_BITS: u32 = 14;
/// Fractional bits kept in subband samples.
const SAMPLE_BITS: u32 = 12;

/// The 80-tap analysis window. The filter bank folds the window into 16
/// sums of 5 taps 16 apart; the cosine that should modulate tap n changes
/// sign every 16 taps, so the window carries that sign instead: the
/// prototype h, symmetric about tap 40, times -1 for taps 16-31 and 48-63.
const fn window() -> [i32; 80] {
    let mut window = [0; 80];
    let mut n = 0;
    while n < 80 {
        let mirrored = if n <= 40 { n } else { 80 - n };
        // Undo the table's sign for taps 16-31 to get h.
        let h = if mirrored >= 16 && mirrored < 32 { -PROTOTYPE[mirrored] } else { PROTOTYPE[mirrored] };
        let signed = if (n / 16) % 2 == 1 { -h } else { h };
        let scaled = signed * (1 << WINDOW_BITS) as f64;
        window[n] = if scaled < 0.0 { (scaled - 0.5) as i32 } else { (scaled + 0.5) as i32 };
        n += 1;
    }
    window
}

static WINDOW: [i32; 80] = window();

/// cos(m pi / 16) for m = 0 to 8, scaled by 2^14.
const COSINES: [i32; 9] = [16384, 16069, 15137, 13623, 11585, 9102, 6270, 3196, 0];

/// cos(m pi / 16) for any m, scaled.
const fn cosine(m: i32) -> i32 {
    let m = m.rem_euclid(32);
    // cos is symmetric about 0 and 16, and cos(16 - m) = -cos(m).
    let m = if m > 16 { 32 - m } else { m };
    if m <= 8 { COSINES[m as usize] } else { -COSINES[(16 - m) as usize] }
}

/// The analysis matrix: band i, folded tap k: cos((i + 1/2)(k - 4) pi / 8).
const fn matrix() -> [[i32; 16]; SUBBANDS] {
    let mut matrix = [[0; 16]; SUBBANDS];
    let mut i = 0;
    while i < SUBBANDS {
        let mut k = 0;
        while k < 16 {
            matrix[i][k] = cosine((2 * i as i32 + 1) * (k as i32 - 4));
            k += 1;
        }
        i += 1;
    }
    matrix
}

static MATRIX: [[i32; 16]; SUBBANDS] = matrix();

/// Loudness weighting: bands' offsets by sampling rate.
const OFFSETS: [[i32; SUBBANDS]; 4] =
    [[-2, 0, 0, 0, 0, 0, 0, 1], [-3, 0, 0, 0, 0, 0, 1, 2], [-4, 0, 0, 0, 0, 0, 1, 2], [-4, 0, 0, 0, 0, 0, 1, 2]];

pub struct Encoder {
    config: Config,
    /// Each channel's last 80 input samples, newest first.
    history: [[i32; 80]; 2],
}

impl Encoder {
    pub fn new(config: Config) -> Self {
        Self { config, history: [[0; 80]; 2] }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Codes one frame of interleaved samples (`frame_samples` per
    /// channel), appending it to `out`.
    pub fn encode(&mut self, pcm: &[i16], out: &mut Vec<u8>) {
        let config = self.config;
        let channels = config.mode.channels();
        assert_eq!(pcm.len(), config.frame_samples() * channels);
        let blocks = config.blocks;

        // Analysis: [block][channel][band], with SAMPLE_BITS fractional bits.
        let mut samples = [[[0i32; SUBBANDS]; 2]; 16];
        for (block, input) in pcm.chunks_exact(SUBBANDS * channels).enumerate() {
            for channel in 0..channels {
                let history = &mut self.history[channel];
                history.copy_within(0..80 - SUBBANDS, SUBBANDS);
                for i in 0..SUBBANDS {
                    // The newest sample goes first.
                    history[SUBBANDS - 1 - i] = i32::from(input[i * channels + channel]);
                }
                samples[block][channel] = analyze(history);
            }
        }

        // Joint stereo: bands sent as sum and difference where that needs
        // smaller scale factors.
        let mut joined = [false; SUBBANDS];
        if config.mode == Mode::JointStereo {
            // The last band can't be joined (its flag is reserved).
            for (band, joined) in joined.iter_mut().enumerate().take(SUBBANDS - 1) {
                let mut peaks = [0i32; 4];
                for block in samples.iter().take(blocks) {
                    let [left, right] = [block[0][band], block[1][band]];
                    let (sum, difference) = ((left >> 1) + (right >> 1), (left >> 1) - (right >> 1));
                    for (peak, value) in peaks.iter_mut().zip([left, right, sum, difference]) {
                        *peak = (*peak).max(value.saturating_abs());
                    }
                }
                let [left, right, sum, difference] = peaks.map(scale_factor);
                if sum + difference < left + right {
                    *joined = true;
                    for block in samples.iter_mut().take(blocks) {
                        let [left, right] = [block[0][band], block[1][band]];
                        block[0][band] = (left >> 1) + (right >> 1);
                        block[1][band] = (left >> 1) - (right >> 1);
                    }
                }
            }
        }

        let mut scale_factors = [[0u8; SUBBANDS]; 2];
        for (channel, factors) in scale_factors.iter_mut().enumerate().take(channels) {
            for (band, factor) in factors.iter_mut().enumerate() {
                let peak = samples.iter().take(blocks).map(|block| block[channel][band].saturating_abs()).max();
                *factor = scale_factor(peak.unwrap_or(0)) as u8;
            }
        }
        let bits = allocate(&config, &scale_factors);

        // The frame.
        let start = out.len();
        let mut writer = BitWriter { out, bits: 0, count: 0 };
        writer.write(0x9C, 8);
        let mode_code = config.mode as u32;
        let blocks_code = (blocks / 4 - 1) as u32;
        let allocation_code = u32::from(config.allocation == Allocation::Snr);
        writer.write(
            u32::from(config.rate.code()) << 6 | blocks_code << 4 | mode_code << 2 | allocation_code << 1 | 1,
            8,
        );
        writer.write(u32::from(config.bitpool), 8);
        writer.write(0, 8); // the CRC, filled in below
        let mut crc_bits = Crc::new();
        crc_bits.byte(writer.out[start + 1]);
        crc_bits.byte(writer.out[start + 2]);
        if config.mode == Mode::JointStereo {
            for &join in &joined {
                writer.write(u32::from(join), 1);
                crc_bits.bits(u32::from(join), 1);
            }
        }
        for factors in scale_factors.iter().take(channels) {
            for &factor in factors {
                writer.write(u32::from(factor), 4);
                crc_bits.bits(u32::from(factor), 4);
            }
        }
        for block in samples.iter().take(blocks) {
            for channel in 0..channels {
                for band in 0..SUBBANDS {
                    let bits = bits[channel][band];
                    if bits == 0 {
                        continue;
                    }
                    let levels = (1i64 << bits) - 1;
                    // (sample / scale + 1) * levels / 2, scale being
                    // 2^(factor + 1).
                    let shift = u32::from(scale_factors[channel][band]) + 2 + SAMPLE_BITS;
                    let offset = 1i64 << (shift - 1);
                    let quantized = ((i64::from(block[channel][band]) + offset) * levels) >> shift;
                    writer.write(quantized.clamp(0, levels - 1) as u32, bits);
                }
            }
        }
        writer.flush();
        out[start + 3] = crc_bits.finish();
        debug_assert_eq!(out.len() - start, config.frame_len());
    }
}

/// One block of one channel through the filter bank.
fn analyze(history: &[i32; 80]) -> [i32; SUBBANDS] {
    let mut folded = [0i64; 16];
    for (k, sum) in folded.iter_mut().enumerate() {
        for j in 0..5 {
            let n = k + 16 * j;
            *sum += i64::from(WINDOW[n]) * i64::from(history[n]);
        }
    }
    let mut bands = [0i32; SUBBANDS];
    for (band, row) in bands.iter_mut().zip(&MATRIX) {
        let sum: i64 = row.iter().zip(&folded).map(|(&m, &y)| i64::from(m) * y).sum();
        let shift = WINDOW_BITS + MATRIX_BITS - SAMPLE_BITS;
        *band = ((sum + (1 << (shift - 1))) >> shift) as i32;
    }
    bands
}

/// The smallest scale factor (0 to 15) whose scale, 2^(factor + 1), is
/// above `peak`.
fn scale_factor(peak: i32) -> i32 {
    let units = peak >> SAMPLE_BITS;
    // units < 2^(factor + 1)
    let mut factor = 0;
    while factor < 15 && units >= 1 << (factor + 1) {
        factor += 1;
    }
    factor
}

/// Bits per channel and band, as the decoder will work them out from
/// the scale factors: both must agree exactly.
fn allocate(config: &Config, scale_factors: &[[u8; SUBBANDS]; 2]) -> [[u32; SUBBANDS]; 2] {
    let channels = config.mode.channels();
    let mut bitneed = [[0i32; SUBBANDS]; 2];
    for channel in 0..channels {
        for band in 0..SUBBANDS {
            let factor = i32::from(scale_factors[channel][band]);
            bitneed[channel][band] = match config.allocation {
                Allocation::Snr => factor,
                Allocation::Loudness if factor == 0 => -5,
                Allocation::Loudness => {
                    let loudness = factor - OFFSETS[usize::from(config.rate.code())][band];
                    if loudness > 0 { loudness / 2 } else { loudness }
                }
            };
        }
    }
    let mut bits = [[0u32; SUBBANDS]; 2];
    match config.mode {
        // Each channel has its own bitpool.
        Mode::Mono | Mode::DualChannel => {
            for channel in 0..channels {
                share(config.bitpool, &bitneed[channel..channel + 1], &mut bits[channel..channel + 1]);
            }
        }
        Mode::Stereo | Mode::JointStereo => share(config.bitpool, &bitneed, &mut bits),
    }
    bits
}

/// Shares `bitpool` among the channels' bands (the specification's
/// procedure, step by step).
fn share(bitpool: u8, bitneed: &[[i32; SUBBANDS]], bits: &mut [[u32; SUBBANDS]]) {
    let bitpool = i32::from(bitpool);
    let max = bitneed.iter().flatten().copied().max().unwrap_or(0);
    // Lower the slice until the bits needed above it fill the bitpool.
    let mut bitcount = 0;
    let mut slicecount = 0;
    let mut bitslice = max + 1;
    loop {
        bitslice -= 1;
        bitcount += slicecount;
        slicecount = 0;
        for &need in bitneed.iter().flatten() {
            if need > bitslice + 1 && need < bitslice + 16 {
                slicecount += 1;
            } else if need == bitslice + 1 {
                slicecount += 2;
            }
        }
        if bitcount + slicecount >= bitpool {
            break;
        }
    }
    if bitcount + slicecount == bitpool {
        bitcount += slicecount;
        bitslice -= 1;
    }
    let mut allotted = [[0i32; SUBBANDS]; 2];
    for (channel, needs) in bitneed.iter().enumerate() {
        for (band, &need) in needs.iter().enumerate() {
            allotted[channel][band] = if need < bitslice + 2 { 0 } else { (need - bitslice).min(16) };
        }
    }
    // What's left goes one bit at a time, band by band, the channels
    // taking turns.
    let channels = bitneed.len();
    let order = (0..SUBBANDS).flat_map(|band| (0..channels).map(move |channel| (channel, band)));
    for (channel, band) in order.clone() {
        if bitcount >= bitpool {
            break;
        }
        let allotted = &mut allotted[channel][band];
        if *allotted >= 2 && *allotted < 16 {
            *allotted += 1;
            bitcount += 1;
        } else if bitneed[channel][band] == bitslice + 1 && bitpool > bitcount + 1 {
            *allotted = 2;
            bitcount += 2;
        }
    }
    for (channel, band) in order {
        if bitcount >= bitpool {
            break;
        }
        let allotted = &mut allotted[channel][band];
        if *allotted < 16 {
            *allotted += 1;
            bitcount += 1;
        }
    }
    for (channel, row) in bits.iter_mut().enumerate() {
        for (band, bits) in row.iter_mut().enumerate() {
            *bits = allotted[channel][band] as u32;
        }
    }
}

struct BitWriter<'a> {
    out: &'a mut Vec<u8>,
    bits: u32,
    count: u32,
}

impl BitWriter<'_> {
    /// Appends the low `count` bits of `value`, most significant first.
    fn write(&mut self, value: u32, count: u32) {
        for bit in (0..count).rev() {
            self.bits = self.bits << 1 | (value >> bit & 1);
            self.count += 1;
            if self.count == 8 {
                self.out.push(self.bits as u8);
                self.bits = 0;
                self.count = 0;
            }
        }
    }

    /// Pads the last byte with zeros.
    fn flush(&mut self) {
        if self.count > 0 {
            self.out.push((self.bits << (8 - self.count)) as u8);
            self.bits = 0;
            self.count = 0;
        }
    }
}

/// The frame's CRC-8 (x^8 + x^4 + x^3 + x^2 + 1, starting at 0x0F) over
/// the header after the sync byte, the join flags and the scale factors.
struct Crc(u8);

impl Crc {
    fn new() -> Self {
        Crc(0x0F)
    }

    fn byte(&mut self, byte: u8) {
        self.bits(u32::from(byte), 8);
    }

    fn bits(&mut self, value: u32, count: u32) {
        for bit in (0..count).rev() {
            let top = (self.0 >> 7) ^ (value >> bit & 1) as u8;
            self.0 <<= 1;
            if top != 0 {
                self.0 ^= 0x1D;
            }
        }
    }

    fn finish(self) -> u8 {
        self.0
    }
}

#[cfg(test)]
mod tests;
