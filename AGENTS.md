# AGENTS.md

How to work on Flux: the rules every change follows, how the code is laid out, how to build and test it, and
step-by-step guides for the common extensions (a new VST3 plugin, a new instrument library, a new MCP tool, a new
agent client). It's written for AI coding agents and people alike.

Making music *with* Flux is a different job. For that, connect an agent to Flux (`flux agents connect`) and let it
read the MCP server's instructions and the `flux://guide` resource. This file is about changing Flux itself.

## Ground rules

These come from the owner and apply to every change. If a task seems to need an exception, stop and ask. Don't
work around them.

1. **No AI audio.** No AI audio models of any kind, for generating, transforming or "enhancing" sound.
2. **No home-made instruments.** Every sound comes from a real recording or someone else's work: sample
   libraries, plugins, and their makers' presets. Never design a synth patch, build an instrument from raw
   oscillators, or ship presets of our own. Mixing, mastering and effects on those sounds are fine.
3. **Official sources only.** Download from the project's own repository or releases, or from the author's own
   site. crates.io counts. No mirrors, re-uploads, "repacks" or cracked builds. Every download is pinned (a full
   git commit, or a SHA-256 and size), and its host must be on `policy.allowed_hosts` in `assets.toml`.
4. **Commercial-safe licenses.** Songs made with Flux ship in a commercial game, so anything that can end up in a
   song (`scope = "audio"`) must be CC0, public domain, a permissive license (MIT, BSD, ...), or CC BY with a
   credit. Non-commercial, share-alike, no-derivatives, copyleft and unknown licenses are blocked. Freeware or
   proprietary terms need `license.approved`, which records **the owner's** decision in their words. An agent
   never writes `approved` on its own judgment. Ask, then quote the answer.
5. **Permissive code only.** Flux is MIT OR Apache-2.0. Crates compiled into it must be under a license in
   [`daw/about.toml`](daw/about.toml)'s `accepted` list. No GPL, LGPL or MPL crates (`symphonia` is MPL, so it's
   out). Tools that are never shipped, such as a GPL synth used as a plugin, may be copyleft.
6. **2 GB per instrument set.** No single library or pack over 2 GB without the owner's approval. The fetcher
   enforces `policy.approval_threshold`.
7. **Never in the repository:** songs, song projects (`*.flux.json`), song build scripts, renders, sample
   libraries, plugins, `plugin-states/` (saved plugin states hold vendors' preset data, which their licenses
   forbid sharing), reference tracks, secrets, and anyone's personal paths or email addresses. `.gitignore`
   covers most of these. Check `git status` before committing anyway. The Flux logo and all songs are all rights
   reserved, not open source.
8. **Agent-first.** Every change to a project is a `Command` in `flux-core`. The GUI and the MCP server are thin
   layers over the same commands, the same undo history and the same activity log. Never add a way to change a
   project that only one of them can use.
9. **Downloads are untrusted.** Archives, preset files, SFZ files and plugin output are input from strangers:
   cap sizes, reject odd paths, never follow links, never panic on malformed data. Plugins run in their own
   process (`flux plugin-host`) so a crash can't take Flux down.
10. **Don't touch the user's machine in tests.** Tests use temporary folders and fake environments. They never
    edit real agent configs, settings or libraries, and GUI snapshots show fake paths (`C:\Users\Alex\...`,
    `D:\Flux Library`), never the machine's own.

## What Flux is

An agent-first DAW for original game music that loops without a seam. An AI agent (over MCP) and a person (in the
window) edit the same project live. Songs are mastered for Roblox by default: −15 LUFS integrated, true peaks at
or below −1 dBTP, a seamless loop, exported as 16-bit FLAC under 20 MB.

Windows only for now, because the VST3 host is Win32.

## Repository layout

