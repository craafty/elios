//! The firmware's commands, built as the byte layouts the 7260's firmware
//! (version 17) expects. Each command has had several layouts over the
//! years; these are the ones this firmware's API flags select (checked
//! against Linux's iwlwifi, which documents them).

use alloc::vec::Vec;

// Command numbers (group 0 unless noted).
pub const PHY_CONTEXT: u8 = 0x08;
pub const ADD_STATION: u8 = 0x18;
pub const TX: u8 = 0x1C;
pub const QUEUE_CONFIG: u8 = 0x1D;
pub const MAC_CONTEXT: u8 = 0x28;
pub const TIME_EVENT: u8 = 0x29;
pub const TIME_EVENT_NOTIFICATION: u8 = 0x2A;
pub const BINDING: u8 = 0x2B;
pub const TIME_QUOTA: u8 = 0x2C;
pub const LINK_QUALITY: u8 = 0x4E;
pub const SCAN_REQUEST: u8 = 0x51;
pub const SCAN_ABORT: u8 = 0x52;
pub const PHY_DB: u8 = 0x6C;
pub const SCAN_COMPLETE: u8 = 0x6D;
pub const DEVICE_POWER: u8 = 0x77;
pub const THERMAL_BACKOFF: u8 = 0x7E;
pub const STATISTICS: u8 = 0x9D;
pub const MISSED_BEACONS: u8 = 0xA2;
pub const RX_PHY: u8 = 0xC0;
pub const RX_MPDU: u8 = 0xC1;
pub const SMART_FIFO: u8 = 0xD1;
/// In group 5 ("data path").
pub const DQA_ENABLE: u8 = 0x00;
pub const DATA_PATH_GROUP: u8 = 5;

/// Context actions.
pub const ADD: u32 = 1;
pub const MODIFY: u32 = 2;
pub const REMOVE: u32 = 3;
/// An unused context slot.
pub const INVALID: u32 = 0xFFFF_FFFF;

/// Station table entries, and the transmit queues ("DQA" layout).
pub const AUX_STATION: u8 = 0;
pub const AP_STATION: u8 = 1;
pub const AUX_QUEUE: u32 = 1;
pub const DATA_QUEUE: u32 = 4;
pub const MANAGEMENT_QUEUE: u32 = 5;
/// The hardware FIFOs queues feed (by access category).
pub const FIFO_BEST_EFFORT: u8 = 1;
pub const FIFO_VOICE: u8 = 3;
pub const FIFO_MULTICAST: u8 = 5;
/// The "TID" for management frames.
pub const MANAGEMENT_TID: u8 = 8;
/// The MAC context index for the aux station (scanning).
const AUX_MAC: u32 = 4;

/// Our one MAC, PHY and binding context: id 0, color 0.
pub const CONTEXT_ID: u32 = 0;

/// Little-endian byte building.
#[derive(Default)]
pub struct Writer(pub Vec<u8>);

impl Writer {
    pub fn u8(&mut self, value: u8) -> &mut Self {
        self.0.push(value);
        self
    }
    pub fn u16(&mut self, value: u16) -> &mut Self {
        self.0.extend_from_slice(&value.to_le_bytes());
        self
    }
    pub fn u32(&mut self, value: u32) -> &mut Self {
        self.0.extend_from_slice(&value.to_le_bytes());
        self
    }
    pub fn u64(&mut self, value: u64) -> &mut Self {
        self.0.extend_from_slice(&value.to_le_bytes());
        self
    }
    pub fn bytes(&mut self, value: &[u8]) -> &mut Self {
        self.0.extend_from_slice(value);
        self
    }
    pub fn zeros(&mut self, count: usize) -> &mut Self {
        self.0.resize(self.0.len() + count, 0);
        self
    }
}

/// Smart FIFO states: off before associating, "full on" after.
pub enum SmartFifo {
    InitOff,
    FullOn { streams: u8 },
}

/// SF_CFG: when the card may hold received frames to save power.
pub fn smart_fifo(state: SmartFifo) -> Vec<u8> {
    let mut w = Writer::default();
    match state {
        SmartFifo::InitOff => {
            w.u32(3).u32(4096).u32(8192);
            for _ in 0..10 {
                w.u32(1_000_000);
            }
            for _ in 0..5 {
                w.u32(400).u32(160);
            }
        }
        SmartFifo::FullOn { streams } => {
            // The watermark depends on how many streams the link uses.
            let watermark = match streams {
                2 => 8192,
                1 => 6144,
                _ => 4096,
            };
            w.u32(1).u32(4096).u32(watermark);
            for _ in 0..10 {
                w.u32(1_000_000);
            }
            for (aging, idle) in [(2016, 320), (2016, 320), (10016, 2016), (2016, 320), (2016, 320)] {
                w.u32(aging).u32(idle);
            }
        }
    }
    w.0
}

