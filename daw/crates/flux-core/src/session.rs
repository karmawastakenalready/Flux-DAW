use crate::commands::{apply, Command};
use crate::model::{Comment, Project, Reply, TrackRef};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Agent,
    Human,
    System,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogEntry {
    pub revision: u64,
    pub source: Source,
    pub summary: String,
    /// Seconds since the session started.
    pub at: f64,
    pub ok: bool,
}

/// A change to the timeline comments. Comments are annotations: they are logged but take no undo step and
/// leave the revision (and so the render) alone.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum CommentOp {
    Add {
        bar: u32,
        #[serde(default)]
        beat: f64,
        #[serde(default)]
        to_bar: Option<u32>,
        #[serde(default)]
        track: Option<TrackRef>,
        text: String,
    },
    Reply {
        id: u64,
        text: String,
    },
    /// Mark done, optionally with a closing reply ("lowered the harp 3 dB in bars 12-16").
    Resolve {
        id: u64,
        #[serde(default)]
        reply: Option<String>,
    },
    Reopen {
        id: u64,
    },
    Edit {
        id: u64,
        text: String,
    },
    Remove {
        id: u64,
    },
}

/// A project being edited, with history and an activity log.
pub struct Session {
    pub project: Project,
    pub path: Option<PathBuf>,
    pub revision: u64,
    /// Bumps on every comment change (comments do not touch `revision`).
    pub comments_rev: u64,
    /// (revision, comments_rev) when the project was last saved, opened or replaced: see `is_dirty`.
    saved: (u64, u64),
    pub log: Vec<LogEntry>,
    undo: Vec<Project>,
    redo: Vec<Project>,
    started: Instant,
}

const MAX_UNDO: usize = 300;

impl Session {
    pub fn new(project: Project) -> Session {
        Session { project, path: None, revision: 0, comments_rev: 0, saved: (0, 0), log: vec![], undo: vec![], redo: vec![], started: Instant::now() }
    }

