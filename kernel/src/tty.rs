//! Terminals: the console (`/dev/console`) and pseudo-terminals
//! (`/dev/ptmx` and `/dev/pts/N`).
//!
//! A terminal's input arrives as bytes, the way a terminal sends them:
//! from the keyboard and serial port for the console, and from a program
//! for a pseudo-terminal (a terminal emulator writes what you type to its
//! *master* side). In canonical mode (the default) the terminal edits a
//! line at a time, echoing as you type, and programs read whole lines;
//! programs can switch to raw mode with the standard termios ioctls.
//! Ctrl+C interrupts the foreground process group.
//!
//! Output goes to the screen and serial port for the console, and to the
//! master side for a pseudo-terminal, where the terminal emulator reads
//! and draws it. That's the whole difference between the two: the line
//! discipline in between is the same.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use crate::errno::{Errno, Result};
use crate::fs::{self, FileKind, Inode, Metadata, POLLHUP, POLLIN, POLLOUT, PollSource, ramfs};
use crate::process::credentials::{self, Gid, Uid};
use crate::process::{self, user};
use crate::sync::{Once, SpinLock};
use crate::{console, log};

// termios flag bits (Linux values).
const ICRNL: u32 = 0o400;
const IXON: u32 = 0o2000;
const OPOST: u32 = 0o1;
const ONLCR: u32 = 0o4;
const CS8: u32 = 0o60;
const CREAD: u32 = 0o200;
const B38400: u32 = 0o17;
const ISIG: u32 = 0o1;
const ICANON: u32 = 0o2;
const ECHO: u32 = 0o10;
const ECHOE: u32 = 0o20;
const ECHOK: u32 = 0o40;
const ECHOCTL: u32 = 0o1000;
const ECHOKE: u32 = 0o4000;
const IEXTEN: u32 = 0o100000;

// ioctl requests.
const TCGETS: u64 = 0x5401;
const TCSETS: u64 = 0x5402;
const TCSETSW: u64 = 0x5403;
const TCSETSF: u64 = 0x5404;
const TCFLSH: u64 = 0x540B;
const TIOCSCTTY: u64 = 0x540E;
const TIOCGPGRP: u64 = 0x540F;
const TIOCSPGRP: u64 = 0x5410;
const TIOCGWINSZ: u64 = 0x5413;
const TIOCSWINSZ: u64 = 0x5414;
const FIONREAD: u64 = 0x541B;
const TIOCNOTTY: u64 = 0x5422;
const TIOCGSID: u64 = 0x5429;
const TIOCGPTN: u64 = 0x8004_5430;
const TIOCSPTLCK: u64 = 0x4004_5431;
const TIOCGPTLCK: u64 = 0x8004_5439;
const TIOCGPTPEER: u64 = 0x5441;
// The termios2 variants, which add explicit baud rates (newer C libraries
// use these).
const TCGETS2: u64 = 0x802C_542A;
const TCSETS2: u64 = 0x402C_542B;
const TCSETSW2: u64 = 0x402C_542C;
const TCSETSF2: u64 = 0x402C_542D;
const BAUD_38400: u32 = 38_400;

const SIGHUP: u32 = 1;
const SIGWINCH: u32 = 28;
/// The `tty` group, which owns terminals.
const TTY_GROUP: u32 = 5;
/// Linux's major number for pseudo-terminal slaves.
const PTS_MAJOR: u64 = 136;

/// The kernel's `struct termios`, as the ioctls exchange it.
#[repr(C)]
#[derive(Clone, Copy)]
struct Termios {
    input_flags: u32,
    output_flags: u32,
    control_flags: u32,
    local_flags: u32,
    line: u8,
    control_chars: [u8; 19],
}

