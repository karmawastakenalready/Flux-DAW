//! Loading a VST3 module (`.vst3` bundle or single file), its factory, and the classes it exports.

use crate::util::{check, cid_hex, str16, str8};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use vst3::Steinberg::*;
use vst3::{ComPtr, Interface};

/// Category of the processor classes a host instantiates ("Audio Module Class").
pub const AUDIO_MODULE_CLASS: &str = "Audio Module Class";
/// Category of edit-controller classes.
pub const CONTROLLER_CLASS: &str = "Component Controller Class";

#[cfg(all(windows, target_arch = "x86_64"))]
const ARCH_DIR: &str = "x86_64-win";
#[cfg(all(windows, target_arch = "aarch64"))]
const ARCH_DIR: &str = "arm64-win";
#[cfg(all(windows, target_arch = "x86"))]
const ARCH_DIR: &str = "x86-win";
#[cfg(not(windows))]
const ARCH_DIR: &str = "x86_64-linux";

/// One class a module's factory exports.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ClassInfo {
    /// Class id as 32 hex digits.
    pub cid: String,
    pub name: String,
    /// "Audio Module Class", "Component Controller Class", ...
    pub category: String,
    /// "|"-separated, e.g. "Instrument|Synth" or "Fx|Delay".
    #[serde(default)]
    pub sub_categories: String,
    #[serde(default)]
    pub vendor: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub sdk_version: String,
    #[serde(default)]
    pub class_flags: u32,
    #[serde(default)]
    pub cardinality: i32,
}

impl ClassInfo {
    pub fn is_processor(&self) -> bool {
        self.category == AUDIO_MODULE_CLASS
    }

    pub fn is_instrument(&self) -> bool {
        self.sub_categories.split('|').any(|s| s.eq_ignore_ascii_case("Instrument"))
    }
}

/// What the factory says about its vendor.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct FactoryInfo {
    pub vendor: String,
    pub url: String,
    pub email: String,
    pub flags: i32,
}

/// Where a plugin's files are: the bundle directory (if any) and the binary to load.
#[derive(Clone, Debug, PartialEq)]
pub struct ModulePaths {
    /// `Foo.vst3` bundle directory (None for single-file plugins).
    pub bundle: Option<PathBuf>,
    /// The library inside it (or the file itself).
    pub binary: PathBuf,
}

/// Resolve a user-given path: a bundle directory, the binary inside a bundle, or a single-file plugin.
pub fn resolve(path: &Path) -> Result<ModulePaths> {
    if path.is_dir() {
        let name = path.file_name().ok_or_else(|| anyhow!("{} has no file name", path.display()))?;
        let bin = path.join("Contents").join(ARCH_DIR).join(name);
        if !bin.is_file() {
            bail!("{} is a directory but has no {}", path.display(), Path::new("Contents").join(ARCH_DIR).join(name).display());
        }
        return Ok(ModulePaths { bundle: Some(path.to_path_buf()), binary: bin });
    }
    if !path.is_file() {
        bail!("plugin not found: {}", path.display());
    }
    // .../Foo.vst3/Contents/<arch>/Foo.vst3 -> bundle .../Foo.vst3
    let bundle = path
        .parent()
        .and_then(|arch| arch.parent())
        .filter(|c| c.file_name().is_some_and(|n| n == "Contents"))
        .and_then(|c| c.parent())
        .filter(|b| b.is_dir())
        .map(Path::to_path_buf);
    Ok(ModulePaths { bundle, binary: path.to_path_buf() })
}

/// `moduleinfo.json` (VST3 SDK 3.7.5+): class information without loading the binary.
pub fn read_moduleinfo(bundle: &Path) -> Option<(FactoryInfo, Vec<ClassInfo>)> {
    let p = [bundle.join("Contents").join("Resources").join("moduleinfo.json"), bundle.join("Contents").join("moduleinfo.json")]
        .into_iter()
        .find(|p| p.is_file())?;
    let text = std::fs::read_to_string(&p).ok()?;
    parse_moduleinfo(&text)
}

