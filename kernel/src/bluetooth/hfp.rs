//! HFP, the hands-free profile: how a headset's microphone (and its call
//! buttons) work with a phone. We play the phone's part, the "audio
//! gateway"; the headset is the "hands-free unit".
//!
//! The two talk AT commands over an RFCOMM channel. The headset asks and
//! we answer: its features and ours (`AT+BRSF`), the indicators a phone
//! has (`AT+CIND`: service, call, signal, battery...) and their values,
//! and that it wants to hear when they change (`AT+CMER`). After that the
//! "service level connection" is up, and the headset's microphone can be
//! used once an audio (SCO) link is opened. Apple's headsets also report
//! their battery (`AT+XAPL`, `AT+IPHONEACCEV`).
//!
//! Who connects: a headset that connects to us (a paired one coming back)
//! usually opens the RFCOMM channel to our gateway itself (SDP says which
//! one, `CHANNEL`); otherwise, and when we connected, we ask its SDP server
//! for its hands-free service's channel and open that.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use super::l2cap::{self, L2cap};
use super::rfcomm::{self, Event, Rfcomm};
use super::sdp::{self, Answer};
use crate::time;

/// Our gateway's RFCOMM server channel.
pub(super) const CHANNEL: u8 = 1;
const SERVED: &[u8] = &[CHANNEL];

/// How long a headset that connected to us gets to open the channel
/// itself before we do.
const HEADSET_LEADS_MS: u64 = 3000;

/// Our features in `+BRSF`: none of the optional ones (no three-way
/// calling, voice recognition, in-band ringing, codec negotiation...).
const OUR_FEATURES: u32 = 0;

/// The indicators we have, in `+CIND` order, and their values: in
/// service, no call, full signal, not roaming, full battery.
const INDICATORS: &str = "(\"service\",(0,1)),(\"call\",(0,1)),(\"callsetup\",(0-3)),(\"callheld\",(0-2)),\
                          (\"signal\",(0-5)),(\"roam\",(0,1)),(\"battchg\",(0-5))";
const INDICATOR_VALUES: &str = "1,0,0,0,5,0,5";

#[derive(Clone, Copy, PartialEq, Debug)]
enum Step {
    /// Waiting for the headset to open the channel (until the time).
    Waiting(u64),
    /// Asking the headset's SDP server where its hands-free service is.
    Asking,
    /// Opening RFCOMM to its channel.
    Connecting(u8),
    /// The RFCOMM channel is open; AT commands set the connection up.
    Talking,
    /// The service level connection is up.
    Ready,
    Failed,
}

pub(super) struct Gateway {
    step: Step,
    pub rfcomm: Rfcomm,
    /// The open DLC carrying AT commands.
    dlci: Option<u8>,
    /// A command arriving in pieces.
    line: Vec<u8>,
    hf_features: u32,
    sdp_sent: bool,
    sdp_transaction: u16,
    /// The answer so far (it may come in pieces).
    sdp_lists: Vec<u8>,
    /// The headset's speaker and microphone volume (0-15), and its
    /// battery level (percent), when it says.
    pub speaker: Option<u8>,
    pub microphone: Option<u8>,
    pub battery: Option<u8>,
    pub errors: Vec<String>,
}

impl Gateway {
    pub fn new(we_connected: bool) -> Self {
        let wait = if we_connected { 0 } else { HEADSET_LEADS_MS };
        Self {
            step: Step::Waiting(time::ticks() + wait),
            rfcomm: Rfcomm::new(SERVED),
            dlci: None,
            line: Vec::new(),
            hf_features: 0,
            sdp_sent: false,
            sdp_transaction: 0,
            sdp_lists: Vec::new(),
            speaker: None,
            microphone: None,
            battery: None,
            errors: Vec::new(),
        }
    }

    /// For `/proc/bluetooth`.
    pub fn describe(&self) -> String {
        let mut text = String::from(match self.step {
            Step::Ready => "hands-free ready",
            Step::Failed => "no hands-free",
            _ => "hands-free connecting",
        });
        if let Some(battery) = self.battery {
            text.push_str(&format!(", battery {battery}%"));
        }
        text
    }

    pub fn deadline(&self) -> Option<u64> {
        match self.step {
            Step::Waiting(at) => Some(at),
            _ => None,
        }
    }