| Path | What's there |
|---|---|
| `daw/crates/flux` | the app: GUI (egui), MCP server (`mcp.rs`, `tools.rs`, `measure.rs`, `plugins.rs`), CLI (`main.rs`), Setup and updates (`dist.rs`, `gui/setup.rs`, `gui/library.rs`) |
| `daw/crates/flux-core` | the project model and the commands that change it; undo; the session |
| `daw/crates/flux-engine` | instruments (sfizz, VST3, rigs), the channel strip and master, offline and live rendering, `paths.rs`, settings |
| `daw/crates/flux-vst3` | VST3 hosting out of process; plugin scanning; vendor preset formats (`vendor.rs`) |
| `daw/crates/flux-nam` | Neural Amp Modeler inference and cabinets for guitar rigs |
| `daw/crates/flux-score` | music theory, `compose` and `lint` |
| `daw/crates/flux-analysis` | loudness, spectra, seam checks, mix checks, instrument cards |
| `daw/crates/flux-sfz` | SFZ parsing; `flux sfz` (wrapper sets, checks, tightened/retuned/evened copies) |
| `daw/crates/flux-assets` | `assets.toml`: manifest, safe fetching, verification, licenses, FLAC ([README](daw/crates/flux-assets/README.md)) |
| `daw/crates/flux-setup` | the instrument library's packs, install/uninstall, agent clients (`agents.rs`), update checks |
| `assets.toml`, `assets.lock` | every instrument, plugin and tool: official source, pinned checksums, license, packs |
| `sfz/`, `catalog/`, `licenses/` | our SFZ mappings and rigs, instrument report cards, license texts. Released builds carry these into the user's library |
| `LICENSES.md` | generated from `assets.toml`. Never edit it by hand |
| `.github/workflows/release.yml` | builds `FluxSetup-<version>.exe` and drafts a GitHub release |
| `compose.py`, `render.py`, ... | the retired Python pipeline. Don't extend it. New work is Rust |

## Building and testing

You need Windows and a recent stable Rust (1.88 or newer: the code uses let chains). From `daw/`:

```powershell
cargo build -p flux                               # target\debug\flux.exe
cargo build -p flux --features dist               # the released program: Setup, the flux.com launcher, the library seed
cargo test --workspace
cargo clippy --workspace --all-targets            # must be clean
cargo clippy -p flux --features dist --all-targets
```

- Tests that need an installed plugin or library skip themselves when it's missing, so a fresh clone passes.
  Install things with `flux assets fetch ID` to run them too.
- GUI tests use egui_kittest and compare pixel snapshots in `daw/crates/flux/tests/snapshots/`. After an
  intended GUI change, run `UPDATE_SNAPSHOTS=1 cargo test -p flux` (in PowerShell: `$env:UPDATE_SNAPSHOTS=1`),
  then **look at every changed PNG** before keeping it.
- The `dist` build reads `git ls-files`, so build it from a git checkout. Release builds also set
  `RUSTFLAGS=-C target-feature=+crt-static` and `FLUX_NOTICES` (see the release workflow).
- `daw/tests/mcp_client.py` drives a running `flux mcp` from a JSON script, for end-to-end checks of tools.
- Don't launch GUI windows or play audio on someone's machine unless they asked for it. Use the headless
  commands (`flux render`, `flux live`, the MCP server without `--gui`) and the snapshot tests.

### Code style

- Match the file you're in. There's no rustfmt config and the code isn't rustfmt-formatted, so **don't run
  `cargo fmt`** (it would rewrite whole files). Lines run up to about 120 characters.
- Files mix LF and CRLF line endings. Keep each file's own.
- Doc comments say what a thing is and why it exists, in plain sentences. Line comments are short, lowercase
  and only for what isn't obvious.
- Errors use `anyhow` with context, and the message tells the reader what to do next. Untrusted or user
  input never panics.
- User-facing text is plain English, in sentence case, without jargon. Say what happened and what to do.
- GUI: dark neutral surfaces, one blue accent, compact, in the manner of Adobe Audition/Premiere. Use the colours
  and sizes in `gui/theme.rs` and the controls in `gui/widgets.rs`. Don't invent new ones. Every new panel or
  dialog gets a kittest test and a snapshot.
- Prefer small, well-named functions and a test next to the code. Don't add a dependency without checking its
  license against rule 5 and asking whether it's worth the weight.

## How to add a VST3 plugin

Flux hosts any VST3 it finds, but a plugin is *supported* once it's in `assets.toml`. Then Setup and the Library
window offer it, agents see it in `not_installed` lists with how to get it, and its license is audited.