/// SCD_QUEUE_CFG: connects a transmit queue to a station, TID and FIFO.
pub fn queue_config(queue: u32, station: u8, tid: u8, fifo: u8, ssn: u16) -> Vec<u8> {
    let mut w = Writer::default();
    // Token, station, TID, queue, action (enable), no aggregation, FIFO,
    // window.
    w.u8(0).u8(station).u8(tid).u8(queue as u8).u8(1).u8(0).u8(fifo).u8(64);
    w.u16(ssn).u16(0);
    w.0
}

// Station flags.
pub const STATION_MIMO2: u32 = 1 << 28;
const STATION_FLAGS_MASK: u32 = 3 << 26 | 3 << 28 | 1 << 17;
const STATION_HT_FLAGS_MASK: u32 = 0xF << 19 | 7 << 23;

/// What ADD_STA says about a station.
pub struct Station<'a> {
    pub id: u8,
    pub mac_id: u32,
    pub address: &'a [u8; 6],
    pub modify: bool,
    pub queues: u32,
    pub flags: u32,
    pub ht: bool,
    pub association_id: u16,
}

/// ADD_STA (version 7, 44 bytes).
pub fn add_station(station: &Station) -> Vec<u8> {
    let mut w = Writer::default();
    let mask = STATION_FLAGS_MASK | if station.ht { STATION_HT_FLAGS_MASK } else { 0 };
    // Add or modify, awake ACs, TIDs with aggregation off (all).
    w.u8(station.modify.into()).u8(0).u16(0xFFFF);
    w.u32(station.mac_id).bytes(station.address).u16(0);
    // Station id; "modify mask" (queues), reserved.
    w.u8(station.id).u8(if station.modify { 0 } else { 1 << 7 }).u16(0);
    w.u32(station.flags).u32(mask);
    // Block-ack changes (none), sleep counts and flags.
    w.u8(0).u8(0).u16(0).u16(0).u16(0);
    w.u16(station.association_id).u16(0).u32(station.queues);
    w.0
}

/// The aux station, which scanning sends probe requests from.
pub fn aux_station() -> Vec<u8> {
    add_station(&Station {
        id: AUX_STATION,
        mac_id: AUX_MAC,
        address: &[0; 6],
        modify: false,
        queues: 1 << AUX_QUEUE,
        flags: 0,
        ht: false,
        association_id: 0,
    })
}

/// A channel: band (true for 2.4 GHz) and number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Channel {
    pub number: u8,
}

impl Channel {
    pub fn is_2ghz(self) -> bool {
        self.number <= 14
    }
    pub fn band(self) -> u8 {
        if self.is_2ghz() { 1 } else { 0 }
    }
}

/// PHY_CONTEXT (version 1): which channel the radio is on, 20 MHz wide.
pub fn phy_context(action: u32, channel: Channel) -> Vec<u8> {
    let mut w = Writer::default();
    w.u32(CONTEXT_ID).u32(action).u32(0).u32(0);
    w.u8(channel.band()).u8(channel.number).u8(0).u8(0);
    // Transmit on both antennas; receive on both, two chains idle and
    // active (diversity, since power saving is off).
    w.u32(0x3).u32(0x3 << 1 | 2 << 10 | 2 << 12).u32(0).u32(0);
    w.0
}

/// BINDING (version 1): ties our MAC context to the PHY context.
pub fn binding(action: u32) -> Vec<u8> {
    let mut w = Writer::default();
    w.u32(CONTEXT_ID).u32(action).u32(CONTEXT_ID).u32(INVALID).u32(INVALID).u32(CONTEXT_ID);
    w.0
}

/// TIME_QUOTA: all the radio's time to our binding.
pub fn time_quota() -> Vec<u8> {
    let mut w = Writer::default();
    w.u32(CONTEXT_ID).u32(128).u32(0);
    for _ in 1..4 {
        w.u32(INVALID).u32(0).u32(0);
    }
    w.0
}

