# flux-score

Flux's composing toolkit: music theory, melody notation, voice leading, bass lines, phrasing, controller
shapes, the chord track, and the score linter. It powers two MCP tools, `compose` and `lint`.

The crate never changes a project. It reads a `flux_core::Project` and returns `flux_core::Command`s
(apply them as one batch, which is one undo step) or a report. Agent edits and GUI edits therefore go
through the same command system.

## Layers

| module | what it does |
|---|---|
| `theory` | note names and spelling, chord symbols (`Cmaj7`, `F#m7b5`, `G7(b9 #11)/B`, `N.C.`), scales and modes, chord scales, and how a pitch class relates to a chord (chord tone, tension, avoid note, outside) |
| `meter` | time signatures and metric weight (6/8 counts dotted quarters) |
| `notation` | melody notation, e.g. `E4:2 G4:1 B4:1 \| A4:3 r:1` |
| `voicing` | voice leading (lowest total motion, no parallels with the bass) and bass lines |
| `phrase` | player-like dynamics: metric accents, arcs to the phrase peak, softened endings, seeded variation |
| `shapes` | ramps, curves through points (linear, smooth or stepped) and swells that follow a line |
| `harmony` | `Project::harmony` as time spans, plus chord charts (`Am \| F C \| G@0 G/B@2.5 \| %`) |
| `pattern` | rhythm patterns for chords, bass lines and arpeggios (`x--x --x-`, `x.o. 5.x.`, `0123 2101`) |
| `compose` | the `compose` tool |
| `lint` | the `lint` tool |

`notation`, `voicing`, `phrase` and `shapes` are ports of `daw/bridge/score.py`. Given the same input
they produce the same output (see Verification below).

## The `compose` tool

```rust
let out = flux_score::compose(&project, &serde_json::json!({
    "action": "chords", "track": "Pad", "from_bar": 1, "to_bar": 4,
    "chords": "Am | F | C | G E7", "pattern": "x--x --x-", "step": 0.5
}))?;
session.apply_batch(&out.commands, Source::Agent)?;          // one undo step
let reply = out.to_json();                                   // {"action", "commands": n, "summary", "warnings"?}
```

| action | writes |
|---|---|
| `melody` | a line from notation, optionally phrased; reports how it fits the chords |
| `chords` | voice-led chords from the chord track (or a chart, which is also written to the chord track), struck in a pattern |
| `bass` | a bass line from the chord track (root, octave, fifth, slash-chord inversions) |
| `arpeggio` | broken chords and ostinatos: voicing tones by index in a pattern |
| `phrase` | reshapes the dynamics of existing notes |
| `swell` | an expression CC that follows a line |
| `curve` | a CC or pitch-bend curve through points |
| `harmony` | sets the chord track from a chart, or reads it back (tones, chord scale, tensions, avoid notes) |
| `fit` | checks a part against the chord track and can move clashing notes to chord tones |

Running an action again over the same range gives the same project. Note actions replace the notes
that start in their bars, but keyswitch blips (notes under 0.05 beat) are kept. `swell` and `curve`
replace the controller data they cover. `compose_schema()` gives the arguments' JSON Schema, and
`compose_tool()` gives the full MCP tool definition.

## The `lint` tool

```rust
use flux_score::lint::{lint, Check, InstrumentCatalog, LintOptions};
let (catalog, skipped) = InstrumentCatalog::load_dir(Path::new("catalog"))?;
let report = lint(&project, &LintOptions { catalog: Some(catalog), ..LintOptions::default() });
println!("{}", report.to_text(40));       // score_lint.py's layout
let json = report.to_json(40);            // for agents
// or, from MCP arguments:
let reply = flux_score::lint_handler(&project, &serde_json::json!({"catalog_dir": "catalog", "min_severity": "warning"}))?;
```

| check | finds | severity |
|---|---|---|
| `clash` | two tracks a minor 2nd, major 7th or minor 9th apart for half a beat or more (passing notes excepted; a minor 2nd or major 7th between two tones of the chord track's chord, as in Bbmaj7 or Dm9, is that chord's colour) | warning |
| `nct` | melody notes on strong beats that are not chord tones: appoggiaturas (resolve by step) or clashes | info / warning |
| `range` | keys the instrument does not play (catalog card runs, or the caller's range), as written | error |
| `mud` | more than 3 sustained pitches within an octave below C3 | warning |
| `parallels` | parallel fifths and octaves between the melody and the lowest sounding voice | info |
| `crossing` | a lower line sounding above a higher one | info |
| `leaps` | leaps wider than `max_leap` inside a phrase (arpeggio-like lines are skipped) | info |
| `overlaps` | duplicate notes (error); a note still sounding when the same key strikes again (warning) | error / warning |
| `controllers` | several CC or pitch-bend points of one kind at the same beat | warning / info |
| `past_end` | notes starting after the end, notes cut by the end of the render, controller data after the end | warning / info |
| `keyswitches` | held keyswitch keys, stray blips on keys that are not keyswitches (needs a card) | info / warning |
| `harmony` | chord marks that cannot be read, or that come after the end | error / info |

Pitches are judged as heard. The track's transposition and its pitch bend (±`bend_range` semitones,
measured a little way into each note) are applied. `range`, `overlaps` and `keyswitches` look at the
keys as written. Each finding is listed once, at the bar where it first happens; the other bars where it
recurs go in `also_bars`. Every finding has a kind, a severity, its bar and beat, the tracks involved, a
message and a suggested fix.

## Examples

```text
cargo run -p flux-score --example lint -- daw/projects/far_roads.flux.json --catalog catalog
cargo run -p flux-score --example mcp_calls
```

## Verification

* `tests/python_parity.rs`: notation, chord tones, metric weights, voice leading, bass lines, ramps,
  curves and swells match `score.py` exactly. Phrasing also matches exactly when random variation is off.
  With variation on, it matches only structurally, because score.py uses Python's Mersenne Twister and
  this crate uses splitmix64.
* `tests/lint_parity.rs`: on far_roads (6/8) and makima_control (4/4, pitch bends), with the build scripts'
  `HARM` as chord marks, `clash`, `nct` and `mud` print the same summary and finding lines as
  `tools/score_lint.py`.

## Dependencies

`flux-core`, `serde`, `serde_json` and `thiserror`. All are MIT or Apache-2.0.
