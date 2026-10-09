//! What a region does when played: its effective opcodes read the way sfizz 1.2.3 reads them.
//!
//! Only the opcodes this crate's tools and reports need are interpreted (key and velocity mapping,
//! keyswitches, round robins, CC conditions, triggers, levels, tuning, loops, envelopes); every opcode stays
//! available as text through [`crate::Region::get`].
//!
//! Value rules (each checked against sfizz):
//! * Opcodes apply in order (`<global>`, `<master>`, `<group>`, then the region); later ones win, and
//!   `key=` sets `lokey`, `hikey` and `pitch_keycenter` at once.
//! * A key that is not a valid note (`lokey=200`, `hikey=x`) gives the opcode's default (0 for `lokey`, 127 for
//!   `hikey`, 60 for `key` and `pitch_keycenter`).
//! * Numbers are read from the front of the value (`60.7` is key 60).
//! * Values are case-sensitive. An unknown `trigger` (`trigger=Release`) means `attack`; an unknown
//!   `loop_mode` (`loop_mode=continuous`) leaves the loop mode unset, so the sample file decides.
//! * `seq_length` and `seq_position` outside 1..=100 fall back to 1.

use crate::value::{leading_float, leading_int, parse_key};
use serde::Serialize;
use std::collections::BTreeMap;

/// What triggers a region.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    #[default]
    Attack,
    Release,
    ReleaseKey,
    First,
    Legato,
}

/// How a region loops (`loop_mode`); `None` in [`RegionProps::loop_mode`] means unset, in which case the
/// sample file decides (a WAV loop makes it `loop_continuous`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopMode {
    NoLoop,
    OneShot,
    LoopContinuous,
    LoopSustain,
}

/// `pitch_keycenter`: a key, or `sample` (the root key stored in the sample file).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyCenter {
    Key(u8),
    Sample,
}

/// An inclusive range of controller values (`loccN`/`hiccN`, `start_loccN`/`start_hiccN`).
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct CcRange {
    pub lo: f64,
    pub hi: f64,
}

/// The interpreted opcodes of a region. Defaults are sfizz's.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RegionProps {
    pub lokey: u8,
    pub hikey: u8,
    pub pitch_keycenter: KeyCenter,
    pub lovel: u8,
    pub hivel: u8,
    pub lochan: u8,
    pub hichan: u8,
    pub lorand: f64,
    pub hirand: f64,
    pub trigger: Trigger,
    pub seq_length: u32,
    pub seq_position: u32,
    /// Keyswitch range of the instrument (`sw_lokey`..`sw_hikey`).
    pub sw_lokey: Option<u8>,
    pub sw_hikey: Option<u8>,
    /// The region plays when the last keyswitch pressed is in this range (`sw_last`, or `sw_lolast`..`sw_hilast`).
    pub sw_last: Option<(u8, u8)>,
    pub sw_down: Option<u8>,
    pub sw_up: Option<u8>,
    pub sw_previous: Option<u8>,
    pub sw_default: Option<u8>,
    pub sw_label: Option<String>,
    /// CC conditions: the region plays only while CC N is inside the range.
    pub cc_conditions: BTreeMap<u32, CcRange>,
    /// CC triggers (`start_loccN`/`start_hiccN`, alias `on_loccN`): moving CC N into the range starts the region.
    pub cc_triggers: BTreeMap<u32, CcRange>,
    pub group: i64,
    pub off_by: Option<i64>,
    pub polyphony: Option<u32>,
    pub note_polyphony: Option<u32>,
    /// dB
    pub volume: f64,
    /// percent
    pub amplitude: f64,
    pub pan: f64,
    pub width: f64,
    /// cents (`tune`, alias `pitch`)
    pub tune: f64,
    pub transpose: i64,
    /// cents per key
    pub pitch_keytrack: f64,
    pub amp_veltrack: f64,
    pub xfin_lovel: u8,
    pub xfin_hivel: u8,
    pub xfout_lovel: u8,
    pub xfout_hivel: u8,
    pub loop_mode: Option<LoopMode>,
    pub loop_start: Option<u64>,
    pub loop_end: Option<u64>,
    pub offset: u64,
    pub end: Option<i64>,
    /// seconds
    pub delay: f64,
    pub ampeg_attack: f64,
    pub ampeg_hold: f64,
    pub ampeg_decay: f64,
    /// percent
    pub ampeg_sustain: f64,
    pub ampeg_release: f64,
    pub reverse: bool,
}