    /// Moves things along: called whenever something happened on the
    /// connection, and at the deadline.
    pub fn service(&mut self, l2cap: &mut L2cap) {
        if let Step::Waiting(at) = self.step {
            if l2cap.channel(rfcomm::PSM).is_some() {
                // The headset opened RFCOMM itself.
                self.step = Step::Talking;
            } else if time::ticks() >= at {
                debug!("bluetooth: asking the headset where its hands-free service is");
                self.step = Step::Asking;
                l2cap.connect(l2cap::SDP_CLIENT);
            }
        }
        if self.step == Step::Asking && !self.sdp_sent && l2cap.is_open(l2cap::SDP_CLIENT) {
            self.sdp_sent = true;
            self.ask(l2cap, &[]);
        }
        if let Step::Connecting(channel) = self.step
            && l2cap.is_open(rfcomm::PSM)
            && !self.rfcomm.started()
        {
            self.rfcomm.connect(channel);
        }
        self.errors.append(&mut self.rfcomm.errors);
        if !self.errors.is_empty() && self.step != Step::Ready {
            self.step = Step::Failed;
        }
        for frame in core::mem::take(&mut self.rfcomm.outbox) {
            l2cap.send(rfcomm::PSM, frame);
        }
    }

    fn ask(&mut self, l2cap: &mut L2cap, continuation: &[u8]) {
        self.sdp_transaction = self.sdp_transaction.wrapping_add(1);
        l2cap.send(l2cap::SDP_CLIENT, sdp::hands_free_query(self.sdp_transaction, continuation));
    }

    /// The headset's SDP server answered.
    pub fn sdp_answer(&mut self, l2cap: &mut L2cap, response: &[u8]) {
        if self.step != Step::Asking {
            return;
        }
        match sdp::read_answer(response) {
            Some(Answer::More(lists, state)) => {
                self.sdp_lists.extend_from_slice(&lists);
                self.ask(l2cap, &state);
            }
            Some(Answer::Done(lists)) => {
                self.sdp_lists.extend_from_slice(&lists);
                match sdp::rfcomm_channel(&self.sdp_lists) {
                    Some(channel) => {
                        debug!("bluetooth: the headset's hands-free service is on RFCOMM channel {channel}");
                        self.step = Step::Connecting(channel);
                        if l2cap.channel(rfcomm::PSM).is_none() {
                            l2cap.connect(rfcomm::PSM);
                        }
                    }
                    None => {
                        info!("bluetooth: the headset has no hands-free service (no microphone)");
                        self.step = Step::Failed;
                    }
                }
            }
            Some(Answer::Error(code)) => {
                self.errors.push(format!("the headset's SDP server answered error {code:#06x}"));
            }
            None => self.errors.push(String::from("a broken SDP answer")),
        }
    }