1. **Check the license.** Commercial music made with it must be allowed. Read the EULA itself, not a store page.
   - A plugin whose own content makes the sound (preset banks, bundled samples) gets `scope = "audio"`. Unless
     the content is CC0/permissive/CC BY, it needs the owner's `approved` (rule 4).
   - A plugin that only processes sound, or an open-source program whose license covers the code and not what
     it renders, gets `scope = "tool"`. Add a `note` saying why. See `surge-xt`.
2. **Add the asset** to `assets.toml` with `kind = "plugin"`. Pick the source type:
   - `external`, for plugins installed with their maker's installer (most commercial freeware). Give the
     official `page`, `instructions`, and `installed = ["{vst3}/Name.vst3"]` so Flux can tell whether it's
     there. Add `{documents}/...` too if the presets live outside the plugin.
   - `archive`, for portable builds on the official releases page. Pin `url`, `sha256`, `size`, `format`,
     `page` and `cache`. Flux downloads it into `instruments/vst/<id>`, which the plugin scan searches.
3. **Add a pack:** a `[[pack]]` with `section = "plugins"`. Only add it to a `[[selection]]` (the quick
   choices) if it's free and broadly useful.
4. **Make its presets load headless** if it saves them in its own format (Vital, Valhalla, Decent Sampler and
   STL Tones already do). In `daw/crates/flux-vst3/src/vendor.rs`:
   - add a `Family` variant;
   - detect it in `family(class_name, vendor)`;
   - give its `extension()` and the standard preset folders in `dirs()`;
   - list its presets;
   - build the component state in `to_state()`;
   - document the state layout in the module docs.

   Test it with *synthetic* presets written in the test. Vendor preset data never goes in the repo. Also extend
   `untrusted_input_never_panics`.

   Plugins with factory programs need none of this: `plugins set_program` loads them. For everything else,
   `set_params` plus `capture` saves the state.
5. **Try it end to end** over MCP:
   - `plugins scan` (rescan=true), then `info` and `presets`;
   - put it on a track, `load_preset`, `render`, `analyze`;
   - check the loop seam and that two renders match.

   If it misbehaves headless (needs its window, drops notes until samples load), handle it in the host. See
   `Family::loads_in_background` and `crate::settle`.
6. **Regenerate the license overview** from the repo root with `flux assets licenses --no-usage --out
   LICENSES.md`. `flux assets status` must show no blockers.

## How to add an instrument library

1. **Find the official source** and read the license file *inside the download*, not just the web page. It
   must pass rule 4. CC BY needs a `credit` with `required = true`. Keep it under 2 GB (rule 6).
2. **Add the asset** to `assets.toml`, following the existing entries:
   - `id` (kebab-case), `name`, `kind = "sample-library"`, `scope = "audio"`;
   - `path = "instruments/<Folder>"`, `version`, `authors`, `homepage`, `description`;
   - `license = { id, file }` (the SPDX id, or a `[[license]]` entry for licenses without one) and `credit`.
3. **Pin the source:**
   - `git`: `url`, the full 40-character `rev` and `download_size`. Use `sparse` to skip what isn't needed.
   - `archive`: `url`, `sha256`, `size`, `format`, `page` and `cache`.
   - If the official host can't be downloaded from directly (a login wall, a Google Drive confirmation page),
     set `by_hand = true` and keep `page`. Setup sends the user there, and *Import from Downloads* (or `flux
     packs import`) picks up the file by its checksum.
   - A new host goes on `allowed_hosts` only if it's the author's own.
4. **Put it in a pack.** Add it to a `[[pack]]`, either an existing one or a new one in the default
   instruments section. A pack that needs a plugin lists it in `needs`. Decide whether a `[[selection]]` gets
   it. Minimal stays small.
5. **Install and check it:**
   - `flux assets fetch ID`, `flux assets verify ID` and `flux assets status`;
   - `list_instruments` shows its SFZ files, and `probe_instrument` gives each one's playable range.
6. **Measure before recommending it:** `instrument_card` writes `catalog/<slug>.json`. Read its flags (harsh,
   sharp attack, uneven, out of tune...), because whoever composes with it can't listen first.
