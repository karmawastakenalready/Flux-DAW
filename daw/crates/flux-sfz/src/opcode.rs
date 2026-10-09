//! Opcode names: which ones sfizz 1.2.3 understands, their aliases, and the numbers inside them.
//!
//! sfizz compares opcode names case-insensitively (`Ampeg_release` works) but header names and variable names
//! case-sensitively. Numbers inside a name are parameters (`amplitude_oncc11`, `eg2_time3`); the tables here
//! write them as `N`.
//!
//! The tables were measured, not copied from documentation: every opcode of the SFZ format this crate knows of
//! and every name used by the instruments in this repository was fed to sfizz 1.2.3 and kept if sfizz did not
//! report it as unknown (`build/flux-sfz_check/scripts/sfizz_unknown_probe.py`). sfizz may accept rare names
//! missing here; every name used in the corpus is covered.

use serde::Serialize;
use std::borrow::Cow;

/// The kind of a header. Header names are case-sensitive: `<Region>` is an unknown header and sfizz ignores
/// its opcodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HeaderKind {
    Control,
    Global,
    Master,
    Group,
    Region,
    Curve,
    Effect,
    Sample,
    /// Any other name; sfizz reports "Unknown header" and ignores the block.
    Unknown,
}

impl HeaderKind {
    pub fn from_name(name: &str) -> HeaderKind {
        match name {
            "control" => HeaderKind::Control,
            "global" => HeaderKind::Global,
            "master" => HeaderKind::Master,
            "group" => HeaderKind::Group,
            "region" => HeaderKind::Region,
            "curve" => HeaderKind::Curve,
            "effect" => HeaderKind::Effect,
            "sample" => HeaderKind::Sample,
            _ => HeaderKind::Unknown,
        }
    }

    /// True for the headers whose opcodes end up in regions (`<global>`, `<master>`, `<group>`, `<region>`).
    pub fn is_region_level(self) -> bool {
        matches!(self, HeaderKind::Global | HeaderKind::Master | HeaderKind::Group | HeaderKind::Region)
    }
}

/// Whether sfizz understands an opcode where it is used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Support {
    Known,
    /// sfizz ignores it (in region-level headers it lists it among the unknown opcodes).
    Unknown,
    /// Not checked: the opcodes of `<effect>` blocks depend on the effect type, `<sample>` blocks and unknown
    /// headers are ignored as a whole.
    Unchecked,
}

/// An opcode name split into its pattern and its numbers: `amplitude_oncc11` is `("amplitude_onccN", [11])`.
/// The pattern is lower-case, so `N` only ever stands for a number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamePattern {
    pub pattern: String,
    pub numbers: Vec<u32>,
}

impl NamePattern {
    pub fn parse(name: &str) -> NamePattern {
        let mut pattern = String::with_capacity(name.len());
        let mut numbers = vec![];
        let mut digits = String::new();
        let mut flush = |digits: &mut String, pattern: &mut String| {
            if !digits.is_empty() {
                pattern.push('N');
                numbers.push(digits.parse().unwrap_or(u32::MAX));
                digits.clear();
            }
        };
        for c in name.chars().map(|c| c.to_ascii_lowercase()) {
            if c.is_ascii_digit() {
                digits.push(c);
            } else {
                flush(&mut digits, &mut pattern);
                pattern.push(c);
            }
        }
        flush(&mut digits, &mut pattern);
        NamePattern { pattern, numbers }
    }

    /// The controller numbers in the name: the numbers directly after `cc` (`xfin_locc1`, `eq2_gaincc7`,
    /// `lfo1_freq_oncc20`). Numbers above 127 are sfizz's extended sources (see [`cc_name`]).
    pub fn cc_numbers(&self) -> Vec<u32> {
        self.pattern
            .match_indices('N')
            .zip(&self.numbers)
            .filter(|((i, _), _)| self.pattern[..*i].ends_with("cc"))
            .map(|(_, &n)| n)
            .collect()
    }
}