impl Termios {
    const DEFAULT: Self = Self {
        input_flags: ICRNL | IXON,
        output_flags: OPOST | ONLCR,
        control_flags: B38400 | CS8 | CREAD,
        local_flags: ISIG | ICANON | ECHO | ECHOE | ECHOK | ECHOCTL | ECHOKE | IEXTEN,
        line: 0,
        // ^C ^\ DEL ^U ^D, VTIME 0, VMIN 1, then the rarely used ones.
        control_chars: [3, 0x1C, 0x7F, 0x15, 4, 0, 1, 0, 0x11, 0x13, 0x1A, 0, 0x12, 0x0F, 0x17, 0x16, 0, 0, 0],
    };
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Escape {
    None,
    /// Saw ESC.
    Start,
    /// Inside `ESC [ ...`.
    Sequence,
}

struct State {
    termios: Termios,
    /// The line being edited, in canonical mode.
    line: Vec<u8>,
    /// Bytes ready for programs to read.
    ready: VecDeque<u8>,
    /// Ctrl+D on an empty line: the next read returns end of file.
    end_of_file: bool,
    /// Escape sequences (arrow keys and such) are dropped in canonical mode.
    escape: Escape,
    /// The process group Ctrl+C is sent to.
    foreground: u32,
    /// Rows and columns (a pseudo-terminal's are set by its emulator).
    window: (u16, u16),
}

/// Where a terminal's output goes.
enum Output {
    Console,
    Pseudo(Weak<Pty>),
}

pub struct Tty {
    inode: u64,
    state: SpinLock<State>,
    readable: Arc<PollSource>,
    output: Output,
    /// Who the terminal belongs to: `login` gives it to the user who logs
    /// in, so their programs can reopen it.
    uid: AtomicU32,
    gid: AtomicU32,
    permissions: AtomicU32,
    rdev: u64,
}

static CONSOLE: Once<Arc<Tty>> = Once::new();

impl Tty {
    fn new(output: Output, uid: Uid, permissions: u32, rdev: u64) -> Self {
        Self {
            inode: ramfs::next_inode_number(),
            state: SpinLock::new(State {
                termios: Termios::DEFAULT,
                line: Vec::new(),
                ready: VecDeque::new(),
                end_of_file: false,
                escape: Escape::None,
                foreground: 0,
                window: (24, 80),
            }),
            readable: Arc::new(PollSource::new()),
            output,
            uid: AtomicU32::new(uid),
            gid: AtomicU32::new(TTY_GROUP),
            permissions: AtomicU32::new(permissions),
            rdev,
        }
    }
}

pub fn console() -> Arc<Tty> {
    if let Some(tty) = CONSOLE.get() {
        return tty.clone();
    }
    // Major 5, minor 1: /dev/console.
    let tty = Arc::new(Tty::new(Output::Console, 0, 0o620, 5 << 8 | 1));
    match CONSOLE.set(tty) {
        Ok(tty) => tty.clone(),
        Err(_) => CONSOLE.get().unwrap().clone(),
    }
}

/// Feeds bytes typed at the console. Callable from interrupt handlers.
pub fn input(bytes: &[u8]) {
    console().receive(bytes);
}

impl Tty {
    /// Writes program output (or echo) where this terminal shows it.
    fn write_output(&self, bytes: &[u8], termios: &Termios) {
        match &self.output {
            Output::Console => {
                for chunk in bytes.utf8_chunks() {
                    log::write_terminal(chunk.valid());
                    if !chunk.invalid().is_empty() {
                        log::write_terminal("\u{FFFD}");
                    }
                }
            }
            Output::Pseudo(pty) => {
                let Some(pty) = pty.upgrade() else { return };
                let newlines = termios.output_flags & (OPOST | ONLCR) == OPOST | ONLCR;
                {
                    let mut buffer = pty.to_master.lock();
                    for &byte in bytes {
                        if byte == b'\n' && newlines {
                            buffer.push_back(b'\r');
                        }
                        buffer.push_back(byte);
                    }
                }
                pty.changed.notify();
            }
        }
    }

