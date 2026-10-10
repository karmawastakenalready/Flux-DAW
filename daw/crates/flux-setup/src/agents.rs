//! Connecting agents (MCP clients) to Flux. Every client keeps its MCP servers in its own config file; [`clients`]
//! finds the ones on this computer, [`connect`] adds a `flux` server that starts this Flux and [`disconnect`] takes
//! it out again. Edits keep the file's comments, formatting and every other setting, are written atomically, and
//! the file as it was is kept beside it as `<name>.flux-backup`. Claude Code is connected through its own `claude`
//! command when it is installed (it rewrites its config file while it runs); clients with YAML configs get a
//! snippet to paste ([`snippet`]).

use anyhow::{Context, Result, anyhow, bail};
use flux_assets::fsutil;
use jsonc_parser::ParseOptions;
use jsonc_parser::cst::{CstInputValue, CstRootNode};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// The name Flux's server goes by in every client's config.
pub const SERVER: &str = "flux";

/// How an agent starts Flux: this Flux's executable as an MCP server on stdio, with its window open (`--gui`) so
/// the user can watch and play along, unless they want it headless.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Launch {
    pub command: String,
    pub args: Vec<String>,
}

impl Launch {
    pub fn new(exe: &Path, gui: bool) -> Launch {
        let exe = std::path::absolute(exe).unwrap_or_else(|_| exe.to_path_buf());
        let mut args = vec!["mcp".to_string()];
        if gui {
            args.push("--gui".into());
        }
        Launch { command: exe.to_string_lossy().into_owned(), args }
    }

    fn gui(&self) -> bool {
        self.args.iter().any(|a| a == "--gui")
    }
}

/// Where clients keep their files: the user's home and settings folders and the programs on PATH (tests point it
/// at a temporary folder).
#[derive(Clone, Debug)]
pub struct Env {
    pub home: PathBuf,
    /// `%APPDATA%`, `~/Library/Application Support` or `~/.config`.
    pub config: PathBuf,
    /// `%LOCALAPPDATA%` (elsewhere the same as `config`).
    pub local: PathBuf,
    pub path: Vec<PathBuf>,
    /// `CODEX_HOME`, when set.
    pub codex_home: Option<PathBuf>,
}

impl Env {
    /// This user's folders.
    pub fn current() -> Result<Env> {
        let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        let home = var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).context("this user has no home folder")?;
        let config = if cfg!(windows) {
            var("APPDATA").unwrap_or_else(|| home.join("AppData").join("Roaming"))
        } else if cfg!(target_os = "macos") {
            home.join("Library").join("Application Support")
        } else {
            var("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"))
        };
        let local = if cfg!(windows) { var("LOCALAPPDATA").unwrap_or_else(|| home.join("AppData").join("Local")) } else { config.clone() };
        let path = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
        Ok(Env { home, config, local, path, codex_home: var("CODEX_HOME") })
    }

    /// A program on PATH, found the way a shell finds it (on Windows `.exe`, then `.cmd` and `.bat`, folder by folder).
    pub fn which(&self, name: &str) -> Option<PathBuf> {
        let exts: &[&str] = if cfg!(windows) { &[".exe", ".cmd", ".bat"] } else { &[""] };
        self.path.iter().flat_map(|d| exts.iter().map(move |e| d.join(format!("{name}{e}")))).find(|p| p.is_file())
    }
}

