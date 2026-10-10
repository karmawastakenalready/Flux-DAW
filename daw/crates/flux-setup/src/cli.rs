//! Command-line front ends: `flux agents` (connect MCP clients), `flux packs` (install and remove instrument
//! packs), and Setup and Uninstall without their windows (`--quiet`). They report through their exit code: 0 ok,
//! 1 something didn't work out, 2 couldn't run.

use crate::agents::{self, Client, Launch, State};
use crate::job::{self, Bundle, Event, Job, LibraryChange, SetupPlan, UninstallPlan};
use anyhow::{Context, Result, bail};
use flux_assets::fsutil::fmt_bytes;
use flux_assets::packs::{Delivery, PackState, Section};
use flux_assets::workspace::{OpenOptions, Workspace};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

pub const AGENTS_HELP: &str = "\
flux agents: connect AI agents (MCP clients) to Flux

usage:
  flux agents [list] [--json]       the agents Flux knows, whether each is installed and connected
  flux agents connect ID...         add Flux to these agents' MCP servers (--found: every agent found here)
          [--headless] [--exe PATH]   without Flux's window / start another flux.exe than this one
  flux agents disconnect ID...      take Flux out again (--all: of every agent)
  flux agents show ID               what to paste into that agent's config by hand (`generic`: most others)

Config files are edited in place, keeping comments and other servers; the old file stays beside it as
<name>.flux-backup. Claude Code is connected with its own `claude mcp add`.";

pub const PACKS_HELP: &str = "\
flux packs: the instrument library's packs

usage:
  flux packs [list] [--json]        every pack with its instruments, sizes and what is installed
  flux packs install NAME...        download and install packs or single libraries (with what they need);
                                    NAME may also be minimal, recommended or everything
  flux packs remove NAME... [--yes] delete installed packs or libraries (and their downloads)
  flux packs import [--from DIR]... install what you downloaded in a browser (the metal guitar rigs):
                                    found by size and checksum in DIR (default: Downloads), loose or zipped";

pub const SETUP_HELP: &str = "\
Flux Setup: install Flux for this user (no administrator rights needed)

usage:
  FluxSetup.exe                     install in a window (an installed Flux: `flux setup`)
  FluxSetup.exe --quiet [options]   install without a window:
      --dir DIR                     the app's folder (default: %LOCALAPPDATA%\\Programs\\Flux, or where it is)
      --library DIR                 the instrument library (default: %LOCALAPPDATA%\\Flux\\Library, or where it is)
      --songs DIR                   where songs are saved (default: Documents\\Flux)
      --packs NAME,...              minimal, recommended (the default), everything, none, or packs and
                                    libraries (`flux packs list`)
      --agents ID,...               found (the default: every agent found here), none, or agent ids
                                    (`flux agents list`)
      --headless-agents             agents start Flux without its window
      --no-path                     don't add Flux to PATH
      --desktop-shortcut            put Flux on the desktop
  FluxSetup.exe --update [--quiet]  update the installed Flux: what is installed and connected stays, and
                                    instruments this version uses newer releases of are replaced

A windowed program returns at once in a terminal; to wait for a quiet install:
  Start-Process .\\FluxSetup.exe -ArgumentList '--quiet' -Wait      (PowerShell)
  start /wait FluxSetup.exe --quiet                                (cmd)";

pub const UNINSTALL_HELP: &str = "\
flux uninstall: remove Flux from this computer

usage:
  flux uninstall                    in a window, which asks whether to delete the instrument library too
  flux uninstall --quiet [--remove-library]
                                    without a window; the library stays unless --remove-library is given

Your songs (Documents\\Flux) always stay, with the plugin settings saved for them; agents are disconnected.";

/// Parsed arguments: positionals, flags and `--name value` options.
struct Args {
    pos: Vec<String>,
    flags: Vec<String>,
    opts: Vec<(String, String)>,
}

impl Args {
    fn parse(args: &[String], flags: &[&str], valued: &[&str]) -> Result<Args> {
        let mut a = Args { pos: vec![], flags: vec![], opts: vec![] };
        let mut it = args.iter();
        while let Some(s) = it.next() {
            match s.strip_prefix("--") {
                Some(n) if flags.contains(&n) => a.flags.push(n.to_string()),
                Some(n) if valued.contains(&n) => a.opts.push((n.to_string(), it.next().with_context(|| format!("--{n} needs a value"))?.clone())),
                Some(n) => bail!("unknown option --{n}"),
                None => a.pos.push(s.clone()),
            }
        }
        Ok(a)
    }