    /// Bytes arriving as if typed: the line discipline.
    pub fn receive(&self, bytes: &[u8]) {
        let mut interrupt_group = None;
        let mut wake = false;
        {
            let mut state = self.state.lock();
            for &byte in bytes {
                let flags = state.termios.local_flags;
                let byte = if byte == b'\r' && state.termios.input_flags & ICRNL != 0 { b'\n' } else { byte };
                let echo = flags & ECHO != 0;
                let termios = state.termios;

                if flags & ISIG != 0 && byte == state.termios.control_chars[0] {
                    if echo {
                        self.write_output(b"^C\n", &termios);
                    }
                    state.line.clear();
                    interrupt_group = Some(state.foreground);
                    continue;
                }

                if flags & ICANON == 0 {
                    state.ready.push_back(byte);
                    if echo {
                        self.write_output(&[byte], &termios);
                    }
                    wake = true;
                    continue;
                }

                // Canonical mode: drop escape sequences, edit the line.
                match (state.escape, byte) {
                    (Escape::None, 0x1B) => {
                        state.escape = Escape::Start;
                        continue;
                    }
                    (Escape::Start, b'[' | b'O') => {
                        state.escape = Escape::Sequence;
                        continue;
                    }
                    (Escape::Sequence, 0x40..=0x7E) | (Escape::Start, _) => {
                        state.escape = Escape::None;
                        continue;
                    }
                    (Escape::Sequence, _) => continue,
                    (Escape::None, _) => {}
                }
                match byte {
                    0x7F | 0x08 => {
                        // Remove one whole character, however many bytes it is.
                        while let Some(removed) = state.line.pop() {
                            if removed & 0xC0 != 0x80 {
                                if echo {
                                    self.write_output(b"\x08 \x08", &termios);
                                }
                                break;
                            }
                        }
                    }
                    0x15 => {
                        // Ctrl+U: erase the whole line.
                        let characters = state.line.iter().filter(|&&b| b & 0xC0 != 0x80).count();
                        state.line.clear();
                        if echo {
                            for _ in 0..characters {
                                self.write_output(b"\x08 \x08", &termios);
                            }
                        }
                    }
                    0x04 => {
                        // Ctrl+D: hand over the line as is; on an empty line,
                        // that's end of file.
                        if state.line.is_empty() {
                            state.end_of_file = true;
                        }
                        let line = core::mem::take(&mut state.line);
                        state.ready.extend(line);
                        wake = true;
                    }
                    b'\n' => {
                        let mut line = core::mem::take(&mut state.line);
                        line.push(b'\n');
                        state.ready.extend(line);
                        if echo {
                            self.write_output(b"\n", &termios);
                        }
                        wake = true;
                    }
                    byte if byte >= 0x20 || byte == b'\t' => {
                        state.line.push(byte);
                        if echo {
                            self.write_output(&[byte], &termios);
                        }
                    }
                    _ => {}
                }
            }
        }
        if wake {
            self.readable.notify();
        }
        if let Some(group) = interrupt_group {
            process::signal_group(group, process::SIGINT);
            self.readable.notify();
        }
    }

    /// Rows and columns.
    fn window_size(&self) -> (u16, u16) {
        match self.output {
            // The screen's, or a standard size without one.
            Output::Console => console::with(|console| console.size())
                .map_or((24, 80), |(columns, rows)| (rows as u16, columns as u16)),
            Output::Pseudo(_) => self.state.lock().window,
        }
    }

    /// A pseudo-terminal whose master side has closed: reading gives end
    /// of file, writing an error.
    fn hung_up(&self) -> bool {
        match &self.output {
            Output::Console => false,
            Output::Pseudo(pty) => pty.upgrade().is_none_or(|pty| pty.master_closed.load(Ordering::Acquire)),
        }
    }

    fn read_input(&self, buf: &mut [u8], nonblocking: bool) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.readable.queue().wait_until(|| {
            if process::signal_pending() {
                return Some(Err(Errno::EINTR));
            }
            let mut state = self.state.lock();
            if state.ready.is_empty() {
                if state.end_of_file {
                    state.end_of_file = false;
                    return Some(Ok(0));
                }
                if self.hung_up() {
                    return Some(Ok(0));
                }
                return nonblocking.then_some(Err(Errno::EAGAIN));
            }
            let canonical = state.termios.local_flags & ICANON != 0;
            let mut count = 0;
            while count < buf.len() {
                let Some(byte) = state.ready.pop_front() else { break };
                buf[count] = byte;
                count += 1;
                // A canonical read returns at most one line.
                if canonical && byte == b'\n' {
                    break;
                }
            }
            Some(Ok(count))
        })
    }