/// moduleinfo.json is JSON5-ish in the wild (trailing commas); this accepts those.
pub fn parse_moduleinfo(text: &str) -> Option<(FactoryInfo, Vec<ClassInfo>)> {
    let cleaned = strip_trailing_commas(text);
    let v: serde_json::Value = serde_json::from_str(&cleaned).ok()?;
    let f = &v["Factory Info"];
    let factory = FactoryInfo {
        vendor: f["Vendor"].as_str().unwrap_or_default().into(),
        url: f["URL"].as_str().unwrap_or_default().into(),
        email: f["E-Mail"].as_str().unwrap_or_default().into(),
        flags: 0,
    };
    let classes = v["Classes"]
        .as_array()?
        .iter()
        .map(|c| ClassInfo {
            cid: c["CID"].as_str().unwrap_or_default().to_ascii_uppercase(),
            name: c["Name"].as_str().unwrap_or_default().into(),
            category: c["Category"].as_str().unwrap_or_default().into(),
            sub_categories: c["Sub Categories"]
                .as_array()
                .map(|a| a.iter().filter_map(|s| s.as_str()).collect::<Vec<_>>().join("|"))
                .unwrap_or_default(),
            vendor: c["Vendor"].as_str().unwrap_or(&factory.vendor).into(),
            version: c["Version"].as_str().unwrap_or_default().into(),
            sdk_version: c["SDKVersion"].as_str().unwrap_or_default().into(),
            class_flags: c["Class Flags"].as_u64().unwrap_or(0) as u32,
            cardinality: c["Cardinality"].as_i64().unwrap_or(0x7FFF_FFFF) as i32,
        })
        .collect();
    Some((factory, classes))
}

fn strip_trailing_commas(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_str = false;
    let mut esc = false;
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if in_str {
            out.push(c);
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        if c == '"' {
            in_str = true;
        }
        if c == ',' {
            let next = chars[i + 1..].iter().find(|c| !c.is_whitespace());
            if matches!(next, Some('}') | Some(']')) {
                continue;
            }
        }
        out.push(c);
    }
    out
}

type GetFactory = unsafe extern "system" fn() -> *mut IPluginFactory;
type InitExit = unsafe extern "system" fn() -> bool;

/// A loaded VST3 binary. Dropping it releases the factory, calls `ExitDll` and unloads the library.
pub struct Module {
    pub paths: ModulePaths,
    factory: Option<ComPtr<IPluginFactory>>,
    exit: Option<InitExit>,
    lib: Option<libloading::Library>,
}

// SAFETY: the factory is a COM object; VST3 factories may be called from any thread, and the
// library handle is only used on drop.
unsafe impl Send for Module {}
unsafe impl Sync for Module {}

impl Module {
    /// Load the library, run its entry point and get the factory.
    pub fn load(path: &Path) -> Result<Module> {
        let paths = resolve(path)?;
        let lib = load_library(&paths.binary).with_context(|| format!("loading {}", paths.binary.display()))?;
        // SAFETY: symbol types match the VST3 module entry points (Windows: InitDll/ExitDll, GetPluginFactory).
        unsafe {
            let init: Option<InitExit> = lib.get::<InitExit>(b"InitDll\0").ok().map(|s| *s);
            let exit: Option<InitExit> = lib.get::<InitExit>(b"ExitDll\0").ok().map(|s| *s);
            if let Some(init) = init
                && !init()
            {
                bail!("{}: InitDll returned false", paths.binary.display());
            }
            let get: GetFactory = *lib
                .get::<GetFactory>(b"GetPluginFactory\0")
                .with_context(|| format!("{} exports no GetPluginFactory (not a VST3 plugin?)", paths.binary.display()))?;
            let raw = get();
            let factory = ComPtr::from_raw(raw).ok_or_else(|| anyhow!("{}: GetPluginFactory returned null", paths.binary.display()))?;
            Ok(Module { paths, factory: Some(factory), exit, lib: Some(lib) })
        }
    }

    pub fn factory(&self) -> &ComPtr<IPluginFactory> {
        self.factory.as_ref().expect("factory is present until drop")
    }

    pub fn factory_info(&self) -> FactoryInfo {
        // SAFETY: zeroed POD out-struct filled by the factory.
        unsafe {
            let mut fi: PFactoryInfo = std::mem::zeroed();
            if self.factory().getFactoryInfo(&mut fi) != kResultOk {
                return FactoryInfo::default();
            }
            FactoryInfo { vendor: str8(&fi.vendor), url: str8(&fi.url), email: str8(&fi.email), flags: fi.flags }
        }
    }

