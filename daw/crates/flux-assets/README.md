# flux-assets

Asset tooling for Flux, in Rust:

* **`assets.toml`**: what lives in `instruments/`, `tools/` and `plugins/`, where it officially comes from,
  the exact version (commit or SHA-256 and size), its license and how to credit it.
* **Fetcher**: installs assets from those official sources only, safely and resumably, and never downloads
  the same thing twice. It replaces `setup_assets.py`.
* **Verification**: re-hashes installs against git blob ids, pinned checksums and a per-asset tree index
  (`assets.lock`).
* **License output**: a policy audit for a commercial Roblox game, a generated `LICENSES.md` overview and an
  in-game credits text.
* **FLAC encoder**: pure Rust, using the engine's `wav16` dither and checking every file it writes. It
  replaces `daw/bridge/to_flac.py`.

It needs no Python. Git is used through the `git` command line. RAR and 7z archives are unpacked with the
system's libarchive `bsdtar` (`C:\Windows\System32\tar.exe`), and only after their SHA-256 has been checked.

## Files

| Path | What |
| --- | --- |
| `assets.toml` | The manifest (hand-written). |
| `assets.lock` | Written by `fetch` / `lock`: per asset, the exact commit or archive, the method, and the tree digest. |
| `assets.lock.d/<id>.tsv` | Tree index of each install: `sha256\tsize\tpath`, one line per file, sorted. |
| `assets.lock.busy` | Present while a fetch, lock or repair runs, so two operations cannot run at once. |
| `<policy.download_dir>/<id>/` | Downloaded archives, kept so that `--force` works offline. |
| `.flux-staging-<id>-*` | A temporary folder next to an install while it is being built. It is renamed into place or removed. |

## Command line

`flux assets <command>`. During development the same front end runs as an example:
`cargo run -p flux-assets --release --example assets -- <ROOT> <command> ...`.

```text
flux assets <command> [options]

Commands:
  status   [ID...] [--no-usage] what is installed, locked, used, and each license's verdict (no hashing)
  verify   [ID...] [--quick]    re-hash installs and compare them with their sources and the lock
  fetch    ID... | --all        install from the official sources pinned in assets.toml
           [--yes] [--dry-run [--check-urls]] [--force] [--max-size SIZE]
  lock     [ID...]              check existing installs against their sources and record them in assets.lock
  repair   ID [--yes]           restore drifted files (git line endings, edits, deletions; pinned files)
  licenses [--out FILE] [--credits FILE] [--check FILE] [--texts DIR] [--print] [--no-usage]
                                license audit; writes the LICENSES.md overview / in-game credits text

Options for every command:
  --manifest FILE   manifest (default: <root>/assets.toml)
  --lock FILE       lock file (default: assets.lock next to the manifest)
  --json            print one JSON document instead of text

--no-usage skips scanning the project files in policy.usage_roots (the USED BY column, usage in LICENSES.md).
Exit codes: 0 ok, 1 problems found (drift, failed or refused fetches, license blockers), 2 bad arguments.
```

Exit codes:

| Code | Meaning |
| --- | --- |
| 0 | Success. |
| 1 | The command ran but found problems: drift or errors (`verify`), failed or refused fetches, a license blocker, a `--check` file missing a library, or a license text that could not be collected. |
| 2 | Bad arguments, or the command could not run (unreadable manifest, another operation running, and so on). |

`--json` prints a single document with the same information. Progress goes to stderr, and only when it is a
terminal.

### status