    /// The ioctls of the terminal side (also accepted on a pty's master).
    fn terminal_ioctl(&self, request: u64, argument: u64) -> Result<u64> {
        match request {
            TCGETS => {
                let termios = self.state.lock().termios;
                user::write_value(argument, &termios)?;
            }
            TCGETS2 => {
                let termios = self.state.lock().termios;
                user::write_value(argument, &(termios, BAUD_38400, BAUD_38400))?;
            }
            TCSETS | TCSETSW | TCSETSF | TCSETS2 | TCSETSW2 | TCSETSF2 => {
                // termios2 starts with a termios, so the same read works.
                let termios: Termios = user::read_value(argument)?;
                let mut state = self.state.lock();
                if request == TCSETSF || request == TCSETSF2 {
                    state.ready.clear();
                }
                // Leaving canonical mode hands over the partial line.
                if termios.local_flags & ICANON == 0 {
                    let line = core::mem::take(&mut state.line);
                    state.ready.extend(line);
                }
                state.termios = termios;
            }
            TCFLSH => self.state.lock().ready.clear(),
            TIOCGPGRP => user::write_value(argument, &self.state.lock().foreground)?,
            TIOCSPGRP => self.state.lock().foreground = user::read_value(argument)?,
            TIOCGSID => user::write_value(argument, &self.state.lock().foreground)?,
            TIOCGWINSZ => {
                let (rows, columns) = self.window_size();
                user::write_value(argument, &[rows, columns, 0, 0])?;
            }
            TIOCSWINSZ => {
                let [rows, columns, _, _]: [u16; 4] = user::read_value(argument)?;
                let group = {
                    let mut state = self.state.lock();
                    let changed = state.window != (rows, columns);
                    state.window = (rows, columns);
                    changed.then_some(state.foreground)
                };
                // The programs in front redraw for the new size.
                if let Some(group) = group.filter(|&group| group != 0) {
                    process::signal_group(group, SIGWINCH);
                }
            }
            FIONREAD => {
                let available = self.state.lock().ready.len() as u32;
                user::write_value(argument, &available)?;
            }
            _ => return Err(Errno::ENOTTY),
        }
        Ok(0)
    }
}

impl Inode for Tty {
    fn metadata(&self) -> Metadata {
        Metadata {
            uid: self.uid.load(Ordering::Relaxed),
            gid: self.gid.load(Ordering::Relaxed),
            rdev: self.rdev,
            ..Metadata::virtual_file(FileKind::CharDevice, self.inode, self.permissions.load(Ordering::Relaxed))
        }
    }

    fn set_permissions(&self, permissions: u32) -> Result<()> {
        self.permissions.store(permissions & 0o7777, Ordering::Relaxed);
        Ok(())
    }

    fn set_owner(&self, uid: Uid, gid: Gid) -> Result<()> {
        self.uid.store(uid, Ordering::Relaxed);
        self.gid.store(gid, Ordering::Relaxed);
        Ok(())
    }

