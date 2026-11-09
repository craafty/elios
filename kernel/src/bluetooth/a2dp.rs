//! A2DP: sending what the computer plays to a Bluetooth headset, as SBC.
//!
//! AVDTP sets the stream up over an L2CAP channel to PSM 0x19: we ask
//! which endpoints the headset has (`DISCOVER`), what its sink can take
//! (`GET_CAPABILITIES`), choose SBC settings (`SET_CONFIGURATION`), and
//! `OPEN` it, which opens a second channel to the same PSM for the audio
//! itself. `START` and `SUSPEND` turn the flow of audio on and off. Either
//! side may lead: a headset reconnecting to us usually asks the questions
//! itself, so we also answer them, for our one endpoint (a source).
//!
//! The audio comes from sound card 0. While a headset's stream is open
//! the card's speakers are off and a thread ([`tap`]) copies what the card
//! is about to play every 10 ms (so the card's clock paces it), applies
//! the card's volume, and hands it to the Bluetooth thread, which resamples
//! it to the stream's rate if they differ, encodes it, and sends packets
//! of as many frames as fit the channel: an RTP header, a byte giving the
//! number of frames, then the frames.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use sbc::{Allocation, Config, Encoder, Mode, Rate};

use super::l2cap::{AVDTP, AVDTP_MEDIA, L2cap};
use crate::sound::{self, Direction};
use crate::sync::SpinLock;
use crate::{task, time};

// Signals.
const DISCOVER: u8 = 0x01;
const GET_CAPABILITIES: u8 = 0x02;
const SET_CONFIGURATION: u8 = 0x03;
const GET_CONFIGURATION: u8 = 0x04;
const RECONFIGURE: u8 = 0x05;
const OPEN: u8 = 0x06;
const START: u8 = 0x07;
const CLOSE: u8 = 0x08;
const SUSPEND: u8 = 0x09;
const ABORT: u8 = 0x0A;
const GET_ALL_CAPABILITIES: u8 = 0x0C;
const DELAY_REPORT: u8 = 0x0D;

// Message types, the low two bits of a message's first byte.
const COMMAND: u8 = 0;
const GENERAL_REJECT: u8 = 1;
const ACCEPT: u8 = 2;
const REJECT: u8 = 3;

// Service categories in capabilities and configurations.
const MEDIA_TRANSPORT: u8 = 0x01;
const MEDIA_CODEC: u8 = 0x07;
const AUDIO: u8 = 0x00;
const SBC: u8 = 0x00;

// Errors.
const BAD_ACP_SEID: u8 = 0x12;
const SEP_IN_USE: u8 = 0x13;
const BAD_STATE: u8 = 0x31;
const UNSUPPORTED_CONFIGURATION: u8 = 0x29;

/// Our one stream endpoint.
const OUR_SEID: u8 = 1;

/// SBC settings we can send: 44.1 and 48 kHz (which every headset takes),
/// any channel mode, any block count, 8 subbands, either allocation,
/// bitpool 2 to 53.
const OUR_SBC: [u8; 4] = [0x30 | 0x0F, 0xF0 | 0x04 | 0x03, 2, 53];

/// How long a command may go unanswered.
const ANSWER_MS: u64 = 4000;
/// How long a headset that opened the signaling channel gets to set the
/// stream up itself before we do.
const HEADSET_LEADS_MS: u64 = 1500;
/// Quiet this long, the stream is suspended (headsets save power).
const IDLE_MS: u64 = 2000;
/// The most audio kept waiting, in milliseconds.
const BACKLOG_MS: usize = 300;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// Waiting for the signaling channel, or for whoever leads.
    Idle,
    Discovering,
    /// Asking about the headset's sinks, one at a time.
    Capabilities,
    Configuring,
    /// Configured (by either side), not yet opened.
    Configured,
    Opening,
    /// Opened; waiting for the media channel.
    MediaWait,
    /// Ready for audio.
    Open,
    Starting,
    Streaming,
    Suspending,
    /// Gave up (it said why in the log); a reconnection tries again.
    Failed,
}

