# Instrument catalog

One report card per sampled instrument (SFZ): every instrument the songs use, plus candidates. The composer can't
listen, so instruments are measured here before they go into a song. Each name links to its JSON card with all the
numbers (per-key pitch and level, brightness per octave, velocity and CC response, keyswitches, license).

Regenerate everything with `python -I tools/instrument_card.py --used`, or card one file with
`python -I tools/instrument_card.py path/to/file.sfz`.

## Flags

| Flag | What it means | Threshold |
|---|---|---|
| harsh | Too much energy above 3 kHz, so it sounds thin and piercing. The rejected lyre measured -28.5 dB; the liked VSCO harp -49.6 dB. The verdicts behind this threshold came from plucked instruments: bowed strings, reeds and brass are naturally brighter (about -10 to -30 dB), so compare those within their own family | hi3k_db > -35 dB in the main range |
| sharp attack | The start of each note is a spiky click well above the body of the note. The rejected zither melody strings measured 17.9 dB | attack > 12 dB |
| uneven | Some notes are much louder or softer than their neighbours. Fix: `tools/sfz_even.py` | neighbouring keys differ by more than 6 dB |
| out of tune | Some notes are off pitch | more than 15 cents (a semitone is 100) |
| short sustain | Held notes die away early because the recordings are short | a held note's recording runs out before 3 s |
| late start | A plucked or struck note whose recording begins with silence, so it sounds behind the beat (it also makes its attack number meaningless) | attack arrives more than 25 ms after note-on |
| level hole | Single keys (or a few) much quieter than the keys around them, e.g. a missing or badly cut sample | at least 10 dB under the median of the keys within 4 semitones |
| pitch slide | The pitch moves during the note (it scoops into or drifts off its note), measured from 0.04-0.2 s to 0.3-0.5 s | more than 20 cents on a key |
| cc dropout | The note goes silent at high mod wheel (CC1) or expression (CC11) values where lower values sound | 40 dB under the loudest value |
| partial / error | Some measurements, or the whole card, failed. See the JSON | |

## Columns

- **keys**: main playable range (C4 = MIDI 60). "+N isolated" = single keys outside it (keyswitches, FX); "N gaps" = silent keys inside it. Kits list every key that sounds.
- **offset**: sounding note minus played key, in semitones. +12 means it sounds an octave above the key you write. "kit" = unpitched (drum or FX map); "mixed" = pitched, but the octave changes across the range (e.g. an organ with several stops). Single keys that sound in another octave are listed in the JSON (`offset_outliers`).
- **tuning**: the worst pitch error of any key, in cents, over the first half second (so a note that starts flat and scoops up into pitch counts as out of tune). Bells, glockenspiel and timpani have inharmonic overtones, so their pitch, offset and tuning readings are rough.
- **evenness**: the biggest level jump between neighbouring keys (sd = spread of all key levels), at velocity 90.
- **hi3k_db / attack**: brightness and attack spike, median over the main range at velocity 70 (lower = softer).
- **sustain**: "held" = the note sustains while the key is down ("6 s+" = still going after 6 s, "held 4.5 s" = the recording runs out after 4.5 s); "rings" = a plucked or struck note dies away (time to -40 dB). "rel" = time to fade after the key is released.
- **license**: from the library's own LICENSE or README. A dagger (†) means the library is missing from `LICENSES.md`, so add it before shipping.

## Cards

