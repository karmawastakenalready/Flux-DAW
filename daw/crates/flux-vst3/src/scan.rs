//! Finding installed VST3 plugins and what they contain, without putting Flux at risk: an unknown
//! binary is only ever loaded in a `flux-plugin-host scan` process (or not at all, when its bundle
//! has a `moduleinfo.json`). Results are cached by the binary's size and modification time.

use crate::module::{self, ClassInfo, FactoryInfo};
use crate::plugin::PluginInfo;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const CACHE_VERSION: u32 = 1;
/// How deep to look inside plugin folders (vendors nest their plugins a level or two).
const MAX_DEPTH: usize = 6;

/// How a plugin's description was obtained.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Read from the bundle's `moduleinfo.json` (not loaded).
    Moduleinfo,
    /// Loaded in a scan process.
    Binary,
    /// Loaded in a scan process and every processor class instantiated.
    Deep,
    /// The scan failed (see `error`).
    Failed,
}

/// One processor class, instantiated during a deep scan.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Instance {
    pub cid: String,
    pub info: Option<PluginInfo>,
    #[serde(default)]
    pub has_editor: bool,
    #[serde(default)]
    pub programs: usize,
    #[serde(default)]
    pub error: Option<String>,
}

/// A VST3 module on disk and its classes.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ScannedPlugin {
    /// What to load: the bundle directory, or the file for single-file plugins.
    pub path: PathBuf,
    pub binary: PathBuf,
    pub size: u64,
    /// Modification time of the binary (ns since the Unix epoch).
    pub modified: u64,
    pub source: Source,
    #[serde(default)]
    pub factory: FactoryInfo,
    #[serde(default)]
    pub classes: Vec<ClassInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub instances: Vec<Instance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ScannedPlugin {
    /// Audio processor classes (what can be loaded as an instrument or effect).
    pub fn processors(&self) -> impl Iterator<Item = &ClassInfo> {
        self.classes.iter().filter(|c| c.is_processor())
    }

    /// The first processor's name, else the file name.
    pub fn name(&self) -> String {
        self.processors().next().map(|c| c.name.clone()).unwrap_or_else(|| file_stem(&self.path))
    }

    pub fn vendor(&self) -> &str {
        self.processors().next().map(|c| c.vendor.as_str()).filter(|v| !v.is_empty()).unwrap_or(&self.factory.vendor)
    }

    pub fn is_instrument(&self) -> bool {
        self.processors().any(ClassInfo::is_instrument)
    }

    pub fn ok(&self) -> bool {
        self.source != Source::Failed
    }
}

fn file_stem(p: &Path) -> String {
    p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// The system's standard VST3 folders that exist (Windows: `%COMMONPROGRAMFILES%\VST3` and
/// `%LOCALAPPDATA%\Programs\Common\VST3`).
pub fn standard_dirs() -> Vec<PathBuf> {
    let mut v = vec![];
    #[cfg(windows)]
    {
        if let Some(c) = std::env::var_os("COMMONPROGRAMFILES") {
            v.push(PathBuf::from(c).join("VST3"));
        }
        if let Some(l) = std::env::var_os("LOCALAPPDATA") {
            v.push(PathBuf::from(l).join("Programs").join("Common").join("VST3"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        v.push(PathBuf::from("/Library/Audio/Plug-Ins/VST3"));
        if let Some(h) = std::env::var_os("HOME") {
            v.push(PathBuf::from(h).join("Library/Audio/Plug-Ins/VST3"));
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(h) = std::env::var_os("HOME") {
            v.push(PathBuf::from(h).join(".vst3"));
        }
        v.push(PathBuf::from("/usr/lib/vst3"));
        v.push(PathBuf::from("/usr/local/lib/vst3"));
    }
    v.retain(|d| d.is_dir());
    v
}

fn is_vst3(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("vst3"))
}

/// Every `.vst3` bundle or file under `dirs` (a `.vst3` path given directly is included too).
/// Sorted, without duplicates.
pub fn find_bundles(dirs: &[PathBuf]) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if is_vst3(&p) {
                // a bundle is a leaf: don't look for plugins inside it
                if ft.is_dir() || ft.is_file() || ft.is_symlink() {
                    out.push(p);
                }
            } else if ft.is_dir() && depth < MAX_DEPTH {
                walk(&p, depth + 1, out);
            }
        }
    }
    let mut out = vec![];
    for d in dirs {
        if is_vst3(d) && d.exists() {
            out.push(d.clone());
        } else {
            walk(d, 0, &mut out);
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|p| seen.insert(std::fs::canonicalize(p).unwrap_or_else(|_| p.clone())));
    out.sort();
    out
}

/// Size and modification time of a plugin's binary (the cache key).
fn stamp(binary: &Path) -> Result<(u64, u64)> {
    let m = std::fs::metadata(binary).with_context(|| format!("reading {}", binary.display()))?;
    let t = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as u64);
    Ok((m.len(), t))
}

#[derive(Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    plugins: Vec<ScannedPlugin>,
}

