//! Hosts one VST3 plugin instance for Flux, out of process (see `flux_vst3::server`).

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    flux_vst3::server::main()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("flux-plugin-host: only Windows is supported so far");
    std::process::exit(2);
}