/// Whether sfizz understands opcode `name` under a header of kind `header`.
pub fn support(header: HeaderKind, name: &str) -> Support {
    let table: &[&str] = match header {
        HeaderKind::Control => CONTROL_OPCODES,
        HeaderKind::Curve => CURVE_OPCODES,
        h if h.is_region_level() => REGION_OPCODES,
        _ => return Support::Unchecked,
    };
    let p = NamePattern::parse(name);
    if table.binary_search(&p.pattern.as_str()).is_ok() { Support::Known } else { Support::Unknown }
}

/// The name sfizz treats an opcode as: lower-case, with aliases resolved (`loopmode` is `loop_mode`, `gain`
/// is `volume`, `pitch` is `tune`, `on_locc1` is `start_locc1`, `volume_cc1` is `volume_oncc1`...). Only the
/// aliases of opcodes this crate interprets are resolved (each one checked against sfizz); other names are
/// just lower-cased.
pub fn canonical_name(name: &str) -> Cow<'_, str> {
    let lower: Cow<'_, str> = if name.bytes().any(|b| b.is_ascii_uppercase()) {
        Cow::Owned(name.to_ascii_lowercase())
    } else {
        Cow::Borrowed(name)
    };
    let simple = match lower.as_ref() {
        "loopmode" => Some("loop_mode"),
        "loopstart" => Some("loop_start"),
        "loopend" => Some("loop_end"),
        "offby" => Some("off_by"),
        "polyphony_group" => Some("group"),
        "bendup" => Some("bend_up"),
        "benddown" => Some("bend_down"),
        "bendstep" => Some("bend_step"),
        "filtype" => Some("fil_type"),
        "gain" => Some("volume"),
        "pitch" => Some("tune"),
        _ => None,
    };
    if let Some(s) = simple {
        return Cow::Borrowed(s);
    }
    let l = lower.as_ref();
    for (prefix, canonical) in [("on_lo", "start_lo"), ("on_hi", "start_hi")] {
        if let Some(rest) = l.strip_prefix(prefix) {
            return Cow::Owned(format!("{canonical}{rest}"));
        }
    }
    // gain_oncc1, pitch_curvecc1...: the CC modulations of the aliased parameters
    for (prefix, canonical) in [("gain_", "volume_"), ("pitch_", "tune_")] {
        if let Some(rest) = l.strip_prefix(prefix)
            && ["oncc", "cc", "curvecc", "smoothcc", "stepcc"].iter().any(|m| {
                rest.strip_prefix(m).is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
            })
        {
            let fixed = format!("{canonical}{rest}");
            return Cow::Owned(cc_to_oncc(&fixed).unwrap_or(fixed));
        }
    }
    match cc_to_oncc(l) {
        Some(fixed) => Cow::Owned(fixed),
        None => lower,
    }
}

/// `volume_cc1` -> `volume_oncc1` for the parameters where sfizz accepts both spellings.
fn cc_to_oncc(name: &str) -> Option<String> {
    const TARGETS: &[&str] = &[
        "volume", "pan", "tune", "amplitude", "width", "position", "delay", "offset", "cutoff", "resonance",
        "fil_veltrack", "amp_veltrack",
    ];
    let (target, digits) = name.split_once("_cc")?;
    (TARGETS.contains(&target) && !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| format!("{target}_oncc{digits}"))
}

