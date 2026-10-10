//! Windows: the application icon and version information, compiled into the executable as resources
//! (Explorer, the taskbar and the file's Properties dialog show them).
//!
//! `--features dist` (the build that is released, and is its own Setup) also carries, in OUT_DIR:
//! * `seed.zip`: the library's own files as tracked in git (the asset manifest, licenses, SFZ mappings, instrument
//!   cards), which Setup writes into the instrument library;
//! * `flux.com`: the terminal launcher (shim/flux_com.rs), compiled with rustc alone;
//! * `notices.txt`: the licenses of the crates compiled into Flux, from the file `FLUX_NOTICES` names (made by
//!   cargo-about in the release workflow), and of the fonts compiled into it.

use std::path::{Path, PathBuf};
#[cfg(feature = "dist")]
use std::{io::Write, process::Command};

/// What the seed holds, as `git ls-files` paths from the repository's root.
#[cfg(feature = "dist")]
const SEED: &[&str] = &["assets.toml", "LICENSE-MIT", "LICENSE-APACHE", "LICENSES.md", "licenses", "sfz", "catalog"];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icons/flux.ico");
    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    #[cfg(feature = "dist")]
    {
        seed(&dir, &out);
        notices(&dir, &out);
        if windows {
            shim(&dir, &out);
        }
    }
    if windows {
        resources(&dir, &out);
    }
}

fn resources(dir: &Path, out: &Path) {
    let icon = dir.join("assets").join("icons").join("flux.ico");
    let version = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    let mut parts: Vec<u16> = version.split(['.', '-', '+']).take(3).map(|p| p.parse().unwrap_or(0)).collect();
    parts.resize(4, 0);
    let numeric = parts.iter().map(u16::to_string).collect::<Vec<_>>().join(",");
    let exe = format!("{}.exe", std::env::var("CARGO_PKG_NAME").expect("CARGO_PKG_NAME"));
    let rc = format!(
        r#"#include <winver.h>

1 ICON "{icon}"

VS_VERSION_INFO VERSIONINFO
FILEVERSION {numeric}
PRODUCTVERSION {numeric}
FILEFLAGSMASK VS_FFI_FILEFLAGSMASK
FILEFLAGS 0
FILEOS VOS_NT_WINDOWS32
FILETYPE VFT_APP
FILESUBTYPE VFT2_UNKNOWN
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904B0"
        BEGIN
            VALUE "FileDescription", "Flux DAW"
            VALUE "FileVersion", "{version}"
            VALUE "InternalName", "{exe}"
            VALUE "OriginalFilename", "{exe}"
            VALUE "ProductName", "Flux"
            VALUE "ProductVersion", "{version}"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1200
    END
END
"#,
        icon = icon.display().to_string().replace('\\', "\\\\"),
    );
    let path = out.join("flux.rc");
    std::fs::write(&path, rc).expect("writing flux.rc");
    embed_resource::compile(&path, embed_resource::NONE).manifest_required().expect("compiling the Windows resources (needs the Windows SDK's rc.exe)");
}

/// `seed.zip`: the [`SEED`] files git tracks, with fixed dates so the same sources make the same zip.
#[cfg(feature = "dist")]
fn seed(dir: &Path, out: &Path) {
    let repo = dir.join("..").join("..").join("..");
    let listed = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["ls-files", "-z", "--"])
        .args(SEED)
        .output()
        .expect("running git ls-files (a dist build is made from a git checkout)");
    assert!(listed.status.success(), "git ls-files failed: {}", String::from_utf8_lossy(&listed.stderr));
    for s in SEED {
        println!("cargo:rerun-if-changed={}", repo.join(s).display());
    }
    let mut names: Vec<&str> = listed.stdout.split(|b| *b == 0).filter(|n| !n.is_empty()).map(|n| std::str::from_utf8(n).expect("a UTF-8 file name")).collect();
    names.sort_unstable();
    assert!(names.contains(&"assets.toml"), "assets.toml isn't tracked in {}", repo.display());
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(zip::DateTime::default())
        .unix_permissions(0o644);
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for name in names {
        // a file deleted but not yet committed is left out
        let Ok(bytes) = std::fs::read(repo.join(name)) else { continue };
        zip.start_file(name, opts).expect("adding to seed.zip");
        zip.write_all(&bytes).expect("adding to seed.zip");
    }
    let bytes = zip.finish().expect("finishing seed.zip").into_inner();
    std::fs::write(out.join("seed.zip"), bytes).expect("writing seed.zip");
}

#[cfg(feature = "dist")]
/// `notices.txt`: `FLUX_NOTICES`, or a note that this build has none (only release builds are handed out), then
/// the licenses of the fonts.
fn notices(dir: &Path, out: &Path) {
    println!("cargo:rerun-if-env-changed=FLUX_NOTICES");
    let mut text = match std::env::var_os("FLUX_NOTICES") {
        Some(p) => {
            println!("cargo:rerun-if-changed={}", Path::new(&p).display());
            std::fs::read(&p).unwrap_or_else(|e| panic!("reading FLUX_NOTICES ({}): {e}", Path::new(&p).display()))
        }
        None => {
            println!("cargo:warning=FLUX_NOTICES isn't set: this build ships without the third-party notices (fine for testing; releases have them)");
            b"This test build of Flux was made without its third-party notices. Released builds list the license of every \
crate compiled into Flux here (gathered by cargo-about; see .github/workflows/release.yml in Flux's source code).\r\n"
                .to_vec()
        }
    };
    // the fonts the GUI draws with, compiled into Flux (SIL Open Font License)
    let fonts = dir.join("assets").join("fonts");
    for (name, file) in [("Inter", "Inter-LICENSE.txt"), ("JetBrains Mono", "JetBrainsMono-OFL.txt")] {
        let path = fonts.join(file);
        println!("cargo:rerun-if-changed={}", path.display());
        let license = std::fs::read(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        text.extend(format!("\n================================================================================\nThe {name} font, compiled into Flux\n\n").as_bytes());
        text.extend(license);
    }
    std::fs::write(out.join("notices.txt"), text).expect("writing notices.txt");
}

#[cfg(feature = "dist")]
/// `flux.com`, built by the same rustc for the same target, with the C runtime linked in (it must run on any
/// Windows without the Visual C++ runtime) and small.
fn shim(dir: &Path, out: &Path) {
    let src = dir.join("shim").join("flux_com.rs");
    println!("cargo:rerun-if-changed={}", dir.join("shim").display());
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let target = std::env::var("TARGET").expect("TARGET");
    let status = Command::new(rustc)
        .args(["--edition", "2024", "--crate-type", "bin", "--crate-name", "flux_com", "--target", &target])
        .args(["-C", "opt-level=s", "-C", "panic=abort", "-C", "debuginfo=0", "-C", "strip=symbols", "-C", "target-feature=+crt-static"])
        .arg("-o")
        .arg(out.join("flux.com"))
        .arg(&src)
        .status()
        .expect("running rustc for flux.com");
    assert!(status.success(), "compiling {} failed", src.display());
}