/// Scans plugins (in parallel, out of process) and keeps a cache.
#[derive(Clone, Debug)]
pub struct Scanner {
    /// `flux-plugin-host` (None = [`crate::client::find_host_exe`]).
    pub exe: Option<PathBuf>,
    /// Arguments before the mode (see [`crate::client::HostOptions::args`]).
    pub args: Vec<String>,
    /// Per plugin. Some plugins check licenses or unpack resources when first loaded.
    pub timeout: Duration,
    /// Scan processes at once.
    pub jobs: usize,
    /// JSON cache file (None = no cache).
    pub cache: Option<PathBuf>,
    /// Instantiate every processor class (slower; verifies the plugin really works and reports
    /// buses, parameter counts and editors).
    pub deep: bool,
    /// Ignore cached results.
    pub rescan: bool,
}

impl Default for Scanner {
    fn default() -> Self {
        Scanner { exe: None, args: vec![], timeout: Duration::from_secs(60), jobs: 4, cache: None, deep: false, rescan: false }
    }
}

impl Scanner {
    fn load_cache(&self) -> HashMap<PathBuf, ScannedPlugin> {
        let Some(path) = &self.cache else { return HashMap::new() };
        let Ok(text) = std::fs::read_to_string(path) else { return HashMap::new() };
        match serde_json::from_str::<CacheFile>(&text) {
            Ok(c) if c.version == CACHE_VERSION => c.plugins.into_iter().map(|p| (p.path.clone(), p)).collect(),
            _ => HashMap::new(),
        }
    }