    fn flag(&self, n: &str) -> bool {
        self.flags.iter().any(|f| f == n)
    }

    fn opt(&self, n: &str) -> Option<&str> {
        self.opts.iter().rev().find(|(k, _)| k == n).map(|(_, v)| v.as_str())
    }

    fn opts(&self, n: &str) -> Vec<&str> {
        self.opts.iter().filter(|(k, _)| k == n).map(|(_, v)| v.as_str()).collect()
    }
}

fn exit(r: Result<u8>) -> ExitCode {
    match r {
        Ok(c) => ExitCode::from(c),
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

/// `flux agents ...`
pub fn agents_main(args: &[String]) -> ExitCode {
    exit(run_agents(args))
}

fn run_agents(args: &[String]) -> Result<u8> {
    let a = Args::parse(args, &["json", "found", "all", "headless", "help"], &["exe"])?;
    if a.flag("help") || a.pos.first().is_some_and(|c| c == "help") {
        println!("{AGENTS_HELP}");
        return Ok(0);
    }
    let exe = match a.opt("exe") {
        Some(p) => PathBuf::from(p),
        None => std::env::current_exe().context("finding this program's file")?,
    };
    let launch = Launch::new(&exe, !a.flag("headless"));
    let env = agents::Env::current()?;
    let clients = agents::clients(&env);
    let cmd = a.pos.first().map_or("list", String::as_str);
    let ids = &a.pos[1.min(a.pos.len())..];
    let pick = |all: bool| -> Result<Vec<&Client>> {
        if all {
            return Ok(clients.iter().filter(|c| c.found && !c.manual).collect());
        }
        if ids.is_empty() {
            bail!("name the agents (`flux agents list` shows their ids)");
        }
        ids.iter().map(|id| clients.iter().find(|c| c.id == id).with_context(|| format!("no agent '{id}' (`flux agents list` shows their ids)"))).collect()
    };
    match cmd {
        "list" => {
            if a.flag("json") {
                let rows: Vec<_> = clients.iter().map(|c| serde_json::json!({ "client": c, "state": agents::state(c, &launch) })).collect();
                println!("{}", serde_json::to_string_pretty(&rows)?);
                return Ok(0);
            }
            println!("Agents start {} {}", launch.command, launch.args.join(" "));
            println!();
            for c in &clients {
                let state = match agents::state(c, &launch) {
                    State::Connected { gui: true } => "connected".to_string(),
                    State::Connected { gui: false } => "connected (headless)".to_string(),
                    State::Other { command } => format!("starts another Flux: {command}"),
                    State::NotConnected if c.found => "not connected".to_string(),
                    State::NotConnected => "not found here".to_string(),
                    State::Unreadable { error } => format!("config unreadable: {error}"),
                    State::Manual if c.found => "paste by hand (`flux agents show`)".to_string(),
                    State::Manual => "not found here".to_string(),
                };
                println!("  {:<16} {:<26} {state}", c.id, c.name);
            }
            Ok(0)
        }
        "connect" => {
            let mut bad = 0;
            for c in pick(a.flag("found"))? {
                let o = agents::connect(c, &launch);
                match o.result {
                    "failed" => {
                        bad += 1;
                        println!("{}: failed: {}", c.name, o.message.unwrap_or_default());
                    }
                    "manual" => println!("{}: paste this into {}:\n\n{}\n{}", c.name, c.file.display(), agents::snippet(c, &launch), c.then),
                    r => println!("{}: {r}. {}", c.name, c.then),
                }
            }
            Ok(u8::from(bad > 0))
        }
        "disconnect" => {
            let mut bad = 0;
            let chosen = if a.flag("all") { clients.iter().filter(|c| !c.manual).collect() } else { pick(false)? };
            for c in chosen {
                let o = agents::disconnect(c);
                if o.result == "not-connected" && a.flag("all") {
                    continue;
                }
                bad += usize::from(!o.ok());
                println!("{}: {}{}", c.name, o.result, o.message.map_or(String::new(), |m| format!(": {m}")));
            }
            Ok(u8::from(bad > 0))
        }
        "show" => {
            let Some(id) = ids.first() else { bail!("name an agent, or `generic`") };
            if id == "generic" {
                print!("{}", agents::generic_snippet(&launch));
                return Ok(0);
            }
            let c = pick(false)?[0];
            let s = agents::snippet(c, &launch);
            print!("{s}{}", if s.ends_with('\n') { "" } else { "\n" });
            if c.id != "claude-code" {
                eprintln!("\n(in {})", c.file.display());
            }
            Ok(0)
        }
        other => bail!("unknown command '{other}'\n\n{AGENTS_HELP}"),
    }
}

/// `flux packs ...` on the library at `root`.
pub fn packs_main(root: &Path, args: &[String]) -> ExitCode {
    exit(run_packs(root, args))
}

fn run_packs(root: &Path, args: &[String]) -> Result<u8> {
    let a = Args::parse(args, &["json", "yes", "help"], &["from"])?;
    if a.flag("help") || a.pos.first().is_some_and(|c| c == "help") {
        println!("{PACKS_HELP}");
        return Ok(0);
    }
    let ws = Workspace::open(root, &OpenOptions::default())?;
    let cmd = a.pos.first().map_or("list", String::as_str);
    let names = &a.pos[1.min(a.pos.len())..];
    match cmd {
        "list" => {
            let packs = job::packs(&ws);
            if a.flag("json") {
                println!("{}", serde_json::to_string_pretty(&packs)?);
                return Ok(0);
            }
            println!("Library: {}", root.display());
            for section in [Section::Core, Section::Instruments, Section::Plugins] {
                println!();
                println!("{}", match section {
                    Section::Core => "Flux",
                    Section::Instruments => "Instruments",
                    Section::Plugins => "Plugins",
                });
                for p in packs.iter().filter(|p| p.section == section) {
                    let state = match p.state {
                        PackState::Installed => "installed",
                        PackState::Partly => "partly installed",
                        PackState::Missing => "",
                    };
                    let size: u64 = p.assets.iter().filter_map(|x| x.size).sum();
                    let size = if size > 0 { fmt_bytes(size) } else { String::new() };
                    let how = |x: &job::AssetView| match (x.installed, x.delivery) {
                        (true, _) => "installed".to_string(),
                        (false, Delivery::Download) => String::new(),
                        (false, Delivery::ByHand) => format!("download in a browser: {}", x.page.as_deref().unwrap_or("")),
                        (false, Delivery::External) => format!("install from {}", x.page.as_deref().unwrap_or("its maker")),
                    };
                    let state = match &p.assets[..] {
                        [x] if !x.installed => how(x),
                        _ => state.to_string(),
                    };
                    println!("  {:<22} {:<26} {size:>9}  {state}", p.id, p.name);
                    if p.assets.len() > 1 {
                        for x in &p.assets {
                            let size = x.size.filter(|s| *s > 0).map(fmt_bytes).unwrap_or_default();
                            println!("    {:<34} {size:>9}  {}", x.id, how(x));
                        }
                    }
                }
            }
            println!();
            let sel: Vec<&str> = ws.loaded.selections().iter().map(|s| s.id.as_str()).chain(["everything"]).collect();
            println!("Quick choices: {}", sel.join(", "));
            Ok(0)
        }
        "install" => {
            let ids = expand(&ws, names)?;
            let space = job::space(&ws, &ids);
            if space.download == 0 {
                println!("Nothing to download: those are installed (or come from their makers' installers).");
            } else {
                println!("Downloading {} (about {} free space needed{}).", fmt_bytes(space.download), fmt_bytes(space.needed), space.free.map_or(String::new(), |f| format!(", {} free", fmt_bytes(f))));
                if !space.enough() {
                    bail!("not enough free space on the library's drive");
                }
            }
            let root = root.to_path_buf();
            let job = Job::spawn(1, {
                let root = root.clone();
                move |r| job::change_library(r, &root, &LibraryChange { install: ids, remove: vec![] })
            });
            let report = job.wait(print_event)?;
            let mut bad = 0;
            for o in &report.fetched {
                if !matches!(o.result.as_str(), "installed" | "present" | "replaced" | "updated" | "skipped") {
                    bad += 1;
                    println!("{}: {}{}", o.id, o.result, o.message.as_ref().map_or(String::new(), |m| format!(": {m}")));
                }
            }
            let ws = Workspace::open(&root, &OpenOptions::default())?;
            print_hand_steps(&job::hand_steps(&ws, &expand(&ws, names)?));
            Ok(u8::from(bad > 0))
        }
        "remove" => {
            let ids: Vec<String> = expand(&ws, names)?.into_iter().filter(|id| ws.loaded.asset(id).is_some_and(|x| ws.is_installed(x))).collect();
            if ids.is_empty() {
                println!("None of those is installed.");
                return Ok(0);
            }
            // songs that would lose sounds (also those in the songs folder outside the library)
            if let Ok(u) = job::usage(root) {
                for id in &ids {
                    let n = u.roots_using(id).len();
                    if n > 0 {
                        println!("{n} song{} use{} {id}: they lose those sounds until it is installed again", if n == 1 { "" } else { "s" }, if n == 1 { "s" } else { "" });
                    }
                }
            }
            if !a.flag("yes") && !confirm(&format!("Delete {}?", ids.join(", "))) {
                bail!("nothing was removed");
            }
            let root = root.to_path_buf();
            let job = Job::spawn(1, move |r| job::change_library(r, &root, &LibraryChange { install: vec![], remove: ids }));
            let report = job.wait(print_event)?;
            for o in &report.removed {
                println!("{}: {}{} ({} freed)", o.id, o.result, o.message.as_ref().map_or(String::new(), |m| format!(": {m}")), fmt_bytes(o.freed));
            }
            Ok(0)
        }
        "import" => {
            let places: Vec<PathBuf> = match a.opts("from") {
                v if !v.is_empty() => v.into_iter().map(PathBuf::from).collect(),
                _ => vec![flux_engine::paths::user_downloads().context("no Downloads folder; pass --from")?],
            };
            let root = root.to_path_buf();
            let ids = names.to_vec();
            let job = Job::spawn(2, move |r| job::import_downloads(r, &root, &ids, &places));
            let (missing, report) = job.wait(print_event)?;
            for id in &report.installed {
                println!("{id}: installed");
            }
            for (id, why) in &report.failed {
                println!("{id}: failed: {why}");
            }
            for w in &missing {
                println!("{}: still needs {} from {}", w.asset, w.name, w.page);
            }
            Ok(u8::from(!report.failed.is_empty() || !missing.is_empty()))
        }
        other => bail!("unknown command '{other}'\n\n{PACKS_HELP}"),
    }
}

/// `flux setup --quiet ...` (or `--help`): Setup without its window, installing the Flux of `bundle`.
pub fn setup_main(args: &[String], bundle: Bundle) -> ExitCode {
    exit(run_setup(args, bundle))
}

fn run_setup(args: &[String], bundle: Bundle) -> Result<u8> {
    let a = Args::parse(args, &["quiet", "update", "headless-agents", "no-path", "desktop-shortcut", "help"], &["dir", "library", "songs", "packs", "agents"])?;
    if a.flag("help") || a.pos.first().is_some_and(|c| c == "help") {
        println!("{SETUP_HELP}");
        return Ok(0);
    }
    if let Some(p) = a.pos.first() {
        bail!("unexpected '{p}'\n\n{SETUP_HELP}");
    }
    let plan = quiet_plan(&a)?;
    println!("Installing Flux {} in {}", bundle.version, plan.app_dir.display());
    println!("Instrument library: {}", plan.library.display());
    let on_path = plan.add_to_path;
    let job = Job::spawn(4, move |r| job::setup(r, &plan, &bundle));
    let report = job.wait(print_event)?;
    for o in report.failures() {
        println!("{}: {}{}", o.id, o.result, o.message.as_ref().map_or(String::new(), |m| format!(": {m}")));
    }
    print_hand_steps(&report.hand);
    println!(
        "{}. Start Flux from the Start menu{}.",
        if report.ok() { "Done" } else { "Done, with the problems above" },
        if on_path { ", or type `flux` in a new terminal" } else { "" }
    );
    Ok(u8::from(!report.ok()))
}

/// The plan of a quiet Setup: [`SetupPlan::suggested`] with the options given.
fn quiet_plan(a: &Args) -> Result<SetupPlan> {
    let mut plan = SetupPlan::suggested(a.flag("update"));
    let dir = |o: &str| -> Result<Option<PathBuf>> {
        a.opt(o).map(|d| std::path::absolute(d).with_context(|| format!("--{o} {d}"))).transpose()
    };
    if let Some(d) = dir("dir")? {
        plan.app_dir = d;
    }
    if let Some(d) = dir("library")? {
        plan.library = d;
    }
    if let Some(d) = dir("songs")? {
        plan.documents = Some(d);
    }
    let list = |s: &str| -> Vec<String> { s.split(',').map(str::trim).filter(|x| !x.is_empty()).map(str::to_string).collect() };
    if let Some(p) = a.opt("packs") {
        plan.assets = list(p);
    }
    if let Some(x) = a.opt("agents") {
        let env = agents::Env::current()?;
        let clients = agents::clients(&env);
        plan.agents = match x {
            "found" => clients.iter().filter(|c| c.found && !c.manual).map(|c| c.id.to_string()).collect(),
            "none" => vec![],
            ids => {
                let ids = list(ids);
                if let Some(bad) = ids.iter().find(|id| !clients.iter().any(|c| c.id == id.as_str())) {
                    bail!("no agent '{bad}' (`flux agents list` shows their ids)");
                }
                ids
            }
        };
    }
    plan.agent_window = !a.flag("headless-agents");
    if a.flag("no-path") {
        plan.add_to_path = false;
    }
    if a.flag("desktop-shortcut") {
        plan.desktop_shortcut = true;
    }
    Ok(plan)
}

/// `flux uninstall --quiet [--remove-library]` (or `--help`): remove the Flux installed in `app_dir`.
pub fn uninstall_main(app_dir: &Path, args: &[String]) -> ExitCode {
    exit(run_uninstall(app_dir, args))
}

fn run_uninstall(app_dir: &Path, args: &[String]) -> Result<u8> {
    let a = Args::parse(args, &["quiet", "remove-library", "help"], &[])?;
    if a.flag("help") || a.pos.first().is_some_and(|c| c == "help") {
        println!("{UNINSTALL_HELP}");
        return Ok(0);
    }
    if let Some(p) = a.pos.first() {
        bail!("unexpected '{p}'\n\n{UNINSTALL_HELP}");
    }
    let plan = UninstallPlan { remove_library: a.flag("remove-library") };
    let dir = app_dir.to_path_buf();
    let job = Job::spawn(3, move |r| job::uninstall(r, &dir, &plan));
    let report = job.wait(print_event)?;
    if let Some(l) = &report.library {
        for k in &l.kept {
            println!("Kept {}", k.display());
        }
    }
    let songs = flux_engine::paths::documents();
    if songs.is_dir() {
        println!("Flux is uninstalled; its folder goes once this command ends. Your songs are in {}.", songs.display());
    } else {
        println!("Flux is uninstalled; its folder goes once this command ends.");
    }
    Ok(u8::from(!report.agents.iter().all(agents::Outcome::ok)))
}

/// Pack ids, selection names and asset ids to asset ids.
fn expand(ws: &Workspace, names: &[String]) -> Result<Vec<String>> {
    if names.is_empty() {
        bail!("name packs or libraries (`flux packs list` shows them)");
    }
    job::expand(ws, names)
}

/// Print what the user has to do in a browser or an installer.
fn print_hand_steps(steps: &[job::AssetView]) {
    for x in steps {
        let page = x.page.as_deref().unwrap_or("its maker's page");
        match x.delivery {
            Delivery::ByHand => println!("{}: download it in a browser from {page}, then run `flux packs import`", x.name),
            _ => println!("{}: install it from {page}", x.name),
        }
    }
}

fn print_event(e: Event) {
    let mut err = std::io::stderr();
    match e {
        Event::Step { title, .. } => println!("{title}"),
        Event::Line(l) => println!("{l}"),
        Event::Progress { label, done, total } if err.is_terminal() => {
            let t = match total {
                Some(t) if t > 0 => format!("  {label}: {} / {} ({:.0}%)", fmt_bytes(done), fmt_bytes(t), done as f64 * 100.0 / t as f64),
                _ => format!("  {label}: {}", fmt_bytes(done)),
            };
            let _ = write!(err, "\r{t:<78}");
            let _ = err.flush();
        }
        Event::Progress { .. } => {}
        Event::ProgressEnd => {
            if err.is_terminal() {
                let _ = write!(err, "\r{:<78}\r", "");
            }
        }
    }
}

fn confirm(q: &str) -> bool {
    if !std::io::stdin().is_terminal() {
        println!("{q} [y/N] (no terminal to answer; pass --yes)");
        return false;
    }
    print!("{q} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    std::io::stdin().read_line(&mut s).is_ok() && matches!(s.trim(), "y" | "Y" | "yes")
}