impl State {
    /// Whether the stream is set up, so the card's sound goes to it.
    fn ready(self) -> bool {
        matches!(self, State::Open | State::Starting | State::Streaming | State::Suspending)
    }
}

pub(super) struct Audio {
    state: State,
    /// Whether we opened the signaling channel (then we lead).
    we_connected: bool,
    /// When we open the signaling channel, if the headset hasn't.
    connect_at: u64,
    /// When we take the lead anyway, if the headset hasn't.
    lead_at: u64,
    /// Our outstanding command: label, signal, when it's overdue.
    pending: Option<(u8, u8, u64)>,
    label: u8,
    /// The headset's sinks still to ask about, and the one in use.
    sinks: Vec<u8>,
    remote_seid: u8,
    config: Option<Config>,
    encoder: Option<Encoder>,
    resampler: Resampler,
    /// Samples (stereo, at the stream's rate) not yet sent.
    pcm: VecDeque<i16>,
    sequence: u16,
    timestamp: u32,
    /// When audio last came.
    heard: u64,
    pub packets: u64,
}

impl Audio {
    /// For a connection we made (then we open the signaling channel now),
    /// or one the headset made (it gets a while to open it).
    pub fn new(we_connected: bool) -> Self {
        Self {
            state: State::Idle,
            we_connected: false,
            connect_at: time::ticks() + if we_connected { 0 } else { 3000 },
            lead_at: u64::MAX,
            pending: None,
            label: 0,
            sinks: Vec::new(),
            remote_seid: 0,
            config: None,
            encoder: None,
            resampler: Resampler::new(),
            pcm: VecDeque::new(),
            sequence: 0,
            timestamp: 0,
            heard: 0,
            packets: 0,
        }
    }

    /// For `/proc/bluetooth`.
    pub fn describe(&self) -> String {
        let what = match self.state {
            State::Idle | State::Discovering | State::Capabilities | State::Configuring => "setting up",
            State::Configured | State::Opening | State::MediaWait => "opening",
            State::Open | State::Starting | State::Suspending => "ready",
            State::Streaming => "playing",
            State::Failed => return String::from("audio failed"),
        };
        match &self.config {
            Some(config) if self.state.ready() => format!("audio {what} ({})", describe_config(config)),
            _ => format!("audio {what}"),
        }
    }

    /// The soonest timer: an overdue answer, taking the lead, suspending.
    pub fn deadline(&self) -> Option<u64> {
        let pending = self.pending.map(|(_, _, overdue)| overdue);
        let lead = (self.state == State::Idle && self.lead_at != u64::MAX).then_some(self.lead_at);
        let idle = (self.state == State::Streaming).then_some(self.heard + IDLE_MS);
        let connect = (self.connect_at != u64::MAX).then_some(self.connect_at);
        [pending, lead, idle, connect].into_iter().flatten().min()
    }

    fn set_state(&mut self, state: State) {
        if self.state == state {
            return;
        }
        debug!("bluetooth: audio {:?} -> {state:?}", self.state);
        let (was, is) = (self.state.ready(), state.ready());
        self.state = state;
        if is && !was {
            if let Some(config) = &self.config {
                info!("bluetooth: audio stream open: {}", describe_config(config));
            }
            sink_added();
        } else if was && !is {
            sink_removed();
        }
        if !is {
            self.pcm.clear();
        }
    }

    fn fail(&mut self, why: &str) {
        warn!("bluetooth: audio: {why}");
        self.pending = None;
        self.set_state(State::Failed);
    }

