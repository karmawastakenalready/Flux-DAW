//! Run `flux flac IN.wav OUT.flac [options]` without the `flux` binary.
//!
//! ```text
//! cargo run --release -p flux-assets --example flac -- in.wav out.flac --loop 0 2116800 --title "Far Roads"
//! ```

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    flux_assets::cli::flac_main(&args)
}