    fn seekable(&self) -> bool {
        false
    }

    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.read_input(buf, false)
    }

    fn read_stream(&self, buf: &mut [u8], nonblocking: bool) -> Result<usize> {
        self.read_input(buf, nonblocking)
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> Result<usize> {
        if self.hung_up() {
            return Err(Errno::EIO);
        }
        let termios = self.state.lock().termios;
        self.write_output(buf, &termios);
        Ok(buf.len())
    }

    fn poll(&self, _flags: u32) -> u16 {
        let state = self.state.lock();
        let ready = if state.ready.is_empty() && !state.end_of_file { POLLOUT } else { POLLIN | POLLOUT };
        drop(state);
        if self.hung_up() { ready | POLLIN | POLLHUP } else { ready }
    }

    fn poll_sources(&self, sources: &mut Vec<Arc<PollSource>>, _depth: usize) -> bool {
        sources.push(self.readable.clone());
        true
    }

    fn ioctl(&self, request: u64, argument: u64) -> Result<u64> {
        match request {
            // Becoming the controlling terminal (and giving it up).
            TIOCSCTTY => {
                let me = self.self_arc().ok_or(Errno::ENOTTY)?;
                *process::current().terminal.lock() = Some(me);
                self.state.lock().foreground = process::current().group();
                Ok(0)
            }
            TIOCNOTTY => {
                *process::current().terminal.lock() = None;
                Ok(0)
            }
            _ => self.terminal_ioctl(request, argument),
        }
    }

    fn as_any(&self) -> Option<&dyn core::any::Any> {
        Some(self)
    }
}

impl Tty {
    /// The `Arc` this terminal lives in (the console's, or a pty's slave).
    fn self_arc(&self) -> Option<Arc<Tty>> {
        match &self.output {
            Output::Console => Some(console()),
            Output::Pseudo(pty) => pty.upgrade().map(|pty| pty.slave.clone()),
        }
    }
}

/// Makes `tty` the running process's controlling terminal if it has none
/// (opening a terminal does that, as for a session leader on Linux).
pub fn adopt(tty: &Arc<dyn Inode>) {
    let Some(terminal) = tty.as_any().and_then(|any| any.downcast_ref::<Tty>()).and_then(Tty::self_arc) else { return };
    let Some(process) = crate::task::current_process() else { return };
    let mut current = process.terminal.lock();
    if current.is_none() {
        *current = Some(terminal);
    }
}

// ----- Pseudo-terminals -----

/// A pseudo-terminal pair: the slave is an ordinary terminal (`Tty`) whose
/// output collects here for the master side to read.
pub struct Pty {
    number: u32,
    slave: Arc<Tty>,
    to_master: SpinLock<VecDeque<u8>>,
    changed: Arc<PollSource>,
    master_closed: AtomicBool,
    /// Descriptors open on the slave side.
    slave_opens: AtomicUsize,
    /// It was opened at least once (until then, the master doesn't see a
    /// hangup).
    slave_was_opened: AtomicBool,
    locked: AtomicBool,
}

static PTYS: SpinLock<BTreeMap<u32, Weak<Pty>>> = SpinLock::new(BTreeMap::new());

/// /dev/pts, made on first use.
fn pts_directory() -> Option<Arc<dyn Inode>> {
    let dev = fs::devfs::directory();
    if let Ok(directory) = dev.lookup("pts") {
        return Some(directory);
    }
    let directory: Arc<dyn Inode> = ramfs::Directory::new(fs::Attributes::root(0o755));
    dev.link("pts", directory.clone()).ok()?;
    Some(directory)
}

/// `/dev/ptmx`: opening it makes a new pseudo-terminal and gives its
/// master side.
pub struct PtyMultiplexer {
    inode: u64,
}

impl PtyMultiplexer {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { inode: ramfs::next_inode_number() })
    }
}

impl Inode for PtyMultiplexer {
    fn metadata(&self) -> Metadata {
        Metadata { rdev: 5 << 8 | 2, gid: TTY_GROUP, ..Metadata::virtual_file(FileKind::CharDevice, self.inode, 0o666) }
    }

    fn open_instance(&self, _flags: u32) -> Option<Result<Arc<dyn Inode>>> {
        Some(open_pty().map(|master| master as Arc<dyn Inode>))
    }
}