    /// An RFCOMM frame from the headset.
    pub fn receive(&mut self, frame: &[u8]) {
        if matches!(self.step, Step::Waiting(_) | Step::Failed) {
            // The headset opened RFCOMM itself.
            self.step = Step::Talking;
            self.errors.clear();
        }
        self.rfcomm.receive(frame);
        for event in core::mem::take(&mut self.rfcomm.events) {
            match event {
                Event::Opened(dlci) => {
                    debug!("bluetooth: hands-free RFCOMM channel {} open", dlci >> 1);
                    self.dlci = Some(dlci);
                    self.step = Step::Talking;
                }
                Event::Closed(dlci) if Some(dlci) == self.dlci => {
                    info!("bluetooth: the headset closed hands-free");
                    self.dlci = None;
                    self.step = Step::Failed;
                }
                Event::Data(dlci, data) if Some(dlci) == self.dlci => {
                    for byte in data {
                        match byte {
                            b'\r' => {
                                let line = core::mem::take(&mut self.line);
                                self.command(&String::from_utf8_lossy(&line));
                            }
                            b'\n' => {}
                            _ if self.line.len() < 512 => self.line.push(byte),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Answers one AT command.
    fn command(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        debug!("bluetooth: hands-free: {line}");
        let upper = line.to_ascii_uppercase();
        let (name, argument) = match upper.find(['=', '?']) {
            Some(at) => (&upper[..at], &line[at..]),
            None => (upper.as_str(), ""),
        };
        let value = argument.strip_prefix('=').unwrap_or("");
        let number = || value.split(',').next().and_then(|text| text.trim().parse::<u32>().ok());
        let mut answer = Vec::new();
        let ok = match (name, argument) {
            ("AT+BRSF", _) => {
                self.hf_features = number().unwrap_or(0);
                answer.push(format!("+BRSF: {OUR_FEATURES}"));
                true
            }
            ("AT+CIND", "=?") => {
                answer.push(format!("+CIND: {INDICATORS}"));
                true
            }
            ("AT+CIND", "?") => {
                answer.push(format!("+CIND: {INDICATOR_VALUES}"));
                true
            }
            ("AT+CMER", _) => {
                if self.step != Step::Ready {
                    info!("bluetooth: hands-free connected (headset features {:#x})", self.hf_features);
                }
                self.step = Step::Ready;
                true
            }
            ("AT+VGS", _) => {
                self.speaker = number().map(|n| n.min(15) as u8);
                true
            }
            ("AT+VGM", _) => {
                self.microphone = number().map(|n| n.min(15) as u8);
                true
            }
            ("AT+COPS", "?") => {
                answer.push(String::from("+COPS: 0,0,\"eliOS\""));
                true
            }
            // Apple's extension: say we take battery reports.
            ("AT+XAPL", _) => {
                answer.push(String::from("+XAPL=iPhone,2"));
                true
            }
            ("AT+IPHONEACCEV", _) => {
                // Count, then key, value pairs; key 1 is the battery, 0-9.
                let fields: Vec<u32> = value.split(',').filter_map(|f| f.trim().parse().ok()).collect();
                for pair in fields.get(1..).unwrap_or(&[]).chunks(2) {
                    if let [1, level] = *pair {
                        self.battery = Some((level.min(9) as u8 + 1) * 10);
                    }
                }
                true
            }
            // Things to just agree to: calling line and call waiting
            // notices, error codes, indicator choices, echo cancelling
            // off, the operator's name format, the call list, the
            // subscriber number, the headset profile's button.
            ("AT+CLIP" | "AT+CCWA" | "AT+CMEE" | "AT+BIA" | "AT+NREC" | "AT+COPS" | "AT+CLCC" | "AT+CNUM", _)
            | ("AT+BTRH", "?")
            | ("AT+CKPD", _) => true,
            // There are no calls to answer, end or make, and no voice
            // recognition, three-way calling, codecs to choose.
            _ => false,
        };
        answer.push(String::from(if ok { "OK" } else { "ERROR" }));
        if !ok {
            debug!("bluetooth: hands-free: answered ERROR to {line}");
        }
        self.send(&answer);
    }

    fn send(&mut self, lines: &[String]) {
        let Some(dlci) = self.dlci else { return };
        let mut text = String::new();
        for line in lines {
            text.push_str("\r\n");
            text.push_str(line);
            text.push_str("\r\n");
        }
        self.rfcomm.send(dlci, text.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds AT commands to a gateway as if over its open channel and
    /// returns what it answered.
    fn talk(gateway: &mut Gateway, command: &str) -> String {
        gateway.command(command);
        let mut answered = String::new();
        for frame in core::mem::take(&mut gateway.rfcomm.outbox) {
            // UIH frames: address, control, length, data, check.
            if frame[1] & !0x10 == 0xEF && frame[0] >> 2 != 0 {
                answered.push_str(&String::from_utf8_lossy(&frame[3..frame.len() - 1]));
            }
        }
        answered
    }

    crate::ktest! {
        fn hfp_sets_up_the_service_level_connection() {
            let mut gateway = Gateway::new(false);
            // A headset opens our channel (SABM on DLCI 0, then on DLCI 2
            // with no negotiation: no credits).
            gateway.receive(&[0x03, 0x3F, 0x01, 0x1C]);
            let check = super::super::rfcomm::fcs(&[0x0B, 0x3F, 0x01]);
            gateway.receive(&[0x0B, 0x3F, 0x01, check]);
            assert_eq!(gateway.dlci, Some(2));
            gateway.rfcomm.outbox.clear();
            assert_eq!(talk(&mut gateway, "AT+BRSF=191"), "\r\n+BRSF: 0\r\n\r\nOK\r\n");
            assert!(talk(&mut gateway, "AT+CIND=?").starts_with("\r\n+CIND: (\"service\",(0,1)),"));
            assert_eq!(talk(&mut gateway, "AT+CIND?"), "\r\n+CIND: 1,0,0,0,5,0,5\r\n\r\nOK\r\n");
            assert_ne!(gateway.step, Step::Ready);
            assert_eq!(talk(&mut gateway, "AT+CMER=3,0,0,1"), "\r\nOK\r\n");
            assert_eq!(gateway.step, Step::Ready);
            assert_eq!(talk(&mut gateway, "at+xapl=05AC-1234-0100,10"), "\r\n+XAPL=iPhone,2\r\n\r\nOK\r\n");
            talk(&mut gateway, "AT+IPHONEACCEV=2,1,7,2,0");
            assert_eq!(gateway.battery, Some(80));
            talk(&mut gateway, "AT+VGS=12");
            assert_eq!(gateway.speaker, Some(12));
            assert_eq!(talk(&mut gateway, "ATD123;"), "\r\nERROR\r\n");
            assert_eq!(gateway.hf_features, 191);
        }
    }
}