    /// Checks on the channels and timers after anything happened.
    pub fn service(&mut self, l2cap: &mut L2cap) {
        let now = time::ticks();
        if l2cap.channel(AVDTP).is_some() {
            self.connect_at = u64::MAX;
        } else if now >= self.connect_at {
            self.connect_at = u64::MAX;
            self.we_connected = true;
            l2cap.connect(AVDTP);
        }
        if !l2cap.is_open(AVDTP) {
            if self.state != State::Idle && self.state != State::Failed {
                debug!("bluetooth: audio signaling channel closed");
                self.config = None;
                self.pending = None;
                self.set_state(State::Idle);
            }
            self.lead_at = u64::MAX;
            return;
        }
        if self.lead_at == u64::MAX && self.state == State::Idle {
            self.lead_at = if self.we_connected { now } else { now + HEADSET_LEADS_MS };
        }
        if self.state == State::Idle && now >= self.lead_at {
            self.lead_at = u64::MAX - 1;
            self.set_state(State::Discovering);
            self.command(l2cap, DISCOVER, &[]);
        }
        if let Some((_, signal, overdue)) = self.pending
            && now >= overdue
        {
            self.fail(&format!("no answer to {}", signal_name(signal)));
            return;
        }
        if self.state == State::MediaWait && l2cap.is_open(AVDTP_MEDIA) {
            self.set_state(State::Open);
        }
        if self.state.ready() && !l2cap.is_open(AVDTP_MEDIA) {
            debug!("bluetooth: audio media channel closed");
            self.set_state(State::Configured);
        }
        if self.state == State::Streaming && now >= self.heard + IDLE_MS && self.pending.is_none() {
            self.set_state(State::Suspending);
            self.command(l2cap, SUSPEND, &[self.remote_seid << 2]);
        }
    }

    /// Audio played (stereo at `rate`): starts the stream if it's
    /// suspended, and sends what makes whole packets. `room` says whether
    /// the controller is keeping up; if not, this audio is dropped.
    pub fn play(&mut self, l2cap: &mut L2cap, rate: u32, samples: &[i16], room: bool) {
        if !self.state.ready() || samples.is_empty() {
            return;
        }
        let Some(config) = self.config else { return };
        self.heard = time::ticks();
        if self.state == State::Open && self.pending.is_none() {
            self.set_state(State::Starting);
            self.command(l2cap, START, &[self.remote_seid << 2]);
        }
        let out_rate = config.rate.hz();
        self.resampler.convert(rate, out_rate, samples, &mut self.pcm);
        let most = out_rate as usize * 2 * BACKLOG_MS / 1000;
        if self.pcm.len() > most {
            let extra = self.pcm.len() - most;
            self.pcm.drain(..extra);
        }
        if self.state != State::Streaming {
            return;
        }
        let Some(channel) = l2cap.channel(AVDTP_MEDIA) else { return };
        let frame_len = config.frame_len();
        let per_packet = (usize::from(channel.remote_mtu).saturating_sub(13) / frame_len).clamp(1, 15);
        let frame_samples = config.frame_samples();
        let encoder = self.encoder.get_or_insert_with(|| Encoder::new(config));
        let mut input = Vec::with_capacity(frame_samples * 2);
        while self.pcm.len() >= per_packet * frame_samples * 2 {
            let mut packet = Vec::with_capacity(13 + per_packet * frame_len);
            packet.extend_from_slice(&[0x80, 0x60]);
            packet.extend_from_slice(&self.sequence.to_be_bytes());
            packet.extend_from_slice(&self.timestamp.to_be_bytes());
            packet.extend_from_slice(&1u32.to_be_bytes());
            packet.push(per_packet as u8);
            for _ in 0..per_packet {
                input.clear();
                let stereo = self.pcm.drain(..frame_samples * 2);
                if config.mode == Mode::Mono {
                    let stereo: Vec<i16> = stereo.collect();
                    input.extend(
                        stereo
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .map(|pair| ((i32::from(pair[0]) + i32::from(pair[1])) / 2) as i16),
                    );
                } else {
                    input.extend(stereo);
                }
                encoder.encode(&input, &mut packet);
            }
            self.sequence = self.sequence.wrapping_add(1);
            self.timestamp = self.timestamp.wrapping_add((per_packet * frame_samples) as u32);
            if room {
                l2cap.send(AVDTP_MEDIA, packet);
                self.packets += 1;
            }
        }
    }