/// Name of a controller number as sfizz numbers them: 0..=127 are MIDI CCs (named here when they have a
/// standard meaning), 128 and up are sfizz/ARIA extended sources.
pub fn cc_name(cc: u32) -> Option<&'static str> {
    Some(match cc {
        0 => "bank select",
        1 => "modulation wheel",
        2 => "breath",
        4 => "foot pedal",
        5 => "portamento time",
        7 => "volume",
        8 => "balance",
        10 => "pan",
        11 => "expression",
        64 => "sustain pedal",
        65 => "portamento",
        66 => "sostenuto pedal",
        67 => "soft pedal",
        71 => "resonance",
        72 => "release time",
        73 => "attack time",
        74 => "brightness",
        91 => "reverb send",
        93 => "chorus send",
        120 => "all sound off",
        121 => "reset all controllers",
        123 => "all notes off",
        128 => "pitch bend (extended CC)",
        129 => "channel aftertouch (extended CC)",
        130 => "polyphonic aftertouch (extended CC)",
        131 => "note-on velocity (extended CC)",
        132 => "note-off velocity (extended CC)",
        133 => "note number (extended CC)",
        134 => "note gate (extended CC)",
        135 => "unipolar random (extended CC)",
        136 => "bipolar random (extended CC)",
        137 => "alternate (extended CC)",
        140 => "key delta (extended CC)",
        141 => "absolute key delta (extended CC)",
        _ => return None,
    })
}

/// Opcodes sfizz 1.2.3 handles in `<control>` (it prints "Unsupported control opcode" for the others).
pub const CONTROL_OPCODES: &[&str] = &[
    "default_path", "hint_ram_based", "hint_stealing", "hint_sustain_cancels_release", "label_ccN", "label_keyN",
    "note_offset", "octave_offset", "set_ccN", "set_hdccN", "set_realccN",
];

/// Opcodes of `<curve>` blocks.
pub const CURVE_OPCODES: &[&str] = &["curve_index", "vN"];