    pub fn open(path: &Path) -> Result<Session> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let project: Project = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let mut s = Session::new(project);
        s.path = Some(path.to_path_buf());
        s.note(Source::System, format!("opened {}", path.display()));
        Ok(s)
    }

    pub fn save(&mut self, path: Option<&Path>) -> Result<PathBuf> {
        let p = match path {
            Some(p) => p.to_path_buf(),
            None => self.path.clone().context("no path: pass one to save")?,
        };
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        std::fs::write(&p, serde_json::to_string_pretty(&self.project)?)?;
        self.path = Some(p.clone());
        self.saved = (self.revision, self.comments_rev);
        self.note(Source::System, format!("saved {}", p.display()));
        Ok(p)
    }

    pub fn note(&mut self, source: Source, summary: String) {
        self.log.push(LogEntry { revision: self.revision, source, summary, at: self.started.elapsed().as_secs_f64(), ok: true });
    }

    /// Apply one command as one undo step.
    pub fn apply(&mut self, cmd: &Command, source: Source) -> Result<Value> {
        self.apply_batch(std::slice::from_ref(cmd), source).map(|mut v| v.pop().unwrap_or(Value::Null))
    }

    /// Apply several commands atomically: all succeed (one undo step) or nothing changes.
    pub fn apply_batch(&mut self, cmds: &[Command], source: Source) -> Result<Vec<Value>> {
        self.apply_batch_labeled(cmds, source, None)
    }

    /// `apply_batch`, logged in the activity as `label` (what the batch does, e.g. "compose chords: Pad bars
    /// 5-8") instead of its first command.
    pub fn apply_batch_labeled(&mut self, cmds: &[Command], source: Source, label: Option<&str>) -> Result<Vec<Value>> {
        let before = self.project.clone();
        let mut results = Vec::with_capacity(cmds.len());
        for (i, c) in cmds.iter().enumerate() {
            match apply(&mut self.project, c) {
                Ok(v) => results.push(v),
                Err(e) => {
                    self.project = before;
                    let msg = format!("{} (command {} of {}: {})", e, i + 1, cmds.len(), c.describe());
                    self.log.push(LogEntry {
                        revision: self.revision,
                        source,
                        summary: format!("FAILED {}", c.describe()),
                        at: self.started.elapsed().as_secs_f64(),
                        ok: false,
                    });
                    anyhow::bail!(msg);
                }
            }
        }
        self.undo.push(before);
        if self.undo.len() > MAX_UNDO {
            self.undo.remove(0);
        }
        self.redo.clear();
        self.revision += 1;
        let summary = match label {
            Some(l) => l.to_string(),
            None if cmds.len() == 1 => cmds[0].describe(),
            None => format!("batch of {}: {}", cmds.len(), cmds[0].describe()),
        };
        self.log.push(LogEntry { revision: self.revision, source, summary, at: self.started.elapsed().as_secs_f64(), ok: true });
        Ok(results)
    }

    /// Add, answer, resolve or remove a timeline comment.
    pub fn comment(&mut self, op: &CommentOp, source: Source) -> Result<Value> {
        let p = &mut self.project;
        let find = |p: &mut Project, id: u64| -> Result<usize> {
            p.comments.iter().position(|c| c.id == id).ok_or_else(|| anyhow!("no comment #{id}"))
        };
        let (summary, out) = match op {
            CommentOp::Add { bar, beat, to_bar, track, text } => {
                if text.trim().is_empty() {
                    bail!("empty comment");
                }
                if *bar == 0 || *bar > p.bars {
                    bail!("bar {bar} is outside the song (1-{})", p.bars);
                }
                let track = match track {
                    Some(r) => Some(p.tracks[p.track_index(r).ok_or_else(|| anyhow!("no track {r}"))?].id),
                    None => None,
                };
                let id = p.comments.iter().map(|c| c.id).max().unwrap_or(0) + 1;
                p.comments.push(Comment {
                    id,
                    bar: *bar,
                    beat: *beat,
                    to_bar: to_bar.filter(|b| *b > *bar),
                    track,
                    text: text.trim().to_string(),
                    author: source,
                    resolved: false,
                    replies: vec![],
                });
                p.comments.sort_by(|a, b| (a.bar, a.beat).partial_cmp(&(b.bar, b.beat)).unwrap());
                (format!("comment #{id} at bar {bar}: {}", clip(text, 60)), json!({"comment_id": id}))
            }
            CommentOp::Reply { id, text } => {
                let i = find(p, *id)?;
                p.comments[i].replies.push(Reply { author: source, text: text.trim().to_string() });
                (format!("reply to comment #{id}: {}", clip(text, 60)), json!({"ok": true}))
            }
            CommentOp::Resolve { id, reply } => {
                let i = find(p, *id)?;
                let c = &mut p.comments[i];
                if let Some(r) = reply.as_ref().filter(|r| !r.trim().is_empty()) {
                    c.replies.push(Reply { author: source, text: r.trim().to_string() });
                }
                c.resolved = true;
                (format!("resolved comment #{id}"), json!({"ok": true}))
            }
            CommentOp::Reopen { id } => {
                let i = find(p, *id)?;
                p.comments[i].resolved = false;
                (format!("reopened comment #{id}"), json!({"ok": true}))
            }
            CommentOp::Edit { id, text } => {
                let i = find(p, *id)?;
                p.comments[i].text = text.trim().to_string();
                (format!("edited comment #{id}"), json!({"ok": true}))
            }
            CommentOp::Remove { id } => {
                let i = find(p, *id)?;
                p.comments.remove(i);
                (format!("removed comment #{id}"), json!({"ok": true}))
            }
        };
        self.comments_rev += 1;
        self.note(source, summary);
        Ok(out)
    }

    pub fn undo(&mut self, source: Source) -> bool {
        if let Some(mut prev) = self.undo.pop() {
            prev.comments = std::mem::take(&mut self.project.comments);
            self.redo.push(std::mem::replace(&mut self.project, prev));
            self.revision += 1;
            self.note(source, "undo".into());
            true
        } else {
            false
        }
    }

    pub fn redo(&mut self, source: Source) -> bool {
        if let Some(mut next) = self.redo.pop() {
            next.comments = std::mem::take(&mut self.project.comments);
            self.undo.push(std::mem::replace(&mut self.project, next));
            self.revision += 1;
            self.note(source, "redo".into());
            true
        } else {
            false
        }
    }

    /// Replace the whole project (open / import / new), keeping history so it can be undone.
    pub fn replace(&mut self, project: Project, source: Source, why: &str) {
        self.undo.push(std::mem::replace(&mut self.project, project));
        self.redo.clear();
        self.revision += 1;
        self.saved = (self.revision, self.comments_rev);
        self.note(source, why.to_string());
    }

    /// Edits or comment changes not written to the project file yet (a freshly opened or new project is clean).
    pub fn is_dirty(&self) -> bool {
        (self.revision, self.comments_rev) != self.saved
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
}

fn clip(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}