    /// A message on the signaling channel.
    pub fn signal(&mut self, l2cap: &mut L2cap, message: &[u8]) {
        let [first, second, body @ ..] = message else { return };
        let (label, packet_type, kind, signal) = (first >> 4, first >> 2 & 3, first & 3, second & 0x3F);
        if packet_type != 0 {
            // Fragmented messages: only needed for long capability lists.
            debug!("bluetooth: audio: fragmented AVDTP message ignored");
            return;
        }
        if kind == COMMAND {
            self.command_received(l2cap, label, signal, body);
            return;
        }
        let Some((pending_label, pending_signal, _)) = self.pending else { return };
        if label != pending_label || signal != pending_signal {
            return;
        }
        self.pending = None;
        if kind == ACCEPT {
            self.accepted(l2cap, signal, body);
        } else {
            let error = if kind == REJECT { body.last().copied().unwrap_or(0) } else { 0 };
            self.rejected(l2cap, signal, kind == GENERAL_REJECT, error);
        }
    }

    fn accepted(&mut self, l2cap: &mut L2cap, signal: u8, body: &[u8]) {
        match (signal, self.state) {
            (DISCOVER, State::Discovering) => {
                self.sinks = body
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .filter(|e| e[0] & 0x02 == 0 && e[1] >> 4 == AUDIO && e[1] & 0x08 != 0)
                    .map(|e| e[0] >> 2)
                    .collect();
                debug!("bluetooth: audio sinks: {:?}", self.sinks);
                self.ask_next_sink(l2cap);
            }
            (GET_CAPABILITIES, State::Capabilities) => {
                let config = sbc_capability(body).and_then(choose).and_then(chosen);
                let Some(config) = config else {
                    debug!("bluetooth: audio sink {} doesn't take our SBC", self.remote_seid);
                    self.ask_next_sink(l2cap);
                    return;
                };
                self.config = Some(config);
                self.encoder = None;
                self.set_state(State::Configuring);
                let mut body = alloc::vec![self.remote_seid << 2, OUR_SEID << 2];
                body.extend_from_slice(&configuration(&config));
                self.command(l2cap, SET_CONFIGURATION, &body);
            }
            (SET_CONFIGURATION, State::Configuring) => {
                self.set_state(State::Opening);
                self.command(l2cap, OPEN, &[self.remote_seid << 2]);
            }
            (OPEN, State::Opening) => {
                self.set_state(State::MediaWait);
                l2cap.connect(AVDTP_MEDIA);
            }
            (START, State::Starting) => {
                self.set_state(State::Streaming);
            }
            (SUSPEND, State::Suspending) => self.set_state(State::Open),
            _ => {}
        }
    }

    fn rejected(&mut self, l2cap: &mut L2cap, signal: u8, general: bool, error: u8) {
        match (signal, self.state) {
            (GET_CAPABILITIES, State::Capabilities) => self.ask_next_sink(l2cap),
            // It's playing (or going to) anyway, or not: either way, stay ready.
            (START, State::Starting) => self.set_state(State::Open),
            (SUSPEND, State::Suspending) => self.set_state(State::Streaming),
            _ => {
                let how = if general { String::from("not understood") } else { format!("error {error:#04x}") };
                self.fail(&format!("{} refused: {how}", signal_name(signal)));
            }
        }
    }

    fn ask_next_sink(&mut self, l2cap: &mut L2cap) {
        if self.sinks.is_empty() {
            self.fail("the headset has no audio sink we can use");
            return;
        }
        self.remote_seid = self.sinks.remove(0);
        self.set_state(State::Capabilities);
        self.command(l2cap, GET_CAPABILITIES, &[self.remote_seid << 2]);
    }

