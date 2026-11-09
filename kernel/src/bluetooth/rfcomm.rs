//! RFCOMM: serial ports over one L2CAP channel (PSM 3), which is what the
//! hands-free profile's AT commands travel on.
//!
//! One L2CAP channel carries a "session": several data link connections
//! (DLCs), each named by a DLCI, plus DLCI 0 for the multiplexer's own
//! messages. A DLCI is a server channel number (what SDP advertises) and a
//! direction bit: DLCs to servers on the side that did *not* start the
//! session are even (channel × 2), those to servers on the side that did
//! are odd. Frames (GSM 07.10's basic option) are an address byte (DLCI, a
//! command/response bit, an extension bit), a control byte (SABM to open,
//! UA to agree, DM to refuse, DISC to close, UIH for data), a length, the
//! data, and a CRC-8 check over the header.
//!
//! Opening a DLC: SABM and UA on DLCI 0 (once per session), a parameter
//! negotiation (PN) on DLCI 0 saying how big frames may be and asking for
//! credit-based flow control, then SABM and UA on the DLC itself, and each
//! side states its modem signals (MSC; "ready"). With credit flow control
//! a side may send one data frame per credit the other has granted;
//! credits are granted in a byte at the start of a UIH frame whose P/F bit
//! is set.
//!
//! Like `l2cap.rs`, this is bookkeeping only: frames to send collect in
//! `outbox` (L2CAP payloads), and what happened in `events`.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// RFCOMM's PSM.
pub(super) const PSM: u16 = 0x0003;

// Frame types (control byte, without the P/F bit).
const SABM: u8 = 0x2F;
const UA: u8 = 0x63;
const DM: u8 = 0x0F;
const DISC: u8 = 0x43;
const UIH: u8 = 0xEF;
const PF: u8 = 0x10;

// Multiplexer messages (the type field's six bits).
const TEST: u8 = 0x08;
const PN: u8 = 0x20;
const RPN: u8 = 0x24;
const RLS: u8 = 0x14;
const MSC: u8 = 0x38;
const FCON: u8 = 0x28;
const FCOFF: u8 = 0x18;
const NSC: u8 = 0x04;

/// The largest frame we take, in data bytes: AT commands are short, and
/// this keeps the length in one byte.
const FRAME_SIZE: u16 = 127;
/// Credits we grant the device; more are granted when it runs low.
const CREDITS: u16 = 7;
/// Modem signals: ready to communicate, ready to receive, data valid.
const SIGNALS: u8 = 0x01 | 0x04 | 0x08 | 0x80;

/// The check sequence's table: CRC-8 with polynomial x^8 + x^2 + x + 1,
/// bits reversed.
const CRC_TABLE: [u8; 256] = {
    let mut table = [0u8; 256];
    let mut index = 0;
    while index < 256 {
        let mut crc = index as u8;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xE0 } else { crc >> 1 };
            bit += 1;
        }
        table[index] = crc;
        index += 1;
    }
    table
};

