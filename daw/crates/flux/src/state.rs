//! State shared by the MCP server, the GUI and the audio thread.
//!
//! Every edit goes through `Shared::apply`, so agent and human changes land in the same undo history
//! and activity log. Renders run on a background thread; waiters block on `changed`.

use crate::audio::{Clip, Player};
use anyhow::{bail, Result};
use flux_core::{Command, CommentOp, Project, Session, Source};
use flux_engine::{RenderOptions, RenderOutput};
use eframe::egui;
use parking_lot::{Condvar, Mutex, MutexGuard};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub enum RenderStatus {
    Idle,
    Running { revision: u64, started: Instant, message: String },
    Failed { revision: u64, error: String },
}

/// A finished render, with the project snapshot it was made from (the timeline maps through it).
pub struct LastRender {
    pub out: RenderOutput,
    pub project: Project,
    pub revision: u64,
    /// Increments per finished render (GUI caches key on it).
    pub seq: u64,
}

/// Ask the GUI (main thread) to do something.
pub enum MainMsg {
    OpenGui,
    Quit,
}

/// One option in an audition: the edits that make it, and its rendered excerpt.
pub struct Variant {
    pub name: String,
    pub commands: Vec<Value>,
    pub clip: Arc<Clip>,
    pub path: PathBuf,
    /// Loudness of the excerpt before the clips were matched (LUFS).
    pub lufs: f64,
}

/// A set of variants the human listens to and picks from (A/B/C...), made by the agent.
pub struct Audition {
    pub id: u64,
    pub label: String,
    pub question: String,
    pub from_bar: u32,
    pub to_bar: u32,
    /// Project revision the variants were made from.
    pub revision: u64,
    pub variants: Vec<Variant>,
    pub pick: Option<usize>,
    /// What the human said about the choice.
    pub note: String,
    pub applied: bool,
}

/// Who chose and who applied an audition variant (shown next to the variants).
#[derive(Clone, Copy, Default)]
pub struct AuditionMarks {
    pub picked_by: Option<Source>,
    pub applied_by: Option<Source>,
    /// The variant applied (it can differ from the pick when one is named explicitly).
    pub applied_variant: Option<usize>,
}

#[derive(Clone, Copy, Default)]
pub struct Focus {
    pub track: Option<u64>,
    pub bar: Option<u32>,
    /// "arrange", "piano_roll", "mixer", "track", "master", "analysis", "comments" or "auditions"
    pub panel: Option<&'static str>,
}

pub struct State {
    pub session: Session,
    pub render: Option<Arc<LastRender>>,
    pub status: RenderStatus,
    pub render_seq: u64,
    pub egui: Option<egui::Context>,
    pub gui_open: bool,
    /// The GUI's native window (Windows HWND), so plugin windows open over it.
    pub window: Option<isize>,
    /// Name of the connected MCP client, if any.
    pub agent: Option<String>,
    /// Agent asked the GUI to show a track / bar.
    pub focus: Option<Focus>,
    pub screenshot_wanted: bool,
    pub screenshot: Option<Arc<egui::ColorImage>>,
    pub auditions: Vec<Audition>,
    /// Bumps when an audition is created, picked or applied (waiters key on it).
    pub audition_rev: u64,
    /// Attribution per audition id (the `Audition` itself only holds the choice).
    pub audition_marks: std::collections::HashMap<u64, AuditionMarks>,
}

pub struct Shared {
    pub state: Mutex<State>,
    pub changed: Condvar,
    pub player: Arc<Player>,
    /// MIDI keyboards and the computer keyboard playing in, and recording.
    pub midi: crate::midi::Midi,
    pub main_tx: Mutex<Option<std::sync::mpsc::Sender<MainMsg>>>,
}