```text
assets.toml: 33 assets; lock assets.lock: 24 locked; usage: 9 roots, 42 files followed
ID                                 SCOPE  LICENSE                            POLICY          INSTALL              LOCK    USED BY
salamander-grand-piano             audio  CC-BY-3.0                          ok, credit      668 files, 748 MB    locked  3 files (daw/projects/cold_metal.flux.json, ...)
metal-gtx                          audio  LicenseRef-UnrealInstruments-Free  NEEDS APPROVAL  3391 files, 1.34 GB  locked  daw/projects/makima_control.flux.json
omnisphere                         audio  LicenseRef-Spectrasonics-EULA      approved        external             -       not used
dexed                              tool   GPL-3.0                            ok              5 files, 7.26 MB     locked  -
...

License findings: 0 blocker(s), 3 warning(s)
  warning  metal-gtx  license-needs-approval  LicenseRef-UnrealInstruments-Free (freeware): ...
```

* `POLICY` is one of `ok`, `ok, credit`, `approved`, `NEEDS APPROVAL` or `BLOCKED`.
* `INSTALL` is the file count and size, or `not installed`, `external` or `n/a`.
* `LOCK` is `locked`, `stale` (the manifest entry changed after locking) or `-`.
* `USED BY` lists the project files that reference the asset. Usage is found by following
  `policy.usage_roots`: `.flux.json` projects and Python scripts, then the SFZ `#include` and `sample=`
  chains they lead to.

### verify

Each asset gets one line, `ID  STATE  FILES, SIZE  SECONDS`, followed by what was checked and up to 10 paths
per issue kind:

```text
vsco-2-ce                          DRIFT          1764 files, 1.64 GB  0.8 s
    git 4403009: 1709 of 1713 checked-out files match their blob ids
    files: 51 of 51 pinned files match their sha256
    lock: tree matches (1764 files, cd23ea2)
    issues: eol-only 4, not-byte-exact 1
    eol-only         LICENSE
Summary: 12 DRIFT, 8 external, 1 not installed, 12 ok
```

* States are `ok`, `DRIFT`, `not installed`, `n/a (other platform)`, `external`, `unverified` and `ERROR`.
* Issue kinds that count as drift: `missing`, `modified`, `eol-only` (the file matches git once CRLF is turned
  back into LF), `extra`, `wrong-commit`, `sparse-mismatch`, `link-or-submodule` and `lock-differs`.
* `lock-stale`, `unlocked` and `not-byte-exact` are notes only.
* `--quick` compares only names and sizes with the lock, so it reads no file contents.

### fetch

Fetch prints the plan first, then the result for each asset:

```text
sfizz                              install (8.29 MB to download)
metal-gtx                          present
Download 8.29 MB for 1 asset(s) from their official sources?   (skipped with --yes)
  sfizz: installed
Summary: 1 installed, 1 present; downloaded 8.29 MB
```

* Results are `installed`, `replaced` (with `--force`), `updated` (missing pinned files added), `present`,
  `skipped` (manual or external), `refused` (over the size limit), `failed`, and in dry runs `planned` or
  `checked`.
* `--all` covers every required asset, plus optional ones that are already installed.
* `--dry-run --check-urls` contacts every source without downloading. Git sources use `ls-remote` and report
  whether the pinned commit is still the branch tip. Archives and files are probed with a one-byte range
  request, and the size the server reports is compared with the pinned size.

### lock

`lock` adopts existing installs, for example ones made by `setup_assets.py`. Each install is checked against
its source before anything is recorded: git blobs, the pinned files, and the cached archive's contents for
archive sources. A modified install is refused, and so is a file that no source accounts for.

### repair

`repair` puts a git or pinned-file install back to its pinned state. It restores edited, deleted and
CRLF-rewritten files, switches the checkout to byte-exact line endings and resets the sparse patterns. It
asks before changing anything. Archive installs are repaired with `fetch ID --force`, which reinstalls from
the cached archive.

### licenses

`licenses` prints the findings and the credits text. The options add to that:

* `--out` writes the LICENSES.md overview: which project uses what, the credits block, sound content, open
  license questions, tools, bundled app assets, and where each license text is.