impl Default for RegionProps {
    fn default() -> Self {
        RegionProps {
            lokey: 0,
            hikey: 127,
            pitch_keycenter: KeyCenter::Key(60),
            lovel: 0,
            hivel: 127,
            lochan: 1,
            hichan: 16,
            lorand: 0.0,
            hirand: 1.0,
            trigger: Trigger::Attack,
            seq_length: 1,
            seq_position: 1,
            sw_lokey: None,
            sw_hikey: None,
            sw_last: None,
            sw_down: None,
            sw_up: None,
            sw_previous: None,
            sw_default: None,
            sw_label: None,
            cc_conditions: BTreeMap::new(),
            cc_triggers: BTreeMap::new(),
            group: 0,
            off_by: None,
            polyphony: None,
            note_polyphony: None,
            volume: 0.0,
            amplitude: 100.0,
            pan: 0.0,
            width: 100.0,
            tune: 0.0,
            transpose: 0,
            pitch_keytrack: 100.0,
            amp_veltrack: 100.0,
            xfin_lovel: 0,
            xfin_hivel: 0,
            xfout_lovel: 127,
            xfout_hivel: 127,
            loop_mode: None,
            loop_start: None,
            loop_end: None,
            offset: 0,
            end: None,
            delay: 0.0,
            ampeg_attack: 0.0,
            ampeg_hold: 0.0,
            ampeg_decay: 0.0,
            ampeg_sustain: 100.0,
            ampeg_release: 0.001,
            reverse: false,
        }
    }
}

/// A problem with a value: the opcode (canonical name) and what was done about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ValueIssue {
    pub message: String,
}

fn vel(v: &str) -> Option<u8> {
    leading_int(v).map(|n| n.clamp(0, 127) as u8)
}

fn chan(v: &str) -> Option<u8> {
    leading_int(v).map(|n| n.clamp(1, 16) as u8)
}

fn seq(v: &str) -> u32 {
    leading_int(v).filter(|n| (1..=100).contains(n)).map_or(1, |n| n as u32)
}