/// One access category's contention settings, from the access point's
/// WMM element (or the defaults).
#[derive(Clone, Copy, Debug)]
pub struct Edca {
    pub cw_min: u16,
    pub cw_max: u16,
    pub aifsn: u8,
    /// In units of 32 microseconds, as WMM gives it.
    pub txop: u16,
}

/// The standard's defaults, in the firmware's order: background, best
/// effort, video, voice.
pub const DEFAULT_EDCA: [Edca; 4] = [
    Edca { cw_min: 15, cw_max: 1023, aifsn: 7, txop: 0 },
    Edca { cw_min: 15, cw_max: 1023, aifsn: 3, txop: 0 },
    Edca { cw_min: 7, cw_max: 15, aifsn: 2, txop: 94 },
    Edca { cw_min: 3, cw_max: 7, aifsn: 2, txop: 47 },
];

/// What the MAC context command says about the network.
pub struct MacContext<'a> {
    pub action: u32,
    pub address: &'a [u8; 6],
    pub bssid: &'a [u8; 6],
    pub cck_rates: u32,
    pub ofdm_rates: u32,
    pub protection: u32,
    pub short_preamble: bool,
    pub short_slot: bool,
    pub ht: bool,
    pub edca: [Edca; 4],
    pub beacon_interval: u16,
    pub dtim_period: u8,
    pub association_id: u16,
    /// None until associated and a beacon gave the DTIM timing.
    pub dtim: Option<Dtim>,
}

/// When the next beacon with buffered group traffic ("DTIM") comes: by the
/// access point's clock (TSF) and by the card's.
pub struct Dtim {
    pub tsf: u64,
    pub time: u32,
    /// When the beacon the timing came from arrived (the card's clock).
    pub beacon_time: u32,
}

/// MAC_CONTEXT_CMD for a station (148 bytes).
pub fn mac_context(context: &MacContext) -> Vec<u8> {
    let mut w = Writer::default();
    // Id and color, action, type (BSS station), TSF timer 0.
    w.u32(CONTEXT_ID).u32(context.action).u32(5).u32(0);
    w.bytes(context.address).u16(0).bytes(context.bssid).u16(0);
    w.u32(context.cck_rates).u32(context.ofdm_rates).u32(context.protection);
    w.u32(if context.short_preamble { 1 << 5 } else { 0 });
    w.u32(if context.short_slot { 1 << 4 } else { 0 });
    // Accept group frames; let beacons through until the DTIM timing is
    // known.
    let filter = 1 << 2 | if context.dtim.is_none() { 1 << 6 } else { 0 };
    w.u32(filter);
    w.u32(1 | if context.ht { 1 << 1 } else { 0 });
    for (index, edca) in context.edca.iter().enumerate() {
        w.u16(edca.cw_min).u16(edca.cw_max).u8(edca.aifsn).u8(1 << index).u16(edca.txop * 32);
    }
    w.zeros(8);
    let mut station = Writer::default();
    let interval = u32::from(context.beacon_interval);
    match &context.dtim {
        Some(dtim) => station.u32(1).u32(dtim.time).u64(dtim.tsf),
        None => station.zeros(16),
    };
    station.u32(interval).u32(0).u32(interval * u32::from(context.dtim_period));
    // Data policy, listen interval, association id, beacon arrival.
    station.u32(0).u32(LISTEN_INTERVAL.into()).u32(context.association_id.into());
    station.u32(context.dtim.as_ref().map_or(0, |dtim| dtim.beacon_time));
    station.0.resize(48, 0);
    w.bytes(&station.0);
    w.0
}

/// How many beacon intervals the access point should keep frames for us
/// (we never sleep, but the field is required).
pub const LISTEN_INTERVAL: u16 = 10;

/// TIME_EVENT_CMD: asks the firmware to stay on the channel (`duration`
/// time units) while associating.
pub fn time_event(action: u32, duration: u32, event_id: u32) -> Vec<u8> {
    let mut w = Writer::default();
    // Id and color, action, event id (when removing; "BSS station
    // aggressive association" type otherwise), apply now, max delay,
    // depends on nothing, interval 1, duration, once, unfragmented,
    // notify start and end, start now.
    let id = if action == REMOVE { event_id } else { 0 };
    w.u32(CONTEXT_ID).u32(action).u32(id).u32(0).u32(500).u32(0).u32(1).u32(duration);
    w.u8(1).u8(0).u16(1 | 1 << 1 | 1 << 11);
    w.0
}