* `--credits` writes the in-game credits.
* `--check FILE` lists the installed or used sound libraries that an existing file never mentions.
* `--texts DIR` collects every license text into `DIR/<id>/`. A text comes from the install where possible;
  otherwise it is downloaded from the pinned `text_url` and checked against `text_sha256`.

## Manifest

The schema is checked strictly: unknown keys are errors, ids are unique, asset folders may not overlap, every
URL host must be on the allowlist, and every SHA-256 and size must be well formed.

```toml
schema = 1

[policy]
allowed_hosts = ["github.com", "codeload.github.com", "*.githubusercontent.com"]
approval_threshold = "2G"            # larger downloads need --max-size (the owner's approval)
download_dir = "tools/_dl"
usage_roots = ["daw/projects/*.flux.json", "pieces/*.py", "instruments.py"]

[credits]
header = ["Music by ..."]            # first lines of the in-game credits
intro = "..."                        # paragraph at the top of the generated LICENSES.md

[[license]]                          # licenses that have no SPDX id
id = "LicenseRef-GeneralUser-GS-2.0"
name = "GeneralUser GS License v2.0"
family = "permissive"                # public-domain | permissive | attribution | freeware | weak-copyleft | ...
match = ["GeneralUser GS", "License v2.0"]

[[asset]]
id = "salamander-grand-piano"
name = "Salamander Grand Piano V3"
kind = "sample-library"
scope = "audio"                      # audio (may end up in the game) | tool | app (bundled with Flux)
path = "instruments/SalamanderGrandPiano"
version = "V3, SFZ edition"
authors = ["Alexander Holm"]
homepage = "https://github.com/sfzinstruments/SalamanderGrandPiano"
license = { id = "CC-BY-3.0", file = "LICENSE" }
credit = { role = "Piano samples", text = "Salamander Grand Piano V3 by Alexander Holm, licensed under CC BY 3.0", required = true }

[[asset.source]]
type = "git"                         # also: archive, files, manual, vendored, external
url = "https://github.com/sfzinstruments/SalamanderGrandPiano"
rev = "3382bf9..."                   # full 40-character commit
download_size = "1.1G"
```

Source types:

* `git`: `url`, `rev` (full commit), optional `branch`, `sparse` patterns, `cone`, `download_size`.
* `archive`: `url`, `sha256`, `size`, `format` (`zip`, `tar`, `tar.gz`, `tar.xz`, `rar`, `7z`), and
  optionally `page`, `file_name`, `cache`, `strip`, `include` globs, `dest`, `upstream_md5` and
  `skipped_entries`.
* `files`: `base_url` and `dest`, plus `[[asset.source.file]]` entries with `path`, `sha256`, `size` and an
  optional `url`.
* `manual`: `page`, `instructions`, and the pinned files to check after a manual download.
* `vendored`: files kept in this repository, with an optional `origin`.
* `external`: installed by another tool (pip, a vendor installer). It is documented here but not managed.

The license policy for `audio` scope:

* CC0, public domain and permissive licenses are allowed.
* CC BY is allowed with a credit.
* Freeware and proprietary licenses need `license.approved = "..."`, which records the owner's decision.
* Non-commercial, share-alike, no-derivatives, copyleft and unknown licenses are blocked.

Tools are never shipped, so a copyleft tool is allowed.

The declared license is checked against the license file on disk. Texts are recognised by their titles and
key sentences, so a GPL text that mentions the LGPL is still read as GPL. A mismatch is reported as a
blocker.

## Fetch safety

* **Network.** Only HTTPS to allowlisted hosts. Every redirect hop is checked again, and an HTML page in
  place of a file is refused. TLS is rustls (ring), and certificates are checked by the operating system's
  verifier.
* **Downloads.** A download goes to `<file>.part`. It resumes with `Range` plus `If-Range`, so a file that
  changed on the server starts over instead of being spliced. It stops at the pinned size and is renamed
  only when the size and SHA-256 match. Downloads above `approval_threshold` are refused unless
  `--max-size` raises the limit.
