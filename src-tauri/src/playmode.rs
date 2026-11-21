//! Vanilla and modded play: switching mods, and keeping each world in its own mode.
//!
//! The game only reads mods at startup and can't say which world it loaded, so
//! a session is watched from outside: whichever saves change while the game runs
//! were played in the mode the mods were in at launch.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::backups;
use crate::mods::ModEntry;
use crate::profiles;
use crate::store::{App, Instance, Mismatch, ModRef, SaveTag, Session};
use crate::util::{is_safe_name, R};

pub const VANILLA: &str = "vanilla";
pub const MODDED: &str = "modded";

/// A session that never shows a running game is dropped after this long.
const SESSION_TIMEOUT: i64 = 600;

pub fn current_mode(mods: &[ModEntry]) -> &'static str {
    if mods.iter().any(|m| m.enabled && !m.core && m.problem.is_none()) {
        MODDED
    } else {
        VANILLA
    }
}

fn label(mode: &str) -> &'static str {
    if mode == VANILLA {
        "Vanilla"
    } else {
        "Modded"
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct SaveEntry {
    pub rel: String,
    pub world: String,
    pub game: String,
    pub played: i64,
}

/// Resolves "World/Game" safely under the Saves folder.
pub fn save_path(root: &Path, rel: &str) -> R<PathBuf> {
    let parts: Vec<&str> = rel.split('/').collect();
    if parts.len() != 2 || !parts.iter().all(|p| is_safe_name(p)) {
        return Err("Invalid save name.".into());
    }
    Ok(root.join(parts[0]).join(parts[1]))
}

fn modified_ms(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Last time the game wrote to a save, in milliseconds. Top-level files such as main.ttw change on every save.
fn last_played(dir: &Path) -> i64 {
    let newest = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.metadata().ok().filter(|m| m.is_file()).map(|m| modified_ms(&m)))
        .max();
    newest.unwrap_or_else(|| fs::metadata(dir).map(|m| modified_ms(&m)).unwrap_or(0))
}

fn subdirs(dir: &Path) -> Vec<(String, PathBuf)> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| (e.file_name().to_string_lossy().to_string(), e.path()))
        .collect()
}

/// Every save as Saves/World/Game, most recently played first.
pub fn list_saves(root: &Path) -> Vec<SaveEntry> {
    let mut out: Vec<SaveEntry> = subdirs(root)
        .into_iter()
        .flat_map(|(world, wpath)| {
            subdirs(&wpath).into_iter().map(move |(game, gpath)| SaveEntry {
                rel: format!("{world}/{game}"),
                world: world.clone(),
                played: last_played(&gpath),
                game,
            })
        })
        .collect();
    out.sort_by(|a, b| b.played.cmp(&a.played).then(a.rel.cmp(&b.rel)));
    out
}

#[derive(Serialize, Debug)]
pub struct SwitchResult {
    pub mode: String,
    pub enabled: usize,
    pub disabled: usize,
    pub missing: Vec<ModRef>,
}

pub fn switch(app: &App, inst: &Instance, mods: &[ModEntry], mode: &str) -> R<SwitchResult> {
    let target: Vec<ModRef> = match mode {
        VANILLA => {
            let current = profiles::snapshot(mods);
            // Remember what was on, so Modded can bring it back.
            if !current.is_empty() {
                let id = inst.id.clone();
                app.write(|d| d.modded_set.insert(id, current))?;
            }
            Vec::new()
        }
        MODDED => app
            .read(|d| d.modded_set.get(&inst.id).cloned())
            .filter(|s| !s.is_empty())
            .ok_or("There's no modded setup to switch back to yet. Turn your mods on in the Mods page. The manager remembers them the next time you switch to Vanilla.")?,
        _ => return Err("Unknown play mode.".into()),
    };
    let r = profiles::apply(inst, mods, &profiles::new_profile(label(mode), target))?;
    Ok(SwitchResult { mode: mode.into(), enabled: r.enabled, disabled: r.disabled, missing: r.missing })
}