    fn command_received(&mut self, l2cap: &mut L2cap, label: u8, signal: u8, body: &[u8]) {
        // The headset leads: no need for us to.
        self.lead_at = u64::MAX - 1;
        let seid = body.first().map_or(0, |b| b >> 2);
        let answer: Result<Vec<u8>, u8> = match signal {
            DISCOVER => {
                let in_use = u8::from(self.config.is_some());
                Ok(alloc::vec![OUR_SEID << 2 | in_use << 1, AUDIO << 4])
            }
            GET_CAPABILITIES | GET_ALL_CAPABILITIES if seid == OUR_SEID => {
                let mut capabilities = alloc::vec![MEDIA_TRANSPORT, 0, MEDIA_CODEC, 6, AUDIO << 4, SBC];
                capabilities.extend_from_slice(&OUR_SBC);
                Ok(capabilities)
            }
            GET_CAPABILITIES | GET_ALL_CAPABILITIES => Err(BAD_ACP_SEID),
            SET_CONFIGURATION if self.config.is_some() => Err(SEP_IN_USE),
            SET_CONFIGURATION | RECONFIGURE => {
                let capabilities = body.get(if signal == SET_CONFIGURATION { 2.. } else { 1.. }).unwrap_or(&[]);
                match sbc_capability(capabilities).and_then(chosen) {
                    Some(_) if signal == RECONFIGURE && self.state != State::Open => Err(BAD_STATE),
                    Some(config) => {
                        if signal == SET_CONFIGURATION {
                            self.remote_seid = body.get(1).map_or(0, |b| b >> 2);
                            // Whatever we were asking is moot now.
                            self.pending = None;
                            self.set_state(State::Configured);
                        }
                        self.config = Some(config);
                        self.encoder = None;
                        Ok(Vec::new())
                    }
                    None => Err(UNSUPPORTED_CONFIGURATION),
                }
            }
            GET_CONFIGURATION => match &self.config {
                Some(config) => {
                    let mut answer = alloc::vec![MEDIA_TRANSPORT, 0];
                    answer.extend_from_slice(&configuration(config)[2..]);
                    Ok(answer)
                }
                None => Err(BAD_STATE),
            },
            OPEN if self.state == State::Configured => {
                self.set_state(State::MediaWait);
                Ok(Vec::new())
            }
            START if matches!(self.state, State::Open | State::Starting) => {
                self.heard = time::ticks();
                self.set_state(State::Streaming);
                Ok(Vec::new())
            }
            SUSPEND if matches!(self.state, State::Streaming | State::Suspending) => {
                self.set_state(State::Open);
                Ok(Vec::new())
            }
            OPEN | START | SUSPEND => Err(BAD_STATE),
            CLOSE | ABORT => {
                self.config = None;
                self.pending = None;
                self.set_state(State::Idle);
                Ok(Vec::new())
            }
            DELAY_REPORT => Ok(Vec::new()),
            _ => {
                debug!("bluetooth: audio: unknown AVDTP signal {signal:#04x}");
                l2cap.send(AVDTP, alloc::vec![label << 4 | GENERAL_REJECT, signal]);
                return;
            }
        };
        let message = match answer {
            Ok(body) => [&[label << 4 | ACCEPT, signal][..], &body].concat(),
            Err(error) => {
                debug!("bluetooth: audio: refused {} ({error:#04x})", signal_name(signal));
                match signal {
                    // These name the category at fault first.
                    SET_CONFIGURATION | RECONFIGURE => alloc::vec![label << 4 | REJECT, signal, MEDIA_CODEC, error],
                    START | SUSPEND => alloc::vec![label << 4 | REJECT, signal, OUR_SEID << 2, error],
                    _ => alloc::vec![label << 4 | REJECT, signal, error],
                }
            }
        };
        l2cap.send(AVDTP, message);
    }