/// Opcodes sfizz 1.2.3 understands in `<global>`, `<master>`, `<group>` and `<region>` (sorted).
pub const REGION_OPCODES: &[&str] = &[
    "amp_keycenter", "amp_keytrack", "amp_random", "amp_velcurve_N", "amp_veltrack", "amp_veltrack_ccN",
    "amp_veltrack_curveccN", "amp_veltrack_onccN", "ampeg_attack", "ampeg_attack_curveccN", "ampeg_attack_onccN",
    "ampeg_attackccN", "ampeg_decay", "ampeg_decay_curveccN", "ampeg_decay_onccN", "ampeg_decayccN", "ampeg_delay",
    "ampeg_delay_curveccN", "ampeg_delay_onccN", "ampeg_delayccN", "ampeg_depth", "ampeg_dynamic", "ampeg_hold",
    "ampeg_hold_curveccN", "ampeg_hold_onccN", "ampeg_holdccN", "ampeg_release", "ampeg_release_curveccN",
    "ampeg_release_onccN", "ampeg_releaseccN", "ampeg_start", "ampeg_start_curveccN", "ampeg_start_onccN",
    "ampeg_startccN", "ampeg_sustain", "ampeg_sustain_curveccN", "ampeg_sustain_onccN", "ampeg_sustainccN",
    "ampeg_velNattack", "ampeg_velNdecay", "ampeg_velNdelay", "ampeg_velNdepth", "ampeg_velNhold",
    "ampeg_velNrelease", "ampeg_velNsustain", "amplfo_delay", "amplfo_depth", "amplfo_depth_onccN",
    "amplfo_depthccN", "amplfo_depthchanaft", "amplfo_fade", "amplfo_freq", "amplfo_freq_onccN", "amplfo_freqccN",
    "amplfo_freqchanaft", "amplitude", "amplitude_ccN", "amplitude_curveccN", "amplitude_onccN",
    "amplitude_smoothccN", "amplitude_stepccN", "bend_down", "bend_smooth", "bend_step", "bend_up", "benddown",
    "bendstep", "bendup", "count", "cutoff", "cutoffN", "cutoffN_ccN", "cutoffN_onccN", "cutoff_ccN",
    "cutoff_chanaft", "cutoff_curveccN", "cutoff_onccN", "cutoff_polyaft", "cutoff_random", "cutoff_smoothccN",
    "cutoff_stepccN", "delay", "delay_ccN", "delay_onccN", "delay_random", "direction", "effectN", "egN_ampeg",
    "egN_amplitude", "egN_amplitude_onccN", "egN_cutoff", "egN_cutoffN", "egN_cutoffN_onccN", "egN_cutoff_onccN",
    "egN_dynamic", "egN_eqNbw", "egN_eqNbw_onccN", "egN_eqNfreq", "egN_eqNfreq_onccN", "egN_eqNgain",
    "egN_eqNgain_onccN", "egN_freq_lfoN", "egN_levelN", "egN_levelN_onccN", "egN_pan", "egN_pan_onccN",
    "egN_pitch", "egN_pitch_onccN", "egN_position", "egN_position_onccN", "egN_resonance", "egN_resonanceN",
    "egN_resonance_onccN", "egN_shapeN", "egN_sustain", "egN_timeN", "egN_timeN_onccN", "egN_volume",
    "egN_volume_onccN", "egN_width", "egN_width_onccN", "end", "eqN_bw", "eqN_bw_onccN", "eqN_bwccN", "eqN_freq",
    "eqN_freq_onccN", "eqN_freqccN", "eqN_gain", "eqN_gain_onccN", "eqN_gainccN", "eqN_type", "eqN_velNfreq",
    "eqN_velNgain", "filN_gain", "filN_keycenter", "filN_keytrack", "filN_random", "filN_type", "filN_veltrack",
    "fil_gain", "fil_gain_onccN", "fil_keycenter", "fil_keytrack", "fil_random", "fil_type", "fil_veltrack",
    "fil_veltrack_ccN", "fil_veltrack_onccN", "fileg_attack", "fileg_attack_onccN", "fileg_attackccN",
    "fileg_decay", "fileg_decay_onccN", "fileg_decayccN", "fileg_delay", "fileg_delay_onccN", "fileg_delayccN",
    "fileg_depth", "fileg_depth_onccN", "fileg_dynamic", "fileg_hold", "fileg_hold_onccN", "fileg_release",
    "fileg_release_onccN", "fileg_releaseccN", "fileg_start", "fileg_sustain", "fileg_sustain_onccN",
    "fileg_sustainccN", "fileg_velNattack", "fileg_velNdecay", "fileg_velNdelay", "fileg_velNdepth",
    "fileg_velNhold", "fileg_velNrelease", "fileg_velNsustain", "fillfo_delay", "fillfo_depth",
    "fillfo_depth_onccN", "fillfo_depthccN", "fillfo_depthchanaft", "fillfo_fade", "fillfo_freq",
    "fillfo_freq_onccN", "fillfo_freqccN", "fillfo_freqchanaft", "filtype", "gain", "gain_ccN", "gain_onccN",
    "global_amplitude", "global_volume", "group", "group_amplitude", "group_volume", "hibend", "hibpm", "hiccN",
    "hichan", "hichanaft", "hihdccN", "hikey", "hipolyaft", "hiprog", "hirand", "hitimer", "hivel", "key",
    "lfoN_amplitude", "lfoN_amplitude_curveccN", "lfoN_amplitude_onccN", "lfoN_beats", "lfoN_count", "lfoN_cutoff",
    "lfoN_cutoffN", "lfoN_cutoffN_onccN", "lfoN_cutoff_onccN", "lfoN_delay", "lfoN_delay_onccN", "lfoN_eqNbw",
    "lfoN_eqNbw_onccN", "lfoN_eqNfreq", "lfoN_eqNfreq_onccN", "lfoN_eqNgain", "lfoN_eqNgain_onccN", "lfoN_fade",
    "lfoN_fade_onccN", "lfoN_freq", "lfoN_freq_curveccN", "lfoN_freq_onccN", "lfoN_freq_smoothccN",
    "lfoN_freq_stepccN", "lfoN_offset", "lfoN_offsetN", "lfoN_pan", "lfoN_pan_onccN", "lfoN_phase",
    "lfoN_phase_onccN", "lfoN_pitch", "lfoN_pitch_curveccN", "lfoN_pitch_onccN", "lfoN_pitch_smoothccN",
    "lfoN_pitch_stepccN", "lfoN_position", "lfoN_position_onccN", "lfoN_ratio", "lfoN_ratioN", "lfoN_resonance",
    "lfoN_resonanceN", "lfoN_resonance_onccN", "lfoN_scale", "lfoN_scaleN", "lfoN_stepN", "lfoN_steps",
    "lfoN_volume", "lfoN_volume_onccN", "lfoN_wave", "lfoN_waveN", "lfoN_width", "lfoN_width_onccN", "lobend",
    "lobpm", "loccN", "lochan", "lochanaft", "lohdccN", "lokey", "loop_count", "loop_crossfade", "loop_end",
    "loop_length_onccN", "loop_mode", "loop_start", "loop_start_onccN", "loopend", "loopmode", "loopstart",
    "lopolyaft", "loprog", "lorand", "lotimer", "lovel", "master_amplitude", "master_volume", "note_polyphony",
    "note_selfmask", "off_by", "off_mode", "off_time", "offby", "offset", "offset_ccN", "offset_onccN",
    "offset_random", "on_hiccN", "on_hihdccN", "on_loccN", "on_lohdccN", "oscillator", "oscillator_detune",
    "oscillator_detune_onccN", "oscillator_mod_depth", "oscillator_mod_depth_onccN", "oscillator_mode",
    "oscillator_multi", "oscillator_phase", "oscillator_quality", "output", "pan", "pan_ccN", "pan_curveccN",
    "pan_onccN", "pan_smoothccN", "pan_stepccN", "pitch", "pitch_ccN", "pitch_curveccN", "pitch_keycenter",
    "pitch_keytrack", "pitch_onccN", "pitch_random", "pitch_smoothccN", "pitch_stepccN", "pitch_veltrack",
    "pitch_veltrack_onccN", "pitcheg_attack", "pitcheg_attack_onccN", "pitcheg_attackccN", "pitcheg_decay",
    "pitcheg_decay_onccN", "pitcheg_decayccN", "pitcheg_delay", "pitcheg_delay_onccN", "pitcheg_delayccN",
    "pitcheg_depth", "pitcheg_depth_onccN", "pitcheg_dynamic", "pitcheg_hold", "pitcheg_hold_onccN",
    "pitcheg_release", "pitcheg_release_onccN", "pitcheg_releaseccN", "pitcheg_start", "pitcheg_start_onccN",
    "pitcheg_sustain", "pitcheg_sustain_onccN", "pitcheg_sustainccN", "pitcheg_velNattack", "pitcheg_velNdecay",
    "pitcheg_velNdelay", "pitcheg_velNdepth", "pitcheg_velNhold", "pitcheg_velNrelease", "pitcheg_velNsustain",
    "pitchlfo_delay", "pitchlfo_depth", "pitchlfo_depth_onccN", "pitchlfo_depthccN", "pitchlfo_depthchanaft",
    "pitchlfo_fade", "pitchlfo_freq", "pitchlfo_freq_onccN", "pitchlfo_freqccN", "pitchlfo_freqchanaft",
    "polyphony", "polyphony_group", "position", "position_ccN", "position_curveccN", "position_onccN",
    "position_smoothccN", "position_stepccN", "resonance", "resonanceN", "resonanceN_onccN", "resonance_ccN",
    "resonance_curveccN", "resonance_onccN", "resonance_smoothccN", "resonance_stepccN", "rt_dead", "rt_decay",
    "sample", "sample_quality", "seq_length", "seq_position", "sostenuto_cc", "sostenuto_lo", "sostenuto_sw",
    "start_hiccN", "start_hihdccN", "start_loccN", "start_lohdccN", "sustain_cc", "sustain_lo", "sustain_sw",
    "sw_default", "sw_down", "sw_hikey", "sw_hilast", "sw_label", "sw_last", "sw_lokey", "sw_lolast",
    "sw_previous", "sw_up", "sw_vel", "transpose", "trigger", "tune", "tune_ccN", "tune_curveccN", "tune_onccN",
    "tune_smoothccN", "tune_stepccN", "volume", "volume_ccN", "volume_curveccN", "volume_onccN",
    "volume_smoothccN", "volume_stepccN", "width", "width_ccN", "width_curveccN", "width_onccN", "width_smoothccN",
    "width_stepccN", "xf_cccurve", "xf_keycurve", "xf_velcurve", "xfin_hiccN", "xfin_hikey", "xfin_hivel",
    "xfin_loccN", "xfin_lokey", "xfin_lovel", "xfout_hiccN", "xfout_hikey", "xfout_hivel", "xfout_loccN",
    "xfout_lokey", "xfout_lovel",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_are_sorted_for_binary_search() {
        for t in [REGION_OPCODES, CONTROL_OPCODES, CURVE_OPCODES] {
            assert!(t.windows(2).all(|w| w[0] < w[1]), "{:?}", t.windows(2).find(|w| w[0] >= w[1]));
        }
    }

    #[test]
    fn patterns_and_cc_numbers() {
        let p = NamePattern::parse("lfo1_freq_oncc20");
        assert_eq!((p.pattern.as_str(), p.numbers.clone()), ("lfoN_freq_onccN", vec![1, 20]));
        assert_eq!(p.cc_numbers(), vec![20]);
        assert_eq!(NamePattern::parse("eq2_gaincc7").cc_numbers(), vec![7]);
        assert_eq!(NamePattern::parse("xfin_locc1").cc_numbers(), vec![1]);
        assert_eq!(NamePattern::parse("amp_velcurve_64").cc_numbers(), Vec::<u32>::new());
        assert_eq!(NamePattern::parse("Ampeg_Release").pattern, "ampeg_release");
    }

    #[test]
    fn support_matches_sfizz() {
        // results of the sfizz 1.2.3 probe
        assert_eq!(support(HeaderKind::Region, "amplitude_oncc11"), Support::Known);
        assert_eq!(support(HeaderKind::Group, "Ampeg_release"), Support::Known);
        assert_eq!(support(HeaderKind::Global, "ampeg_attack_shape"), Support::Unknown);
        assert_eq!(support(HeaderKind::Global, "label_cc1"), Support::Unknown);
        assert_eq!(support(HeaderKind::Region, "pitcheg_depthcc1"), Support::Unknown);
        assert_eq!(support(HeaderKind::Region, "var01_volume"), Support::Unknown);
        assert_eq!(support(HeaderKind::Control, "label_cc1"), Support::Known);
        assert_eq!(support(HeaderKind::Control, "lokey"), Support::Unknown);
        assert_eq!(support(HeaderKind::Curve, "v064"), Support::Known);
        assert_eq!(support(HeaderKind::Effect, "anything"), Support::Unchecked);
    }

    #[test]
    fn aliases() {
        assert_eq!(canonical_name("loopmode"), "loop_mode");
        assert_eq!(canonical_name("LoopStart"), "loop_start");
        assert_eq!(canonical_name("gain"), "volume");
        assert_eq!(canonical_name("gain_oncc7"), "volume_oncc7");
        assert_eq!(canonical_name("gain_cc7"), "volume_oncc7");
        assert_eq!(canonical_name("pitch"), "tune");
        assert_eq!(canonical_name("pitch_keycenter"), "pitch_keycenter");
        assert_eq!(canonical_name("pitch_veltrack"), "pitch_veltrack");
        assert_eq!(canonical_name("pitch_cc1"), "tune_oncc1");
        assert_eq!(canonical_name("pitch_curvecc1"), "tune_curvecc1");
        assert_eq!(canonical_name("on_locc64"), "start_locc64");
        assert_eq!(canonical_name("on_hihdcc3"), "start_hihdcc3");
        assert_eq!(canonical_name("volume_cc11"), "volume_oncc11");
        assert_eq!(canonical_name("ampeg_attackcc1"), "ampeg_attackcc1");
        assert_eq!(canonical_name("polyphony_group"), "group");
    }
}