/// Tags saves changed during `session`. Returns worlds that belonged to the other mode.
pub fn reconcile(tags: &mut HashMap<String, SaveTag>, session: &Session, saves: &[SaveEntry], now: i64) -> Vec<Mismatch> {
    let mut out = Vec::new();
    for e in saves.iter().filter(|e| session.baseline.get(&e.rel) != Some(&e.played)) {
        match tags.get_mut(&e.rel) {
            Some(t) if t.mode != session.mode => out.push(Mismatch {
                save: e.rel.clone(),
                world: e.world.clone(),
                game: e.game.clone(),
                expected: t.mode.clone(),
                played: session.mode.clone(),
                backup: t.backup.clone(),
                when: now,
            }),
            Some(t) => t.played = e.played,
            None => {
                tags.insert(e.rel.clone(), SaveTag { mode: session.mode.clone(), played: e.played, ..Default::default() });
            }
        }
    }
    out
}

pub fn new_session(app: &App, mode: &str, seen_running: bool) -> Session {
    let baseline = app.saves.as_deref().map(list_saves).unwrap_or_default().into_iter().map(|e| (e.rel, e.played)).collect();
    Session { mode: mode.into(), started: chrono::Utc::now().timestamp(), seen_running, baseline }
}

/// Called on every status poll: starts, updates and closes sessions.
pub fn observe(app: &App, inst: &Instance, running: bool, mode: &str) -> R<()> {
    let now = chrono::Utc::now().timestamp();
    let session = app.read(|d| d.sessions.get(&inst.id).cloned());
    match (running, session) {
        (true, Some(s)) if !s.seen_running => app.write(|d| {
            if let Some(s) = d.sessions.get_mut(&inst.id) {
                s.seen_running = true;
            }
        }),
        // Started outside the manager, e.g. from Steam. The mods were set at startup either way.
        (true, None) => {
            let s = new_session(app, mode, true);
            app.write(|d| {
                d.sessions.insert(inst.id.clone(), s);
            })
        }
        (false, Some(s)) if s.seen_running || now - s.started > SESSION_TIMEOUT => {
            let saves = app.saves.as_deref().map(list_saves).unwrap_or_default();
            app.write(|d| {
                d.sessions.remove(&inst.id);
                let found = reconcile(d.save_tags.entry(inst.id.clone()).or_default(), &s, &saves, now);
                let open = d.mismatches.entry(inst.id.clone()).or_default();
                for m in found {
                    open.retain(|o| o.save != m.save);
                    open.push(m);
                }
            })
        }
        _ => Ok(()),
    }
}

/// Before launching in `mode`, backs up every world of the other mode that changed since its last backup.
pub fn protect(app: &App, inst: &Instance, mode: &str, progress: &dyn Fn(&str)) -> R<usize> {
    let Some(root) = app.saves.clone() else { return Ok(0) };
    let (tags, open) = app.read(|d| {
        (
            d.save_tags.get(&inst.id).cloned().unwrap_or_default(),
            d.mismatches.get(&inst.id).map(|v| v.iter().map(|m| m.save.clone()).collect::<Vec<_>>()).unwrap_or_default(),
        )
    });
    let mut count = 0;
    for e in list_saves(&root) {
        let Some(t) = tags.get(&e.rel) else { continue };
        let current = t.backup.as_deref().is_some_and(|b| backups::exists(app, inst, b)) && e.played <= t.backed;
        // A world awaiting a decision may already be damaged, so its good copy is kept as is.
        if t.mode == mode || current || open.contains(&e.rel) {
            continue;
        }
        progress(&format!("Backing up {} before launch", e.game));
        let b = backups::snapshot_save(app, inst, &root, &e.rel, &format!("{} world: {} ({})", label(&t.mode), e.game, e.world))?;
        let rel = e.rel.clone();
        app.write(|d| {
            if let Some(t) = d.save_tags.get_mut(&inst.id).and_then(|m| m.get_mut(&rel)) {
                t.backup = Some(b.id.clone());
                t.backed = e.played;
            }
        })?;
        if let Some(old) = &t.backup {
            let _ = backups::delete(app, inst, old);
        }
        count += 1;
    }
    Ok(count)
}

pub fn set_tag(app: &App, inst: &Instance, rel: &str, mode: Option<&str>) -> R<()> {
    let root = app.saves.clone().ok_or("The game's Saves folder couldn't be found.")?;
    let path = save_path(&root, rel)?;
    if let Some(m) = mode {
        if m != VANILLA && m != MODDED {
            return Err("Unknown play mode.".into());
        }
    }
    let played = last_played(&path);
    app.write(|d| {
        let tags = d.save_tags.entry(inst.id.clone()).or_default();
        match mode {
            Some(m) => {
                let t = tags.entry(rel.to_string()).or_insert_with(|| SaveTag { played, ..Default::default() });
                t.mode = m.to_string();
            }
            None => {
                tags.remove(rel);
            }
        }
        if let Some(open) = d.mismatches.get_mut(&inst.id) {
            open.retain(|o| o.save != rel);
        }
    })
}