    fn save_cache(&self, all: &HashMap<PathBuf, ScannedPlugin>) -> Result<()> {
        let Some(path) = &self.cache else { return Ok(()) };
        let mut plugins: Vec<ScannedPlugin> = all.values().cloned().collect();
        plugins.sort_by(|a, b| a.path.cmp(&b.path));
        let text = serde_json::to_string_pretty(&CacheFile { version: CACHE_VERSION, plugins })?;
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Describe each plugin, from the cache when its binary is unchanged.
    pub fn scan(&self, plugins: &[PathBuf]) -> Vec<ScannedPlugin> {
        let mut cache = if self.rescan { HashMap::new() } else { self.load_cache() };
        let mut out: Vec<Option<ScannedPlugin>> = vec![None; plugins.len()];
        let mut todo = vec![];
        for (i, p) in plugins.iter().enumerate() {
            let hit = cache.get(p).filter(|c| {
                stamp(&c.binary).is_ok_and(|(size, modified)| size == c.size && modified == c.modified)
                    && (!self.deep || c.source == Source::Deep)
            });
            match hit {
                Some(c) => out[i] = Some(c.clone()),
                None => todo.push(i),
            }
        }
        let next = AtomicUsize::new(0);
        let results: Vec<(usize, ScannedPlugin)> = std::thread::scope(|s| {
            let workers: Vec<_> = (0..self.jobs.clamp(1, 16).min(todo.len().max(1)))
                .map(|_| {
                    s.spawn(|| {
                        let mut mine = vec![];
                        loop {
                            let k = next.fetch_add(1, Ordering::Relaxed);
                            let Some(&i) = todo.get(k) else { break };
                            mine.push((i, self.scan_one(&plugins[i])));
                        }
                        mine
                    })
                })
                .collect();
            workers.into_iter().flat_map(|w| w.join().unwrap_or_default()).collect()
        });
        for (i, r) in results {
            cache.insert(r.path.clone(), r.clone());
            out[i] = Some(r);
        }
        // forget plugins that are gone
        cache.retain(|p, _| p.exists());
        if let Err(e) = self.save_cache(&cache) {
            eprintln!("[flux-vst3] warning: saving the plugin cache: {e:#}");
        }
        out.into_iter().flatten().collect()
    }

    /// Describe one plugin (no cache).
    pub fn scan_one(&self, path: &Path) -> ScannedPlugin {
        let failed = |binary: PathBuf, size, modified, e: anyhow::Error| ScannedPlugin {
            path: path.to_path_buf(),
            binary,
            size,
            modified,
            source: Source::Failed,
            factory: FactoryInfo::default(),
            classes: vec![],
            instances: vec![],
            error: Some(format!("{e:#}")),
        };
        let paths = match module::resolve(path) {
            Ok(p) => p,
            Err(e) => return failed(path.to_path_buf(), 0, 0, e),
        };
        let (size, modified) = match stamp(&paths.binary) {
            Ok(s) => s,
            Err(e) => return failed(paths.binary, 0, 0, e),
        };
        if !self.deep
            && let Some((factory, classes)) = paths.bundle.as_deref().and_then(module::read_moduleinfo)
            && classes.iter().any(ClassInfo::is_processor)
        {
            return ScannedPlugin { path: path.to_path_buf(), binary: paths.binary, size, modified, source: Source::Moduleinfo, factory, classes, instances: vec![], error: None };
        }
        match self.run_scan_process(path) {
            Ok(v) => match parse_scan(&v, self.deep) {
                Ok((factory, classes, instances)) => ScannedPlugin {
                    path: path.to_path_buf(),
                    binary: paths.binary,
                    size,
                    modified,
                    source: if self.deep { Source::Deep } else { Source::Binary },
                    factory,
                    classes,
                    instances,
                    error: None,
                },
                Err(e) => failed(paths.binary, size, modified, e),
            },
            Err(e) => failed(paths.binary, size, modified, e),
        }
    }

    #[cfg(windows)]
    fn run_scan_process(&self, path: &Path) -> Result<Value> {
        use std::io::Read;
        use std::os::windows::io::AsRawHandle;
        use std::os::windows::process::CommandExt;
        let exe = match &self.exe {
            Some(e) => e.clone(),
            None => crate::client::find_host_exe()?,
        };
        let mut cmd = std::process::Command::new(&exe);
        cmd.args(&self.args).arg("scan").arg(path);
        if self.deep {
            cmd.arg("--deep");
        }
        let mut child = cmd
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .spawn()
            .with_context(|| format!("starting {}", exe.display()))?;
        if let Some(job) = crate::sys::job() {
            let _ = job.assign(child.as_raw_handle() as _);
        }
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::ipc::read_frame(&mut stdout));
        });
        let log = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = stderr.read_to_string(&mut s);
            s
        });
        let frame = rx.recv_timeout(self.timeout);
        let timed_out = frame.is_err();
        if timed_out {
            let _ = child.kill();
        }
        let status = child.wait().ok();
        let log = log.join().unwrap_or_default();
        let tail = log.lines().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
        if timed_out {
            bail!("scanning took longer than {:.0} s{}", self.timeout.as_secs_f64(), if tail.is_empty() { String::new() } else { format!("; output:\n{tail}") });
        }
        match frame.expect("checked") {
            Ok(Some((v, _))) => {
                if v["ok"].as_bool() == Some(true) {
                    Ok(v["result"].clone())
                } else {
                    bail!("{}", v["error"].as_str().unwrap_or("the scan failed"))
                }
            }
            Ok(None) | Err(_) => {
                let how = status.and_then(|s| s.code()).map(|c| crate::client::describe_exit(c as u32)).unwrap_or_else(|| "exited".into());
                bail!("the plugin {how} while being scanned{}", if tail.is_empty() { String::new() } else { format!("; output:\n{tail}") })
            }
        }
    }

    #[cfg(not(windows))]
    fn run_scan_process(&self, _path: &Path) -> Result<Value> {
        bail!("scanning plugin binaries is only supported on Windows so far")
    }
}