| Name | Keys | Offset | Tuning | Evenness | hi3k_db | Attack | Sustain | Flags | License |
|---|---|---|---|---|---|---|---|---|---|
| [capital_city/bells](capital-city-bells.json) | C4-G5 (60-79) | +0 | +7c | 6.9 dB (sd 3.4) | -22.4 | 12.2 | rings 6 s+, rel 3.8 s | harsh, sharp attack, uneven | CC0 1.0 |
| [capital_city/flute](capital-city-flute.json) | C4-C7 (60-96) | +0 | +15c | 6.5 dB (sd 6.5) | -25.0 | -19.6 | held 5.66 s, rel 0.54 s | harsh, uneven | CC0 1.0 |
| [cithara-barbarica/02-cithara_barbarica_finger](cithara-barbarica-02-cithara-barbarica-finger.json) | E3-C5 (52-72) | +12 | +4c | 7.3 dB (sd 4.2) | -24.9 | 14.3 | rings 2.74 s, rel 0.18 s | harsh, sharp attack, uneven | CC0 1.0 |
| [far_roads/basses](far-roads-basses.json) | C1-C4 (24-60) | +0 | -10c | 14.5 dB (sd 5.5) | -43.0 | -9.4 | held 6 s+, rel 0.38 s | uneven, pitch slide | CC0 1.0 |
| [far_roads/cellos](far-roads-cellos.json) | C2-F5 (36-77) | +0 | -21c | 6.0 dB (sd 3.2) | -24.5 | -9.6 | held 6 s+, rel 0.38 s | harsh, out of tune | CC0 1.0 |
| [far_roads/clarinet](far-roads-clarinet.json) | D3-F#6 (50-90) | +0 | -6c | 8.0 dB (sd 4.0) | -31.7 | -25.2 | held 6 s+, rel 0.4 s | harsh, uneven | CC0 1.0 |
| [far_roads/flute](far-roads-flute.json) | C4-C7 (60-96) | +0 | +2c | 14.7 dB (sd 7.7) | -38.1 | -31.9 | held 6 s+, rel 0.38 s | uneven | CC0 1.0 |
| [far_roads/flute_vib](far-roads-flute-vib.json) | C4-C7 (60-96) | +0 | -13c | 23.4 dB (sd 9.0) | -34.9 | -33.2 | held 6 s+, rel 0.38 s | harsh, uneven | CC0 1.0 |
| [far_roads/harp](far-roads-harp.json) | E1-F7 (28-101) | +0 | -31c | 20.7 dB (sd 7.1) | -53.1 | 5.7 | rings 2.02 s, rel 1.78 s | late start, uneven, out of tune | CC0 1.0 |
| [far_roads/horn](far-roads-horn.json) | A1-F5 (33-77) | +0 | -88c | 9.9 dB (sd 6.7) | -67.2 | -4.9 | held 6 s+, rel 0.54 s | uneven, out of tune | CC0 1.0 |
| [far_roads/oboe](far-roads-oboe.json) | A#3-F6 (58-89) | +0 | +7c | 11.0 dB (sd 3.2) | -27.8 | -9.1 | held 6 s+, rel 0.4 s | harsh, uneven | CC0 1.0 |
| [far_roads/timpani](far-roads-timpani.json) | C2-C4 (36-60) | -12 | +33c | 3.9 dB (sd 3.1) | -54.7 | 17.3 | rings 2.82 s, rel 1.64 s | sharp attack, out of tune | CC0 1.0 |
| [far_roads/violas](far-roads-violas.json) | C3-D6 (48-86) | +0 | +26c | 6.4 dB (sd 2.4) | -18.4 | -9.5 | held 6 s+, rel 0.4 s | harsh, uneven, out of tune | CC0 1.0 |
| [far_roads/violins](far-roads-violins.json) | G3-D6 (55-86) | +0 | -26c | 7.3 dB (sd 2.8) | -13.4 | -16.3 | held 6 s+, rel 0.42 s | harsh, uneven, out of tune | CC0 1.0 |
| [karoryfer-bigcat.cello/02- Bowed (mod wheel)](karoryfer-bigcat-cello-02-bowed-mod-wheel.json) | C1-F5 (24-77) | +12 | +9c | 6.1 dB (sd 3.5) | -18.0 | 6.2 | held 6 s+, rel 0.3 s | harsh, uneven | CC0 1.0 † |
| [karoryfer.black-and-green-guitars/02-black_keyswitch](karoryfer-black-and-green-guitars-02-black-keyswitch.json) | E2-C5 (40-72) | +0 | +21c | 5.1 dB (sd 4.2) | -42.9 | 9.2 | rings 4.1 s, rel 0.7 s | out of tune | CC0 1.0 † |
| [karoryfer.cowsynth/cowsynth_shifty_pad](karoryfer-cowsynth-cowsynth-shifty-pad.json) | C-1-C8 (0-108) | +0 | +1c | 7.7 dB (sd 1.8) | -18.0 | -8.5 | held 6 s+, rel 2.34 s | harsh, uneven | CC0 1.0 † |
| [karoryfer.growlybass/growlybass_dirty](karoryfer-growlybass-growlybass-dirty.json) | A1-C6 (33-84), 1 gaps | -12 | +24c | 19.4 dB (sd 4.9) | -23.4 | 13.2 | rings 5.54 s, rel 0.26 s | harsh, sharp attack, uneven, out of tune | CC0 1.0 † |
| [legato_vocal_tutorial/01-no_legato_polyphonic_only_no_vibrato_a](legato-vocal-tutorial-01-no-legato-polyphonic-only-no-vibrato-a.json) | C3-A#4 (48-70) | -12 | +90c | 3.5 dB (sd 1.4) | -27.5 | -1.5 | held 6 s+, rel 0.24 s | harsh, out of tune, pitch slide | CC0 1.0 † |
| [makima/synth_bass](makima-synth-bass.json) | C1-C6 (24-84) | mixed | +45c | 9.9 dB (sd 3.8) | -17.3 | 12.8 | held 6 s+, rel 0.38 s | harsh, sharp attack, uneven, out of tune | CC0 1.0 † |
| [SalamanderGrandPiano/Salamander Grand Piano V3](salamandergrandpiano-salamander-grand-piano-v3.json) | A0-C8 (21-108) | +0 | +38c | 7.9 dB (sd 5.3) | -34.6 | 9.3 | rings 5.18 s, rel 2.22 s | harsh, uneven, out of tune | CC BY 3.0 |
| [sfz/01-no_legato_polyphonic_only_no_vibrato_a_retune](sfz-01-no-legato-polyphonic-only-no-vibrato-a-retune.json) | C3-A#4 (48-70) | -12 | +108c | 3.5 dB (sd 1.4) | -27.9 | -2.7 | held 6 s+, rel 0.22 s | harsh, out of tune, pitch slide | CC0 1.0 † |
| [sfz/CelloEnsPizz_retune_even](sfz-celloenspizz-retune-even.json) | C2-F5 (36-77) | +0 | +26c | 7.2 dB (sd 2.5) | -62.0 | 7.7 | rings 1.42 s, rel 2.06 s | uneven, out of tune, pitch slide | CC0 1.0 |
| [sfz/cold_metal_black](sfz-cold-metal-black.json) | E2-D6 (40-86) | +0 | +10c | 5.0 dB (sd 3.9) | -30.9 | 8.9 | rings 4.6 s, rel 0.46 s | harsh | CC0 1.0 † |
| [sfz/cold_metal_black_stac](sfz-cold-metal-black-stac.json) | E2-D6 (40-86) | +0 | +32c | 9.2 dB (sd 3.2) | -15.8 | 25.2 | rings 0.28 s | harsh, sharp attack, uneven, out of tune | CC0 1.0 † |
| [sfz/cold_metal_clock](sfz-cold-metal-clock.json) | B3-F4: 5 keys | kit | - | - | -30.2 | >60 | rings 0.04 s | harsh, sharp attack | CC0 1.0 † |
| [sfz/cold_metal_growly_dirty](sfz-cold-metal-growly-dirty.json) | A1-C6 (33-84), 1 gaps | -12 | +24c | 19.4 dB (sd 4.9) | -23.4 | 13.2 | rings 5.54 s, rel 0.04 s | harsh, sharp attack, uneven, out of tune | CC0 1.0 † |
| [sfz/cold_metal_growly_vicious](sfz-cold-metal-growly-vicious.json) | A1-C6 (33-84), 1 gaps | -12 | +40c | 19.9 dB (sd 4.8) | -15.4 | 19.9 | rings 0.24 s | harsh, sharp attack, uneven, out of tune | CC0 1.0 † |
| [sfz/cold_metal_industrial](sfz-cold-metal-industrial.json) | C3-A4: 19 keys | kit | - | - | -10.9 | 17.8 | rings 0.62 s | harsh, sharp attack | CC0 1.0 † |
| [sfz/far_roads_folk_harp](sfz-far-roads-folk-harp.json) | C2-A6 (36-93) | +0 | -9c | 10.4 dB (sd 2.3) | -45.0 | 10.4 | rings 2.34 s, rel 1.88 s | uneven | CC0 1.0 † |
| [sfz/far_roads_perc](sfz-far-roads-perc.json) | C2-G#3: 5 keys | kit | - | - | -27.9 | 15.2 | rings 0.8 s, rel 2.54 s | harsh, sharp attack | CC0 1.0 |
| [sfz/makima_fx](sfz-makima-fx.json) | E1-D#3: 18 keys | kit | - | - | -16.2 | 5.0 | rings 1.78 s, rel 0.94 s | harsh | CC0 1.0 † |
| [sfz/makima_kit](sfz-makima-kit.json) | A#1-A#3: 20 keys | kit | - | - | -8.0 | 17.4 | rings 0.24 s | harsh, sharp attack | CC0 1.0 † |
| [sfz/medieval_perc](sfz-medieval-perc.json) | C2-F#3: 4 keys | kit | - | - | -32.4 | 16.8 | rings 6 s+, rel 10 s+ | harsh, sharp attack | CC0 1.0 |
| [sfz/sonicpi_808](sfz-sonicpi-808.json) | C1-B2 (24-47) | +12 | +1c | 0.3 dB (sd 1.7) | -85.5 | 5.8 | rings 0.82 s | - | CC0 1.0 † |
| [sfz/sphinx_perc](sfz-sphinx-perc.json) | C2-F3: 17 keys | kit | - | - | -45.3 | 9.8 | rings 1.6 s, rel 1.7 s | - | CC0 1.0 † |
| [sfz/ViolinEnsPizz_retune_even](sfz-violinenspizz-retune-even.json) | G3-D6 (55-86) | +0 | -15c | 10.1 dB (sd 2.6) | -32.7 | 9.2 | rings 0.6 s | harsh, uneven | CC0 1.0 |
| [temporal_shift/basses](temporal-shift-basses.json) | C1-C4 (24-60) | +0 | -10c | 14.5 dB (sd 5.5) | -43.0 | -9.4 | held 6 s+, rel 0.38 s | uneven | CC0 1.0 |
| [temporal_shift/cellos](temporal-shift-cellos.json) | C2-F5 (36-77) | +0 | -21c | 6.0 dB (sd 3.2) | -24.5 | -9.6 | held 6 s+, rel 0.38 s | harsh, out of tune | CC0 1.0 |
| [temporal_shift/flute](temporal-shift-flute.json) | C4-C7 (60-96) | +0 | +15c | 6.5 dB (sd 6.5) | -25.0 | -19.6 | held 5.66 s, rel 0.54 s | harsh, uneven | CC0 1.0 |
| [temporal_shift/glock](temporal-shift-glock.json) | G4-C7 (67-96) | +12 | -78c | 5.7 dB (sd 2.0) | -13.9 | 17.6 | rings 2.3 s, rel 4.14 s | harsh, sharp attack, out of tune | CC0 1.0 |
| [temporal_shift/horn](temporal-shift-horn.json) | A1-F5 (33-77) | +0 | -88c | 9.9 dB (sd 6.7) | -67.2 | -4.9 | held 6 s+, rel 0.54 s | uneven, out of tune | CC0 1.0 |
| [temporal_shift/organ](temporal-shift-organ.json) | C2-C7 (36-96) | +0 | +25c | 12.5 dB (sd 5.2) | -46.5 | -32.0 | held 6 s+, rel 0.54 s | uneven, out of tune | CC0 1.0 |
| [temporal_shift/organ_pedal](temporal-shift-organ-pedal.json) | C2-F#4 (36-66) | +0 | +13c | 11.7 dB (sd 4.0) | -56.9 | -32.6 | held 6 s+, rel 0.5 s | uneven | CC0 1.0 |
| [temporal_shift/percussion](temporal-shift-percussion.json) | G#1-G#7: 47 keys | kit | - | - | -12.5 | 16.7 | rings 0.38 s | harsh, sharp attack | CC0 1.0 |
| [temporal_shift/trombone](temporal-shift-trombone.json) | A#1-F4 (34-65) | +0 | +1c | 5.2 dB (sd 5.2) | -50.9 | -4.6 | held 6 s+, rel 0.46 s | - | CC0 1.0 |
| [temporal_shift/trumpet](temporal-shift-trumpet.json) | E3-C6 (52-84) | +0 | -12c | 4.1 dB (sd 3.7) | -17.6 | 6.0 | held 6 s+, rel 0.38 s | harsh | CC0 1.0 |
| [temporal_shift/violas](temporal-shift-violas.json) | C3-D6 (48-86) | +0 | +26c | 6.4 dB (sd 2.4) | -18.4 | -9.5 | held 6 s+, rel 0.4 s | harsh, uneven, out of tune | CC0 1.0 |
| [temporal_shift/violin_solo](temporal-shift-violin-solo.json) | G3-C7 (55-96) | +0 | +16c | 9.3 dB (sd 3.8) | -9.2 | -5.3 | held 6 s+, rel 0.42 s | harsh, uneven, out of tune | CC0 1.0 |
| [temporal_shift/violins](temporal-shift-violins.json) | G3-D6 (55-86) | +0 | -26c | 7.3 dB (sd 2.8) | -13.4 | -16.3 | held 6 s+, rel 0.42 s | harsh, uneven, out of tune | CC0 1.0 |
| [VCSL/Baroque Alto Recorder - Sustain](vcsl-baroque-alto-recorder-sustain.json) | F4-G6 (65-91) | +0 | +10c | 5.0 dB (sd 5.1) | -28.0 | -0.1 | held 6 s+, rel 0.24 s | harsh | CC0 1.0 † |
| [VCSL/Baroque Alto Recorder - SusVib](vcsl-baroque-alto-recorder-susvib.json) | F4-G6 (65-91) | +0 | -8c | 9.7 dB (sd 6.9) | -27.8 | 0.4 | held 6 s+, rel 0.24 s | harsh, uneven | CC0 1.0 † |
| [VCSL/Baroque Soprano Recorder - Sustain](vcsl-baroque-soprano-recorder-sustain.json) | C5-D7 (72-98) | +0 | +11c | 5.7 dB (sd 4.7) | -18.3 | 3.9 | held 6 s+, rel 0.22 s | harsh | CC0 1.0 † |
| [VCSL/Baroque Tenor Recorder - SusVib](vcsl-baroque-tenor-recorder-susvib.json) | C4-D6 (60-86) | +0 | -17c | 7.3 dB (sd 7.5) | -27.2 | -2.7 | held 4.58 s, rel 0.24 s | harsh, uneven, out of tune | CC0 1.0 † |
| [VCSL/Folk Harp](vcsl-folk-harp.json) | C2-A6 (36-93) | +0 | -9c | 14.5 dB (sd 5.3) | -45.0 | 10.4 | rings 2.36 s, rel 8.62 s | uneven | CC0 1.0 † |
| [VCSL/Frame Drum](vcsl-frame-drum.json) | C4-F4: 6 keys | kit | - | - | -28.7 | 10.4 | rings 0.68 s, rel 2.66 s | harsh | CC0 1.0 † |
| [VCSL/Psaltery, Bowed and Plucked - LongBow](vcsl-psaltery-bowed-and-plucked-longbow.json) | G4-G6 (67-91) | +0 | -8c | 3.8 dB (sd 2.4) | -0.3 | -2.0 | held 3.12 s, rel 1.44 s | harsh | CC0 1.0 † |
| [VCSL/Psaltery, Bowed and Plucked - Pluck](vcsl-psaltery-bowed-and-plucked-pluck.json) | G4-G6 (67-91) | +0 | -10c | 6.4 dB (sd 1.8) | -4.2 | 13.2 | rings 3.24 s, rel 3.0 s | harsh, sharp attack, uneven | CC0 1.0 † |
| [VCSL/Strumstick](vcsl-strumstick.json) | C3-C6 (48-84) | +0 | +7c | 7.3 dB (sd 2.2) | -30.9 | 14.1 | rings 1.9 s, rel 3.36 s | harsh, sharp attack, uneven | CC0 1.0 † |
| [virtuosity_drums/01-basic-kit](virtuosity-drums-01-basic-kit.json) | B1-C6: 48 keys | kit | - | - | -20.6 | 19.7 | rings 0.58 s, rel 0.18 s | harsh, sharp attack | CC0 1.0 † |
| [VSCO-2-CE/CelloEnsPizz](vsco-2-ce-celloenspizz.json) | C2-F5 (36-77) | +0 | -20c | 7.4 dB (sd 2.9) | -62.0 | 7.7 | rings 1.42 s, rel 1.5 s | uneven, out of tune, pitch slide | CC0 1.0 |
| [VSCO-2-CE/SViolinVib](vsco-2-ce-sviolinvib.json) | G3-C7 (55-96) | +0 | -47c | 9.3 dB (sd 3.6) | -9.2 | 1.3 | held 6 s+, rel 0.6 s | harsh, uneven, out of tune | CC0 1.0 |
| [VSCO-2-CE/ViolinEnsPizz](vsco-2-ce-violinenspizz.json) | G3-D6 (55-86) | +0 | +16c | 9.5 dB (sd 2.8) | -32.7 | 9.2 | rings 0.6 s | harsh, uneven, out of tune | CC0 1.0 |