pub(super) fn fcs(bytes: &[u8]) -> u8 {
    0xFF - bytes.iter().fold(0xFF, |crc, &byte| CRC_TABLE[usize::from(crc ^ byte)])
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum DlcState {
    /// We sent PN and wait for its answer.
    Negotiating,
    /// We sent SABM and wait for UA.
    Connecting,
    /// The device negotiated it (PN) but hasn't opened it yet.
    Offered,
    Open,
}

struct Dlc {
    dlci: u8,
    state: DlcState,
    /// Credit-based flow control agreed.
    credits_on: bool,
    /// Frames we may still send, and the device may still send us.
    credits_out: u16,
    credits_in: u16,
    frame_size: u16,
    /// Data waiting for credits.
    waiting: VecDeque<Vec<u8>>,
}

#[derive(Debug, PartialEq)]
pub(super) enum Event {
    /// The DLC is open (either side opened it): its DLCI.
    Opened(u8),
    Data(u8, Vec<u8>),
    Closed(u8),
}

pub(super) struct Rfcomm {
    /// Whether we started the session (None: no session yet).
    initiator: Option<bool>,
    session_open: bool,
    dlcs: Vec<Dlc>,
    /// Server channels the device may open.
    served: &'static [u8],
    /// L2CAP payloads to send on the RFCOMM channel.
    pub outbox: Vec<Vec<u8>>,
    pub events: Vec<Event>,
    pub errors: Vec<String>,
}

impl Rfcomm {
    pub fn new(served: &'static [u8]) -> Self {
        Self {
            initiator: None,
            session_open: false,
            dlcs: Vec::new(),
            served,
            outbox: Vec::new(),
            events: Vec::new(),
            errors: Vec::new(),
        }
    }

    pub fn started(&self) -> bool {
        self.initiator.is_some()
    }

    /// Opens a DLC to the device's server `channel` (starting the session
    /// first if there is none). `Event::Opened` says when it's open.
    pub fn connect(&mut self, channel: u8) {
        if self.initiator.is_none() {
            self.initiator = Some(true);
            self.send_frame(0, SABM | PF, &[], true);
        }
        // The device's servers are even if we started the session.
        let dlci = channel << 1 | u8::from(self.initiator == Some(false));
        self.dlcs.push(Dlc {
            dlci,
            state: DlcState::Negotiating,
            credits_on: false,
            credits_out: 0,
            credits_in: 0,
            frame_size: FRAME_SIZE,
            waiting: VecDeque::new(),
        });
        if self.session_open {
            self.negotiate(dlci);
        }
    }

    /// Sends data on an open DLC, as credits allow.
    pub fn send(&mut self, dlci: u8, data: &[u8]) {
        let Some(dlc) = self.dlcs.iter_mut().find(|dlc| dlc.dlci == dlci && dlc.state == DlcState::Open) else {
            debug!("bluetooth: RFCOMM DLCI {dlci} isn't open; dropped {} bytes", data.len());
            return;
        };
        for piece in data.chunks(usize::from(dlc.frame_size)) {
            dlc.waiting.push_back(piece.to_vec());
        }
        self.flush(dlci);
    }

    /// Handles one frame (an L2CAP payload).
    pub fn receive(&mut self, frame: &[u8]) {
        let [address, control, rest @ ..] = frame else { return };
        let dlci = address >> 2;
        let (length, header) = match rest {
            [low, ..] if low & 1 != 0 => (usize::from(low >> 1), 3),
            [low, high, ..] => (usize::from(low >> 1) | usize::from(*high) << 7, 4),
            _ => return,
        };
        let kind = control & !PF;
        let poll = control & PF != 0;
        // UIH's check covers address and control only.
        let checked = if kind == UIH { 2 } else { header };
        // A credits byte (not counted in the length) may come first.
        let end = header + length + usize::from(kind == UIH && poll && self.credits_on(dlci));
        let Some(&check) = frame.get(end) else { return };
        if fcs(&frame[..checked]) != check {
            debug!("bluetooth: RFCOMM frame with a bad check dropped");
            return;
        }
        let mut info = &frame[header..end];
        match kind {
            SABM => self.sabm(dlci),
            UA => self.ua(dlci),
            DM => {
                if dlci == 0 {
                    self.errors.push(String::from("the device refused RFCOMM"));
                    self.reset();
                } else if let Some(index) = self.dlcs.iter().position(|dlc| dlc.dlci == dlci) {
                    let dlc = self.dlcs.remove(index);
                    if dlc.state == DlcState::Open {
                        self.events.push(Event::Closed(dlci));
                    } else {
                        self.errors.push(format!("the device refused RFCOMM channel {}", dlci >> 1));
                    }
                }
            }
            DISC => {
                self.send_frame(dlci, UA | PF, &[], false);
                if dlci == 0 {
                    self.reset();
                } else if let Some(index) = self.dlcs.iter().position(|dlc| dlc.dlci == dlci) {
                    self.dlcs.remove(index);
                    self.events.push(Event::Closed(dlci));
                }
            }
            UIH if dlci == 0 => self.multiplexer(info),
            UIH => {
                let Some(dlc) = self.dlcs.iter_mut().find(|dlc| dlc.dlci == dlci && dlc.state == DlcState::Open) else {
                    return;
                };
                if dlc.credits_on && poll {
                    let [credits, data @ ..] = info else { return };
                    dlc.credits_out += u16::from(*credits);
                    info = data;
                }
                if !info.is_empty() {
                    if dlc.credits_on {
                        dlc.credits_in = dlc.credits_in.saturating_sub(1);
                    }
                    self.events.push(Event::Data(dlci, info.to_vec()));
                }
                self.flush(dlci);
            }
            _ => {}
        }
    }

    fn credits_on(&self, dlci: u8) -> bool {
        self.dlcs.iter().any(|dlc| dlc.dlci == dlci && dlc.credits_on)
    }

    fn reset(&mut self) {
        for dlc in self.dlcs.drain(..) {
            if dlc.state == DlcState::Open {
                self.events.push(Event::Closed(dlc.dlci));
            }
        }
        self.initiator = None;
        self.session_open = false;
    }

    fn sabm(&mut self, dlci: u8) {
        if dlci == 0 {
            if self.initiator == Some(true) {
                // Both sides started a session at once; ours goes on.
                self.send_frame(0, DM | PF, &[], false);
                return;
            }
            self.initiator = Some(false);
            self.session_open = true;
            self.send_frame(0, UA | PF, &[], false);
            return;
        }
        // Our servers are even if the device started the session.
        let ours = dlci & 1 == u8::from(self.initiator == Some(true));
        if !self.session_open || !ours || !self.served.contains(&(dlci >> 1)) {
            debug!("bluetooth: refused RFCOMM channel {}", dlci >> 1);
            self.send_frame(dlci, DM | PF, &[], false);
            return;
        }
        if !self.dlcs.iter().any(|dlc| dlc.dlci == dlci) {
            // Opened without negotiating: the defaults (no credits).
            self.dlcs.push(Dlc {
                dlci,
                state: DlcState::Offered,
                credits_on: false,
                credits_out: 0,
                credits_in: 0,
                frame_size: FRAME_SIZE,
                waiting: VecDeque::new(),
            });
        }
        self.send_frame(dlci, UA | PF, &[], false);
        self.opened(dlci);
    }

    fn ua(&mut self, dlci: u8) {
        if dlci == 0 {
            if self.initiator == Some(true) && !self.session_open {
                self.session_open = true;
                let waiting: Vec<u8> = self.dlcs.iter().map(|dlc| dlc.dlci).collect();
                waiting.into_iter().for_each(|dlci| self.negotiate(dlci));
            }
            return;
        }
        if self.dlcs.iter().any(|dlc| dlc.dlci == dlci && dlc.state == DlcState::Connecting) {
            self.opened(dlci);
        }
    }

    /// A DLC just opened: say we're ready, and start granting credits.
    fn opened(&mut self, dlci: u8) {
        let Some(dlc) = self.dlcs.iter_mut().find(|dlc| dlc.dlci == dlci) else { return };
        dlc.state = DlcState::Open;
        self.multiplexer_message(MSC, true, &[dlci << 2 | 0x03, SIGNALS]);
        self.events.push(Event::Opened(dlci));
        self.flush(dlci);
    }

    /// Sends PN for one of our DLCs.
    fn negotiate(&mut self, dlci: u8) {
        let [low, high] = FRAME_SIZE.to_le_bytes();
        // Convergence layer 0xF: credit flow control, please; 7 credits.
        self.multiplexer_message(PN, true, &[dlci, 0xF0, 0, 0, low, high, 0, CREDITS as u8]);
    }

    /// Messages on DLCI 0: type (with a command bit), length, value.
    fn multiplexer(&mut self, mut data: &[u8]) {
        while let [kind, rest @ ..] = data {
            let (length, at) = match rest {
                [low, ..] if low & 1 != 0 => (usize::from(low >> 1), 1),
                [low, high, ..] => (usize::from(low >> 1) | usize::from(*high) << 7, 2),
                _ => return,
            };
            let Some(value) = rest.get(at..at + length) else { return };
            data = &rest[at + length..];
            let command = kind & 0x02 != 0;
            match (kind >> 2, command) {
                (PN, true) => self.negotiation_request(value),
                (PN, false) => self.negotiation_answer(value),
                (MSC, true) => self.multiplexer_message(MSC, false, value),
                (RPN, true) => {
                    // Whatever port settings: agreed (9600 8N1 if it asks).
                    let settings = if value.len() == 8 {
                        value.to_vec()
                    } else {
                        [value.first().copied().unwrap_or(0), 0x03, 0x03, 0, 0, 0x11, 0x13, 0xFF, 0xFF].to_vec()
                    };
                    let settings = &settings[..8.min(settings.len())];
                    self.multiplexer_message(RPN, false, settings);
                }
                (RLS | TEST, true) => self.multiplexer_message(kind >> 2, false, value),
                (FCON | FCOFF, true) => self.multiplexer_message(kind >> 2, false, &[]),
                (NSC, _) => debug!("bluetooth: the device didn't understand an RFCOMM message: {value:02x?}"),
                (_, true) => self.multiplexer_message(NSC, false, &[*kind]),
                _ => {}
            }
        }
    }

    /// The device proposes parameters for a DLC it's about to open.
    fn negotiation_request(&mut self, value: &[u8]) {
        let [dlci, layer, priority, _, low, high, _, credits, ..] = *value else { return };
        let dlci = dlci & 0x3F;
        let credits_on = layer >> 4 == 0xF;
        let frame_size = u16::from_le_bytes([low, high]).clamp(23, FRAME_SIZE);
        let dlc = match self.dlcs.iter_mut().find(|dlc| dlc.dlci == dlci) {
            Some(dlc) => dlc,
            None => {
                self.dlcs.push(Dlc {
                    dlci,
                    state: DlcState::Offered,
                    credits_on,
                    credits_out: 0,
                    credits_in: 0,
                    frame_size,
                    waiting: VecDeque::new(),
                });
                self.dlcs.last_mut().unwrap()
            }
        };
        dlc.credits_on = credits_on;
        dlc.frame_size = frame_size;
        dlc.credits_out = if credits_on { u16::from(credits & 7) } else { 0 };
        dlc.credits_in = if credits_on { CREDITS } else { 0 };
        let [low, high] = frame_size.to_le_bytes();
        let (layer, credits) = if credits_on { (0xE0, CREDITS as u8) } else { (0, 0) };
        self.multiplexer_message(PN, false, &[dlci, layer, priority, 0, low, high, 0, credits]);
    }

    /// The device's answer to our PN: then open the DLC.
    fn negotiation_answer(&mut self, value: &[u8]) {
        let [dlci, layer, _, _, low, high, _, credits, ..] = *value else { return };
        let dlci = dlci & 0x3F;
        let Some(dlc) = self.dlcs.iter_mut().find(|dlc| dlc.dlci == dlci && dlc.state == DlcState::Negotiating) else {
            return;
        };
        dlc.credits_on = layer >> 4 == 0xE;
        dlc.frame_size = u16::from_le_bytes([low, high]).clamp(23, FRAME_SIZE);
        dlc.credits_out = if dlc.credits_on { u16::from(credits & 7) } else { 0 };
        dlc.credits_in = if dlc.credits_on { CREDITS } else { 0 };
        dlc.state = DlcState::Connecting;
        self.send_frame(dlci, SABM | PF, &[], true);
    }

    /// Sends what's waiting on a DLC that credits allow, and grants more
    /// credits when the device is running out.
    fn flush(&mut self, dlci: u8) {
        let Some(index) = self.dlcs.iter().position(|dlc| dlc.dlci == dlci && dlc.state == DlcState::Open) else {
            return;
        };
        let mut frames = Vec::new();
        let dlc = &mut self.dlcs[index];
        if dlc.credits_on && dlc.credits_in <= CREDITS / 2 {
            // Credits only, no data: the frame costs the device nothing.
            frames.push((Some((CREDITS - dlc.credits_in) as u8), Vec::new()));
            dlc.credits_in = CREDITS;
        }
        while !dlc.credits_on || dlc.credits_out > 0 {
            let Some(data) = dlc.waiting.pop_front() else { break };
            if dlc.credits_on {
                dlc.credits_out -= 1;
            }
            frames.push((None, data));
        }
        for (credits, data) in frames {
            self.send_data(dlci, credits, &data);
        }
    }

    fn multiplexer_message(&mut self, kind: u8, command: bool, value: &[u8]) {
        let mut message = Vec::with_capacity(2 + value.len());
        message.push(kind << 2 | u8::from(command) << 1 | 1);
        message.push((value.len() as u8) << 1 | 1);
        message.extend_from_slice(value);
        self.send_data(0, None, &message);
    }

    /// A UIH frame (always a command, as far as the C/R bit goes).
    fn send_data(&mut self, dlci: u8, credits: Option<u8>, data: &[u8]) {
        let address = self.address(dlci, true);
        let control = if credits.is_some() { UIH | PF } else { UIH };
        let mut frame = Vec::with_capacity(6 + data.len());
        frame.extend_from_slice(&[address, control]);
        push_length(&mut frame, data.len());
        frame.extend(credits);
        frame.extend_from_slice(data);
        frame.push(fcs(&[address, control]));
        self.outbox.push(frame);
    }

    fn send_frame(&mut self, dlci: u8, control: u8, data: &[u8], command: bool) {
        let address = self.address(dlci, command);
        let mut frame = alloc::vec![address, control];
        push_length(&mut frame, data.len());
        frame.extend_from_slice(data);
        let check = fcs(&frame[..frame.len() - data.len()]);
        frame.push(check);
        self.outbox.push(frame);
    }

    /// The address byte: the C/R bit is set on the initiator's commands
    /// and the responder's responses.
    fn address(&self, dlci: u8, command: bool) -> u8 {
        let initiator = self.initiator != Some(false);
        dlci << 2 | u8::from(command == initiator) << 1 | 1
    }
}

fn push_length(frame: &mut Vec<u8>, length: usize) {
    if length < 128 {
        frame.push((length as u8) << 1 | 1);
    } else {
        frame.push((length as u8) << 1);
        frame.push((length >> 7) as u8);
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    crate::ktest! {
        fn rfcomm_opens_a_channel_both_ways() {
            // Known frames: SABM on DLCI 0 from the initiator, its UA.
            assert_eq!(fcs(&[0x03, 0x3F, 0x01]), 0x1C);
            assert_eq!(fcs(&[0x03, 0x73, 0x01]), 0xD7);

            // We open the device's channel 2.
            let mut ours = Rfcomm::new(&[1]);
            ours.connect(2);
            assert_eq!(ours.outbox.remove(0), vec![0x03, 0x3F, 0x01, 0x1C]);
            ours.receive(&[0x03, 0x73, 0x01, 0xD7]);
            // PN for DLCI 4, asking for credits.
            let pn = ours.outbox.remove(0);
            assert_eq!(&pn[..3], &[0x03, 0xEF, 0x15]);
            assert_eq!(&pn[3..13], &[0x83, 0x11, 4, 0xF0, 0, 0, 127, 0, 0, 7]);

            // The device's side, played by a second Rfcomm.
            let mut device = Rfcomm::new(&[2]);
            device.receive(&[0x03, 0x3F, 0x01, 0x1C]);
            assert_eq!(device.outbox.remove(0), vec![0x03, 0x73, 0x01, 0xD7]);
            device.receive(&pn);
            let answer = device.outbox.remove(0);
            ours.receive(&answer);
            let sabm = ours.outbox.remove(0);
            assert_eq!(sabm[0], 4 << 2 | 0x03);
            device.receive(&sabm);
            assert_eq!(device.events, vec![Event::Opened(4)]);
            // UA, then its MSC.
            for frame in core::mem::take(&mut device.outbox) {
                ours.receive(&frame);
            }
            assert_eq!(ours.events, vec![Event::Opened(4)]);
            for frame in core::mem::take(&mut ours.outbox) {
                device.receive(&frame);
            }
            for frame in core::mem::take(&mut device.outbox) {
                ours.receive(&frame);
            }
            ours.outbox.clear();

            // Data, more than the credits granted: the rest waits for
            // the device to grant more.
            ours.events.clear();
            for _ in 0..9 {
                ours.send(4, b"AT\r");
            }
            assert_eq!(ours.outbox.len(), 7);
            let mut received = 0;
            for frame in core::mem::take(&mut ours.outbox) {
                device.receive(&frame);
            }
            for event in core::mem::take(&mut device.events) {
                if let Event::Data(4, data) = event {
                    assert_eq!(data, b"AT\r");
                    received += 1;
                }
            }
            assert_eq!(received, 7);
            // The device ran low and granted more.
            for frame in core::mem::take(&mut device.outbox) {
                ours.receive(&frame);
            }
            assert_eq!(ours.outbox.len(), 2);

            // A channel we don't serve is refused.
            let mut refusing = Rfcomm::new(&[1]);
            refusing.receive(&[0x03, 0x3F, 0x01, 0x1C]);
            refusing.outbox.clear();
            let address = 3 << 3 | 0x03;
            let check = fcs(&[address, 0x3F, 0x01]);
            refusing.receive(&[address, 0x3F, 0x01, check]);
            assert_eq!(refusing.outbox[0][1], DM | PF);
        }
    }
}