impl Shared {
    pub fn new(session: Session) -> Arc<Shared> {
        Arc::new(Shared {
            state: Mutex::new(State {
                session,
                render: None,
                status: RenderStatus::Idle,
                render_seq: 0,
                egui: None,
                gui_open: false,
                window: None,
                agent: None,
                focus: None,
                screenshot_wanted: false,
                screenshot: None,
                auditions: vec![],
                audition_rev: 0,
                audition_marks: Default::default(),
            }),
            changed: Condvar::new(),
            player: Player::new(),
            midi: crate::midi::Midi::new(),
            main_tx: Mutex::new(None),
        })
    }

    pub fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock()
    }

    pub fn repaint(st: &State) {
        if let Some(c) = &st.egui {
            c.request_repaint();
        }
    }

    pub fn apply(&self, cmds: &[Command], source: Source) -> Result<Vec<Value>> {
        let mut st = self.lock();
        let r = st.session.apply_batch(cmds, source);
        if r.is_ok() {
            self.player.project_changed(&st.session.project);
        }
        Self::repaint(&st);
        self.changed.notify_all();
        r
    }

    pub fn with_session<R>(&self, f: impl FnOnce(&mut Session) -> R) -> R {
        let mut st = self.lock();
        let rev = st.session.revision;
        let r = f(&mut st.session);
        if st.session.revision != rev {
            self.player.project_changed(&st.session.project);
        }
        Self::repaint(&st);
        self.changed.notify_all();
        r
    }

    /// Add, answer, resolve or remove a timeline comment.
    pub fn comment(&self, op: &CommentOp, source: Source) -> Result<Value> {
        let mut st = self.lock();
        let r = st.session.comment(op, source);
        Self::repaint(&st);
        self.changed.notify_all();
        r
    }

    /// Record the human's (or agent's) choice in an audition and wake anyone waiting for it.
    pub fn pick(&self, id: u64, pick: Option<usize>, note: Option<String>, source: Source) -> Result<()> {
        let mut st = self.lock();
        let a = st.auditions.iter_mut().find(|a| a.id == id).ok_or_else(|| anyhow::anyhow!("no audition {id}"))?;
        if let Some(i) = pick
            && i >= a.variants.len() {
                bail!("audition {id} has {} variants", a.variants.len());
            }
        a.pick = pick;
        let picked = a.pick.is_some();
        if let Some(n) = note {
            a.note = n;
        }
        let msg = match pick {
            Some(i) => format!("audition {id} ({}): picked {} '{}'", a.label, letter(i), a.variants[i].name),
            None => format!("audition {id} ({}): pick cleared", a.label),
        };
        st.audition_marks.entry(id).or_default().picked_by = picked.then_some(source);
        st.audition_rev += 1;
        st.session.note(source, msg);
        Self::repaint(&st);
        self.changed.notify_all();
        Ok(())
    }

    /// Play the song (live playback starts from the current project; an audition clip hands back to the song).
    pub fn play(&self) -> Result<(), String> {
        if self.player.mode() == crate::audio::Mode::Clip {
            self.player.set_mode(self.player.song_mode());
        }
        let p = self.lock().session.project.clone();
        self.player.play(Some(&p))
    }

    /// Make an audition variant's edits for real (one undo step). `variant` None = the pick.
    /// Returns (variant index, command results, the revision the audition was made from).
    pub fn apply_audition(&self, id: u64, variant: Option<usize>, source: Source) -> Result<(usize, Vec<Value>, u64)> {
        let (i, label, name, raw, rev) = {
            let st = self.lock();
            let au = st.auditions.iter().find(|a| a.id == id).ok_or_else(|| anyhow::anyhow!("no audition {id}"))?;
            if au.applied {
                bail!("audition {id} was already applied");
            }
            let i = variant.or(au.pick).ok_or_else(|| anyhow::anyhow!("audition {id} has no pick yet"))?;
            let v = au.variants.get(i).ok_or_else(|| anyhow::anyhow!("audition {id} has {} variants", au.variants.len()))?;
            (i, au.label.clone(), v.name.clone(), v.commands.clone(), au.revision)
        };
        let cmds = raw
            .into_iter()
            .enumerate()
            .map(|(k, c)| serde_json::from_value::<Command>(c).map_err(|e| anyhow::anyhow!("command {}: {e}", k + 1)))
            .collect::<Result<Vec<_>>>()?;
        let results = if cmds.is_empty() { vec![] } else { self.apply(&cmds, source)? };
        let mut st = self.lock();
        let mut had_pick = true;
        if let Some(au) = st.auditions.iter_mut().find(|a| a.id == id) {
            au.applied = true;
            had_pick = au.pick.is_some();
            au.pick.get_or_insert(i);
        }
        let marks = st.audition_marks.entry(id).or_default();
        marks.applied_by = Some(source);
        marks.applied_variant = Some(i);
        if !had_pick {
            marks.picked_by = Some(source);
        }
        st.audition_rev += 1;
        st.session.note(source, format!("audition {id} ({label}): applied {} '{name}'", letter(i)));
        Self::repaint(&st);
        self.changed.notify_all();
        Ok((i, results, rev))
    }

    /// The time map the playhead follows: the live project's, or that of the project the render came from.
    pub fn timeline_map(st: &State, player: &Player) -> Option<flux_core::TimeMap> {
        match player.mode() {
            crate::audio::Mode::Render => st.render.as_ref().map(|r| flux_core::TimeMap::new(&r.project)),
            _ => Some(flux_core::TimeMap::new(&st.session.project)),
        }
    }

    /// Start rendering the current project in the background. Returns the revision being rendered.
    pub fn start_render(self: &Arc<Self>, stems_dir: Option<PathBuf>) -> Result<u64> {
        self.start_render_with(stems_dir, false)
    }

    /// `start_render`, optionally also rendering one stem per track layer.
    pub fn start_render_with(self: &Arc<Self>, stems_dir: Option<PathBuf>, layers: bool) -> Result<u64> {
        let (project, revision) = {
            let mut st = self.lock();
            if let RenderStatus::Running { revision, .. } = st.status {
                if revision == st.session.revision && !layers {
                    return Ok(revision);
                }
                bail!("a render of revision {revision} is still running; wait for it first");
            }
            let rev = st.session.revision;
            st.status = RenderStatus::Running { revision: rev, started: Instant::now(), message: "starting".into() };
            Self::repaint(&st);
            (st.session.project.clone(), rev)
        };
        let me = self.clone();
        std::thread::Builder::new().name("render".into()).spawn(move || {
            let prog = me.clone();
            let progress: flux_engine::render::Progress = Arc::new(move |m: String| {
                let mut st = prog.lock();
                if let RenderStatus::Running { message, .. } = &mut st.status {
                    *message = m;
                }
                Self::repaint(&st);
            });
            let opts = RenderOptions { stems_dir, progress: Some(progress), layers, ..Default::default() };
            let result = flux_engine::render(&project, &opts);
            let mut st = me.lock();
            match result {
                Ok(out) => {
                    st.render_seq += 1;
                    let lr = Arc::new(LastRender { out, project, revision, seq: st.render_seq });
                    me.player.load(lr.clone());
                    st.render = Some(lr);
                    st.status = RenderStatus::Idle;
                }
                Err(e) => st.status = RenderStatus::Failed { revision, error: format!("{e:#}") },
            }
            Self::repaint(&st);
            me.changed.notify_all();
        })?;
        Ok(revision)
    }

    /// Block until no render is running (or the timeout passes). Returns false on timeout.
    pub fn wait_render(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut st = self.lock();
        while matches!(st.status, RenderStatus::Running { .. }) {
            if self.changed.wait_until(&mut st, deadline).timed_out() {
                return false;
            }
        }
        true
    }

    pub fn send_main(&self, m: MainMsg) -> bool {
        self.main_tx.lock().as_ref().map(|tx| tx.send(m).is_ok()).unwrap_or(false)
    }
}

/// Variant label: A, B, C...
pub fn letter(i: usize) -> char {
    (b'A' + (i % 26) as u8) as char
}
