//! The Wayland wire format. A message is a header (the object it's for,
//! then its total size and opcode packed in one word) and arguments, each
//! 32-bit aligned: integers, fixed-point numbers (24.8), strings and
//! arrays (a length, then the bytes, padded), object ids. File descriptors
//! don't travel in the bytes but alongside them, as `SCM_RIGHTS`, in the
//! order the messages use them.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;

use crate::sys;

/// A message received: a request (by the compositor) or an event (by a
/// client).
pub struct Message {
    pub object: u32,
    pub opcode: u16,
    pub arguments: Vec<u8>,
}

/// One end of a connection: what's been read but not yet handled, and
/// what's waiting to be sent.
pub struct Connection {
    pub fd: i32,
    input: Vec<u8>,
    input_fds: VecDeque<i32>,
    output: Vec<u8>,
    output_fds: Vec<i32>,
}

/// Why a connection is being given up.
#[derive(Debug)]
pub enum Broken {
    Closed,
    Protocol(&'static str),
}

impl Connection {
    pub fn new(fd: i32) -> Self {
        Self { fd, input: Vec::new(), input_fds: VecDeque::new(), output: Vec::new(), output_fds: Vec::new() }
    }

    /// Reads whatever the other side has sent.
    pub fn read(&mut self) -> Result<(), Broken> {
        let mut buffer = [0u8; 16384];
        loop {
            let mut fds = Vec::new();
            match sys::receive_with_fds(self.fd, &mut buffer, &mut fds) {
                Ok(0) => return Err(Broken::Closed),
                Ok(count) => {
                    self.input.extend_from_slice(&buffer[..count]);
                    self.input_fds.extend(fds);
                    if count < buffer.len() {
                        return Ok(());
                    }
                }
                Err(eli::Error::AGAIN) => return Ok(()),
                Err(_) => return Err(Broken::Closed),
            }
        }
    }

    /// The next complete message, if one has arrived.
    pub fn next_message(&mut self) -> Result<Option<Message>, Broken> {
        if self.input.len() < 8 {
            return Ok(None);
        }
        let object = u32::from_le_bytes(self.input[..4].try_into().unwrap());
        let word = u32::from_le_bytes(self.input[4..8].try_into().unwrap());
        let size = (word >> 16) as usize;
        if size < 8 || !size.is_multiple_of(4) {
            return Err(Broken::Protocol("bad message size"));
        }
        if self.input.len() < size {
            return Ok(None);
        }
        let arguments = self.input[8..size].to_vec();
        self.input.drain(..size);
        Ok(Some(Message { object, opcode: word as u16, arguments }))
    }

    /// The next descriptor the other side sent (for an `fd` argument).
    pub fn take_fd(&mut self) -> Option<i32> {
        self.input_fds.pop_front()
    }

    pub fn send(&mut self, message: Outgoing) {
        let size = (message.bytes.len() + 8) as u32;
        self.output.extend_from_slice(&message.object.to_le_bytes());
        self.output.extend_from_slice(&(size << 16 | u32::from(message.opcode)).to_le_bytes());
        self.output.extend_from_slice(&message.bytes);
        self.output_fds.extend(message.fds);
    }

    /// Whether anything is still waiting to be sent.
    pub fn has_output(&self) -> bool {
        !self.output.is_empty()
    }

    /// Sends what's queued, as much as the socket takes.
    pub fn flush(&mut self) -> Result<(), Broken> {
        while !self.output.is_empty() {
            // At most 28 descriptors per message, as libwayland does.
            let fds: Vec<i32> = self.output_fds.iter().take(28).copied().collect();
            match sys::send_with_fds(self.fd, &self.output, &fds) {
                Ok(count) => {
                    self.output.drain(..count);
                    for fd in self.output_fds.drain(..fds.len()) {
                        let _ = eli::io::close(fd);
                    }
                }
                Err(eli::Error::AGAIN) => return Ok(()),
                Err(_) => return Err(Broken::Closed),
            }
        }
        Ok(())
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        for fd in self.input_fds.drain(..).chain(self.output_fds.drain(..)) {
            let _ = eli::io::close(fd);
        }
        let _ = eli::io::close(self.fd);
    }
}

/// Reads a message's arguments in order.
pub struct Arguments<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Arguments<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    pub fn uint(&mut self) -> Result<u32, Broken> {
        let bytes = self.bytes.get(self.position..self.position + 4).ok_or(Broken::Protocol("message too short"))?;
        self.position += 4;
        Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
    }

    pub fn int(&mut self) -> Result<i32, Broken> {
        self.uint().map(|value| value as i32)
    }

    /// A 24.8 fixed-point number.
    pub fn fixed(&mut self) -> Result<f64, Broken> {
        self.int().map(|value| f64::from(value) / 256.0)
    }

    pub fn array(&mut self) -> Result<Vec<u8>, Broken> {
        let len = self.uint()? as usize;
        let bytes = self.bytes.get(self.position..self.position + len).ok_or(Broken::Protocol("array too long"))?;
        self.position += len.next_multiple_of(4);
        Ok(bytes.to_vec())
    }

    /// A string (`None` for a null one).
    pub fn string(&mut self) -> Result<Option<String>, Broken> {
        let mut bytes = self.array()?;
        if bytes.is_empty() {
            return Ok(None);
        }
        bytes.pop(); // the terminating NUL
        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
    }
}

/// A message being built: an event (from the compositor) or a request
/// (from a client).
pub struct Outgoing {
    object: u32,
    opcode: u16,
    bytes: Vec<u8>,
    fds: Vec<i32>,
}

impl Outgoing {
    pub fn new(object: u32, opcode: u16) -> Self {
        Self { object, opcode, bytes: Vec::new(), fds: Vec::new() }
    }

    pub fn uint(mut self, value: u32) -> Self {
        self.bytes.extend_from_slice(&value.to_le_bytes());
        self
    }

    pub fn int(self, value: i32) -> Self {
        self.uint(value as u32)
    }

    pub fn fixed(self, value: f64) -> Self {
        self.int((value * 256.0) as i32)
    }

    pub fn array(mut self, data: &[u8]) -> Self {
        self.bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        self.bytes.extend_from_slice(data);
        self.bytes.resize(self.bytes.len().next_multiple_of(4), 0);
        self
    }

    pub fn string(self, text: &str) -> Self {
        let mut data = Vec::from(text.as_bytes());
        data.push(0);
        self.array(&data)
    }

    /// A descriptor, which is closed once sent.
    pub fn fd(mut self, fd: i32) -> Self {
        self.fds.push(fd);
        self
    }
}