    /// Every class, with the richest info the factory offers (IPluginFactory3 > 2 > 1).
    pub fn classes(&self) -> Result<Vec<ClassInfo>> {
        let f = self.factory();
        let f2 = f.cast::<IPluginFactory2>();
        let f3 = f.cast::<IPluginFactory3>();
        let vendor = self.factory_info().vendor;
        // SAFETY: zeroed POD out-structs filled by the factory; indices are within countClasses.
        unsafe {
            let n = f.countClasses();
            let mut out = Vec::with_capacity(n.max(0) as usize);
            for i in 0..n {
                if let Some(f3) = &f3 {
                    let mut ci: PClassInfoW = std::mem::zeroed();
                    if f3.getClassInfoUnicode(i, &mut ci) == kResultOk {
                        out.push(ClassInfo {
                            cid: cid_hex(&ci.cid),
                            name: str16(&ci.name),
                            category: str8(&ci.category),
                            sub_categories: str8(&ci.subCategories),
                            vendor: Some(str16(&ci.vendor)).filter(|v| !v.is_empty()).unwrap_or_else(|| vendor.clone()),
                            version: str16(&ci.version),
                            sdk_version: str16(&ci.sdkVersion),
                            class_flags: ci.classFlags,
                            cardinality: ci.cardinality,
                        });
                        continue;
                    }
                }
                if let Some(f2) = &f2 {
                    let mut ci: PClassInfo2 = std::mem::zeroed();
                    if f2.getClassInfo2(i, &mut ci) == kResultOk {
                        out.push(ClassInfo {
                            cid: cid_hex(&ci.cid),
                            name: str8(&ci.name),
                            category: str8(&ci.category),
                            sub_categories: str8(&ci.subCategories),
                            vendor: Some(str8(&ci.vendor)).filter(|v| !v.is_empty()).unwrap_or_else(|| vendor.clone()),
                            version: str8(&ci.version),
                            sdk_version: str8(&ci.sdkVersion),
                            class_flags: ci.classFlags,
                            cardinality: ci.cardinality,
                        });
                        continue;
                    }
                }
                let mut ci: PClassInfo = std::mem::zeroed();
                check(f.getClassInfo(i, &mut ci), "IPluginFactory::getClassInfo")?;
                out.push(ClassInfo {
                    cid: cid_hex(&ci.cid),
                    name: str8(&ci.name),
                    category: str8(&ci.category),
                    vendor: vendor.clone(),
                    cardinality: ci.cardinality,
                    ..Default::default()
                });
            }
            Ok(out)
        }
    }

    /// Give the factory the host context (IPluginFactory3), if it wants one.
    ///
    /// # Safety
    /// `host` must be a live `FUnknown` that stays alive while the factory may use it.
    pub unsafe fn set_host_context(&self, host: *mut FUnknown) {
        if let Some(f3) = self.factory().cast::<IPluginFactory3>() {
            // SAFETY: per the caller.
            unsafe { f3.setHostContext(host) };
        }
    }

    /// Instantiate class `cid` and return it as interface `I`.
    pub fn create<I: Interface>(&self, cid: &TUID) -> Result<ComPtr<I>> {
        let mut obj: *mut std::ffi::c_void = std::ptr::null_mut();
        let iid: TUID = I::IID.map(|b| b as std::ffi::c_char);
        // SAFETY: cid/iid are 16-byte ids; obj receives an add-ref'd pointer on success.
        unsafe {
            let r = self.factory().createInstance(cid.as_ptr(), iid.as_ptr(), &mut obj);
            check(r, &format!("createInstance({})", cid_hex(cid)))?;
            ComPtr::from_raw(obj as *mut I).ok_or_else(|| anyhow!("createInstance({}) returned null", cid_hex(cid)))
        }
    }
}

impl Drop for Module {
    fn drop(&mut self) {
        self.factory = None;
        if let Some(exit) = self.exit {
            // SAFETY: ExitDll is the module's documented teardown entry point, called once after the factory is released.
            unsafe { exit() };
        }
        self.lib = None;
    }
}

#[cfg(windows)]
fn load_library(path: &Path) -> Result<libloading::Library> {
    use libloading::os::windows::{Library, LOAD_WITH_ALTERED_SEARCH_PATH};
    // altered search path: the plugin's own folder is searched for the DLLs it depends on
    // SAFETY: loading a plugin runs its initialisers; that is the point of hosting it (out of process).
    let lib = unsafe { Library::load_with_flags(path, LOAD_WITH_ALTERED_SEARCH_PATH) }?;
    Ok(lib.into())
}

#[cfg(not(windows))]
fn load_library(path: &Path) -> Result<libloading::Library> {
    // SAFETY: as above.
    Ok(unsafe { libloading::Library::new(path) }?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moduleinfo_with_trailing_commas() {
        let text = r#"{
            "Name": "Test",
            "Factory Info": { "Vendor": "ACME", "URL": "https://acme", "E-Mail": "a@b", "Flags": {"Unicode": true,}, },
            "Classes": [
                { "CID": "ABCDEF0123456789ABCDEF0123456789", "Category": "Audio Module Class", "Name": "Synth, \"Big\"",
                  "Sub Categories": ["Instrument", "Synth",], "Version": "1.2.3", "SDKVersion": "VST 3.7.9", "Cardinality": 2147483647, },
            ],
        }"#;
        let (f, c) = parse_moduleinfo(text).unwrap();
        assert_eq!(f.vendor, "ACME");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].name, "Synth, \"Big\"");
        assert_eq!(c[0].sub_categories, "Instrument|Synth");
        assert!(c[0].is_processor() && c[0].is_instrument());
        assert_eq!(c[0].vendor, "ACME");
    }

    #[test]
    fn resolve_rejects_missing() {
        assert!(resolve(Path::new("Z:/definitely/not/here.vst3")).is_err());
    }
}
