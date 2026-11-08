//! An HD Audio codec: the chip that turns the controller's digital streams
//! into sound and back (Realtek's ALC292 in the ThinkPad T440p, QEMU's own
//! in the emulator).
//!
//! A codec is a graph of *widgets*, each a node with a number: converters
//! (DACs for output, ADCs for input), mixers and selectors that route
//! signals, and *pin complexes*, the jacks and built-in speakers and
//! microphones. The firmware describes each pin in its "configuration
//! default": what it is (speaker, headphone, microphone), where, and
//! whether anything is connected to it at all. Like Linux's generic codec
//! driver, eliOS finds a route through the graph from each useful output
//! pin to a DAC and from an ADC to a microphone, then unmutes and selects
//! along it. No per-model tables needed for basic sound.
//!
//! The codec is driven by *verbs*: 32-bit commands naming the codec, a
//! widget and an operation, each answered with a 32-bit response.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// Sends verbs to a codec: the controller's command rings, or a test's
/// simulation. `verb` is 12 bits with an 8-bit payload, or (for the
/// format and amplifier verbs) 4 bits with a 16-bit payload.
pub trait Link {
    fn send(&mut self, nid: u16, verb: Verb) -> u32;
}

#[derive(Clone, Copy, Debug)]
pub enum Verb {
    Short(u16, u8),
    Long(u8, u16),
}

// Verbs.
const GET_PARAMETER: u16 = 0xF00;
const GET_CONNECTION_SELECT: u16 = 0xF01;
const GET_CONNECTION_LIST: u16 = 0xF02;
const SET_CONNECTION_SELECT: u16 = 0x701;
const SET_POWER_STATE: u16 = 0x705;
const SET_CHANNEL_STREAM: u16 = 0x706;
const SET_PIN_CONTROL: u16 = 0x707;
const SET_EAPD: u16 = 0x70C;
const GET_PIN_SENSE: u16 = 0xF09;
const SET_PIN_SENSE: u16 = 0x709;
const GET_CONFIG_DEFAULT: u16 = 0xF1C;
const SET_STREAM_FORMAT: u8 = 0x2;
const SET_AMP: u8 = 0x3;

// Parameters (GET_PARAMETER).
const PARAM_VENDOR: u8 = 0x00;
const PARAM_NODE_COUNT: u8 = 0x04;
const PARAM_FUNCTION_TYPE: u8 = 0x05;
const PARAM_WIDGET_CAPS: u8 = 0x09;
const PARAM_PIN_CAPS: u8 = 0x0C;
const PARAM_IN_AMP_CAPS: u8 = 0x0D;
const PARAM_CONNECTION_LENGTH: u8 = 0x0E;
const PARAM_OUT_AMP_CAPS: u8 = 0x12;

// Widget capabilities.
const CAP_IN_AMP: u32 = 1 << 1;
const CAP_OUT_AMP: u32 = 1 << 2;
const CAP_AMP_OVERRIDE: u32 = 1 << 3;
const CAP_CONNECTIONS: u32 = 1 << 8;
const CAP_DIGITAL: u32 = 1 << 9;
const CAP_POWER: u32 = 1 << 10;

// Pin capabilities.
/// Sensing must be started (SET_PIN_SENSE) before each reading.
const PIN_TRIGGER: u32 = 1 << 1;
const PIN_PRESENCE: u32 = 1 << 2;
const PIN_OUTPUT: u32 = 1 << 4;
const PIN_INPUT: u32 = 1 << 5;
const PIN_VREF_80: u32 = 1 << 12;
const PIN_EAPD: u32 = 1 << 16;

// Pin control bits.
const PIN_OUT_ENABLE: u8 = 0x40;
const PIN_IN_ENABLE: u8 = 0x20;
const PIN_HEADPHONE: u8 = 0x80;
const PIN_CONTROL_VREF_80: u8 = 0x04;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Output,
    Input,
    Mixer,
    Selector,
    Pin,
    Other,
}

/// What a pin is, from its configuration default.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Device {
    LineOut,
    Speaker,
    Headphone,
    LineIn,
    Microphone,
    Other,
}

