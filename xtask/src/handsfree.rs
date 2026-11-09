//! The pretend headset's hands-free side (`bluetooth.rs`): RFCOMM and the
//! headset's half of HFP's AT conversation, so the kernel's audio gateway
//! can be tested. Written apart from the kernel's RFCOMM, from the
//! specifications, so the two check each other.
//!
//! The headset's hands-free service is on RFCOMM channel 2 (its SDP server
//! says so). When the host opens it, the headset answers; when the headset
//! connected back by itself, it asks the host's SDP server for the
//! gateway's channel and opens that. Either way, once the channel is open
//! it sends the commands that set up the service level connection, then
//! reports its battery and volume, and logs what the host answered.

use std::collections::VecDeque;

/// The headset's hands-free RFCOMM channel.
pub const CHANNEL: u8 = 2;

const SABM: u8 = 0x2F;
const UA: u8 = 0x63;
const DISC: u8 = 0x43;
const UIH: u8 = 0xEF;
const PF: u8 = 0x10;
const PN: u8 = 0x20;
const MSC: u8 = 0x38;
const CREDITS: u16 = 7;

const COMMANDS: [&str; 7] = [
    "AT+BRSF=0",
    "AT+CIND=?",
    "AT+CIND?",
    "AT+CMER=3,0,0,1",
    "AT+XAPL=05AC-1234-0100,10",
    "AT+IPHONEACCEV=1,1,7",
    "AT+VGS=12",
];

fn fcs(bytes: &[u8]) -> u8 {
    let mut crc = 0xFFu8;
    for &byte in bytes {
        crc ^= byte;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xE0 } else { crc >> 1 };
        }
    }
    0xFF - crc
}

pub struct HandsFree {
    initiator: bool,
    /// Our DLC (0 until there is one).
    dlci: u8,
    open: bool,
    credits_on: bool,
    credits_out: u16,
    credits_in: u16,
    waiting: VecDeque<Vec<u8>>,
    line: Vec<u8>,
    /// The next command to send.
    step: usize,
    /// What the host answered, line by line.
    pub answers: Vec<String>,
    pub done: bool,
}

impl HandsFree {
    /// The host is opening our channel.
    pub fn responder() -> Self {
        Self {
            initiator: false,
            dlci: 0,
            open: false,
            credits_on: false,
            credits_out: 0,
            credits_in: 0,
            waiting: VecDeque::new(),
            line: Vec::new(),
            step: 0,
            answers: Vec::new(),
            done: false,
        }
    }

    /// We open the host's gateway on `channel`: returns the first frame.
    pub fn initiator(channel: u8) -> (Self, Vec<u8>) {
        let mut hands_free = Self::responder();
        hands_free.initiator = true;
        // The host's servers are even: it didn't start the session.
        hands_free.dlci = channel << 1;
        let frame = hands_free.frame(0, SABM | PF, true, &[]);
        (hands_free, frame)
    }

    fn address(&self, dlci: u8, command: bool) -> u8 {
        dlci << 2 | u8::from(command == self.initiator) << 1 | 1
    }

    fn frame(&self, dlci: u8, control: u8, command: bool, data: &[u8]) -> Vec<u8> {
        assert!(data.len() < 128);
        let address = self.address(dlci, command);
        let mut frame = vec![address, control, (data.len() as u8) << 1 | 1];
        frame.extend_from_slice(data);
        let checked = if control & !PF == UIH { 2 } else { 3 };
        frame.push(fcs(&frame[..checked]));
        frame
    }

    fn uih(&self, dlci: u8, credits: Option<u8>, data: &[u8]) -> Vec<u8> {
        let address = self.address(dlci, true);
        let control = if credits.is_some() { UIH | PF } else { UIH };
        let mut frame = vec![address, control, (data.len() as u8) << 1 | 1];
        frame.extend(credits);
        frame.extend_from_slice(data);
        frame.push(fcs(&[address, control]));
        frame
    }

    fn multiplexer(&self, kind: u8, command: bool, value: &[u8]) -> Vec<u8> {
        let mut message = vec![kind << 2 | u8::from(command) << 1 | 1, (value.len() as u8) << 1 | 1];
        message.extend_from_slice(value);
        self.uih(0, None, &message)
    }