fn open_pty() -> Result<Arc<Master>> {
    let number = {
        let mut ptys = PTYS.lock();
        ptys.retain(|_, pty| pty.strong_count() > 0);
        (0..).find(|number| !ptys.contains_key(number)).unwrap_or(0)
    };
    let uid = credentials::current().euid;
    let pty = Arc::new_cyclic(|me: &Weak<Pty>| Pty {
        number,
        slave: Arc::new(Tty::new(Output::Pseudo(me.clone()), uid, 0o620, PTS_MAJOR << 8 | u64::from(number))),
        to_master: SpinLock::new(VecDeque::new()),
        changed: Arc::new(PollSource::new()),
        master_closed: AtomicBool::new(false),
        slave_opens: AtomicUsize::new(0),
        slave_was_opened: AtomicBool::new(false),
        locked: AtomicBool::new(true),
    });
    PTYS.lock().insert(number, Arc::downgrade(&pty));
    let node = Arc::new(SlaveNode { pty: Arc::downgrade(&pty), inode: pty.slave.inode });
    if let Some(directory) = pts_directory() {
        let _ = directory.link(&format!("{number}"), node);
    }
    Ok(Arc::new(Master { pty }))
}

/// The master side, as an open file: reading gives what programs wrote to
/// the terminal; writing types into it.
pub struct Master {
    pty: Arc<Pty>,
}

impl Drop for Master {
    fn drop(&mut self) {
        let pty = &self.pty;
        pty.master_closed.store(true, Ordering::Release);
        if let Some(directory) = pts_directory() {
            let _ = directory.unlink(&format!("{}", pty.number));
        }
        // The terminal hung up: its programs are told.
        let group = pty.slave.state.lock().foreground;
        if group != 0 {
            process::signal_group(group, SIGHUP);
        }
        pty.slave.readable.notify();
    }
}

impl Inode for Master {
    fn metadata(&self) -> Metadata {
        Metadata {
            rdev: 5 << 8 | 2,
            gid: TTY_GROUP,
            ..Metadata::virtual_file(FileKind::CharDevice, self.pty.slave.inode + 1, 0o620)
        }
    }

    fn seekable(&self) -> bool {
        false
    }

    fn read_stream(&self, buf: &mut [u8], nonblocking: bool) -> Result<usize> {
        let pty = &self.pty;
        let count = pty.changed.queue().wait_until(|| {
            if process::signal_pending() {
                return Some(Err(Errno::EINTR));
            }
            let mut output = pty.to_master.lock();
            if output.is_empty() {
                // Every slave descriptor closed (the shell exited): Linux
                // reports that as an error, which is how terminal
                // emulators notice.
                if pty.slave_was_opened.load(Ordering::Acquire) && pty.slave_opens.load(Ordering::Acquire) == 0 {
                    return Some(Err(Errno::EIO));
                }
                return nonblocking.then_some(Err(Errno::EAGAIN));
            }
            let count = buf.len().min(output.len());
            for (slot, byte) in buf.iter_mut().zip(output.drain(..count)) {
                *slot = byte;
            }
            Some(Ok(count))
        })?;
        pty.changed.notify();
        Ok(count)
    }

    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.read_stream(buf, false)
    }

    fn write_stream(&self, buf: &[u8], _nonblocking: bool) -> Result<usize> {
        self.pty.slave.receive(buf);
        Ok(buf.len())
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> Result<usize> {
        self.write_stream(buf, false)
    }

    fn poll(&self, _flags: u32) -> u16 {
        let pty = &self.pty;
        let mut ready = POLLOUT;
        if !pty.to_master.lock().is_empty() {
            ready |= POLLIN;
        }
        if pty.slave_was_opened.load(Ordering::Acquire) && pty.slave_opens.load(Ordering::Acquire) == 0 {
            ready |= POLLHUP;
        }
        ready
    }

    fn poll_sources(&self, sources: &mut Vec<Arc<PollSource>>, _depth: usize) -> bool {
        sources.push(self.pty.changed.clone());
        true
    }

    fn ioctl(&self, request: u64, argument: u64) -> Result<u64> {
        let pty = &self.pty;
        match request {
            TIOCGPTN => user::write_value(argument, &pty.number).map(|()| 0),
            TIOCSPTLCK => {
                let lock: i32 = user::read_value(argument)?;
                pty.locked.store(lock != 0, Ordering::Release);
                Ok(0)
            }
            TIOCGPTLCK => user::write_value(argument, &i32::from(pty.locked.load(Ordering::Acquire))).map(|()| 0),
            // Opens the slave side directly, as a new descriptor.
            TIOCGPTPEER => {
                let slave = open_slave(pty)?;
                let path = format!("/dev/pts/{}", pty.number);
                let file = fs::OpenFile::new(slave, path, argument as u32 & !fs::O_CLOEXEC | fs::O_RDWR);
                let close_on_exec = argument as u32 & fs::O_CLOEXEC != 0;
                process::current().files.lock().insert(file, close_on_exec, 0).map(|fd| fd as u64)
            }
            _ => pty.slave.terminal_ioctl(request, argument),
        }
    }
}