/// TX rate values ("rate_n_flags", version 1).
pub mod rate {
    pub const CCK: u32 = 1 << 9;
    pub const HT: u32 = 1 << 8;
    pub const ANTENNA_A: u32 = 1 << 14;
    pub const ANTENNA_AB: u32 = 3 << 14;
    /// 1 Mbit/s (CCK), the lowest 2.4 GHz rate.
    pub const CCK_1M: u32 = 10 | CCK | ANTENNA_A;
    /// 6 Mbit/s (OFDM), the lowest 5 GHz rate.
    pub const OFDM_6M: u32 = 13 | ANTENNA_A;

    /// The signal ("PLCP") value of a legacy rate in units of 500 kbit/s,
    /// with its flags.
    pub fn legacy(rate: u8) -> Option<u32> {
        Some(match rate {
            2 => 10 | CCK,
            4 => 20 | CCK,
            11 => 55 | CCK,
            22 => 110 | CCK,
            12 => 13,
            18 => 15,
            24 => 5,
            36 => 7,
            48 => 9,
            72 => 11,
            96 => 1,
            108 => 3,
            _ => return None,
        })
    }
}

/// LQ_CMD: the rates the firmware tries for a station's data frames, best
/// first (each tried twice before moving down).
pub fn link_quality(station: u8, rates: &[u32], mimo: bool) -> Vec<u8> {
    let mut w = Writer::default();
    // Station, power reduction, control, flags, MIMO delimiter (the first
    // entry that isn't two-stream), antennas for one and two streams.
    let mimo_entries = rates.iter().take(16).filter(|&&rate| rate & rate::HT != 0 && rate & 0x8 != 0).count();
    w.u8(station).u8(0).u16(0).u8(0).u8(if mimo { mimo_entries as u8 } else { 0 });
    w.u8(1).u8(3);
    w.u8(0).u8(0).u8(0).u8(0);
    // Aggregation limits (unused: no aggregation), reserved.
    w.u16(4000).u8(3).u8(64).u32(0);
    for index in 0..16 {
        w.u32(rates[index.min(rates.len() - 1)]);
    }
    w.u32(0);
    w.0
}

/// SCAN_OFFLOAD_REQUEST_CMD (the "LMAC" scan): probes for `ssid` (any
/// network if empty) on `channels`.
pub fn scan_request(channels: &[Channel], max_channels: usize, mac: &[u8; 6], ssid: &[u8]) -> Vec<u8> {
    let mut w = Writer::default();
    let rx_chain = 3u16 << 1 | 3 << 7 | 3 << 4 | 1;
    // Reserved, channel count, dwell times (active, passive, fragmented,
    // extended), reserved, receive chains.
    w.u32(0).u8(channels.len() as u8).u8(10).u8(110).u8(44).u8(90).u8(0).u16(rx_chain);
    // Flags: extended dwell, and "pre-connection" for a named network;
    // report every network ("pass all").
    let scan_flags = 1 << 0 | 1 << 7 | if ssid.is_empty() { 0 } else { 1 << 2 };
    // Max time away and suspend time (unassociated: 0), band of the first
    // channel, filter (group frames and beacons).
    w.u32(scan_flags).u32(0).u32(0).u32(channels.first().map_or(1, |c| c.band()).into()).u32(1 << 2 | 1 << 6);
    // Probe requests from the aux station: firmware sequence numbers, no
    // Bluetooth; 1 Mbit/s on 2.4 GHz, 6 Mbit/s on 5 GHz.
    for rate in [rate::CCK_1M, rate::OFDM_6M] {
        w.u32(1 << 13 | 1 << 12).u32(rate).u8(AUX_STATION).zeros(3);
    }
    // The SSIDs to probe for (one, or none: the wildcard).
    w.u8(0).u8(ssid.len() as u8).bytes(ssid).zeros(32 - ssid.len());
    w.zeros(19 * 34);
    // Priority, one iteration, no delay; schedule: once.
    w.u32(6).u32(1).u32(0);
    w.u16(0).u8(1).u8(1).zeros(4);
    // No "energy based scan" options.
    w.zeros(8);
    for index in 0..max_channels {
        match channels.get(index) {
            // "Partial" channel, probing with SSID entry 0.
            Some(channel) => w.u32(1 << 28 | 1 << 1).u16(channel.number.into()).u16(1).u32(0),
            None => w.zeros(12),
        };
    }
    // The probe request: a header (the SSID element follows it), 2.4 GHz
    // elements, 5 GHz elements, common elements.
    let mut probe = Writer::default();
    probe.u16(0x0040).u16(0).bytes(&[0xFF; 6]).bytes(mac).bytes(&[0xFF; 6]).u16(0);
    probe.u8(0).u8(0);
    let header_len = probe.0.len();
    probe.bytes(&super::ieee80211::supported_rates(true));
    let band_2 = probe.0.len();
    probe.bytes(&super::ieee80211::supported_rates(false));
    let band_5 = probe.0.len();
    probe.bytes(&super::ieee80211::ht_capabilities());
    let end = probe.0.len();
    w.u16(0).u16(header_len as u16);
    w.u16(header_len as u16).u16((band_2 - header_len) as u16);
    w.u16(band_2 as u16).u16((band_5 - band_2) as u16);
    w.u16(band_5 as u16).u16((end - band_5) as u16);
    probe.0.resize(512, 0);
    w.bytes(&probe.0);
    w.0
}

