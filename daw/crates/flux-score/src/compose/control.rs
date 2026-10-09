//! Controller data: expression swells and CC / pitch-bend curves. Both replace the data they cover, so
//! running them again over the same span gives the same track.

use super::args::{CurveArgs, SwellArgs};
use super::{Ctx, Done, is_blip, note_spec, range_label, starts_in};
use crate::error::{Error, Result};
use crate::shapes::{Point, curve as shape_curve, swell as shape_swell, to_bends, to_cc};
use flux_core::commands::{BendSpec, CcSpec};
use flux_core::{CcPoint, Command, Note, TrackRef};
use serde_json::json;
use std::collections::BTreeMap;

fn cc_specs(points: &[CcPoint]) -> Vec<CcSpec> {
    points.iter().map(|c| CcSpec { bar: None, beat: c.beat, value: c.value }).collect()
}

fn check_cc(cc: u8) -> Result<()> {
    if cc > 127 {
        return Err(Error::invalid(format!("cc must be a controller number 0-127, got {cc}")));
    }
    Ok(())
}

pub(crate) fn swell(cx: &mut Ctx, a: SwellArgs) -> Result<Done> {
    check_cc(a.cc)?;
    let t = cx.track(&a.track)?;
    let source = match &a.source {
        Some(r) => cx.track(r)?,
        None => t,
    };
    let (start, end) = cx.bars(a.from_bar, a.to_bar)?;
    if !(0.0..=127.0).contains(&a.lo) || !(0.0..=127.0).contains(&a.hi) || a.lo > a.hi {
        return Err(Error::invalid(format!("lo and hi must be CC values 0-127 with lo <= hi, got {} and {}", a.lo, a.hi)));
    }
    let step = a.step.unwrap_or(0.25);
    for (name, v) in [("long", a.long), ("step", step), ("rest", a.rest)] {
        if !(v.is_finite() && v > 0.0) {
            return Err(Error::invalid(format!("{name} must be a positive number of beats, got {v}")));
        }
    }
    let notes: Vec<Note> = source.notes.iter().filter(|n| starts_in(n, start, end) && !is_blip(n)).copied().collect();
    if notes.is_empty() {
        return Err(Error::invalid(format!("{} has no notes in {} for the swell to follow", source.name, range_label(source, a.from_bar, a.to_bar))));
    }
    let pts: Vec<Point> = shape_swell(&notes, a.lo, a.hi, a.long, step, a.rest).into_iter().filter(|p| p.beat >= start - 1e-9 && p.beat < end - 1e-9).collect();
    let ccs = to_cc(&pts, a.cc);
    let tref = TrackRef::Id(t.id);
    let replaced = t.ccs.iter().filter(|c| c.cc == a.cc && c.beat >= start - 1e-9 && c.beat < end - 1e-9).count();
    let mut commands = Vec::new();
    if replaced > 0 {
        commands.push(Command::ClearCc { track: tref.clone(), cc: Some(a.cc), from_bar: Some(a.from_bar), to_bar: Some(a.to_bar) });
    }
    if !ccs.is_empty() {
        commands.push(Command::AddCc { track: tref, cc: a.cc, points: cc_specs(&ccs) });
    }
    let values: Vec<u8> = ccs.iter().map(|c| c.value).collect();
    let summary = json!({
        "track": t.name,
        "source": source.name,
        "cc": a.cc,
        "bars": [a.from_bar, a.to_bar],
        "points": ccs.len(),
        "replaced": replaced,
        "value_range": [values.iter().min(), values.iter().max()],
    });
    Ok(Done { commands, summary, label: format!("CC{} swell on {}", a.cc, range_label(t, a.from_bar, a.to_bar)) })
}

