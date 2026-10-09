//! Run `flux assets ...` without the `flux` binary, against any project root.
//!
//! ```text
//! cargo run -p flux-assets --example assets -- ROOT status [--manifest FILE] [--lock FILE] [--json]
//! cargo run -p flux-assets --example assets -- ROOT verify --quick
//! cargo run -p flux-assets --example assets -- ROOT licenses --out LICENSES.md --credits credits.txt
//! ```
//!
//! It calls [`flux_assets::cli::assets_main`], the entry point meant for `flux assets` in the `flux` binary.

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((root, rest)) = args.split_first() else {
        eprintln!("usage: assets ROOT <command> [options]\n\n{}", flux_assets::cli::ASSETS_HELP);
        return ExitCode::from(2);
    };
    flux_assets::cli::assets_main(&PathBuf::from(root), rest)
}