    fn command(&mut self, l2cap: &mut L2cap, signal: u8, body: &[u8]) {
        let label = self.label;
        self.label = (self.label + 1) & 0x0F;
        self.pending = Some((label, signal, time::ticks() + ANSWER_MS));
        l2cap.send(AVDTP, [&[label << 4 | COMMAND, signal][..], body].concat());
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        if self.state.ready() {
            sink_removed();
        }
    }
}

fn signal_name(signal: u8) -> &'static str {
    match signal {
        DISCOVER => "DISCOVER",
        GET_CAPABILITIES => "GET_CAPABILITIES",
        SET_CONFIGURATION => "SET_CONFIGURATION",
        OPEN => "OPEN",
        START => "START",
        SUSPEND => "SUSPEND",
        _ => "an AVDTP command",
    }
}

/// The SBC codec information in a list of capabilities (or a
/// configuration): four bytes, as in [`OUR_SBC`].
fn sbc_capability(mut capabilities: &[u8]) -> Option<[u8; 4]> {
    while let [category, length, rest @ ..] = capabilities {
        let value = rest.get(..usize::from(*length))?;
        if *category == MEDIA_CODEC && value.len() >= 6 && value[0] >> 4 == AUDIO && value[1] == SBC {
            return value[2..6].try_into().ok();
        }
        capabilities = &rest[value.len()..];
    }
    None
}

/// The settings to use with a sink that takes `theirs`: the best that
/// both sides take.
fn choose(theirs: [u8; 4]) -> Option<[u8; 4]> {
    let both = [theirs[0] & OUR_SBC[0], theirs[1] & OUR_SBC[1]];
    let pick = |bits: u8, preference: &[u8]| preference.iter().copied().find(|bit| bits & bit != 0);
    // 48 kHz first: what calls and most programs play.
    let rate = pick(both[0], &[0x10, 0x20])?;
    let mode = pick(both[0], &[0x01, 0x02, 0x04, 0x08])?;
    let blocks = pick(both[1], &[0x10, 0x20, 0x40, 0x80])?;
    let subbands = pick(both[1], &[0x04])?;
    let allocation = pick(both[1], &[0x01, 0x02])?;
    // The usual "high quality" bitpools, within what the sink takes.
    let wanted = if rate == 0x10 { 51 } else { 53 };
    let bitpool = wanted.min(theirs[3]).max(theirs[2].max(OUR_SBC[2]));
    let config = [rate | mode, blocks | subbands | allocation, bitpool, bitpool];
    chosen(config)?;
    Some(config)
}

/// The encoder settings of a configuration (one bit chosen of each).
fn chosen(sbc: [u8; 4]) -> Option<Config> {
    let rate = match sbc[0] >> 4 {
        0x8 => Rate::Hz16000,
        0x4 => Rate::Hz32000,
        0x2 => Rate::Hz44100,
        0x1 => Rate::Hz48000,
        _ => return None,
    };
    let mode = match sbc[0] & 0x0F {
        0x8 => Mode::Mono,
        0x4 => Mode::DualChannel,
        0x2 => Mode::Stereo,
        0x1 => Mode::JointStereo,
        _ => return None,
    };
    let blocks = match sbc[1] >> 4 {
        0x8 => 4,
        0x4 => 8,
        0x2 => 12,
        0x1 => 16,
        _ => return None,
    };
    let allocation = match sbc[1] & 0x03 {
        0x1 => Allocation::Loudness,
        0x2 => Allocation::Snr,
        _ => return None,
    };
    // Only 8 subbands (4 would need a second filter bank).
    if sbc[1] & 0x0C != 0x04 {
        return None;
    }
    // The bitpool to use is the most allowed.
    let bitpool = sbc[3];
    if !(2..=250).contains(&bitpool) {
        return None;
    }
    Some(Config { rate, blocks, mode, allocation, bitpool })
}