    /// A frame from the host: returns the frames to answer with.
    pub fn receive(&mut self, frame: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let &[address, control, length, ref rest @ ..] = frame else { return out };
        assert!(length & 1 != 0, "the host's RFCOMM frames are short");
        let (dlci, kind, poll) = (address >> 2, control & !PF, control & PF != 0);
        let credit_byte = kind == UIH && poll && dlci != 0 && self.credits_on;
        let length = usize::from(length >> 1) + usize::from(credit_byte);
        let (Some(info), Some(&check)) = (rest.get(..length), rest.get(length)) else { return out };
        let checked = if kind == UIH { 2 } else { 3 };
        assert_eq!(fcs(&frame[..checked]), check, "the host's RFCOMM check");
        match (kind, dlci) {
            (SABM, 0) => out.push(self.frame(0, UA | PF, false, &[])),
            (SABM, _) => {
                assert_eq!(dlci, CHANNEL << 1, "the host opened our channel");
                self.dlci = dlci;
                out.push(self.frame(dlci, UA | PF, false, &[]));
                self.opened(&mut out);
            }
            (UA, 0) => {
                // Our session is up: negotiate the DLC, asking for credits.
                let value = [self.dlci, 0xF0, 0, 0, 127, 0, 0, CREDITS as u8];
                out.push(self.multiplexer(PN, true, &value));
            }
            (UA, _) if dlci == self.dlci && !self.open => self.opened(&mut out),
            (DISC, _) => {
                out.push(self.frame(dlci, UA | PF, false, &[]));
                self.open = false;
            }
            (UIH, 0) => self.control(info, &mut out),
            (UIH, _) if dlci == self.dlci => {
                let mut data = info;
                if credit_byte {
                    self.credits_out += u16::from(data[0]);
                    data = &data[1..];
                }
                if !data.is_empty() && self.credits_on {
                    self.credits_in -= 1;
                    if self.credits_in <= 2 {
                        out.push(self.uih(self.dlci, Some((CREDITS - self.credits_in) as u8), &[]));
                        self.credits_in = CREDITS;
                    }
                }
                for &byte in data {
                    if byte == b'\n' {
                        let line = String::from_utf8_lossy(&std::mem::take(&mut self.line)).trim().to_string();
                        if line.is_empty() {
                            continue;
                        }
                        if line == "OK" || line == "ERROR" {
                            self.answers.push(format!("{} -> {line}", COMMANDS[self.step.saturating_sub(1)]));
                            self.next_command();
                        } else {
                            self.answers.push(line);
                        }
                    } else if byte != b'\r' {
                        self.line.push(byte);
                    }
                }
            }
            _ => {}
        }
        self.flush(&mut out);
        out
    }

    /// Multiplexer messages from the host.
    fn control(&mut self, info: &[u8], out: &mut Vec<Vec<u8>>) {
        let [kind, length, ref value @ ..] = *info else { return };
        let value = &value[..usize::from(length >> 1).min(value.len())];
        let command = kind & 0x02 != 0;
        match (kind >> 2, command) {
            (PN, true) => {
                // The host asks for credits; we agree and grant 7.
                assert_eq!(value[1] >> 4, 0xF, "the host asks for credit flow control");
                self.credits_on = true;
                self.credits_out = u16::from(value[7] & 7);
                self.credits_in = CREDITS;
                let mut answer = value.to_vec();
                answer[1] = 0xE0;
                answer[7] = CREDITS as u8;
                out.push(self.multiplexer(PN, false, &answer));
            }
            (PN, false) => {
                self.credits_on = value[1] >> 4 == 0xE;
                self.credits_out = u16::from(value[7] & 7);
                self.credits_in = CREDITS;
                out.push(self.frame(self.dlci, SABM | PF, true, &[]));
            }
            (MSC, true) => out.push(self.multiplexer(MSC, false, value)),
            _ => {}
        }
    }

    fn opened(&mut self, out: &mut Vec<Vec<u8>>) {
        self.open = true;
        out.push(self.multiplexer(MSC, true, &[self.dlci << 2 | 0x03, 0x8D]));
        self.next_command();
    }

    fn next_command(&mut self) {
        match COMMANDS.get(self.step) {
            Some(command) => {
                self.waiting.push_back(format!("{command}\r").into_bytes());
                self.step += 1;
            }
            None => self.done = true,
        }
    }

    fn flush(&mut self, out: &mut Vec<Vec<u8>>) {
        while self.open && (!self.credits_on || self.credits_out > 0) {
            let Some(data) = self.waiting.pop_front() else { break };
            if self.credits_on {
                self.credits_out -= 1;
            }
            out.push(self.uih(self.dlci, None, &data));
        }
    }
}

/// The headset's SDP server's answer to a search: its hands-free record's
/// protocol list (L2CAP, then RFCOMM channel 2) if the search was for
/// hands-free (0x111E), otherwise nothing.
pub fn sdp_answer(request: &[u8]) -> Vec<u8> {
    let transaction = [request[1], request[2]];
    let wanted = request.windows(3).any(|window| window == [0x19, 0x11, 0x1E]);
    let lists: Vec<u8> = if wanted {
        vec![
            0x35, 0x13, 0x35, 0x11, 0x09, 0x00, 0x04, 0x35, 0x0C, 0x35, 0x03, 0x19, 0x01, 0x00, 0x35, 0x05, 0x19, 0x00,
            0x03, 0x08, CHANNEL,
        ]
    } else {
        vec![0x35, 0x00]
    };
    let mut response = vec![0x07, transaction[0], transaction[1]];
    response.extend_from_slice(&(lists.len() as u16 + 3).to_be_bytes());
    response.extend_from_slice(&(lists.len() as u16).to_be_bytes());
    response.extend_from_slice(&lists);
    response.push(0);
    response
}

/// A search of the host's SDP server for the hands-free gateway (0x111F),
/// asking for its protocol list.
pub fn sdp_query() -> Vec<u8> {
    vec![0x06, 0x00, 0x01, 0x00, 0x0D, 0x35, 0x03, 0x19, 0x11, 0x1F, 0x02, 0x00, 0x35, 0x03, 0x09, 0x00, 0x04, 0x00]
}

/// The RFCOMM channel in the host's answer.
pub fn gateway_channel(response: &[u8]) -> Option<u8> {
    response.windows(5).find(|window| window[..4] == [0x19, 0x00, 0x03, 0x08]).map(|window| window[4])
}
