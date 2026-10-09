//! Windows: the application icon and version information, compiled into the executable as resources
//! (Explorer, the taskbar and the file's Properties dialog show them).

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icons/flux.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
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