* **Archives.** Archives are untrusted input. Extraction rejects the following:
  * absolute, drive and UNC paths, and `..`
  * NUL and control characters, and names Windows forbids or reserves
  * alternate data streams
  * duplicate entries and names that differ only in case
  * links, devices and FIFOs
  
  Files are created with `create_new`. Entry count, entry size and total size are limited by the bytes
  actually written. Extraction goes into a fresh staging folder, which is verified and then renamed into
  place, so an interrupted fetch never leaves a half-installed asset behind.
* **Git.** Clones are partial (`--filter=blob:none`) and can be sparse. They are fetched by commit id and
  pinned to it. Afterwards every file is compared with its blob id, and the checkout is set to byte-exact
  line endings.
* **Nothing twice.** Installed assets are reported as `present`, missing pinned files are added one by one,
  and cached archives are reused while their SHA-256 matches.

## FLAC

```rust
use flux_assets::flac::{self, BitDepth, Dither, FlacOptions};

// Planar f32 stereo from the engine: 16-bit, TPDF seed 1 (identical samples to wav16 export).
let opts = FlacOptions::default().tag("TITLE", "Capital City").looped(0, 2_646_000);
let report = flac::write_f32_stereo(path, 44_100, &left, &right, &opts)?;

// 24-bit without dither, in memory.
let opts = FlacOptions { bits: BitDepth::B24, dither: Dither::None, ..FlacOptions::default() };
let (bytes, report) = flac::encode_f32(48_000, &[&left, &right], &opts)?;

// WAV to FLAC: integer WAVs stay lossless at their own depth; float WAVs are quantised as above.
let report = flac::wav_to_flac(Path::new("in.wav"), Path::new("out.flac"), &FlacOptions::default())?;

// Independent check of any FLAC: frame CRCs, sample count, and the decoded MD5 against STREAMINFO.
let info = flac::decode_check(&std::fs::read("out.flac")?)?;
```

Output files have the following properties:

* The metadata blocks are STREAMINFO (with the MD5), SEEKTABLE (one point every 10 s) and VORBIS_COMMENT
  (vendor string, then `TITLE`, `ARTIST` or any other tags, then `LOOPSTART` and `LOOPLENGTH` in samples).
* The block size is fixed at 4096, and every core is used.
* Files are written atomically.
* Each file is decoded again with claxon and must match the input's MD5 (`verify: false` turns this off).
* `FlacReport` gives the clip and non-finite counts, the size, the MD5 and the timings.
* Sample rates are limited to 96 kHz (a flacenc limit). The channel count is 1 to 8.

Command line: `flux flac IN.wav OUT.flac [--bits 16|24] [--no-dither] [--seed N] [--title T] [--artist A]
[--tag NAME=VALUE]... [--loop START LENGTH] [--threads N] [--no-verify] [--json]`. It prints:

```text
out.flac: 44100 Hz, 2 ch, 16-bit, 130.89 s -> 12.30 MB (53.3% of PCM), encoded in 0.08 s, verified (MD5 ...)
```

## Tests

`cargo test -p flux-assets` covers the following:

* the manifest schema and its validation
* license detection and the policy rules
* glob and path rules
* the tree index format
* HTTP: resume, `If-Range` restarts, redirects off the allowlist, HTML refusal, size caps and checksum
  mismatches, against a local test server
* malicious archives: zip-slip, absolute and UNC paths, device names, ADS, links, case collisions, bombs
  and lying headers
* end to end against a local git repository and HTTP server: fetch, re-fetch without downloads, drift,
  repair, re-lock, size limits, confirmation, the busy marker, and `--check-urls`
* the FLAC encoder: round trips at 16 and 24 bit, edge lengths and levels, tags and loop points, and golden
  dither values
* sample equality with `flux_engine::dsp::to_i16_tpdf`, which is the only reason `flux-engine` is a
  dev-dependency