/// `/dev/pts/N`, through which the slave side is opened.
struct SlaveNode {
    pty: Weak<Pty>,
    inode: u64,
}

impl Inode for SlaveNode {
    fn metadata(&self) -> Metadata {
        match self.pty.upgrade() {
            Some(pty) => pty.slave.metadata(),
            None => Metadata::virtual_file(FileKind::CharDevice, self.inode, 0),
        }
    }

    fn set_permissions(&self, permissions: u32) -> Result<()> {
        self.pty.upgrade().ok_or(Errno::EIO)?.slave.set_permissions(permissions)
    }

    fn set_owner(&self, uid: Uid, gid: Gid) -> Result<()> {
        self.pty.upgrade().ok_or(Errno::EIO)?.slave.set_owner(uid, gid)
    }

    fn open_instance(&self, _flags: u32) -> Option<Result<Arc<dyn Inode>>> {
        let Some(pty) = self.pty.upgrade() else { return Some(Err(Errno::EIO)) };
        if pty.locked.load(Ordering::Acquire) {
            return Some(Err(Errno::EIO));
        }
        Some(open_slave(&pty))
    }
}

fn open_slave(pty: &Arc<Pty>) -> Result<Arc<dyn Inode>> {
    if pty.master_closed.load(Ordering::Acquire) {
        return Err(Errno::EIO);
    }
    pty.slave_opens.fetch_add(1, Ordering::AcqRel);
    pty.slave_was_opened.store(true, Ordering::Release);
    Ok(Arc::new(SlaveHandle { pty: pty.clone() }))
}

/// An open descriptor on the slave side: the terminal itself, counted so
/// the master knows when the last one closes.
struct SlaveHandle {
    pty: Arc<Pty>,
}

impl Drop for SlaveHandle {
    fn drop(&mut self) {
        if self.pty.slave_opens.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.pty.changed.notify();
        }
    }
}

impl Inode for SlaveHandle {
    fn metadata(&self) -> Metadata {
        self.pty.slave.metadata()
    }

    fn set_permissions(&self, permissions: u32) -> Result<()> {
        self.pty.slave.set_permissions(permissions)
    }

    fn set_owner(&self, uid: Uid, gid: Gid) -> Result<()> {
        self.pty.slave.set_owner(uid, gid)
    }

    fn seekable(&self) -> bool {
        false
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.pty.slave.read_at(offset, buf)
    }

    fn read_stream(&self, buf: &mut [u8], nonblocking: bool) -> Result<usize> {
        self.pty.slave.read_stream(buf, nonblocking)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize> {
        self.pty.slave.write_at(offset, buf)
    }

    fn poll(&self, flags: u32) -> u16 {
        self.pty.slave.poll(flags)
    }

    fn poll_sources(&self, sources: &mut Vec<Arc<PollSource>>, depth: usize) -> bool {
        self.pty.slave.poll_sources(sources, depth)
    }

    fn ioctl(&self, request: u64, argument: u64) -> Result<u64> {
        self.pty.slave.ioctl(request, argument)
    }

    fn as_any(&self) -> Option<&dyn core::any::Any> {
        Some(self.pty.slave.as_ref())
    }
}