impl RegionProps {
    /// Apply one opcode (canonical name, expanded value). `key_shift` is `octave_offset * 12 + note_offset`
    /// from `<control>`. Returns a description of the problem if the value is not valid.
    pub(crate) fn apply(&mut self, name: &str, value: &str, key_shift: i32) -> Option<ValueIssue> {
        let key = |v: &str, default: u8| -> (u8, bool) {
            match parse_key(v) {
                Some(k) => (shift(k, key_shift), true),
                None => (default, false),
            }
        };
        let bad_key = |default: u8| ValueIssue {
            message: format!("`{name}={value}` is not a valid key (0..127 or a note name); sfizz uses {default}"),
        };
        let opt_key = |v: &str| parse_key(v).map(|k| shift(k, key_shift));
        let float = |target: &mut f64| {
            if let Some(f) = leading_float(value) {
                *target = f;
            }
        };
        match name {
            "lokey" => {
                let (k, ok) = key(value, 0);
                self.lokey = k;
                if !ok {
                    return Some(bad_key(0));
                }
            }
            "hikey" => {
                let (k, ok) = key(value, 127);
                self.hikey = k;
                if !ok {
                    return Some(bad_key(127));
                }
            }
            "key" => {
                let (k, ok) = key(value, 60);
                self.lokey = k;
                self.hikey = k;
                self.pitch_keycenter = KeyCenter::Key(k);
                if !ok {
                    return Some(bad_key(60));
                }
            }
            "pitch_keycenter" => {
                if value.trim() == "sample" {
                    self.pitch_keycenter = KeyCenter::Sample;
                } else {
                    let (k, ok) = key(value, 60);
                    self.pitch_keycenter = KeyCenter::Key(k);
                    if !ok {
                        return Some(bad_key(60));
                    }
                }
            }
            "lovel" => self.lovel = vel(value).unwrap_or(self.lovel),
            "hivel" => self.hivel = vel(value).unwrap_or(self.hivel),
            "lochan" => self.lochan = chan(value).unwrap_or(self.lochan),
            "hichan" => self.hichan = chan(value).unwrap_or(self.hichan),
            "lorand" => float(&mut self.lorand),
            "hirand" => float(&mut self.hirand),
            "trigger" => match value.trim() {
                "attack" => self.trigger = Trigger::Attack,
                "release" => self.trigger = Trigger::Release,
                "release_key" => self.trigger = Trigger::ReleaseKey,
                "first" => self.trigger = Trigger::First,
                "legato" => self.trigger = Trigger::Legato,
                other => {
                    self.trigger = Trigger::Attack;
                    return Some(ValueIssue { message: format!("unknown trigger `{other}`; sfizz uses attack") });
                }
            },
            "seq_length" => self.seq_length = seq(value),
            "seq_position" => self.seq_position = seq(value),
            "sw_lokey" => self.sw_lokey = opt_key(value),
            "sw_hikey" => self.sw_hikey = opt_key(value),
            "sw_last" => self.sw_last = opt_key(value).map(|k| (k, k)),
            "sw_lolast" => {
                if let Some(k) = opt_key(value) {
                    let hi = self.sw_last.map_or(k, |(_, h)| h.max(k));
                    self.sw_last = Some((k, hi));
                }
            }
            "sw_hilast" => {
                if let Some(k) = opt_key(value) {
                    let lo = self.sw_last.map_or(k, |(l, _)| l.min(k));
                    self.sw_last = Some((lo, k));
                }
            }
            "sw_down" => self.sw_down = opt_key(value),
            "sw_up" => self.sw_up = opt_key(value),
            "sw_previous" => self.sw_previous = opt_key(value),
            "sw_default" => self.sw_default = opt_key(value),
            "sw_label" => self.sw_label = Some(value.to_owned()),
            "group" => self.group = leading_int(value).unwrap_or(self.group),
            "off_by" => self.off_by = leading_int(value),
            "polyphony" => self.polyphony = leading_int(value).and_then(|n| u32::try_from(n).ok()),
            "note_polyphony" => self.note_polyphony = leading_int(value).and_then(|n| u32::try_from(n).ok()),
            "volume" => float(&mut self.volume),
            "amplitude" => float(&mut self.amplitude),
            "pan" => float(&mut self.pan),
            "width" => float(&mut self.width),
            "tune" => float(&mut self.tune),
            "transpose" => self.transpose = leading_int(value).unwrap_or(self.transpose),
            "pitch_keytrack" => float(&mut self.pitch_keytrack),
            "amp_veltrack" => float(&mut self.amp_veltrack),
            "xfin_lovel" => self.xfin_lovel = vel(value).unwrap_or(self.xfin_lovel),
            "xfin_hivel" => self.xfin_hivel = vel(value).unwrap_or(self.xfin_hivel),
            "xfout_lovel" => self.xfout_lovel = vel(value).unwrap_or(self.xfout_lovel),
            "xfout_hivel" => self.xfout_hivel = vel(value).unwrap_or(self.xfout_hivel),
            "loop_mode" => match value.trim() {
                "no_loop" => self.loop_mode = Some(LoopMode::NoLoop),
                "one_shot" => self.loop_mode = Some(LoopMode::OneShot),
                "loop_continuous" => self.loop_mode = Some(LoopMode::LoopContinuous),
                "loop_sustain" => self.loop_mode = Some(LoopMode::LoopSustain),
                other => {
                    self.loop_mode = None;
                    return Some(ValueIssue {
                        message: format!(
                            "unknown loop_mode `{other}`: sfizz treats the loop mode as unset, so the sample file \
                             decides (it knows no_loop, one_shot, loop_continuous, loop_sustain)"
                        ),
                    });
                }
            },
            "loop_start" => self.loop_start = leading_int(value).and_then(|n| u64::try_from(n).ok()),
            "loop_end" => self.loop_end = leading_int(value).and_then(|n| u64::try_from(n).ok()),
            "offset" => self.offset = leading_int(value).and_then(|n| u64::try_from(n).ok()).unwrap_or(self.offset),
            "end" => self.end = leading_int(value),
            "delay" => float(&mut self.delay),
            "ampeg_attack" => float(&mut self.ampeg_attack),
            "ampeg_hold" => float(&mut self.ampeg_hold),
            "ampeg_decay" => float(&mut self.ampeg_decay),
            "ampeg_sustain" => float(&mut self.ampeg_sustain),
            "ampeg_release" => float(&mut self.ampeg_release),
            "direction" => self.reverse = value.trim() == "reverse",
            _ => self.apply_cc(name, value),
        }
        None
    }

