//! Mixer, effect and output state, and its translation into control requests.
//!
//! The firmware's GET requests return canned values (docs/protocol.md, GET behavior),
//! so this state is the only record of what the device is doing. The daemon writes all
//! of it to the device on connect and persists it to disk.
//!
//! Levels are dB, `None` means off. Pan runs from -1 (left) to 1 (right). Effect
//! volume, duration and feedback run from 0 to 1, as in the vendor control panel.

use serde::{Deserialize, Serialize};

pub const SOURCES: usize = 16; // in1..in8 (in7/in8 = S/PDIF), daw1..daw8
pub const BUSES: usize = 4; // outputs 1/2, 3/4, 5/6, 7/8 (S/PDIF)
pub const OUTPUTS: usize = 8;
pub const RETURNS: usize = 4; // effect return reaches outputs 1 to 4 only

const UNIT_MIXER: u8 = 5;
const UNIT_FX: u8 = 6;
const UNIT_RETURN: u8 = 7;
const UNIT_TRIM: u8 = 8;
const UNIT_OUTPUT: u8 = 9;
const CTL_MUTE: u8 = 1;
const CTL_VOLUME: u8 = 2;
const CTL_SEND: u8 = 9;
const OFF: i16 = -32768;
const MIN_DB: f64 = -128.0;

pub const PROGRAMS: [&str; 8] = ["room1", "room2", "room3", "hall1", "hall2", "plate", "delay", "echo"];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Send {
    pub level: Option<f64>,
    pub pan: f64,
    pub mute: bool,
    pub solo: bool,
}

