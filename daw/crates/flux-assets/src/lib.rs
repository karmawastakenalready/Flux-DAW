//! Flux's asset tooling: the instrument / tool / plugin manifest with its license policy, a safe resumable
//! fetcher that only talks to official sources, install verification against pinned checksums, the generated
//! license overview and in-game credits, and a pure-Rust FLAC encoder for exports.
//!
//! # Pieces
//!
//! * [`manifest`]: `assets.toml`, schema 1. Every asset has a path, a version, its official sources (`git`,
//!   `archive`, `files`, `manual`, `vendored`, `external`), each pinned by commit or SHA-256 and size, a license
//!   (SPDX id or a `LicenseRef-*` defined in the manifest, plus the license file inside the install) and a credit
//!   line. Unknown fields are errors.
//! * [`license`] and [`audit`]: detect licenses from their text, classify them, and apply the project's policy
//!   (sound content must be CC0 / CC BY / permissive; non-commercial, share-alike, no-derivatives, copyleft or
//!   unknown sound content is blocked; freeware and purchased licenses need a recorded owner approval).
//! * [`fetch`]: install or update assets. HTTPS only, host allowlist on every redirect, resumable downloads
//!   with a size cap, SHA-256 checked before anything is extracted, safe extraction (no absolute paths, `..`,
//!   links or device files; size and entry-count limits) into a staging folder that is swapped in atomically,
//!   sparse/partial git clones pinned to a commit. Nothing is downloaded twice: present installs are checked and
//!   kept, and downloaded archives stay in a cache that is reused while its SHA-256 matches.
//! * [`lock`] and [`verify`]: `assets.lock` plus one tree index per asset (`assets.lock.d/<id>.tsv`, see
//!   [`hash`]); `verify` re-hashes installs and reports missing, modified, extra and line-ending-only files.
//! * [`credits`] and [`usage`]: the LICENSES.md overview and the credits text, with which project files use
//!   which asset (found by following `.flux.json` projects, Python scripts and SFZ includes).
//! * [`flac`]: 16/24-bit FLAC from the engine's float buffers, with the engine's TPDF dither, STREAMINFO MD5,
//!   Vorbis comments including `LOOPSTART` / `LOOPLENGTH`, and a decode check of every file written.
//! * [`cli`]: `flux assets status|verify|fetch|lock|repair|licenses` and `flux flac`.
//!
//! # Example
//!
//! ```no_run
//! use flux_assets::flac::{self, FlacOptions};
//! use flux_assets::verify::{self, VerifyOptions};
//! use flux_assets::workspace::{ConsoleUi, OpenOptions, Workspace};
//! use std::path::Path;
//!
//! # fn main() -> anyhow::Result<()> {
//! let ws = Workspace::open(Path::new("."), &OpenOptions::default())?;
//! let assets = ws.loaded.select(&["vsco-2-ce".to_string()])?;
//! for check in verify::verify(&ws, &assets, VerifyOptions { quick: false }, &mut ConsoleUi::default()) {
//!     println!("{}: {}", check.id, check.summary());
//! }
//!
//! let (left, right) = (vec![0.0f32; 44100], vec![0.0f32; 44100]);
//! let opts = FlacOptions::default().tag("TITLE", "Far Roads").looped(0, 44100);
//! let report = flac::write_f32_stereo(Path::new("out.flac"), 44100, &left, &right, &opts)?;
//! println!("{} bytes, MD5 {}", report.bytes, report.md5);
//! # Ok(())
//! # }
//! ```

pub mod audit;
pub mod cli;
pub mod credits;
pub mod extract;
pub mod fetch;
pub mod flac;
pub mod fsutil;
pub mod git;
pub mod glob;
pub mod hash;
pub mod http;
pub mod license;
pub mod lock;
pub mod manifest;
pub mod usage;
pub mod verify;
pub mod workspace;