impl Device {
    fn name(self) -> &'static str {
        match self {
            Device::LineOut => "line out",
            Device::Speaker => "speaker",
            Device::Headphone => "headphones",
            Device::LineIn => "line in",
            Device::Microphone => "microphone",
            Device::Other => "other",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Widget {
    pub nid: u16,
    pub kind: Kind,
    caps: u32,
    pub connections: Vec<u16>,
    pin_caps: u32,
    config: u32,
    in_amp: u32,
    out_amp: u32,
}

impl Widget {
    fn digital(&self) -> bool {
        self.caps & CAP_DIGITAL != 0
    }

    pub fn device(&self) -> Device {
        match self.config >> 20 & 0xF {
            0x0 => Device::LineOut,
            0x1 => Device::Speaker,
            0x2 => Device::Headphone,
            0x8 => Device::LineIn,
            0xA => Device::Microphone,
            _ => Device::Other,
        }
    }

    /// Something's wired to it (a jack, or a built-in device).
    fn connected(&self) -> bool {
        self.config >> 30 != 1
    }

    /// Built in (a laptop's speakers and microphone), not a jack.
    pub fn built_in(&self) -> bool {
        self.config >> 30 == 2
    }
}

/// An amplifier's steps: `count` steps, of which `zero` is 0 dB.
#[derive(Clone, Copy, Debug, Default)]
pub struct AmpSteps {
    pub count: u32,
    pub zero: u32,
}

fn amp_steps(caps: u32) -> AmpSteps {
    AmpSteps { count: caps >> 8 & 0x7F, zero: caps & 0x7F }
}

/// A route through the graph: for output, from a pin to a DAC; for input,
/// from an ADC to a pin.
#[derive(Clone, Debug)]
pub struct Path {
    pub nodes: Vec<u16>,
    pub device: Device,
}

pub struct Codec {
    pub address: u8,
    pub vendor: u32,
    pub widgets: BTreeMap<u16, Widget>,
    /// Routes to every useful output pin, speakers first.
    pub outputs: Vec<Path>,
    /// The route from the microphone (or line in) to an ADC.
    pub input: Option<Path>,
    /// The last [`set_volume`](Self::set_volume) for playback and capture
    /// (step, and bit 31 for unmuted), put back whenever a stream starts:
    /// opening a route unmutes every amp on it, the volume amp included,
    /// and a muted card (sound going to a Bluetooth headset) must stay
    /// muted when a program starts playing.
    levels: [AtomicU32; 2],
    /// Headphones are plugged in, so the speakers stay off.
    headphones_in: AtomicBool,
}

fn parameter(link: &mut impl Link, nid: u16, parameter: u8) -> u32 {
    link.send(nid, Verb::Short(GET_PARAMETER, parameter))
}

impl Codec {
    /// Reads the codec's graph and finds the routes. `None` if it has no
    /// audio function (a modem, say).
    pub fn discover(link: &mut impl Link, address: u8) -> Option<Self> {
        let vendor = parameter(link, 0, PARAM_VENDOR);
        let nodes = parameter(link, 0, PARAM_NODE_COUNT);
        let (first, count) = ((nodes >> 16 & 0xFF) as u16, (nodes & 0xFF) as u16);
        let afg = (first..first + count).find(|&nid| parameter(link, nid, PARAM_FUNCTION_TYPE) & 0xFF == 0x01)?;
        // Power the function group up.
        link.send(afg, Verb::Short(SET_POWER_STATE, 0));
        let default_in_amp = parameter(link, afg, PARAM_IN_AMP_CAPS);
        let default_out_amp = parameter(link, afg, PARAM_OUT_AMP_CAPS);

        let nodes = parameter(link, afg, PARAM_NODE_COUNT);
        let (first, count) = ((nodes >> 16 & 0xFF) as u16, (nodes & 0xFF) as u16);
        let mut widgets = BTreeMap::new();
        for nid in first..first + count {
            let caps = parameter(link, nid, PARAM_WIDGET_CAPS);
            let kind = match caps >> 20 & 0xF {
                0 => Kind::Output,
                1 => Kind::Input,
                2 => Kind::Mixer,
                3 => Kind::Selector,
                4 => Kind::Pin,
                _ => Kind::Other,
            };
            let connections = if caps & CAP_CONNECTIONS != 0 { connection_list(link, nid) } else { Vec::new() };
            let (pin_caps, config) = if kind == Kind::Pin {
                (parameter(link, nid, PARAM_PIN_CAPS), link.send(nid, Verb::Short(GET_CONFIG_DEFAULT, 0)))
            } else {
                (0, 0)
            };
            let overridden = caps & CAP_AMP_OVERRIDE != 0;
            let in_amp = if overridden { parameter(link, nid, PARAM_IN_AMP_CAPS) } else { default_in_amp };
            let out_amp = if overridden { parameter(link, nid, PARAM_OUT_AMP_CAPS) } else { default_out_amp };
            if caps & CAP_POWER != 0 {
                link.send(nid, Verb::Short(SET_POWER_STATE, 0));
            }
            widgets.insert(nid, Widget { nid, kind, caps, connections, pin_caps, config, in_amp, out_amp });
        }
        let mut codec = Codec {
            address,
            vendor,
            widgets,
            outputs: Vec::new(),
            input: None,
            levels: Default::default(),
            headphones_in: AtomicBool::new(false),
        };
        codec.find_routes(link);
        Some(codec)
    }

    fn find_routes(&mut self, link: &mut impl Link) {
        // Output pins, speakers first (they're what a laptop plays through).
        let rank = |device: Device| match device {
            Device::Speaker => 0,
            Device::Headphone => 1,
            Device::LineOut => 2,
            _ => 3,
        };
        let mut pins: Vec<&Widget> = self
            .widgets
            .values()
            .filter(|w| w.kind == Kind::Pin && w.connected() && w.pin_caps & PIN_OUTPUT != 0 && !w.digital())
            .filter(|w| rank(w.device()) < 3)
            .collect();
        pins.sort_by_key(|w| (rank(w.device()), w.nid));
        let outputs: Vec<Path> = pins
            .iter()
            .filter_map(|pin| {
                let nodes = self.route(pin.nid, &|w| w.kind == Kind::Output && !w.digital(), &mut Vec::new())?;
                Some(Path { nodes, device: pin.device() })
            })
            .collect();
        self.outputs = outputs;

        // The microphone: one plugged into a jack, else a built-in one,
        // else line in.
        struct Candidate {
            nid: u16,
            device: Device,
            built_in: bool,
            plugged: bool,
        }
        let pins: Vec<(u16, Device, bool, bool)> = self
            .widgets
            .values()
            .filter(|w| w.kind == Kind::Pin && w.connected() && w.pin_caps & PIN_INPUT != 0 && !w.digital())
            .filter(|w| matches!(w.device(), Device::Microphone | Device::LineIn))
            .map(|w| (w.nid, w.device(), w.built_in(), w.pin_caps & PIN_PRESENCE != 0))
            .collect();
        let candidates: Vec<Candidate> = pins
            .into_iter()
            .map(|(nid, device, built_in, senses)| Candidate {
                nid,
                device,
                built_in,
                plugged: senses && link.send(nid, Verb::Short(GET_PIN_SENSE, 0)) & (1 << 31) != 0,
            })
            .collect();
        let preferences: [fn(&Candidate) -> bool; 3] = [
            |c| c.device == Device::Microphone && !c.built_in && c.plugged,
            |c| c.device == Device::Microphone && c.built_in,
            |_| true,
        ];
        let chosen = preferences.iter().find_map(|preferred| candidates.iter().find(|c| preferred(c)).map(|c| c.nid));
        self.input = chosen.and_then(|pin| {
            let device = self.widgets[&pin].device();
            self.widgets
                .values()
                .filter(|w| w.kind == Kind::Input && !w.digital())
                .find_map(|adc| self.route(adc.nid, &|w| w.nid == pin, &mut Vec::new()))
                .map(|nodes| Path { nodes, device })
        });
    }

    /// A route from `from` along connections to a widget that `goal`
    /// accepts, through mixers and selectors only (depth-first, short
    /// routes first).
    fn route(&self, from: u16, goal: &dyn Fn(&Widget) -> bool, seen: &mut Vec<u16>) -> Option<Vec<u16>> {
        const MOST_HOPS: usize = 6;
        if seen.len() >= MOST_HOPS || seen.contains(&from) {
            return None;
        }
        let widget = self.widgets.get(&from)?;
        seen.push(from);
        for &next in &widget.connections {
            let Some(target) = self.widgets.get(&next) else { continue };
            if goal(target) {
                return Some(alloc::vec![from, next]);
            }
        }
        for &next in &widget.connections {
            let Some(target) = self.widgets.get(&next) else { continue };
            if !matches!(target.kind, Kind::Mixer | Kind::Selector) {
                continue;
            }
            if let Some(mut rest) = self.route(next, goal, seen) {
                rest.insert(0, from);
                return Some(rest);
            }
        }
        seen.pop();
        None
    }

    /// Sets up every output route to play stream `tag` in `format`.
    pub fn start_output(&self, link: &mut impl Link, tag: u8, format: u16) {
        for path in &self.outputs {
            self.open_route(link, path, true);
            let dac = *path.nodes.last().expect("routes have two ends");
            link.send(dac, Verb::Long(SET_STREAM_FORMAT, format));
            link.send(dac, Verb::Short(SET_CHANNEL_STREAM, tag << 4));
        }
        self.restore_volume(link, true);
        self.switch_outputs(link);
    }

    /// Puts the volume amp back to the last setting (if there was one).
    fn restore_volume(&self, link: &mut impl Link, output: bool) {
        let level = self.levels[usize::from(!output)].load(Ordering::Relaxed);
        if level != 0 {
            self.set_volume(link, output, level & 0x7F, level & 1 << 31 != 0);
        }
    }

    /// Sets up the input route to record into stream `tag` in `format`.
    pub fn start_input(&self, link: &mut impl Link, tag: u8, format: u16) {
        if let Some(path) = &self.input {
            self.open_route(link, path, false);
            let adc = path.nodes[0];
            link.send(adc, Verb::Long(SET_STREAM_FORMAT, format));
            link.send(adc, Verb::Short(SET_CHANNEL_STREAM, tag << 4));
        }
        self.restore_volume(link, false);
    }

    /// Stops converters from taking part in a stream.
    pub fn stop(&self, link: &mut impl Link, output: bool) {
        let converters: Vec<u16> = if output {
            self.outputs.iter().filter_map(|path| path.nodes.last().copied()).collect()
        } else {
            self.input.iter().map(|path| path.nodes[0]).collect()
        };
        for converter in converters {
            link.send(converter, Verb::Short(SET_CHANNEL_STREAM, 0));
        }
    }

    /// Unmutes and selects along a route, and turns on its pin.
    fn open_route(&self, link: &mut impl Link, path: &Path, output: bool) {
        for (index, &nid) in path.nodes.iter().enumerate() {
            let widget = &self.widgets[&nid];
            // The widget this one takes its signal from (for output, the
            // next one toward the DAC; for input, toward the pin).
            let source = path.nodes.get(index + 1).copied();
            if let Some(source) = source
                && let Some(position) = widget.connections.iter().position(|&c| c == source)
            {
                match widget.kind {
                    Kind::Mixer => {
                        // Only our input: others (like a microphone looped
                        // back to the speakers) stay muted.
                        for other in 0..widget.connections.len() {
                            let unmuted = other == position;
                            self.set_amp(link, widget, false, other as u8, unmuted, amp_steps(widget.in_amp).zero);
                        }
                    }
                    _ => {
                        if widget.connections.len() > 1 {
                            link.send(nid, Verb::Short(SET_CONNECTION_SELECT, position as u8));
                        }
                        if widget.caps & CAP_IN_AMP != 0 {
                            let steps = amp_steps(widget.in_amp);
                            self.set_amp(link, widget, false, position as u8, true, steps.zero);
                        }
                    }
                }
            }
            if widget.caps & CAP_OUT_AMP != 0 {
                self.set_amp(link, widget, true, 0, true, amp_steps(widget.out_amp).zero);
            }
            if widget.kind == Kind::Pin {
                let control = if output {
                    PIN_OUT_ENABLE | if widget.device() == Device::Headphone { PIN_HEADPHONE } else { 0 }
                } else {
                    let vref = widget.device() == Device::Microphone && widget.pin_caps & PIN_VREF_80 != 0;
                    PIN_IN_ENABLE | if vref { PIN_CONTROL_VREF_80 } else { 0 }
                };
                link.send(nid, Verb::Short(SET_PIN_CONTROL, control));
                if output && widget.pin_caps & PIN_EAPD != 0 {
                    // External amplifier power (laptop speakers need it).
                    link.send(nid, Verb::Short(SET_EAPD, 0x02));
                }
            }
        }
    }

    fn set_amp(&self, link: &mut impl Link, widget: &Widget, output: bool, index: u8, unmuted: bool, gain: u32) {
        let direction = if output { 1 << 15 } else { 1 << 14 };
        let payload = direction
            | 3 << 12 // both channels
            | u16::from(index & 0xF) << 8
            | if unmuted { 0 } else { 1 << 7 }
            | (gain.min(0x7F) as u16);
        link.send(widget.nid, Verb::Long(SET_AMP, payload));
    }

    /// The amplifier that sets the playback volume: the first output amp on
    /// the first route, nearest the DAC.
    pub fn volume_amp(&self, output: bool) -> Option<(u16, bool, AmpSteps)> {
        if output {
            let path = self.outputs.first()?;
            path.nodes.iter().rev().find_map(|nid| {
                let widget = &self.widgets[nid];
                (widget.caps & CAP_OUT_AMP != 0 && amp_steps(widget.out_amp).count > 0)
                    .then(|| (*nid, true, amp_steps(widget.out_amp)))
            })
        } else {
            let path = self.input.as_ref()?;
            path.nodes.iter().find_map(|nid| {
                let widget = &self.widgets[nid];
                (widget.caps & CAP_IN_AMP != 0 && amp_steps(widget.in_amp).count > 0)
                    .then(|| (*nid, false, amp_steps(widget.in_amp)))
            })
        }
    }

    /// Sets a volume amplifier (from [`volume_amp`](Self::volume_amp)) to
    /// `step`, or mutes it. Every output route gets the same.
    pub fn set_volume(&self, link: &mut impl Link, output: bool, step: u32, unmuted: bool) {
        let Some((nid, _, _)) = self.volume_amp(output) else { return };
        // Never 0, so 0 means "not set yet".
        let level = step.min(0x7F) | if unmuted { 1 << 31 } else { 1 << 30 };
        self.levels[usize::from(!output)].store(level, Ordering::Relaxed);
        let targets: Vec<u16> = if output {
            // The same kind of amp on the other routes (their own DACs).
            let position = self.outputs[0].nodes.iter().position(|&n| n == nid).unwrap_or(0);
            let from_end = self.outputs[0].nodes.len() - position;
            self.outputs
                .iter()
                .filter_map(|path| path.nodes.len().checked_sub(from_end).map(|i| path.nodes[i]))
                .collect()
        } else {
            alloc::vec![nid]
        };
        for target in targets {
            let widget = &self.widgets[&target];
            let index = if output {
                0
            } else {
                let path = self.input.as_ref().expect("checked by volume_amp");
                let position = path.nodes.iter().position(|&n| n == target).unwrap_or(0);
                path.nodes
                    .get(position + 1)
                    .and_then(|source| widget.connections.iter().position(|c| c == source))
                    .unwrap_or(0) as u8
            };
            self.set_amp(link, widget, output, index, unmuted, step);
        }
        if output {
            self.switch_outputs(link);
        }
    }

    /// Whether playback is muted (by the volume setting, or because sound
    /// goes to a Bluetooth headset).
    fn output_muted(&self) -> bool {
        let level = self.levels[0].load(Ordering::Relaxed);
        level != 0 && level & 1 << 31 == 0
    }

    /// Turns each output pin on or off to match the mute setting and the
    /// headphone jack, and mutes or unmutes the mixers on the routes.
    ///
    /// Muting the volume amp alone isn't enough: on Realtek codecs (the
    /// T440p's ALC292) the DACs' amps can't mute at all (their amp
    /// capabilities have no mute bit), so the mute is done where it works,
    /// as Linux does: the pins' outputs (which always switch off), the
    /// pins' own amps, and the mixers' inputs.
    fn switch_outputs(&self, link: &mut impl Link) {
        let muted = self.output_muted();
        let headphones_in = self.headphones_in.load(Ordering::Relaxed);
        for path in &self.outputs {
            let on = !muted && !(headphones_in && path.device == Device::Speaker);
            let pin = &self.widgets[&path.nodes[0]];
            let control = if !on {
                0
            } else if pin.device() == Device::Headphone {
                PIN_OUT_ENABLE | PIN_HEADPHONE
            } else {
                PIN_OUT_ENABLE
            };
            link.send(pin.nid, Verb::Short(SET_PIN_CONTROL, control));
            if pin.pin_caps & PIN_EAPD != 0 {
                // The external amplifier (laptop speakers) powers down too.
                link.send(pin.nid, Verb::Short(SET_EAPD, if on { 0x02 } else { 0 }));
            }
            if pin.caps & CAP_OUT_AMP != 0 {
                self.set_amp(link, pin, true, 0, on, amp_steps(pin.out_amp).zero);
            }
            // Mixers are shared between routes (speakers and headphones
            // share one on the T440p), so they follow the mute alone.
            for pair in path.nodes.windows(2) {
                let widget = &self.widgets[&pair[0]];
                if widget.kind != Kind::Mixer {
                    continue;
                }
                if let Some(position) = widget.connections.iter().position(|&c| c == pair[1]) {
                    self.set_amp(link, widget, false, position as u8, !muted, amp_steps(widget.in_amp).zero);
                }
            }
        }
    }

    /// Headphone jacks that can tell whether something is plugged in.
    fn headphone_jacks(&self) -> impl Iterator<Item = &Widget> {
        self.widgets.values().filter(|w| {
            w.kind == Kind::Pin
                && w.connected()
                && w.device() == Device::Headphone
                && w.pin_caps & PIN_PRESENCE != 0
                && w.pin_caps & PIN_OUTPUT != 0
        })
    }

    /// A pin's sense reading (bit 31: something is plugged in).
    fn sense(&self, link: &mut impl Link, pin: &Widget) -> u32 {
        if pin.pin_caps & PIN_TRIGGER != 0 {
            link.send(pin.nid, Verb::Short(SET_PIN_SENSE, 0));
        }
        link.send(pin.nid, Verb::Short(GET_PIN_SENSE, 0))
    }

    /// Whether headphones are plugged in (so the speakers should be quiet).
    pub fn headphones_plugged(&self, link: &mut impl Link) -> bool {
        self.headphone_jacks().any(|pin| self.sense(link, pin) & (1 << 31) != 0)
    }

    /// The headphone jacks' pin capabilities and sense readings, for the
    /// log (automute can only be checked on real hardware).
    pub fn describe_jacks(&self, link: &mut impl Link) -> String {
        let jacks: Vec<String> = self
            .headphone_jacks()
            .map(|pin| {
                alloc::format!(
                    "{:#x} (config {:08x}, pin caps {:08x}, sense {:08x})",
                    pin.nid,
                    pin.config,
                    pin.pin_caps,
                    self.sense(link, pin)
                )
            })
            .collect();
        if jacks.is_empty() { String::from("none that sense plugs") } else { jacks.join(", ") }
    }

    /// Turns the speakers off (headphones plugged in) or back on: their
    /// pins' outputs, and their pins' own amplifiers too, since on some
    /// laptops (the T440p) speakers and headphones share a DAC and mixer.
    pub fn set_speakers(&self, link: &mut impl Link, on: bool) {
        self.headphones_in.store(!on, Ordering::Relaxed);
        self.switch_outputs(link);
    }

    /// The amps on the output routes and whether each can mute (bit 31 of
    /// its capabilities), for the log: what muting relies on.
    pub fn describe_amps(&self) -> String {
        let mut seen = Vec::new();
        let mut parts = Vec::new();
        for path in &self.outputs {
            for nid in &path.nodes {
                let widget = &self.widgets[nid];
                if seen.contains(nid) {
                    continue;
                }
                seen.push(*nid);
                for (has, caps, name) in [(CAP_IN_AMP, widget.in_amp, "in"), (CAP_OUT_AMP, widget.out_amp, "out")] {
                    if widget.caps & has != 0 {
                        let mute = if caps & 1 << 31 != 0 { "mutes" } else { "can't mute" };
                        parts.push(alloc::format!("{nid:#x} {name} {caps:08x} ({mute})"));
                    }
                }
            }
        }
        parts.join(", ")
    }

    /// A one-line description of the routes, for the log.
    pub fn describe(&self) -> String {
        let route = |path: &Path| {
            let nodes: Vec<String> = path.nodes.iter().map(|nid| alloc::format!("{nid:#x}")).collect();
            alloc::format!("{} {}", path.device.name(), nodes.join("-"))
        };
        let mut parts: Vec<String> = self.outputs.iter().map(route).collect();
        if let Some(input) = &self.input {
            parts.push(route(input));
        }
        if parts.is_empty() {
            parts.push(String::from("no usable routes"));
        }
        alloc::format!("{:04x}:{:04x}, {}", self.vendor >> 16, self.vendor & 0xFFFF, parts.join(", "))
    }
}

/// Reads a widget's connection list (short or long form entries, with
/// ranges).
fn connection_list(link: &mut impl Link, nid: u16) -> Vec<u16> {
    let info = parameter(link, nid, PARAM_CONNECTION_LENGTH);
    let length = (info & 0x7F) as usize;
    let long = info & 0x80 != 0;
    let (per_response, bits) = if long { (2, 16) } else { (4, 8) };
    let mut list: Vec<u16> = Vec::new();
    let mut index = 0;
    while index < length {
        let response = link.send(nid, Verb::Short(GET_CONNECTION_LIST, index as u8));
        for slot in 0..per_response {
            if index + slot >= length {
                break;
            }
            let entry = (response >> (slot * bits)) & ((1 << bits) - 1);
            let range = entry & (1 << (bits - 1)) != 0;
            let value = (entry & ((1 << (bits - 1)) - 1)) as u16;
            match (range, list.last().copied()) {
                (true, Some(previous)) if value > previous => list.extend(previous + 1..=value),
                _ => list.push(value),
            }
        }
        index += per_response;
    }
    // Which one is selected now doesn't matter; reading it keeps some
    // codecs' state consistent with what we'll set.
    let _ = link.send(nid, Verb::Short(GET_CONNECTION_SELECT, 0));
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A simulated codec shaped like the ALC292 in a ThinkPad: DACs 0x02
    /// and 0x03, mixers 0x0C and 0x0D, speaker pin 0x14, headphone jack
    /// 0x21, ADC 0x08 behind mixer 0x23, internal microphone 0x12 and a
    /// microphone jack 0x18. It records the verbs it gets. Its amps are
    /// the real one's: the DACs' have steps but can't mute, the speaker
    /// pin's and the output mixers' can only mute.
    struct Simulated {
        widgets: BTreeMap<u16, (u32, Vec<u16>, u32, u32)>,
        sent: Vec<(u16, u32)>,
        /// Whether the headphone jack (0x21) senses a plug.
        headphones_in: bool,
    }

    impl Simulated {
        fn thinkpad() -> Self {
            let mut widgets = BTreeMap::new();
            let wcaps = |kind: u32, extra: u32| kind << 20 | extra;
            widgets.insert(0x02, (wcaps(0, CAP_OUT_AMP), alloc::vec![], 0, 0));
            widgets.insert(0x03, (wcaps(0, CAP_OUT_AMP), alloc::vec![], 0, 0));
            widgets.insert(0x08, (wcaps(1, CAP_IN_AMP | CAP_CONNECTIONS), alloc::vec![0x23], 0, 0));
            let mixer = CAP_IN_AMP | CAP_CONNECTIONS | CAP_AMP_OVERRIDE;
            widgets.insert(0x0C, (wcaps(2, mixer), alloc::vec![0x02, 0x0B], 0, 0));
            widgets.insert(0x0D, (wcaps(2, mixer), alloc::vec![0x03, 0x0B], 0, 0));
            widgets.insert(0x0B, (wcaps(2, CAP_IN_AMP | CAP_CONNECTIONS), alloc::vec![0x18, 0x12], 0, 0));
            widgets.insert(0x23, (wcaps(2, CAP_IN_AMP | CAP_CONNECTIONS), alloc::vec![0x18, 0x12, 0x0B], 0, 0));
            // Pins: (caps, connections, pin caps, configuration default).
            let output = PIN_OUTPUT | PIN_EAPD;
            let speaker = CAP_CONNECTIONS | CAP_OUT_AMP | CAP_AMP_OVERRIDE;
            widgets.insert(0x14, (wcaps(4, speaker), alloc::vec![0x0C], output, 0x9017_0110));
            let headphone = PIN_OUTPUT | PIN_PRESENCE;
            widgets.insert(0x21, (wcaps(4, CAP_CONNECTIONS), alloc::vec![0x0D], headphone, 0x0221_1020));
            widgets.insert(0x12, (wcaps(4, 0), alloc::vec![], PIN_INPUT, 0x90A6_0130));
            let jack = PIN_INPUT | PIN_PRESENCE | PIN_VREF_80;
            widgets.insert(0x18, (wcaps(4, 0), alloc::vec![], jack, 0x03A1_1040));
            // An unconnected pin, which must be ignored.
            widgets.insert(0x17, (wcaps(4, CAP_CONNECTIONS), alloc::vec![0x0C], output, 0x4000_0000));
            Self { widgets, sent: Vec::new(), headphones_in: false }
        }
    }

    impl Link for Simulated {
        fn send(&mut self, nid: u16, verb: Verb) -> u32 {
            let code = match verb {
                Verb::Short(verb, payload) => u32::from(verb) << 8 | u32::from(payload),
                Verb::Long(verb, payload) => u32::from(verb) << 16 | u32::from(payload),
            };
            self.sent.push((nid, code));
            match (nid, verb) {
                (0, Verb::Short(GET_PARAMETER, PARAM_VENDOR)) => 0x10EC_0292,
                (0, Verb::Short(GET_PARAMETER, PARAM_NODE_COUNT)) => 0x0001_0001,
                (1, Verb::Short(GET_PARAMETER, PARAM_FUNCTION_TYPE)) => 0x01,
                (1, Verb::Short(GET_PARAMETER, PARAM_NODE_COUNT)) => 0x0002_0022,
                // Steps (0x57 of them, 0 dB at 0x57), no mute.
                (1, Verb::Short(GET_PARAMETER, PARAM_OUT_AMP_CAPS)) => 0x0005_5757,
                (1, Verb::Short(GET_PARAMETER, PARAM_IN_AMP_CAPS)) => 0x0003_3F00,
                (nid, Verb::Short(verb, parameter)) => {
                    let Some((caps, connections, pin_caps, config)) = self.widgets.get(&nid) else { return 0 };
                    match (verb, parameter) {
                        (GET_PARAMETER, PARAM_WIDGET_CAPS) => *caps,
                        (GET_PARAMETER, PARAM_PIN_CAPS) => *pin_caps,
                        (GET_PARAMETER, PARAM_CONNECTION_LENGTH) => connections.len() as u32,
                        // Mute only.
                        (GET_PARAMETER, PARAM_OUT_AMP_CAPS | PARAM_IN_AMP_CAPS) => 0x8000_0000,
                        (GET_CONNECTION_LIST, index) => connections
                            .iter()
                            .skip(index as usize)
                            .take(4)
                            .enumerate()
                            .fold(0, |word, (slot, &c)| word | u32::from(c) << (slot * 8)),
                        (GET_CONFIG_DEFAULT, _) => *config,
                        // The microphone jack has something plugged in.
                        (GET_PIN_SENSE, _) if nid == 0x18 => 1 << 31,
                        (GET_PIN_SENSE, _) if nid == 0x21 && self.headphones_in => 1 << 31,
                        _ => 0,
                    }
                }
                _ => 0,
            }
        }
    }

    crate::ktest! {
        fn codec_routes_are_found_like_a_laptops() {
            let mut link = Simulated::thinkpad();
            let codec = Codec::discover(&mut link, 0).expect("an audio function group");
            assert_eq!(codec.vendor, 0x10EC_0292);
            // Speaker first, then headphones; the unconnected pin is skipped.
            let routes: Vec<(Device, Vec<u16>)> = codec.outputs.iter().map(|p| (p.device, p.nodes.clone())).collect();
            assert_eq!(
                routes,
                alloc::vec![
                    (Device::Speaker, alloc::vec![0x14, 0x0C, 0x02]),
                    (Device::Headphone, alloc::vec![0x21, 0x0D, 0x03]),
                ]
            );
            // The plugged-in microphone jack wins over the built-in one.
            let input = codec.input.as_ref().expect("a microphone route");
            assert_eq!(input.nodes, alloc::vec![0x08, 0x23, 0x18]);
        }

        fn headphones_mute_the_speakers() {
            let mut link = Simulated::thinkpad();
            let codec = Codec::discover(&mut link, 0).unwrap();
            assert!(!codec.headphones_plugged(&mut link));
            link.headphones_in = true;
            assert!(codec.headphones_plugged(&mut link));
            link.sent.clear();
            codec.set_speakers(&mut link, false);
            // The speaker pin's output is off and its amplifier muted; the
            // headphones and the mixers stay on.
            assert!(link.sent.contains(&(0x14, 0x707 << 8)));
            assert!(link.sent.contains(&(0x14, 0x3 << 16 | 0xB080)));
            assert!(link.sent.contains(&(0x21, 0x707 << 8 | 0xC0)));
            assert!(link.sent.contains(&(0x0C, 0x3 << 16 | 0x7000)));
            link.sent.clear();
            codec.set_speakers(&mut link, true);
            assert!(link.sent.contains(&(0x14, 0x707 << 8 | 0x40)));
            assert!(link.sent.contains(&(0x14, 0x3 << 16 | 0xB000)));
        }

        fn codec_routes_are_opened() {
            let mut link = Simulated::thinkpad();
            let codec = Codec::discover(&mut link, 0).unwrap();
            link.sent.clear();
            codec.start_output(&mut link, 1, 0x11);
            let sent = &link.sent;
            // The speaker's pin is enabled with its amplifier powered (EAPD).
            assert!(sent.contains(&(0x14, 0x707 << 8 | 0x40)));
            assert!(sent.contains(&(0x14, 0x70C << 8 | 0x02)));
            // Headphones get the headphone amp.
            assert!(sent.contains(&(0x21, 0x707 << 8 | 0xC0)));
            // Mixer 0x0C passes the DAC (input 0) and mutes the loopback (input 1).
            assert!(sent.contains(&(0x0C, 0x3 << 16 | 0x7000)));
            assert!(sent.contains(&(0x0C, 0x3 << 16 | 0x7180)));
            // Both DACs join stream 1 in the format given.
            for dac in [0x02, 0x03] {
                assert!(sent.contains(&(dac, 0x706 << 8 | 0x10)));
                assert!(sent.contains(&(dac, 0x2 << 16 | 0x11)));
            }
        }

        fn starting_a_stream_keeps_the_volume() {
            let mut link = Simulated::thinkpad();
            let codec = Codec::discover(&mut link, 0).unwrap();
            // Muted at step 0x20 (as while sound goes to a headset).
            codec.set_volume(&mut link, true, 0x20, false);
            link.sent.clear();
            codec.start_output(&mut link, 1, 0x11);
            // Opening the routes unmutes the DACs' amps, then the setting
            // comes back last, so that is what they're left at.
            for dac in [0x02u16, 0x03] {
                let last = link.sent.iter().rev().find(|&&(nid, code)| nid == dac && code >> 16 == 0x3);
                assert_eq!(last, Some(&(dac, 0x3 << 16 | 0xB0A0)));
            }
            // The DACs' amps can't mute, so nothing may play: every pin's
            // output is off, the speaker's amp and amplifier (EAPD) off,
            // and the mixers' inputs from the DACs muted.
            let last = |link: &Simulated, nid: u16, kind: u32| {
                link.sent.iter().rev().find(|&&(n, code)| n == nid && code & kind == kind).map(|&(_, code)| code)
            };
            let pin_control = (SET_PIN_CONTROL as u32) << 8;
            let eapd = (SET_EAPD as u32) << 8;
            for pin in [0x14, 0x21] {
                assert_eq!(last(&link, pin, pin_control).map(|c| c & 0xFF), Some(0));
            }
            assert_eq!(last(&link, 0x14, eapd), Some(eapd));
            assert_eq!(last(&link, 0x14, 0x3 << 16 | 0x8000), Some(0x3 << 16 | 0xB080));
            assert_eq!(last(&link, 0x0C, 0x3 << 16 | 0x4000), Some(0x3 << 16 | 0x7080));
            assert_eq!(last(&link, 0x0D, 0x3 << 16 | 0x4000), Some(0x3 << 16 | 0x7080));
            // Unmuting (the headset gone) turns them all back on.
            link.sent.clear();
            codec.set_volume(&mut link, true, 0x20, true);
            assert!(link.sent.contains(&(0x14, 0x707 << 8 | 0x40)));
            assert!(link.sent.contains(&(0x14, 0x70C << 8 | 0x02)));
            assert!(link.sent.contains(&(0x21, 0x707 << 8 | 0xC0)));
            assert!(link.sent.contains(&(0x0C, 0x3 << 16 | 0x7000)));
            assert!(link.sent.contains(&(0x14, 0x3 << 16 | 0xB000)));
        }
    }
}