impl Default for Send {
    fn default() -> Self {
        Send { level: None, pan: 0.0, mute: false, solo: false }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Fx {
    pub program: u8,
    pub volume: f64,
    pub duration: f64,
    pub feedback: f64,
    pub sends: Vec<Option<f64>>,
    pub returns: Vec<Option<f64>>,
    /// Per bus 1/2 and 3/4: silence the effect return on that bus. The firmware has no
    /// known bypass request (docs/protocol.md, Still open), so this zeroes unit 7.
    pub bypass: Vec<bool>,
}

impl Default for Fx {
    fn default() -> Self {
        Fx {
            program: 3, // hall1
            volume: 1.0,
            duration: 0.5,
            feedback: 0.5,
            sends: vec![None; SOURCES],
            returns: vec![Some(0.0); RETURNS],
            bypass: vec![false; RETURNS / 2],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Output {
    pub level: Option<f64>,
    pub mute: bool,
}

impl Default for Output {
    fn default() -> Self {
        Output { level: Some(0.0), mute: false }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Mixer {
    /// `buses[b][s]`: source `s` into output pair `b`.
    pub buses: Vec<Vec<Send>>,
    /// Stereo link per source pair (in1/2, in3/4, .., daw7/8). Linked pairs use the
    /// balance law, each side feeding only its own output.
    pub link: Vec<bool>,
    pub fx: Fx,
    pub outputs: Vec<Output>,
    /// Stereo link per output pair. Clients move both faders together; no effect on
    /// the device.
    pub out_link: Vec<bool>,
}

impl Default for Mixer {
    /// DAW 1 to 8 straight to outputs 1 to 8 at 0 dB. Inputs and effect sends off.
    fn default() -> Self {
        let mut buses = vec![vec![Send::default(); SOURCES]; BUSES];
        for (b, bus) in buses.iter_mut().enumerate() {
            bus[8 + 2 * b].level = Some(0.0);
            bus[8 + 2 * b + 1].level = Some(0.0);
        }
        Mixer {
            buses,
            link: vec![true; SOURCES / 2],
            fx: Fx::default(),
            outputs: vec![Output::default(); OUTPUTS],
            out_link: vec![true; OUTPUTS / 2],
        }
    }
}

/// One SET_CUR request.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Ctl {
    pub unit: u8,
    pub control: u8,
    pub channel: u8,
    pub data: Vec<u8>,
}

impl Ctl {
    fn new(unit: u8, control: u8, channel: usize, data: Vec<u8>) -> Self {
        Ctl { unit, control, channel: channel as u8, data }
    }

    pub fn key(&self) -> (u8, u8, u8) {
        (self.unit, self.control, self.channel)
    }
}

fn s16(v: i16) -> Vec<u8> {
    v.to_le_bytes().to_vec()
}

/// dB to the firmware's S16 in 1/256 dB. The device mutes anything above about +1 dB,
/// so clamp at 0 (docs/protocol.md, SET tests).
fn db_word(db: Option<f64>) -> i16 {
    match db {
        Some(d) if d.is_finite() && d > MIN_DB => (d.min(0.0) * 256.0).round() as i16,
        _ => OFF,
    }
}

fn gain_word(gain: f64) -> i16 {
    if gain <= 0.0 {
        OFF
    } else {
        db_word(Some(20.0 * gain.log10()))
    }
}

/// Vendor fader law, `MixerUtils::FaderPositionToGain` (protocol.md). `x` from 0 to 1.
fn fader_gain(x: f64) -> f64 {
    if x <= 1.19209e-7 {
        return 0.0;
    }
    let db = if x > 0.375 { 20.0 - 40.0 * 0.5f64.powf((x - 0.375) / 0.625) } else { -75.0 * 0.266667f64.powf(x / 0.375) };
    10f64.powf(db / 20.0)
}

/// Constant-power pan, -3 dB at center (`PanToStereoGain`). `pan` from -1 to 1.
fn pan_gains(pan: f64) -> (f64, f64) {
    let p = (pan.clamp(-1.0, 1.0) + 1.0) / 2.0;
    if (0.4999..0.50394).contains(&p) {
        (std::f64::consts::FRAC_1_SQRT_2, std::f64::consts::FRAC_1_SQRT_2)
    } else if p >= 0.9999 {
        (0.0, 1.0)
    } else if p <= 0.0001 {
        (1.0, 0.0)
    } else {
        let a = p * std::f64::consts::FRAC_PI_2;
        (a.cos(), a.sin())
    }
}

/// Balance for linked pairs (`BalanceToStereoGain`). `pan` from -1 to 1.
fn balance_gains(pan: f64) -> (f64, f64) {
    let b = (pan.clamp(-1.0, 1.0) + 1.0) / 2.0;
    if (0.4999..0.50394).contains(&b) {
        (1.0, 1.0)
    } else if b < 0.5 {
        (1.0, fader_gain(2.0 * b))
    } else {
        (fader_gain(2.0 * (1.0 - b)), 1.0)
    }
}

impl Mixer {
    /// Repairs vectors of the wrong length, for example from a hand-edited file.
    pub fn normalize(&mut self) {
        let d = Mixer::default();
        self.buses.resize(BUSES, vec![Send::default(); SOURCES]);
        for bus in &mut self.buses {
            bus.resize(SOURCES, Send::default());
        }
        self.link.resize(SOURCES / 2, true);
        self.fx.sends.resize(SOURCES, None);
        self.fx.returns.resize(RETURNS, Some(0.0));
        self.fx.bypass.resize(RETURNS / 2, false);
        self.outputs.resize(OUTPUTS, Output::default());
        self.out_link.resize(OUTPUTS / 2, true);
        if self.fx.program as usize >= PROGRAMS.len() {
            self.fx.program = d.fx.program;
        }
    }

    /// Linear gains from source `s` into the left and right output of bus `b`.
    fn node_gains(&self, b: usize, s: usize) -> (f64, f64) {
        let bus = &self.buses[b];
        let send = &bus[s];
        let any_solo = bus.iter().any(|x| x.solo);
        let level = match send.level {
            Some(l) if !send.mute && (!any_solo || send.solo) => 10f64.powf(l.min(0.0) / 20.0),
            _ => return (0.0, 0.0),
        };
        if self.link[s / 2] {
            let (l, r) = balance_gains(send.pan);
            if s % 2 == 0 {
                (level * l, 0.0)
            } else {
                (0.0, level * r)
            }
        } else {
            let (l, r) = pan_gains(send.pan);
            (level * l, level * r)
        }
    }

    /// Every request needed to put the device in this state.
    pub fn controls(&self) -> Vec<Ctl> {
        let mut v = Vec::with_capacity(200);
        for b in 0..BUSES {
            for s in 0..SOURCES {
                let (l, r) = self.node_gains(b, s);
                v.push(Ctl::new(UNIT_MIXER, (2 * b + 1) as u8, s + 1, s16(gain_word(l))));
                v.push(Ctl::new(UNIT_MIXER, (2 * b + 2) as u8, s + 1, s16(gain_word(r))));
            }
        }
        for (s, send) in self.fx.sends.iter().enumerate() {
            v.push(Ctl::new(UNIT_MIXER, CTL_SEND, s + 1, s16(db_word(*send))));
        }
        let unit = |x: f64, max: f64| (x.clamp(0.0, 1.0) * max).round();
        v.push(Ctl::new(UNIT_FX, 1, 0, vec![self.fx.program]));
        v.push(Ctl::new(UNIT_FX, 2, 0, vec![unit(self.fx.volume, 127.0) as u8]));
        v.push(Ctl::new(UNIT_FX, 3, 0, (unit(self.fx.duration, 32512.0) as u16).to_le_bytes().to_vec()));
        v.push(Ctl::new(UNIT_FX, 4, 0, vec![unit(self.fx.feedback, 127.0) as u8]));
        for (o, ret) in self.fx.returns.iter().enumerate() {
            let ret = if self.fx.bypass[o / 2] { None } else { *ret };
            v.push(Ctl::new(UNIT_RETURN, CTL_VOLUME, o + 1, s16(db_word(ret))));
            v.push(Ctl::new(UNIT_RETURN, CTL_MUTE, o + 1, s16(0)));
        }
        for (o, out) in self.outputs.iter().enumerate() {
            v.push(Ctl::new(UNIT_OUTPUT, CTL_VOLUME, o + 1, s16(db_word(out.level))));
            v.push(Ctl::new(UNIT_OUTPUT, CTL_MUTE, o + 1, s16(out.mute as i16)));
            // The vendor daemon sets unit 8 to 0 dB at startup (InitializeHardware).
            v.push(Ctl::new(UNIT_TRIM, CTL_VOLUME, o + 1, s16(0)));
        }
        v
    }

    /// Applies one `path = value` assignment. Paths:
    ///   bus<1-4>.<src>.level|pan|mute|solo    src = in1..in8, daw1..daw8
    ///   link.<src>, link.out<1-8>             either member of the pair
    ///   fx.program                            room1..echo or 0..7
    ///   fx.volume|duration|feedback           0..1
    ///   fx.send.<src>                         dB
    ///   fx.return.<1-4>                       dB
    ///   fx.bypass.<1-2>                       bus 1 or 2
    ///   out<1-8>.level|mute
    pub fn set(&mut self, path: &str, value: &str) -> Result<(), String> {
        let parts: Vec<&str> = path.split('.').collect();
        match parts.as_slice() {
            [bus, src, field] if bus.starts_with("bus") => {
                let b = index(&bus[3..], BUSES).ok_or_else(|| format!("bad bus '{bus}', use bus1..bus4"))?;
                let s = source(src)?;
                let send = &mut self.buses[b][s];
                match *field {
                    "level" => send.level = db(value)?,
                    "pan" => send.pan = number(value, -1.0, 1.0)?,
                    "mute" => send.mute = boolean(value)?,
                    "solo" => send.solo = boolean(value)?,
                    _ => return Err(format!("bad field '{field}', use level, pan, mute or solo")),
                }
            }
            ["link", out] if out.starts_with("out") => {
                let o = index(&out[3..], OUTPUTS).ok_or_else(|| format!("bad output '{out}', use out1..out8"))?;
                self.out_link[o / 2] = boolean(value)?;
            }
            ["link", src] => self.link[source(src)? / 2] = boolean(value)?,
            ["fx", "program"] => {
                self.fx.program = match PROGRAMS.iter().position(|p| *p == value) {
                    Some(i) => i as u8,
                    None => number(value, 0.0, 7.0)? as u8,
                }
            }
            ["fx", "volume"] => self.fx.volume = number(value, 0.0, 1.0)?,
            ["fx", "duration"] => self.fx.duration = number(value, 0.0, 1.0)?,
            ["fx", "feedback"] => self.fx.feedback = number(value, 0.0, 1.0)?,
            ["fx", "send", src] => self.fx.sends[source(src)?] = db(value)?,
            ["fx", "return", o] => {
                let o = index(o, RETURNS).ok_or_else(|| format!("bad return '{o}', use 1..4"))?;
                self.fx.returns[o] = db(value)?;
            }
            ["fx", "bypass", b] => {
                let b = index(b, RETURNS / 2).ok_or_else(|| format!("bad bus '{b}', bypass exists on bus 1 and 2"))?;
                self.fx.bypass[b] = boolean(value)?;
            }
            [out, field] if out.starts_with("out") => {
                let o = index(&out[3..], OUTPUTS).ok_or_else(|| format!("bad output '{out}', use out1..out8"))?;
                match *field {
                    "level" => self.outputs[o].level = db(value)?,
                    "mute" => self.outputs[o].mute = boolean(value)?,
                    _ => return Err(format!("bad field '{field}', use level or mute")),
                }
            }
            _ => return Err(format!("unknown path '{path}'")),
        }
        Ok(())
    }
}

/// 1-based index string to 0-based index below `n`.
fn index(s: &str, n: usize) -> Option<usize> {
    s.parse::<usize>().ok().filter(|i| (1..=n).contains(i)).map(|i| i - 1)
}

fn source(s: &str) -> Result<usize, String> {
    let bad = || format!("bad source '{s}', use in1..in8 or daw1..daw8");
    if let Some(n) = s.strip_prefix("in") {
        index(n, 8).ok_or_else(bad)
    } else if let Some(n) = s.strip_prefix("daw") {
        index(n, 8).map(|i| i + 8).ok_or_else(bad)
    } else {
        Err(bad())
    }
}

fn number(v: &str, min: f64, max: f64) -> Result<f64, String> {
    v.parse::<f64>()
        .ok()
        .filter(|x| x.is_finite() && (min..=max).contains(x))
        .ok_or_else(|| format!("bad value '{v}', expected {min} to {max}"))
}

fn db(v: &str) -> Result<Option<f64>, String> {
    match v {
        "off" | "-inf" => Ok(None),
        _ => number(v, MIN_DB, 0.0).map(Some).map_err(|_| format!("bad level '{v}', expected dB from -128 to 0 or 'off'")),
    }
}

fn boolean(v: &str) -> Result<bool, String> {
    match v {
        "on" | "true" | "1" => Ok(true),
        "off" | "false" | "0" => Ok(false),
        _ => Err(format!("bad value '{v}', expected on or off")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(m: &Mixer, unit: u8, control: u8, channel: u8) -> i16 {
        let c = m.controls().into_iter().find(|c| c.key() == (unit, control, channel)).unwrap();
        i16::from_le_bytes([c.data[0], c.data[1]])
    }

    #[test]
    fn default_routes_daw_pairs_to_outputs() {
        let m = Mixer::default();
        assert_eq!(word(&m, 5, 1, 9), 0); // daw1 -> out1
        assert_eq!(word(&m, 5, 2, 9), OFF); // daw1 -/-> out2 (linked)
        assert_eq!(word(&m, 5, 2, 10), 0); // daw2 -> out2
        assert_eq!(word(&m, 5, 7, 15), 0); // daw7 -> out7
        assert_eq!(word(&m, 5, 1, 1), OFF); // in1 off
    }

    #[test]
    fn unlinked_center_pan_is_minus_3_db() {
        let mut m = Mixer::default();
        m.set("link.in1", "off").unwrap();
        m.set("bus1.in1.level", "0").unwrap();
        assert_eq!(word(&m, 5, 1, 1), -771); // -3.01 dB * 256
        assert_eq!(word(&m, 5, 2, 1), -771);
        m.set("bus1.in1.pan", "-1").unwrap();
        assert_eq!(word(&m, 5, 1, 1), 0);
        assert_eq!(word(&m, 5, 2, 1), OFF);
    }

    #[test]
    fn solo_mutes_the_rest_of_the_bus() {
        let mut m = Mixer::default();
        m.set("bus1.in1.level", "-6").unwrap();
        m.set("bus1.in1.solo", "on").unwrap();
        assert_eq!(word(&m, 5, 1, 9), OFF); // daw1 silenced on bus 1
        assert_eq!(word(&m, 5, 1, 1), -6 * 256);
        assert_eq!(word(&m, 5, 3, 11), 0); // bus 2 unaffected
    }

    #[test]
    fn fx_and_outputs() {
        let mut m = Mixer::default();
        m.set("fx.program", "delay").unwrap();
        m.set("fx.send.in1", "-10").unwrap();
        m.set("out3.level", "-20").unwrap();
        m.set("out3.mute", "on").unwrap();
        let c = m.controls();
        assert!(c.contains(&Ctl::new(6, 1, 0, vec![6])));
        assert_eq!(word(&m, 5, 9, 1), -2560);
        assert_eq!(word(&m, 9, 2, 3), -5120);
        assert_eq!(word(&m, 9, 1, 3), 1);
        m.set("fx.bypass.1", "on").unwrap();
        assert_eq!(word(&m, 7, 2, 1), OFF);
        assert_eq!(word(&m, 7, 2, 3), 0);
        assert!(m.set("bus5.in1.level", "0").is_err());
        assert!(m.set("bus1.in1.level", "3").is_err());
    }

    #[test]
    fn round_trips_through_json() {
        let mut m = Mixer::default();
        m.set("bus2.in3.pan", "0.25").unwrap();
        let back: Mixer = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back, m);
    }
}