/// "restore" puts back the pre-launch backup. "relabel" accepts the world as the mode it was played in.
pub fn resolve(app: &App, inst: &Instance, rel: &str, action: &str, progress: &dyn Fn(&str)) -> R<()> {
    let m = app
        .read(|d| d.mismatches.get(&inst.id).and_then(|v| v.iter().find(|m| m.save == rel).cloned()))
        .ok_or("That warning was already handled.")?;
    match action {
        "restore" => {
            let id = m.backup.clone().filter(|b| backups::exists(app, inst, b)).ok_or("There's no backup of this world to restore.")?;
            backups::restore(app, inst, &id, progress)?;
        }
        "relabel" => {}
        _ => return Err("Unknown action.".into()),
    }
    app.write(|d| {
        if action == "relabel" {
            if let Some(t) = d.save_tags.get_mut(&inst.id).and_then(|t| t.get_mut(rel)) {
                t.mode = m.played.clone();
            }
        }
        if let Some(open) = d.mismatches.get_mut(&inst.id) {
            open.retain(|o| o.save != rel);
        }
    })
}

#[derive(Serialize, Debug)]
pub struct SaveView {
    #[serde(flatten)]
    pub entry: SaveEntry,
    pub mode: Option<String>,
    pub backup: Option<String>,
    pub mismatch: bool,
}

#[derive(Serialize, Debug)]
pub struct PlayInfo {
    pub mode: String,
    pub can_restore_modded: bool,
    pub saves: Vec<SaveView>,
    pub mismatches: Vec<Mismatch>,
    pub saves_dir: Option<String>,
}

pub fn info(app: &App, inst: &Instance, mods: &[ModEntry]) -> PlayInfo {
    let saves = app.saves.as_deref().map(list_saves).unwrap_or_default();
    let (tags, mismatches, has_set) = app.read(|d| {
        (
            d.save_tags.get(&inst.id).cloned().unwrap_or_default(),
            d.mismatches.get(&inst.id).cloned().unwrap_or_default(),
            d.modded_set.get(&inst.id).is_some_and(|s| !s.is_empty()),
        )
    });
    PlayInfo {
        mode: current_mode(mods).into(),
        can_restore_modded: has_set,
        saves: saves
            .into_iter()
            .map(|e| {
                let tag = tags.get(&e.rel);
                SaveView {
                    mode: tag.map(|t| t.mode.clone()),
                    backup: tag.and_then(|t| t.backup.clone()).filter(|b| backups::exists(app, inst, b)),
                    mismatch: mismatches.iter().any(|m| m.save == e.rel),
                    entry: e,
                }
            })
            .collect(),
        mismatches,
        saves_dir: app.saves.as_ref().map(|p| p.to_string_lossy().to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(rel: &str, played: i64) -> SaveEntry {
        let (w, g) = rel.split_once('/').unwrap();
        SaveEntry { rel: rel.into(), world: w.into(), game: g.into(), played }
    }

    #[test]
    fn reconcile_tags_new_saves_and_flags_the_other_mode() {
        let mut tags = HashMap::new();
        tags.insert("W/Vanilla".to_string(), SaveTag { mode: VANILLA.into(), played: 10, ..Default::default() });
        let baseline = [("W/Vanilla".to_string(), 10), ("W/Old".to_string(), 20)].into_iter().collect();
        let s = Session { mode: MODDED.into(), started: 100, seen_running: true, baseline };
        let saves = [entry("W/Vanilla", 150), entry("W/New", 160), entry("W/Old", 20)];
        let found = reconcile(&mut tags, &s, &saves, 200);
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].save.as_str(), found[0].expected.as_str()), ("W/Vanilla", VANILLA));
        assert_eq!(tags["W/New"].mode, MODDED);
        assert!(!tags.contains_key("W/Old"), "saves untouched during the session stay untagged");
        assert_eq!(tags["W/Vanilla"].mode, VANILLA, "a mismatch never silently relabels");
    }

    #[test]
    fn rejects_unsafe_save_names() {
        let root = Path::new("C:\\Saves");
        assert!(save_path(root, "W/G").is_ok());
        assert!(save_path(root, "../G").is_err());
        assert!(save_path(root, "W/G/extra").is_err());
        assert!(save_path(root, "W").is_err());
    }
}