pub(crate) fn curve(cx: &mut Ctx, a: CurveArgs) -> Result<Done> {
    let t = cx.track(&a.track)?;
    let cc = match (a.cc, a.bend) {
        (Some(_), true) => return Err(Error::invalid("give either cc or bend: true, not both")),
        (None, false) => return Err(Error::invalid("give cc (a controller number) or bend: true")),
        (Some(cc), false) => {
            check_cc(cc)?;
            Some(cc)
        }
        (None, true) => None,
    };
    if a.points.is_empty() {
        return Err(Error::invalid("points is empty"));
    }
    let step = a.step.unwrap_or(0.5);
    if !(step.is_finite() && step > 0.0) {
        return Err(Error::invalid(format!("step must be a positive number of beats, got {step}")));
    }
    let (lo, hi) = if cc.is_some() { (0.0, 127.0) } else { (-8192.0, 8191.0) };
    let mut pairs: Vec<(f64, f64)> = Vec::with_capacity(a.points.len());
    for (i, p) in a.points.iter().enumerate() {
        let beat = match p.bar {
            Some(bar) => {
                cx.bars(bar, bar).map_err(|e| Error::invalid(format!("point {}: {e}", i + 1)))?;
                cx.bar_start(bar) + p.beat
            }
            None => p.beat,
        };
        if !(beat.is_finite() && beat >= 0.0 && beat <= cx.p.total_beats() + 1e-9) {
            return Err(Error::invalid(format!("point {}: beat {beat} is outside the song (0-{})", i + 1, cx.p.total_beats())));
        }
        if !(lo..=hi).contains(&p.value) {
            return Err(Error::invalid(format!("point {}: value {} is outside {lo}-{hi}", i + 1, p.value)));
        }
        if let Some(&(prev, _)) = pairs.last()
            && beat <= prev + 1e-9
        {
            return Err(Error::invalid(format!("point {}: points must be in time order (beat {beat} is not after {prev})", i + 1)));
        }
        pairs.push((beat, p.value));
    }
    let pts = shape_curve(&pairs, step, a.shape);
    let (b0, b1) = (pairs[0].0, pairs[pairs.len() - 1].0);
    let covered = |beat: f64| beat >= b0 - 1e-9 && beat <= b1 + 1e-9;
    // the bars the span touches: the clear commands work in whole bars, so what they would wrongly take
    // (outside the span) is written back
    let (first_bar, last_bar) = (cx.bar_beat(b0).0, cx.bar_beat(b1).0.min(cx.p.bars));
    let (ba, bb) = (cx.bar_start(first_bar), cx.bar_start(last_bar + 1));
    let in_bars = |beat: f64| beat >= ba - 1e-9 && beat < bb - 1e-9;
    let tref = TrackRef::Id(t.id);
    let mut commands = Vec::new();
    let replaced;
    match cc {
        Some(cc) => {
            let mine = |c: &&CcPoint| c.cc == cc && in_bars(c.beat);
            replaced = t.ccs.iter().filter(mine).filter(|c| covered(c.beat)).count();
            let kept: Vec<CcPoint> = t.ccs.iter().filter(mine).filter(|c| !covered(c.beat)).copied().collect();
            if t.ccs.iter().any(|c| c.cc == cc && in_bars(c.beat)) {
                commands.push(Command::ClearCc { track: tref.clone(), cc: Some(cc), from_bar: Some(first_bar), to_bar: Some(last_bar) });
            }
            let mut all = to_cc(&pts, cc);
            all.extend(kept);
            all.sort_by(|x, y| x.beat.total_cmp(&y.beat));
            commands.push(Command::AddCc { track: tref, cc, points: cc_specs(&all) });
        }
        None => {
            replaced = t.bends.iter().filter(|b| in_bars(b.beat) && covered(b.beat)).count();
            // flux-core has no bends-only clear: ClearBars takes notes and CCs too, so they are put back
            if t.bends.iter().any(|b| in_bars(b.beat)) {
                commands.push(Command::ClearBars { track: Some(tref.clone()), from_bar: first_bar, bars: last_bar - first_bar + 1 });
                let notes: Vec<Note> = t.notes.iter().filter(|n| in_bars(n.start)).copied().collect();
                if !notes.is_empty() {
                    commands.push(Command::AddNotes { track: tref.clone(), notes: notes.iter().map(note_spec).collect() });
                }
                let mut by_cc: BTreeMap<u8, Vec<CcPoint>> = BTreeMap::new();
                for c in t.ccs.iter().filter(|c| in_bars(c.beat)) {
                    by_cc.entry(c.cc).or_default().push(*c);
                }
                for (cc, points) in by_cc {
                    commands.push(Command::AddCc { track: tref.clone(), cc, points: cc_specs(&points) });
                }
            }
            let mut all: Vec<(f64, i16)> = to_bends(&pts).into_iter().map(|b| (b.beat, b.value)).collect();
            all.extend(t.bends.iter().filter(|b| in_bars(b.beat) && !covered(b.beat)).map(|b| (b.beat, b.value)));
            all.sort_by(|x, y| x.0.total_cmp(&y.0));
            commands.push(Command::AddBend { track: tref, points: all.into_iter().map(|(beat, value)| BendSpec { bar: None, beat, value }).collect() });
        }
    }
    let what = match cc {
        Some(cc) => format!("CC{cc}"),
        None => "pitch bend".to_string(),
    };
    let summary = json!({
        "track": t.name,
        "data": what,
        "from": cx.at_json(b0),
        "to": cx.at_json(b1),
        "shape": a.shape,
        "points": pts.len(),
        "replaced": replaced,
    });
    Ok(Done { commands, summary, label: format!("{what} curve on {}", range_label(t, first_bar, last_bar)) })
}