fn parse_scan(v: &Value, deep: bool) -> Result<(FactoryInfo, Vec<ClassInfo>, Vec<Instance>)> {
    let factory: FactoryInfo = serde_json::from_value(v["factory"].clone()).unwrap_or_default();
    let list = v["classes"].as_array().ok_or_else(|| anyhow!("the scan returned no class list"))?;
    let mut classes = vec![];
    let mut instances = vec![];
    for c in list {
        let class: ClassInfo = serde_json::from_value(c.clone()).context("reading a class from the scan")?;
        if deep && class.is_processor() {
            let inst = &c["instance"];
            instances.push(Instance {
                cid: class.cid.clone(),
                info: serde_json::from_value(inst["info"].clone()).ok(),
                has_editor: inst["has_editor"].as_bool().unwrap_or(false),
                programs: inst["program_lists"].as_array().map_or(0, |l| l.iter().map(|x| x["programs"].as_u64().unwrap_or(0) as usize).sum()),
                error: c["error"].as_str().map(String::from),
            });
        }
        classes.push(class);
    }
    Ok((factory, classes, instances))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_bundles_without_descending_into_them() {
        let dir = std::env::temp_dir().join(format!("flux-scan-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let bundle = dir.join("Vendor").join("Synth.vst3");
        std::fs::create_dir_all(bundle.join("Contents").join("x86_64-win")).unwrap();
        std::fs::write(bundle.join("Contents").join("x86_64-win").join("Synth.vst3"), b"x").unwrap();
        std::fs::write(dir.join("Single.VST3"), b"x").unwrap();
        std::fs::write(dir.join("readme.txt"), b"x").unwrap();
        let found = find_bundles(std::slice::from_ref(&dir));
        assert_eq!(found, vec![dir.join("Single.VST3"), bundle.clone()]);
        // given directly
        assert_eq!(find_bundles(std::slice::from_ref(&bundle)), vec![bundle]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_scan_results() {
        let v = serde_json::json!({
            "factory": {"vendor": "V", "url": "", "email": "", "flags": 0},
            "classes": [
                {"cid": "ABCDEF019182FAEB4447534244657864", "name": "Synth", "category": "Audio Module Class", "sub_categories": "Instrument|Synth"},
                {"cid": "ABCDEF019182FAEB4447534244657865", "name": "Synth Controller", "category": "Component Controller Class"},
            ]
        });
        let (f, c, i) = parse_scan(&v, false).unwrap();
        assert_eq!(f.vendor, "V");
        assert_eq!(c.len(), 2);
        assert!(i.is_empty());
        let p = ScannedPlugin {
            path: "Synth.vst3".into(),
            binary: "x".into(),
            size: 1,
            modified: 1,
            source: Source::Binary,
            factory: f,
            classes: c,
            instances: vec![],
            error: None,
        };
        assert_eq!(p.name(), "Synth");
        assert!(p.is_instrument());
        assert_eq!(p.vendor(), "V");
    }
}
