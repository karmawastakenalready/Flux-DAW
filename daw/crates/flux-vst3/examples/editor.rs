//! Open a plugin's editor through a plugin host and keep it up for a while (manual testing).
//!
//!     cargo run -p flux-vst3 --example editor -- "plugins/surge-xt/Surge XT.vst3" 20

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use flux_vst3::client::{HostOptions, LoadOptions, PluginHost};
    let path = std::env::args().nth(1).expect("usage: editor <plugin.vst3> [seconds]");
    let secs: u64 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(30);
    let exe = std::env::current_exe()?.parent().and_then(|d| d.parent()).map(|d| d.join("flux-plugin-host.exe"));
    let host = PluginHost::spawn(HostOptions { exe, label: path.clone(), ..Default::default() })?;
    let setup = flux_vst3::Setup { sample_rate: 48000.0, max_block: 512, offline: false };
    let (loaded, _port) = host.load(&LoadOptions::instrument(&path, setup))?;
    println!("{} by {} ({} params)", loaded.info.class.name, loaded.info.class.vendor, loaded.info.params);
    host.open_editor(&format!("{} - Flux", loaded.info.class.name), None)?;
    let end = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while std::time::Instant::now() < end && host.alive() {
        for e in host.take_events() {
            println!("{e:?}");
            if matches!(e, flux_vst3::ipc::HostEvent::EditorClosed) {
                return Ok(());
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    host.shutdown()
}

#[cfg(not(windows))]
fn main() {}