    fn apply_cc(&mut self, name: &str, value: &str) {
        let Some(f) = leading_float(value) else { return };
        let split = |prefix: &str| name.strip_prefix(prefix).and_then(|d| d.parse::<u32>().ok());
        if let Some(cc) = split("locc") {
            self.cc_conditions.entry(cc).or_insert(CcRange { lo: 0.0, hi: 127.0 }).lo = f;
        } else if let Some(cc) = split("hicc") {
            self.cc_conditions.entry(cc).or_insert(CcRange { lo: 0.0, hi: 127.0 }).hi = f;
        } else if let Some(cc) = split("start_locc") {
            self.cc_triggers.entry(cc).or_insert(CcRange { lo: 0.0, hi: 127.0 }).lo = f;
        } else if let Some(cc) = split("start_hicc") {
            self.cc_triggers.entry(cc).or_insert(CcRange { lo: 0.0, hi: 127.0 }).hi = f;
        }
    }

    /// The pitch center as a key (`None` when it comes from the sample file).
    pub fn keycenter(&self) -> Option<u8> {
        match self.pitch_keycenter {
            KeyCenter::Key(k) => Some(k),
            KeyCenter::Sample => None,
        }
    }

    /// True if the region plays on note-on (as opposed to note-off or CC triggers).
    pub fn is_note_on(&self) -> bool {
        matches!(self.trigger, Trigger::Attack | Trigger::First | Trigger::Legato) && self.cc_triggers.is_empty()
    }

    /// True if the region plays on note-off.
    pub fn is_release(&self) -> bool {
        matches!(self.trigger, Trigger::Release | Trigger::ReleaseKey)
    }

    /// The velocities at which the region sounds, taking crossfades into account (`xfin_lovel` raises the
    /// bottom: below it the region is silent).
    pub fn audible_velocities(&self) -> (u8, u8) {
        let mut lo = self.lovel;
        if self.xfin_hivel > self.xfin_lovel {
            lo = lo.max(self.xfin_lovel.saturating_add(1));
        }
        let mut hi = self.hivel;
        if self.xfout_hivel > self.xfout_lovel && self.xfout_hivel < 127 {
            hi = hi.min(self.xfout_hivel.saturating_sub(1));
        }
        (lo, hi.max(lo))
    }
}

fn shift(key: u8, by: i32) -> u8 {
    (i32::from(key) + by).clamp(0, 127) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(ops: &[(&str, &str)]) -> RegionProps {
        let mut p = RegionProps::default();
        for (n, v) in ops {
            p.apply(n, v, 0);
        }
        p
    }

    #[test]
    fn keys_follow_sfizz() {
        let p = props(&[("key", "66"), ("lokey", "200")]);
        assert_eq!((p.lokey, p.hikey, p.keycenter()), (0, 66, Some(66)));
        let p = props(&[("lokey", "c4"), ("hikey", "e4"), ("pitch_keycenter", "d4")]);
        assert_eq!((p.lokey, p.hikey, p.keycenter()), (60, 64, Some(62)));
        let p = props(&[("pitch_keycenter", "sample")]);
        assert_eq!(p.pitch_keycenter, KeyCenter::Sample);
        let mut p = RegionProps::default();
        assert!(p.apply("hikey", "x", 0).is_some());
        assert_eq!(p.hikey, 127);
        p.apply("sw_last", "60", 14);
        assert_eq!(p.sw_last, Some((74, 74)));
    }

    #[test]
    fn enumerations_are_case_sensitive() {
        let mut p = RegionProps::default();
        assert!(p.apply("trigger", "Release", 0).is_some());
        assert_eq!(p.trigger, Trigger::Attack);
        p.apply("loop_mode", "one_shot", 0);
        assert!(p.apply("loop_mode", "continuous", 0).is_some());
        assert_eq!(p.loop_mode, None);
        p.apply("loop_mode", "loop_continuous", 0);
        assert_eq!(p.loop_mode, Some(LoopMode::LoopContinuous));
    }

    #[test]
    fn seq_and_cc() {
        let p = props(&[("seq_length", "0"), ("seq_position", "200")]);
        assert_eq!((p.seq_length, p.seq_position), (1, 1));
        let p = props(&[("seq_length", "4"), ("seq_position", "3"), ("locc64", "64"), ("start_locc20", "1")]);
        assert_eq!((p.seq_length, p.seq_position), (4, 3));
        assert_eq!(p.cc_conditions[&64], CcRange { lo: 64.0, hi: 127.0 });
        assert!(!p.is_note_on());
    }

    #[test]
    fn crossfaded_layers() {
        let soft = props(&[("xfout_lovel", "60"), ("xfout_hivel", "100")]);
        assert_eq!(soft.audible_velocities(), (0, 99));
        let loud = props(&[("xfin_lovel", "60"), ("xfin_hivel", "100")]);
        assert_eq!(loud.audible_velocities(), (61, 127));
    }
}
