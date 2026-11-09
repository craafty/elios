//! Sound, through Linux's ALSA device interface, so programs built for
//! Linux (Firefox, through its `libasound`; `aplay`) play and record on
//! eliOS unchanged.
//!
//! Each sound card gets the files ALSA's library opens: `/dev/snd/controlC0`
//! (what the card is, its mixer controls) and `pcmC0D0p` and `pcmC0D0c`
//! (playback and capture). A driver (`drivers::hda`) provides a card's
//! streams through [`Hardware`] and its volume through [`Mixer`]; the
//! ALSA state machine and buffer bookkeeping are in [`pcm`].
//!
//! The hardware offers one format, 16-bit stereo at 44.1 or 48 kHz. ALSA's
//! library converts anything else (formats, rates, channel counts) itself,
//! with its `plug` layer, which `/etc/asound.conf` puts in front.

pub mod control;
pub mod pcm;
pub mod remote;
pub mod timer;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::errno::Result;
use crate::fs::{self, Inode, PollSource, ramfs};
use crate::sync::SpinLock;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Direction {
    Playback,
    Capture,
}

/// One of a sound card's streams, as its driver runs it. Samples are
/// 16-bit stereo; the buffer is a ring the device loops over, split into
/// periods, with an event at the end of each.
pub trait Hardware: Send + Sync {
    fn direction(&self) -> Direction;
    /// Sample rates offered, lowest first.
    fn rates(&self) -> &[u32];
    /// Sets up a buffer of `periods` periods of `period_bytes`.
    fn configure(&self, rate: u32, period_bytes: usize, periods: usize) -> Result<()>;
    fn release(&self);
    /// Stops, rewinds to the start of the buffer and silences it.
    fn prepare(&self);
    fn write_buffer(&self, offset: usize, data: &[u8]);
    fn read_buffer(&self, offset: usize, data: &mut [u8]);
    // Starting, stopping and pausing must work in any context (with
    // interrupts off, inside `poll`): they only touch the DMA engine.
    fn start(&self);
    fn stop(&self);
    /// Stops or resumes, keeping the position.
    fn pause(&self, paused: bool);
    /// Where the device is in the buffer, in bytes.
    fn position(&self) -> usize;
    /// Notified at every period's end.
    fn events(&self) -> Arc<PollSource>;
}

/// A card's volume controls.
pub trait Mixer: Send + Sync {
    /// How many steps the playback (`output`) or capture volume has, if it
    /// can be set.
    fn volume_steps(&self, output: bool) -> Option<u32>;
    /// The step to start at (0 dB).
    fn default_volume(&self, output: bool) -> u32;
    fn set_volume(&self, output: bool, step: u32, unmuted: bool);
}

pub struct Card {
    pub index: usize,
    /// Like "HDA Intel PCH".
    pub name: &'static str,
    pub playback: Arc<pcm::Pcm>,
    pub capture: Option<Arc<pcm::Pcm>>,
    pub mixer: Arc<dyn Mixer>,
    /// Mixer settings: (step, on) for playback and capture.
    pub volumes: SpinLock<[(u32, bool); 2]>,
    /// Notified when a mixer setting changes (for programs watching).
    pub changed: Arc<PollSource>,
}

static CARDS: SpinLock<Vec<Arc<Card>>> = SpinLock::new(Vec::new());