/// A configuration's service capabilities, as SET_CONFIGURATION sends
/// them.
fn configuration(config: &Config) -> Vec<u8> {
    let rate = match config.rate {
        Rate::Hz16000 => 0x80,
        Rate::Hz32000 => 0x40,
        Rate::Hz44100 => 0x20,
        Rate::Hz48000 => 0x10,
    };
    let mode = match config.mode {
        Mode::Mono => 0x08,
        Mode::DualChannel => 0x04,
        Mode::Stereo => 0x02,
        Mode::JointStereo => 0x01,
    };
    let blocks = match config.blocks {
        4 => 0x80,
        8 => 0x40,
        12 => 0x20,
        _ => 0x10,
    };
    let allocation = if config.allocation == Allocation::Loudness { 0x01 } else { 0x02 };
    let codec = [rate | mode, blocks | 0x04 | allocation, OUR_SBC[2].min(config.bitpool), config.bitpool];
    [&[MEDIA_TRANSPORT, 0, MEDIA_CODEC, 6, AUDIO << 4, SBC][..], &codec].concat()
}

fn describe_config(config: &Config) -> String {
    let mode = match config.mode {
        Mode::Mono => "mono",
        Mode::DualChannel => "dual channel",
        Mode::Stereo => "stereo",
        Mode::JointStereo => "joint stereo",
    };
    let rate = config.rate.hz();
    format!("SBC {}.{} kHz {mode}, bitpool {}", rate / 1000, rate % 1000 / 100, config.bitpool)
}

/// Changes the rate of stereo audio by linear interpolation: plain, but
/// enough for the usual 44.1/48 kHz mismatch.
struct Resampler {
    /// How far between `last` and the next input frame the next output
    /// frame falls, in 1/2^32ths.
    position: u64,
    last: [i16; 2],
}

impl Resampler {
    fn new() -> Self {
        Self { position: 0, last: [0; 2] }
    }

    fn convert(&mut self, from: u32, to: u32, input: &[i16], out: &mut VecDeque<i16>) {
        if from == to || from == 0 {
            out.extend(input);
            return;
        }
        let step = (u64::from(from) << 32) / u64::from(to);
        for frame in input.as_chunks::<2>().0 {
            while self.position < 1 << 32 {
                let fraction = (self.position >> 16) as i64;
                for channel in 0..2 {
                    let (a, b) = (i64::from(self.last[channel]), i64::from(frame[channel]));
                    out.push_back((a + (((b - a) * fraction) >> 16)) as i16);
                }
                self.position += step;
            }
            self.position -= 1 << 32;
            self.last = [frame[0], frame[1]];
        }
    }
}

// What the card plays, on its way to the Bluetooth thread (for A2DP
// streams and voice links, which are both "sinks" here).

struct Feed {
    /// Streams ready for audio.
    sinks: usize,
    rate: u32,
    samples: Vec<i16>,
}

static FEED: SpinLock<Feed> = SpinLock::new(Feed { sinks: 0, rate: 0, samples: Vec::new() });
static TAP_STARTED: AtomicBool = AtomicBool::new(false);

pub(super) fn sink_added() {
    let first = {
        let mut feed = FEED.lock();
        feed.sinks += 1;
        feed.sinks == 1
    };
    if first {
        sound::redirect_playback(true);
        if !TAP_STARTED.swap(true, Ordering::AcqRel) {
            task::spawn("bluetooth-audio", tap);
        }
    }
}

pub(super) fn sink_removed() {
    let last = {
        let mut feed = FEED.lock();
        feed.sinks = feed.sinks.saturating_sub(1);
        if feed.sinks == 0 {
            feed.samples.clear();
        }
        feed.sinks == 0
    };
    if last {
        sound::redirect_playback(false);
    }
}