7. **Fix problems in copies, never in the download.** The installed files stay byte-exact to their pin.
   - Use `flux sfz tighten|retune|even`, or a wrapper SFZ in `sfz/`, for range, tuning or level fixes.
   - `flux sfz build` builds the wrapper sets defined in `sfz/sets/`.
8. **Regenerate `LICENSES.md`** (`flux assets licenses --no-usage --out LICENSES.md`).

## How to add or change an MCP tool

Each tool costs every agent context on every session, so first ask whether it should be a new tool at all.

- **Changing a project** is never a new tool:
  - add a `Command` variant in `daw/crates/flux-core/src/commands.rs`, with validation and undo;
  - give it a schema in `command_s()` in `daw/crates/flux/src/tools.rs`;
  - `edit` and the GUI then both get it.
- **A new option or action on an existing tool** beats a new tool. Most tools take an `action` or a `detail`.
- **A new tool** is for a genuinely new capability, such as a new kind of measurement.

When you do add one:

1. **Put the logic in the crate it belongs to,** with unit tests there:
   - `flux-score` for theory and composing;
   - `flux-analysis` for measurements;
   - `flux-engine` for rendering;
   - `flux-assets` or `flux-setup` for the library.

   The tool layer only parses arguments and formats results.
2. **Register it.** In `tools.rs`, use `tool(name, description, properties, required)` in `list()` and add an
   arm to `dispatch()`. Measurement tools go in `measure.rs` (`list()` and `call()`).
3. **Write the description for an agent that has never seen Flux:**
   - what the tool does and what it returns;
   - units and defaults;
   - roughly how long a slow call takes.

   Use the shared conventions: bars are 1-based, `beat` is a 0-based offset in quarter notes, and pitches are
   names or MIDI numbers.
4. **Return JSON** through `text(json!(...))`, and images as `Content::Png`. Errors are `anyhow` errors that
   say what to try instead. Changes to the project go through `sh.apply(&cmds, Source::Agent)`, so they're
   undoable and show up in `activity`.
5. **Update the docs** if the workflow changes: `INSTRUCTIONS` and the guide in `daw/crates/flux/src/mcp.rs`,
   and the tool count in the README badge.
6. **Try it** with `daw/tests/mcp_client.py`, or connect an agent and use it for real.

## How to add an agent client

`flux agents connect` and Setup's Agents page know each client's config file. To add one:

1. **Add a `Spec`** to `clients()` in `daw/crates/flux-setup/src/agents.rs`:
   - `id` and `name`;
   - `store`: a JSON key and `Shape`, Codex-style TOML, or a manual snippet for formats Flux shouldn't edit;
   - `then`: what the user does next for the agent to see Flux;
   - `places`: the config files, each with the folder whose presence means the client is installed;
   - `programs` and `installs`.
2. **Use the client's own documentation** for the file location and entry shape. If it differs from every
   existing `Shape`, add one and test its snippet.
3. **Extend the tests** in `agents.rs`: detection, connecting while keeping comments and other servers, and the
   snippet. They run against a temporary `Env` and must never touch real config files.

## Releasing

Only the owner decides when a release goes out. Don't push tags, run the release workflow or publish a release
unless they've asked for it.

1. Bump `version` in `[workspace.package]` in `daw/Cargo.toml`, build (to update `Cargo.lock`), and commit.
2. Push a tag that matches it: `git tag v0.2.0` and then `git push origin v0.2.0`. The Release workflow:
   - runs the installer crates' tests;
   - gathers the third-party notices with cargo-about (a crate under a license not on the `accepted` list
     fails the release);
   - builds the static `flux.exe` and names it `FluxSetup-<version>.exe`;
   - signs it if the signing secrets are set;
   - adds a `.sha256` and attaches everything to a **draft** release.
3. A person reviews the draft: reads the notes, installs it on a clean machine, opens a song and renders it.
   Then they publish. Flux's update check (at most once a day, and it can be turned off) only sees published
   releases, so nothing reaches users before that.

The workflow can also be run by hand from the Actions tab. That builds the Setup as an artifact and makes no
release.