/// What a client's server entry looks like.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Shape {
    /// `{"command", "args"}`
    Plain,
    /// `{"type": "stdio", "command", "args"}`
    Stdio,
    /// `{"type": "local", "command", "args", "tools": ["*"]}` (GitHub Copilot CLI)
    Copilot,
    /// `{"type": "local", "command": [program, args...], "enabled": true}` (OpenCode)
    OpenCode,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Store {
    /// A JSON or JSONC file with the servers in an object under `key`.
    Json { key: &'static str, shape: Shape },
    /// Codex's `config.toml`, `[mcp_servers.flux]`.
    Toml,
    /// A YAML file Flux leaves alone: the user pastes [`snippet`] into it.
    Continue,
    Goose,
}

/// An agent Flux knows how to connect.
#[derive(Clone, Debug, Serialize)]
pub struct Client {
    pub id: &'static str,
    pub name: &'static str,
    /// It seems to be installed: its settings folder or its program was found.
    pub found: bool,
    /// The config file the `flux` server goes in.
    pub file: PathBuf,
    /// Claude Code's `claude` command, used to add and remove the server when it is installed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cli: Option<PathBuf>,
    /// Flux can't edit the file; connecting means pasting [`snippet`] into it.
    pub manual: bool,
    /// What to do after connecting for the agent to see Flux.
    pub then: &'static str,
    #[serde(skip)]
    store: Store,
}

struct Spec {
    id: &'static str,
    name: &'static str,
    store: Store,
    then: &'static str,
    /// Config files to use, each with the folder whose presence means the client uses it: the first existing file
    /// wins, else the first file whose folder exists, else the first file.
    places: Vec<(PathBuf, PathBuf)>,
    /// Programs that also mean the client is installed (on PATH, or at these paths).
    programs: Vec<&'static str>,
    installs: Vec<PathBuf>,
}

/// Every agent Flux knows, with what was found of it on this computer.
pub fn clients(env: &Env) -> Vec<Client> {
    let h = &env.home;
    let json = |key, shape| Store::Json { key, shape };
    let both = |file: PathBuf| {
        let dir = fsutil::parent_dir(&file).to_path_buf();
        (file, dir)
    };
    let mut desktop: Vec<(PathBuf, PathBuf)> = msix_folders(&env.local, "Claude_")
        .into_iter()
        .map(|p| both(p.join("LocalCache").join("Roaming").join("Claude").join("claude_desktop_config.json")))
        .collect();
    desktop.push(both(env.config.join("Claude").join("claude_desktop_config.json")));
    let codex = env.codex_home.clone().unwrap_or_else(|| h.join(".codex"));
    let vscode_ext = env.config.join("Code").join("User").join("globalStorage").join("saoudrizwan.claude-dev");
    let specs = vec![
        Spec {
            id: "claude-code",
            name: "Claude Code",
            store: json("mcpServers", Shape::Stdio),
            then: "Start a new Claude Code session (/mcp lists flux).",
            places: vec![(h.join(".claude.json"), h.join(".claude"))],
            programs: vec!["claude"],
            installs: vec![h.join(".local").join("bin").join(exe("claude"))],
        },
        Spec {
            id: "claude-desktop",
            name: "Claude Desktop",
            store: json("mcpServers", Shape::Plain),
            then: "Quit Claude Desktop (from its tray icon too) and open it again.",
            places: desktop,
            programs: vec![],
            installs: vec![],
        },
        Spec {
            id: "codex",
            name: "Codex",
            store: Store::Toml,
            then: "Start a new Codex session, or restart the Codex app or editor extension.",
            places: vec![(codex.join("config.toml"), codex)],
            programs: vec!["codex"],
            installs: vec![env.local.join("Programs").join("OpenAI").join("Codex").join("bin").join(exe("codex"))],
        },
        Spec {
            id: "cursor",
            name: "Cursor",
            store: json("mcpServers", Shape::Plain),
            then: "Restart Cursor; flux is listed in its MCP settings.",
            places: vec![both(h.join(".cursor").join("mcp.json"))],
            programs: vec![],
            installs: vec![],
        },
        Spec {
            id: "vscode",
            name: "VS Code (GitHub Copilot)",
            store: json("servers", Shape::Stdio),
            then: "VS Code picks it up; allow flux when it asks whether to trust it.",
            places: vec![both(env.config.join("Code").join("User").join("mcp.json"))],
            programs: vec![],
            installs: vec![],
        },
        Spec {
            id: "vscode-insiders",
            name: "VS Code Insiders",
            store: json("servers", Shape::Stdio),
            then: "VS Code picks it up; allow flux when it asks whether to trust it.",
            places: vec![both(env.config.join("Code - Insiders").join("User").join("mcp.json"))],
            programs: vec![],
            installs: vec![],
        },
        Spec {
            id: "visual-studio",
            name: "Visual Studio",
            store: json("servers", Shape::Stdio),
            then: "Restart Visual Studio (17.14 or later).",
            places: vec![(h.join(".mcp.json"), env.local.join("Microsoft").join("VisualStudio"))],
            programs: vec![],
            installs: vec![],
        },
        Spec {
            id: "gemini",
            name: "Gemini CLI",
            store: json("mcpServers", Shape::Plain),
            then: "Start a new Gemini CLI session (/mcp lists flux).",
            places: vec![(h.join(".gemini").join("settings.json"), h.join(".gemini").join("settings.json"))],
            programs: vec!["gemini"],
            installs: vec![],
        },
        Spec {
            id: "antigravity",
            name: "Antigravity",
            store: json("mcpServers", Shape::Plain),
            then: "Refresh Antigravity's MCP servers (Manage MCP Servers).",
            places: vec![
                both(h.join(".gemini").join("config").join("mcp_config.json")),
                both(h.join(".gemini").join("antigravity").join("mcp_config.json")),
            ],
            programs: vec!["agy"],
            installs: vec![],
        },
        Spec {
            id: "devin",
            name: "Devin Desktop",
            store: json("mcpServers", Shape::Plain),
            then: "Refresh the MCP servers in Devin Desktop, or restart it.",
            places: vec![both(env.config.join("devin").join("mcp_config.json"))],
            programs: vec!["devin"],
            installs: vec![],
        },
        Spec {
            id: "windsurf",
            name: "Windsurf",
            store: json("mcpServers", Shape::Plain),
            then: "Refresh the MCP servers in Windsurf, or restart it.",
            places: vec![(h.join(".codeium").join("windsurf").join("mcp_config.json"), h.join(".codeium").join("windsurf"))],
            programs: vec![],
            installs: vec![],
        },
        Spec {
            id: "cline",
            name: "Cline",
            store: json("mcpServers", Shape::Plain),
            then: "Cline picks it up; restart VS Code if it doesn't.",
            places: vec![
                (h.join(".cline").join("data").join("settings").join("cline_mcp_settings.json"), h.join(".cline")),
                (vscode_ext.join("settings").join("cline_mcp_settings.json"), vscode_ext),
            ],
            programs: vec!["cline"],
            installs: vec![],
        },
        Spec {
            id: "zed",
            name: "Zed",
            store: json("context_servers", Shape::Plain),
            then: "Zed picks it up; the Agent panel's settings list flux.",
            places: vec![if cfg!(windows) { both(env.config.join("Zed").join("settings.json")) } else { both(h.join(".config").join("zed").join("settings.json")) }],
            programs: vec![],
            installs: vec![],
        },
        Spec {
            id: "copilot-cli",
            name: "GitHub Copilot CLI",
            store: json("mcpServers", Shape::Copilot),
            then: "Start a new Copilot CLI session.",
            places: vec![both(h.join(".copilot").join("mcp-config.json"))],
            programs: vec!["copilot"],
            installs: vec![],
        },
        Spec {
            id: "lm-studio",
            name: "LM Studio",
            store: json("mcpServers", Shape::Plain),
            then: "LM Studio picks it up.",
            places: vec![both(h.join(".lmstudio").join("mcp.json"))],
            programs: vec![],
            installs: vec![],
        },
        Spec {
            id: "kiro",
            name: "Kiro",
            store: json("mcpServers", Shape::Plain),
            then: "Kiro picks it up.",
            places: vec![(h.join(".kiro").join("settings").join("mcp.json"), h.join(".kiro"))],
            programs: vec![],
            installs: vec![],
        },
        Spec {
            id: "amp",
            name: "Amp",
            store: json("amp.mcpServers", Shape::Plain),
            then: "Start a new Amp session.",
            places: vec![both(h.join(".config").join("amp").join("settings.json"))],
            programs: vec!["amp"],
            installs: vec![],
        },
        Spec {
            id: "opencode",
            name: "OpenCode",
            store: json("mcp", Shape::OpenCode),
            then: "Start a new OpenCode session.",
            places: vec![
                (h.join(".config").join("opencode").join("opencode.jsonc"), h.join(".config").join("opencode").join("opencode.jsonc")),
                both(h.join(".config").join("opencode").join("opencode.json")),
            ],
            programs: vec!["opencode"],
            installs: vec![],
        },
        Spec {
            id: "continue",
            name: "Continue",
            store: Store::Continue,
            then: "Paste this under mcpServers in Continue's config.yaml; Continue reloads it.",
            places: vec![both(h.join(".continue").join("config.yaml"))],
            programs: vec![],
            installs: vec![],
        },
        Spec {
            id: "goose",
            name: "Goose",
            store: Store::Goose,
            then: "Paste this under extensions in Goose's config.yaml, then restart Goose.",
            places: vec![both(env.config.join("Block").join("goose").join("config").join("config.yaml"))],
            programs: vec!["goose"],
            installs: vec![],
        },
    ];
    specs.into_iter().map(|s| client(env, s)).collect()
}

fn client(env: &Env, s: Spec) -> Client {
    let file = s
        .places
        .iter()
        .find(|(f, _)| f.is_file())
        .or_else(|| s.places.iter().find(|(_, d)| d.exists()))
        .unwrap_or(&s.places[0])
        .0
        .clone();
    let program = s.programs.iter().find_map(|p| env.which(p)).or_else(|| s.installs.iter().find(|p| p.is_file()).cloned());
    let found = program.is_some() || s.places.iter().any(|(_, d)| d.exists());
    let cli = if s.id == "claude-code" { program } else { None };
    let manual = matches!(s.store, Store::Continue | Store::Goose);
    Client { id: s.id, name: s.name, found, file, cli, manual, then: s.then, store: s.store }
}

/// Folders of installed Microsoft Store (MSIX) apps whose package name starts with `prefix`: such apps see
/// `%APPDATA%` redirected into their package folder.
fn msix_folders(local: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(local.join("Packages"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(prefix))
        .map(|e| e.path())
        .collect();
    v.sort();
    v
}

fn exe(name: &str) -> String {
    if cfg!(windows) { format!("{name}.exe") } else { name.to_string() }
}

/// Whether a client has Flux.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum State {
    /// Its `flux` server starts this Flux (`gui`: with its window).
    Connected { gui: bool },
    /// Its `flux` server starts another program (another Flux install or checkout).
    Other { command: String },
    NotConnected,
    /// Its config file can't be read; Flux leaves it alone.
    Unreadable { error: String },
    /// Flux doesn't edit this client's config ([`Client::manual`]).
    Manual,
}

/// Whether `c` has Flux, and which.
pub fn state(c: &Client, launch: &Launch) -> State {
    match read_entry(c) {
        Ok(None) if c.manual => State::Manual,
        Ok(None) => State::NotConnected,
        Ok(Some((command, args))) if same_program(&command, &launch.command) => State::Connected { gui: args.iter().any(|a| a == "--gui") },
        Ok(Some((command, _))) => State::Other { command },
        Err(e) => State::Unreadable { error: format!("{e:#}") },
    }
}

/// The program and arguments of `c`'s `flux` server, if it has one.
fn read_entry(c: &Client) -> Result<Option<(String, Vec<String>)>> {
    let Some(text) = read_text(&c.file)? else { return Ok(None) };
    let strings = |v: Option<&Value>| -> Vec<String> { v.and_then(Value::as_array).into_iter().flatten().filter_map(|a| a.as_str().map(String::from)).collect() };
    match c.store {
        Store::Json { key, shape } => {
            let doc = parse_json(&text)?;
            let Some(e) = doc.get(key).and_then(|s| s.get(SERVER)) else { return Ok(None) };
            if shape == Shape::OpenCode {
                let mut cmd = strings(e.get("command"));
                if cmd.is_empty() {
                    return Ok(Some((String::new(), vec![])));
                }
                let program = cmd.remove(0);
                return Ok(Some((program, cmd)));
            }
            Ok(Some((e.get("command").and_then(Value::as_str).unwrap_or_default().to_string(), strings(e.get("args")))))
        }
        Store::Toml => {
            let doc: toml_edit::DocumentMut = text.parse().context("it is not valid TOML")?;
            let Some(e) = doc.get("mcp_servers").and_then(|s| s.get(SERVER)) else { return Ok(None) };
            let command = e.get("command").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let args = e.get("args").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
            Ok(Some((command, args)))
        }
        Store::Continue | Store::Goose => Ok(None),
    }
}

/// What [`connect`] or [`disconnect`] did for one client.
#[derive(Clone, Debug, Serialize)]
pub struct Outcome {
    pub client: &'static str,
    /// `connected`, `unchanged`, `disconnected`, `not-connected`, `manual` (paste the snippet) or `failed`.
    pub result: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The config file as it was before Flux changed it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup: Option<PathBuf>,
}

impl Outcome {
    fn new(c: &Client, result: &'static str) -> Outcome {
        Outcome { client: c.id, result, message: None, backup: None }
    }

    fn failed(c: &Client, e: anyhow::Error) -> Outcome {
        Outcome { message: Some(format!("{e:#}")), ..Outcome::new(c, "failed") }
    }

    pub fn ok(&self) -> bool {
        self.result != "failed"
    }
}

/// Give `c` a `flux` server that starts `launch` (replacing a `flux` server that starts something else, and
/// keeping any settings the user added to it).
pub fn connect(c: &Client, launch: &Launch) -> Outcome {
    if c.manual {
        return Outcome { message: Some(c.then.into()), ..Outcome::new(c, "manual") };
    }
    match state(c, launch) {
        State::Connected { gui } if gui == launch.gui() => return Outcome::new(c, "unchanged"),
        State::Unreadable { error } => {
            return Outcome::failed(c, anyhow!("{} can't be read ({error}); Flux leaves it alone: add flux to it by hand", c.file.display()));
        }
        _ => {}
    }
    if let Some(cli) = &c.cli {
        let run = || -> Result<()> {
            if read_entry(c)?.is_some() {
                run_cli(cli, &["mcp", "remove", "--scope", "user", SERVER])?;
            }
            let mut args = vec!["mcp", "add", "--scope", "user", "--transport", "stdio", SERVER, "--", &launch.command];
            args.extend(launch.args.iter().map(String::as_str));
            run_cli(cli, &args)?;
            Ok(())
        };
        return match run() {
            Ok(()) => Outcome::new(c, "connected"),
            Err(e) => Outcome::failed(c, e),
        };
    }
    match edit(c, Some(launch)) {
        Ok(Edit::Unchanged) => Outcome::new(c, "unchanged"),
        Ok(Edit::Written { backup }) => Outcome { backup, ..Outcome::new(c, "connected") },
        Err(e) => Outcome::failed(c, e),
    }
}

/// Take `c`'s `flux` server out (whichever Flux it starts).
pub fn disconnect(c: &Client) -> Outcome {
    if c.manual {
        return Outcome { message: Some(format!("remove flux from {} by hand", c.file.display())), ..Outcome::new(c, "manual") };
    }
    match read_entry(c) {
        Ok(None) => return Outcome::new(c, "not-connected"),
        Ok(Some(_)) => {}
        Err(e) => return Outcome::failed(c, e.context(format!("{} can't be read; Flux leaves it alone", c.file.display()))),
    }
    if let Some(cli) = &c.cli {
        return match run_cli(cli, &["mcp", "remove", "--scope", "user", SERVER]) {
            Ok(_) => Outcome::new(c, "disconnected"),
            Err(e) => Outcome::failed(c, e),
        };
    }
    match edit(c, None) {
        Ok(Edit::Unchanged) => Outcome::new(c, "not-connected"),
        Ok(Edit::Written { backup }) => Outcome { backup, ..Outcome::new(c, "disconnected") },
        Err(e) => Outcome::failed(c, e),
    }
}

fn run_cli(cli: &Path, args: &[&str]) -> Result<String> {
    let out = fsutil::quiet_command(cli)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {}", cli.display()))?;
    let text = |b: &[u8]| String::from_utf8_lossy(b).trim().to_string();
    if !out.status.success() {
        let why = [text(&out.stderr), text(&out.stdout)].into_iter().find(|t| !t.is_empty()).unwrap_or_else(|| out.status.to_string());
        bail!("`claude {}` failed: {why}", args.join(" "));
    }
    Ok(text(&out.stdout))
}

/// The fields of the server entry a client gets, in the order they are written.
fn entry(shape: Shape, launch: &Launch) -> Vec<(&'static str, Value)> {
    let (command, args) = (json!(launch.command), json!(launch.args));
    match shape {
        Shape::Plain => vec![("command", command), ("args", args)],
        Shape::Stdio => vec![("type", json!("stdio")), ("command", command), ("args", args)],
        Shape::Copilot => vec![("type", json!("local")), ("command", command), ("args", args), ("tools", json!(["*"]))],
        Shape::OpenCode => {
            let mut cmd = vec![launch.command.clone()];
            cmd.extend(launch.args.iter().cloned());
            vec![("type", json!("local")), ("command", json!(cmd)), ("enabled", json!(true))]
        }
    }
}

fn object(fields: &[(&str, Value)]) -> CstInputValue {
    CstInputValue::Object(fields.iter().map(|(k, v)| (k.to_string(), input(v))).collect())
}

enum Edit {
    Unchanged,
    Written { backup: Option<PathBuf> },
}

/// Put `launch` in `c`'s config file (or, with `None`, take the `flux` server out).
fn edit(c: &Client, launch: Option<&Launch>) -> Result<Edit> {
    let original = read_text(&c.file)?;
    if original.is_none() && launch.is_none() {
        return Ok(Edit::Unchanged);
    }
    let text = original.clone().unwrap_or_default();
    let (bom, body) = match text.strip_prefix('\u{feff}') {
        Some(b) => ("\u{feff}", b),
        None => ("", text.as_str()),
    };
    let mut out = match c.store {
        Store::Json { key, shape } => edit_json(body, key, launch.map(|l| entry(shape, l)))?,
        Store::Toml => edit_toml(body, launch)?,
        Store::Continue | Store::Goose => bail!("Flux doesn't edit {}", c.file.display()),
    };
    if original.is_none() && !out.ends_with('\n') {
        out.push('\n');
    }
    let out = format!("{bom}{out}");
    if original.as_deref() == Some(out.as_str()) {
        return Ok(Edit::Unchanged);
    }
    // the file may be a link into a dotfiles folder: write where it points
    let target = match fs::symlink_metadata(&c.file) {
        Ok(m) if m.file_type().is_symlink() => fs::canonicalize(&c.file).with_context(|| format!("following {}", c.file.display()))?,
        _ => c.file.clone(),
    };
    let backup = match &original {
        Some(o) => {
            let b = target.with_file_name(format!("{}.flux-backup", target.file_name().unwrap_or_default().to_string_lossy()));
            fs::write(&b, o).with_context(|| format!("backing up {}", target.display()))?;
            Some(b)
        }
        None => None,
    };
    fsutil::atomic_write(&target, out.as_bytes())?;
    Ok(Edit::Written { backup })
}

fn edit_json(body: &str, key: &str, value: Option<Vec<(&'static str, Value)>>) -> Result<String> {
    let body = if body.trim().is_empty() { "{}" } else { body };
    parse_json(body)?;
    let root = CstRootNode::parse(body, &ParseOptions::default()).map_err(|e| anyhow!("it is not valid JSON: {e}"))?;
    let obj = root.object_value().context("its top level is not an object")?;
    match value {
        Some(fields) => {
            let servers = obj.object_value_or_create(key).with_context(|| format!("its \"{key}\" is not an object"))?;
            match servers.get(SERVER) {
                // keep what the user added to the entry (environment, auto-approved tools...)
                Some(p) if p.object_value().is_some() => {
                    let e = p.object_value().expect("checked");
                    for (k, v) in &fields {
                        match e.get(k) {
                            Some(f) => f.set_value(input(v)),
                            None => {
                                e.append(k, input(v));
                            }
                        }
                    }
                }
                Some(p) => p.set_value(object(&fields)),
                None => {
                    servers.append(SERVER, object(&fields));
                }
            }
        }
        None => {
            if let Some(p) = obj.object_value(key).and_then(|s| s.get(SERVER)) {
                p.remove();
            }
        }
    }
    let out = root.to_string();
    parse_json(&out).context("Flux's edit didn't come out as valid JSON (nothing was written)")?;
    Ok(out)
}

fn edit_toml(body: &str, launch: Option<&Launch>) -> Result<String> {
    use toml_edit::{Array, DocumentMut, Item, Table, value};
    let mut doc: DocumentMut = body.parse().context("it is not valid TOML")?;
    match launch {
        Some(l) => {
            let args = Array::from_iter(l.args.iter().map(String::as_str));
            let servers = doc.entry("mcp_servers").or_insert_with(|| {
                let mut t = Table::new();
                t.set_implicit(true);
                Item::Table(t)
            });
            if let Some(t) = servers.as_table_mut() {
                let e = t.entry(SERVER).or_insert_with(|| Item::Table(Table::new()));
                let e = e.as_table_like_mut().context("its mcp_servers.flux is not a table")?;
                e.insert("command", value(l.command.as_str()));
                e.insert("args", value(args));
            } else if let Some(t) = servers.as_inline_table_mut() {
                let mut e = t.get(SERVER).and_then(|v| v.as_inline_table()).cloned().unwrap_or_default();
                e.insert("command", l.command.as_str().into());
                e.insert("args", args.into());
                t.insert(SERVER, e.into());
            } else {
                bail!("its mcp_servers is not a table");
            }
        }
        None => {
            if let Some(s) = doc.get_mut("mcp_servers").and_then(Item::as_table_like_mut) {
                s.remove(SERVER);
            }
        }
    }
    Ok(doc.to_string())
}

fn input(v: &Value) -> CstInputValue {
    match v {
        Value::Null => CstInputValue::Null,
        Value::Bool(b) => CstInputValue::Bool(*b),
        Value::Number(n) => CstInputValue::Number(n.to_string()),
        Value::String(s) => CstInputValue::String(s.clone()),
        Value::Array(a) => CstInputValue::Array(a.iter().map(input).collect()),
        Value::Object(o) => CstInputValue::Object(o.iter().map(|(k, v)| (k.clone(), input(v))).collect()),
    }
}

fn parse_json(text: &str) -> Result<Value> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    match jsonc_parser::parse_to_serde_value::<Option<Value>>(text, &ParseOptions::default()) {
        Ok(Some(v)) if v.is_object() => Ok(v),
        Ok(None) => Ok(Value::Object(Map::new())),
        Ok(Some(_)) => bail!("its top level is not an object"),
        Err(e) => bail!("it is not valid JSON: {e}"),
    }
}

/// The file's text, or `None` when there is no file.
fn read_text(file: &Path) -> Result<Option<String>> {
    match fs::read(file) {
        Ok(b) => String::from_utf8(b).map(Some).with_context(|| format!("{} is not UTF-8 text", file.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", file.display())),
    }
}

fn same_program(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        let p = fs::canonicalize(s).unwrap_or_else(|_| PathBuf::from(s));
        let s = p.to_string_lossy().replace('/', "\\");
        let s = s.strip_prefix(r"\\?\").unwrap_or(&s).to_string();
        if cfg!(windows) { s.to_lowercase() } else { s }
    };
    norm(a) == norm(b)
}

/// What to put in `c`'s config by hand (or, for Claude Code, the command to run).
pub fn snippet(c: &Client, launch: &Launch) -> String {
    match c.store {
        Store::Json { .. } if c.id == "claude-code" => {
            let mut s = format!("claude mcp add --scope user --transport stdio {SERVER} -- \"{}\"", launch.command);
            for a in &launch.args {
                s.push(' ');
                s.push_str(a);
            }
            s
        }
        Store::Json { key, shape } => json_snippet(key, shape, launch),
        Store::Toml => edit_toml("", Some(launch)).expect("a new TOML document"),
        Store::Continue => {
            let mut s = format!("mcpServers:\n  - name: {SERVER}\n    command: {}\n    args:\n", yaml(&launch.command));
            for a in &launch.args {
                s.push_str(&format!("      - {}\n", yaml(a)));
            }
            s
        }
        Store::Goose => {
            let args: Vec<String> = launch.args.iter().map(|a| yaml(a)).collect();
            format!(
                "extensions:\n  {SERVER}:\n    name: {SERVER}\n    type: stdio\n    cmd: {}\n    args: [{}]\n    enabled: true\n    timeout: 300\n",
                yaml(&launch.command),
                args.join(", ")
            )
        }
    }
}

/// The `mcpServers` entry most other MCP clients read.
pub fn generic_snippet(launch: &Launch) -> String {
    json_snippet("mcpServers", Shape::Plain, launch)
}

fn json_snippet(key: &str, shape: Shape, launch: &Launch) -> String {
    let mut s = edit_json("{}", key, Some(entry(shape, launch))).expect("a new JSON document");
    s.push('\n');
    s
}

fn yaml(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(root: &Path) -> Env {
        Env { home: root.join("home"), config: root.join("roaming"), local: root.join("local"), path: vec![], codex_home: None }
    }

    fn find<'a>(cs: &'a [Client], id: &str) -> &'a Client {
        cs.iter().find(|c| c.id == id).unwrap()
    }

    fn launch() -> Launch {
        Launch { command: r"C:\Users\x\AppData\Local\Programs\Flux\flux.exe".into(), args: vec!["mcp".into(), "--gui".into()] }
    }

    #[test]
    fn detects_clients_by_their_folders() {
        let t = tempfile::tempdir().unwrap();
        let e = env(t.path());
        assert!(clients(&e).iter().all(|c| !c.found));
        fs::create_dir_all(e.home.join(".cursor")).unwrap();
        fs::create_dir_all(e.home.join(".gemini/antigravity")).unwrap();
        fs::create_dir_all(e.local.join("Packages/Claude_abc123/LocalCache/Roaming/Claude")).unwrap();
        let cs = clients(&e);
        assert!(find(&cs, "cursor").found);
        assert_eq!(find(&cs, "cursor").file, e.home.join(".cursor").join("mcp.json"));
        // Antigravity's folder moved between versions: the one that exists is used
        assert_eq!(find(&cs, "antigravity").file, e.home.join(".gemini").join("antigravity").join("mcp_config.json"));
        // the Store build of Claude Desktop sees %APPDATA% redirected into its package
        let d = find(&cs, "claude-desktop");
        assert!(d.found);
        assert!(d.file.starts_with(e.local.join("Packages").join("Claude_abc123")), "{}", d.file.display());
        assert!(!find(&cs, "codex").found);
        assert!(find(&cs, "goose").manual);
    }

    #[test]
    fn connects_keeping_comments_and_other_servers() {
        let t = tempfile::tempdir().unwrap();
        let e = env(t.path());
        let file = e.config.join("Zed").join("settings.json");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        let before = "{\n  // my theme\n  \"theme\": \"One Dark\",\n  \"context_servers\": {\n    \"other\": { \"command\": \"x\" }, // keep\n  },\n}\n";
        fs::write(&file, before).unwrap();
        let cs = clients(&e);
        let zed = find(&cs, "zed");
        if !cfg!(windows) {
            return; // Zed's file lives elsewhere off Windows
        }
        assert_eq!(state(zed, &launch()), State::NotConnected);
        let o = connect(zed, &launch());
        assert_eq!(o.result, "connected", "{o:?}");
        let after = fs::read_to_string(&file).unwrap();
        assert!(after.contains("// my theme") && after.contains("// keep") && after.contains("\"other\""), "{after}");
        assert_eq!(fs::read_to_string(o.backup.unwrap()).unwrap(), before);
        assert_eq!(state(zed, &launch()), State::Connected { gui: true });
        assert_eq!(connect(zed, &launch()).result, "unchanged");

        // a headless launch updates the entry in place
        let headless = Launch { args: vec!["mcp".into()], ..launch() };
        assert_eq!(connect(zed, &headless).result, "connected");
        assert_eq!(state(zed, &launch()), State::Connected { gui: false });

        assert_eq!(disconnect(zed).result, "disconnected");
        let gone = fs::read_to_string(&file).unwrap();
        assert!(!gone.contains("flux.exe") && gone.contains("\"other\"") && gone.contains("// my theme"), "{gone}");
        assert_eq!(state(zed, &launch()), State::NotConnected);
        assert_eq!(disconnect(zed).result, "not-connected");
    }

    #[test]
    fn keeps_what_the_user_added_to_the_entry() {
        let t = tempfile::tempdir().unwrap();
        let e = env(t.path());
        let file = e.home.join(".cursor").join("mcp.json");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "\u{feff}{\"mcpServers\":{\"flux\":{\"command\":\"D:/old/flux.exe\",\"args\":[\"mcp\"],\"env\":{\"A\":\"1\"}}}}").unwrap();
        let cs = clients(&e);
        let c = find(&cs, "cursor");
        assert_eq!(state(c, &launch()), State::Other { command: "D:/old/flux.exe".into() });
        assert_eq!(connect(c, &launch()).result, "connected");
        let text = fs::read_to_string(&file).unwrap();
        assert!(text.starts_with('\u{feff}'), "the byte-order mark stays");
        let v = parse_json(&text).unwrap();
        assert_eq!(v["mcpServers"]["flux"]["env"]["A"], "1");
        assert_eq!(v["mcpServers"]["flux"]["args"], json!(["mcp", "--gui"]));
        assert_eq!(state(c, &launch()), State::Connected { gui: true });
    }

    #[test]
    fn creates_missing_files_and_leaves_broken_ones_alone() {
        let t = tempfile::tempdir().unwrap();
        let e = env(t.path());
        fs::create_dir_all(e.home.join(".kiro")).unwrap();
        let cs = clients(&e);
        let kiro = find(&cs, "kiro");
        assert_eq!(connect(kiro, &launch()).result, "connected");
        let v = parse_json(&fs::read_to_string(&kiro.file).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["flux"]["command"], launch().command);

        let file = e.home.join(".cursor").join("mcp.json");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "{ \"mcpServers\": { oops").unwrap();
        let cs = clients(&e);
        let c = find(&cs, "cursor");
        assert!(matches!(state(c, &launch()), State::Unreadable { .. }));
        assert_eq!(connect(c, &launch()).result, "failed");
        assert_eq!(fs::read_to_string(&file).unwrap(), "{ \"mcpServers\": { oops");
    }

    #[test]
    fn edits_codex_toml() {
        let t = tempfile::tempdir().unwrap();
        let e = env(t.path());
        let file = e.home.join(".codex").join("config.toml");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        let before = "# my settings\nmodel = \"x\"\n\n[mcp_servers.other]\ncommand = \"y\" # keep\n";
        fs::write(&file, before).unwrap();
        let cs = clients(&e);
        let c = find(&cs, "codex");
        assert!(c.found);
        assert_eq!(connect(c, &launch()).result, "connected");
        let after = fs::read_to_string(&file).unwrap();
        assert!(after.starts_with(before), "{after}");
        let doc: toml_edit::DocumentMut = after.parse().unwrap();
        assert_eq!(doc["mcp_servers"]["flux"]["command"].as_str(), Some(launch().command.as_str()));
        assert_eq!(state(c, &launch()), State::Connected { gui: true });
        assert_eq!(disconnect(c).result, "disconnected");
        assert_eq!(fs::read_to_string(&file).unwrap(), before);

        // servers written as an inline table stay inline
        fs::write(&file, "mcp_servers = { other = { command = \"y\" } }\n").unwrap();
        assert_eq!(connect(c, &launch()).result, "connected");
        let doc: toml_edit::DocumentMut = fs::read_to_string(&file).unwrap().parse().unwrap();
        assert!(doc["mcp_servers"].is_inline_table());
        assert_eq!(state(c, &launch()), State::Connected { gui: true });
    }

    #[test]
    fn snippets() {
        let t = tempfile::tempdir().unwrap();
        let cs = clients(&env(t.path()));
        let l = launch();
        let cont = snippet(find(&cs, "continue"), &l);
        assert!(cont.contains(r"command: 'C:\Users\x\AppData\Local\Programs\Flux\flux.exe'"), "{cont}");
        assert!(snippet(find(&cs, "goose"), &l).contains("args: ['mcp', '--gui']"));
        assert!(snippet(find(&cs, "claude-code"), &l).starts_with("claude mcp add --scope user --transport stdio flux -- \"C:\\Users"));
        let vs: Value = serde_json::from_str(&snippet(find(&cs, "vscode"), &l)).unwrap();
        assert_eq!(vs["servers"]["flux"]["type"], "stdio");
        let oc: Value = serde_json::from_str(&snippet(find(&cs, "opencode"), &l)).unwrap();
        assert_eq!(oc["mcp"]["flux"]["command"][1], "mcp");
        let codex: toml_edit::DocumentMut = snippet(find(&cs, "codex"), &l).parse().unwrap();
        assert_eq!(codex["mcp_servers"]["flux"]["args"].as_array().unwrap().len(), 2);
        assert_eq!(state(find(&cs, "continue"), &l), State::Manual);
    }
}