/// Audio that came from the card since the last call: its rate and
/// samples.
pub(super) fn take() -> Option<(u32, Vec<i16>)> {
    let mut feed = FEED.lock();
    if feed.samples.is_empty() {
        return None;
    }
    Some((feed.rate, core::mem::take(&mut feed.samples)))
}

/// Whether [`take`] has something.
pub(super) fn waiting() -> bool {
    !FEED.lock().samples.is_empty()
}

/// Copies what card 0 plays to [`FEED`] while a headset wants it.
fn tap() {
    let mut cursor = None;
    let mut bytes = Vec::new();
    loop {
        if FEED.lock().sinks == 0 {
            cursor = None;
            task::sleep_ms(50);
            continue;
        }
        let Some(pcm) = sound::find_pcm(0, Direction::Playback) else {
            task::sleep_ms(500);
            continue;
        };
        bytes.clear();
        // 20 ms ahead of the card (at 48 kHz): more than its FIFO holds.
        if let Some(rate) = pcm.tap(&mut cursor, 960, &mut bytes)
            && !bytes.is_empty()
        {
            let gain = i64::from(sound::playback_gain());
            {
                let mut feed = FEED.lock();
                if feed.rate != rate {
                    feed.samples.clear();
                    feed.rate = rate;
                }
                let most = rate as usize * 2 * BACKLOG_MS / 1000;
                if feed.samples.len() < most {
                    let samples = bytes.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b));
                    feed.samples.extend(samples.map(|s| ((i64::from(s) * gain) >> 16) as i16));
                }
            }
            super::wake();
        }
        task::sleep_ms(10);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::ktest! {
        fn a2dp_chooses_the_best_sbc_both_take() {
            // A typical headset: everything, bitpool 2 to 53.
            let config = choose([0xFF, 0xFF, 2, 53]).unwrap();
            assert_eq!(config, [0x11, 0x15, 51, 51]);
            let chosen = chosen(config).unwrap();
            assert_eq!(chosen.rate, Rate::Hz48000);
            assert_eq!(chosen.mode, Mode::JointStereo);
            assert_eq!(chosen.blocks, 16);
            // 44.1 kHz stereo only, small bitpools.
            assert_eq!(choose([0x22, 0x15, 2, 35]).unwrap(), [0x22, 0x15, 35, 35]);
            // 4 subbands only: we can't.
            assert!(choose([0xFF, 0xF3, 2, 53]).is_none());
        }

        fn a2dp_resampling_keeps_the_length_and_level() {
            let mut resampler = Resampler::new();
            let input: Vec<i16> = (0..44100 * 2).map(|n| if n % 2 == 0 { 1000 } else { -1000 }).collect();
            let mut out = VecDeque::new();
            resampler.convert(44100, 48000, &input, &mut out);
            assert!((out.len() as i64 - 96000).abs() <= 4, "{} samples", out.len());
            assert!(out.iter().skip(4).step_by(2).all(|&s| s == 1000));
        }

        fn a2dp_streams_to_a_headset_that_leads() {
            let mut l2cap = L2cap::new(&[AVDTP, AVDTP_MEDIA]);
            let mut audio = Audio::new(false);
            // The headset asks what we have, and configures our source.
            audio.signal(&mut l2cap, &[0x00, DISCOVER]);
            let mut message = alloc::vec![0x10, SET_CONFIGURATION, OUR_SEID << 2, 3 << 2];
            message.extend_from_slice(&[MEDIA_TRANSPORT, 0, MEDIA_CODEC, 6, 0, 0, 0x21, 0x15, 2, 53]);
            audio.signal(&mut l2cap, &message);
            audio.signal(&mut l2cap, &[0x20, OPEN, OUR_SEID << 2]);
            assert_eq!(audio.state, State::MediaWait);
            assert_eq!(audio.remote_seid, 3);
            let config = audio.config.unwrap();
            assert_eq!((config.rate, config.mode, config.bitpool), (Rate::Hz44100, Mode::JointStereo, 53));
        }
    }
}