/// Makes a sound card of a driver's streams, with its files in /dev/snd.
pub fn register_card(
    name: &'static str,
    playback: Arc<dyn Hardware>,
    capture: Option<Arc<dyn Hardware>>,
    mixer: Arc<dyn Mixer>,
) {
    let index = CARDS.lock().len();
    let volumes = [(mixer.default_volume(true), true), (mixer.default_volume(false), true)];
    mixer.set_volume(true, volumes[0].0, true);
    mixer.set_volume(false, volumes[1].0, true);
    let card = Arc::new(Card {
        index,
        name,
        playback: pcm::Pcm::new(index, playback),
        capture: capture.map(|hardware| pcm::Pcm::new(index, hardware)),
        mixer,
        volumes: SpinLock::new(volumes),
        changed: Arc::new(PollSource::new()),
    });
    CARDS.lock().push(card.clone());
    let Some(directory) = directory() else { return };
    let _ = directory.link(&alloc::format!("controlC{index}"), control::ControlNode::new(card.clone()));
    let _ = directory.link(&alloc::format!("pcmC{index}D0p"), pcm::PcmNode::new(card.playback.clone()));
    if let Some(capture) = &card.capture {
        let _ = directory.link(&alloc::format!("pcmC{index}D0c"), pcm::PcmNode::new(capture.clone()));
    }
    if directory.lookup("timer").is_err() {
        let _ = directory.link("timer", timer::TimerNode::new());
    }
    info!("sound: card {index}: {name}{}", if card.capture.is_some() { ", with a microphone" } else { "" });
}

/// Card `card`'s stream in `direction`, if there is one.
pub fn find_pcm(card: usize, direction: Direction) -> Option<Arc<pcm::Pcm>> {
    CARDS.lock().get(card)?.pcm(direction).cloned()
}

/// Whether card 0's speakers are off because its sound goes to a
/// Bluetooth headset instead.
static REDIRECTED: AtomicBool = AtomicBool::new(false);

/// Turns card 0's speakers off (`true`) while what it plays is sent to a
/// headset, or back on. Its volume setting still applies to the headset
/// ([`playback_gain`]).
pub fn redirect_playback(redirected: bool) {
    if REDIRECTED.swap(redirected, Ordering::AcqRel) == redirected {
        return;
    }
    let Some(card) = CARDS.lock().first().cloned() else { return };
    let (step, on) = card.volumes.lock()[0];
    card.mixer.set_volume(true, step, on && !redirected);
    info!("sound: speakers {}", if redirected { "off (sound goes to a headset)" } else { "on" });
}

/// Whether card `index`'s speakers are kept off by [`redirect_playback`].
fn speakers_off(index: usize) -> bool {
    index == 0 && REDIRECTED.load(Ordering::Acquire)
}

/// Card 0's playback volume as a gain for samples sent elsewhere, in
/// 1/65536ths: the setting's fraction of 0 dB, cubed (loudness goes
/// roughly with the cube of amplitude steps), and nothing when muted.
pub fn playback_gain() -> u32 {
    let Some(card) = CARDS.lock().first().cloned() else { return 1 << 16 };
    let (step, on) = card.volumes.lock()[0];
    if !on {
        return 0;
    }
    let full = u64::from(card.mixer.default_volume(true).max(1));
    let fraction = u64::from(step).min(full) * 65536 / full;
    (fraction * fraction / 65536 * fraction / 65536) as u32
}

/// /dev/snd, made on first use.
fn directory() -> Option<Arc<dyn Inode>> {
    let dev = fs::devfs::directory();
    if let Ok(directory) = dev.lookup("snd") {
        return Some(directory);
    }
    let directory: Arc<dyn Inode> = ramfs::Directory::new(fs::Attributes::root(0o755));
    dev.link("snd", directory.clone()).ok()?;
    Some(directory)
}

/// The `audio` group, which may use sound devices.
pub const AUDIO_GROUP: u32 = 29;

/// /proc/asound/cards, as Linux formats it.
pub fn cards_text() -> String {
    let mut text = String::new();
    for card in CARDS.lock().iter() {
        text.push_str(&alloc::format!(
            "{:>2} [{:<15}]: HDA-Intel - {}\n                      {}\n",
            card.index,
            card.id(),
            card.name,
            card.name
        ));
    }
    if text.is_empty() {
        text.push_str("--- no soundcards ---\n");
    }
    text
}

impl Card {
    /// A short name ("PCH"), from the long one.
    pub fn id(&self) -> &'static str {
        self.name.rsplit(' ').next().unwrap_or("Card")
    }
}