// TX command flags.
pub const TX_ACK: u32 = 1 << 3;
pub const TX_STATION_RATE: u32 = 1 << 4;
pub const TX_NO_BLUETOOTH: u32 = 1 << 12;
pub const TX_FIRMWARE_SEQUENCE: u32 = 1 << 13;
pub const TX_PADDED_HEADER: u32 = 1 << 20;

/// What the TX command says about a frame.
pub struct TxFrame {
    pub len: u16,
    pub flags: u32,
    /// Ignored with [`TX_STATION_RATE`].
    pub rate: u32,
    pub station: u8,
    pub tid: u8,
    pub management: bool,
    pub association_request: bool,
    /// EAPOL: sent as data but with management's power-save timeout.
    pub port_control: bool,
    /// The physical address of the TX command's "scratch" field, which the
    /// firmware writes back to.
    pub scratch: u64,
}

/// TX_CMD's parameters (version 6, 56 bytes); the 802.11 header follows.
pub fn tx_parameters(frame: &TxFrame) -> Vec<u8> {
    let mut w = Writer::default();
    w.u16(frame.len).u16(0).u32(frame.flags).u32(0).u32(frame.rate);
    w.u8(frame.station).u8(0).u8(0).u8(0).zeros(16).u32(0);
    // Lifetime (infinite), the scratch address.
    w.u32(0xFFFF_FFFF).u32(frame.scratch as u32).u8((frame.scratch >> 32) as u8);
    // Retry limits: RTS, then data (fewer for management frames).
    w.u8(60).u8(if frame.management { 3 } else { 15 }).u8(frame.tid);
    let power_save_timeout = if frame.association_request {
        3
    } else if frame.management || frame.port_control {
        2
    } else {
        0
    };
    w.u16(power_save_timeout).u16(0);
    w.0
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::ktest! {
        fn commands_have_the_firmwares_sizes() {
            assert_eq!(smart_fifo(SmartFifo::InitOff).len(), 4 + 8 + 40 + 40);
            assert_eq!(queue_config(4, 1, 0, 1, 0).len(), 12);
            assert_eq!(aux_station().len(), 44);
            assert_eq!(phy_context(ADD, Channel { number: 6 }).len(), 36);
            assert_eq!(binding(ADD).len(), 24);
            assert_eq!(time_quota().len(), 48);
            let context = MacContext {
                action: ADD,
                address: &[0; 6],
                bssid: &[0; 6],
                cck_rates: 0xF,
                ofdm_rates: 0x15,
                protection: 0,
                short_preamble: true,
                short_slot: true,
                ht: true,
                edca: DEFAULT_EDCA,
                beacon_interval: 100,
                dtim_period: 1,
                association_id: 0,
                dtim: None,
            };
            assert_eq!(mac_context(&context).len(), 148);
            assert_eq!(time_event(ADD, 600, 0).len(), 36);
            assert_eq!(link_quality(1, &[rate::CCK_1M], false).len(), 88);
            let channels = [Channel { number: 1 }, Channel { number: 36 }];
            assert_eq!(scan_request(&channels, 40, &[0; 6], b"home").len(), 764 + 40 * 12 + 16 + 512);
            let frame = TxFrame {
                len: 24,
                flags: 0,
                rate: 0,
                station: 1,
                tid: 8,
                management: true,
                association_request: false,
                port_control: false,
                scratch: 0,
            };
            assert_eq!(tx_parameters(&frame).len(), 56);
        }
    }
}
