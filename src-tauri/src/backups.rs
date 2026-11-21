//! Zip snapshots of mods and saves, with restore.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::mods::{self, ModEntry};
use crate::store::{App, Instance};
use crate::util::{extract_zip, io_msg, is_safe_name, move_dir, replace_dir, write_atomic, zip_add_dir, zip_create, R};

/// Automatic single-mod backups kept per game folder.
const AUTO_KEEP: usize = 20;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Backup {
    pub id: String,
    pub label: String,
    /// "full" (every mod folder), "mod" (only the folders inside) or "save" (one world save).
    pub kind: String,
    pub auto: bool,
    pub created: i64,
    pub size: u64,
    pub has_mods: bool,
    pub has_saves: bool,
    /// For kind "save": the save's path under Saves, as "World/Game".
    #[serde(default)]
    pub save: Option<String>,
}

fn dir_for(app: &App, inst: &Instance) -> PathBuf {
    app.dir.join("backups").join(&inst.id)
}

fn new_id() -> String {
    format!("{}_{}", chrono::Local::now().format("%Y%m%d-%H%M%S"), &uuid::Uuid::new_v4().to_string()[..6])
}

fn finish(app: &App, inst: &Instance, tmp: &Path, mut meta: Backup) -> R<Backup> {
    let dir = dir_for(app, inst);
    let zip_path = dir.join(format!("{}.zip", meta.id));
    fs::rename(tmp, &zip_path).map_err(|e| io_msg("Could not save backup", e))?;
    meta.size = fs::metadata(&zip_path).map(|m| m.len()).unwrap_or(0);
    let json = serde_json::to_vec_pretty(&meta).map_err(|e| e.to_string())?;
    write_atomic(&dir.join(format!("{}.json", meta.id)), &json)?;
    Ok(meta)
}

pub fn list(app: &App, inst: &Instance) -> Vec<Backup> {
    let mut out: Vec<Backup> = fs::read_dir(dir_for(app, inst))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| serde_json::from_slice(&fs::read(e.path()).ok()?).ok())
        .collect();
    out.sort_by(|a: &Backup, b: &Backup| b.created.cmp(&a.created).then(b.id.cmp(&a.id)));
    out
}

pub fn exists(app: &App, inst: &Instance, id: &str) -> bool {
    is_safe_name(id) && dir_for(app, inst).join(format!("{id}.zip")).is_file()
}

pub fn delete(app: &App, inst: &Instance, id: &str) -> R<()> {
    if !is_safe_name(id) {
        return Err("Invalid backup id.".into());
    }
    let dir = dir_for(app, inst);
    let _ = fs::remove_file(dir.join(format!("{id}.zip")));
    fs::remove_file(dir.join(format!("{id}.json"))).map_err(|e| io_msg("Could not delete backup", e))
}

/// Only single-mod backups rotate. World backups are managed by play mode tracking.
fn prune_auto(app: &App, inst: &Instance) {
    for old in list(app, inst).into_iter().filter(|b| b.auto && b.kind == "mod").skip(AUTO_KEEP) {
        let _ = delete(app, inst, &old.id);
    }
}

fn section(root: &str, enabled: bool) -> String {
    format!("{root}/{}", if enabled { "enabled" } else { "disabled" })
}

/// Saves one mod before it gets overwritten or deleted.
pub fn snapshot_mod(app: &App, inst: &Instance, m: &ModEntry) -> R<Backup> {
    let id = new_id();
    let tmp = dir_for(app, inst).join(format!("{id}.partial"));
    let mut zip = zip_create(&tmp)?;
    zip_add_dir(&mut zip, Path::new(&m.path), &format!("{}/{}", section(&m.root, m.enabled), m.folder))?;
    zip.finish().map_err(|e| io_msg("Could not finish backup", e))?;
    let meta = Backup {
        id,
        label: format!("{} {}", m.title, m.version).trim().to_string(),
        kind: "mod".into(),
        auto: true,
        created: chrono::Utc::now().timestamp(),
        size: 0,
        has_mods: true,
        has_saves: false,
        save: None,
    };
    let out = finish(app, inst, &tmp, meta)?;
    prune_auto(app, inst);
    Ok(out)
}

/// Saves one world. `rel` is "World/Game" under the Saves folder.
pub fn snapshot_save(app: &App, inst: &Instance, saves_root: &Path, rel: &str, label: &str) -> R<Backup> {
    let src = crate::playmode::save_path(saves_root, rel)?;
    let id = new_id();
    let tmp = dir_for(app, inst).join(format!("{id}.partial"));
    let build = || -> R<()> {
        let mut zip = zip_create(&tmp)?;
        zip_add_dir(&mut zip, &src, &format!("saves/{rel}"))?;
        zip.finish().map_err(|e| io_msg("Could not finish backup", e))?;
        Ok(())
    };
    if let Err(e) = build() {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    let meta = Backup {
        id,
        label: label.to_string(),
        kind: "save".into(),
        auto: true,
        created: chrono::Utc::now().timestamp(),
        size: 0,
        has_mods: false,
        has_saves: true,
        save: Some(rel.to_string()),
    };
    finish(app, inst, &tmp, meta)
}

pub fn create(app: &App, inst: &Instance, label: &str, with_mods: bool, with_saves: bool, auto: bool, progress: &dyn Fn(&str)) -> R<Backup> {
    let id = new_id();
    let tmp = dir_for(app, inst).join(format!("{id}.partial"));
    let build = || -> R<()> {
        let mut zip = zip_create(&tmp)?;
        if with_mods {
            for root in mods::roots(inst) {
                for (dir, on) in [(&root.enabled, true), (&root.disabled, false)] {
                    if dir.is_dir() {
                        progress(&format!("Backing up {}", dir.display()));
                        zip_add_dir(&mut zip, dir, &section(root.key, on))?;
                    }
                }
            }
        }
        if with_saves {
            if let Some(saves) = app.saves.as_ref().filter(|s| s.is_dir()) {
                progress("Backing up saves");
                zip_add_dir(&mut zip, saves, "saves")?;
            }
        }
        zip.finish().map_err(|e| io_msg("Could not finish backup", e))?;
        Ok(())
    };
    if let Err(e) = build() {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    let meta = Backup {
        id,
        label: label.to_string(),
        kind: "full".into(),
        auto,
        created: chrono::Utc::now().timestamp(),
        size: 0,
        has_mods: with_mods,
        has_saves: with_saves,
        save: None,
    };
    finish(app, inst, &tmp, meta)
}

fn replace_or_clear(staged: &Path, target: &Path) -> R<()> {
    if staged.is_dir() {
        replace_dir(staged, target).map_err(|e| io_msg("Could not restore folder", e))
    } else if target.is_dir() {
        fs::remove_dir_all(target).map_err(|e| io_msg("Could not clear folder", e))
    } else {
        Ok(())
    }
}

pub fn restore(app: &App, inst: &Instance, id: &str, progress: &dyn Fn(&str)) -> R<()> {
    if !is_safe_name(id) {
        return Err("Invalid backup id.".into());
    }
    let meta = list(app, inst).into_iter().find(|b| b.id == id).ok_or("That backup no longer exists.")?;
    let saves_root = app.saves.clone();

    // A restore replaces live files, so make it undoable first.
    if meta.kind == "save" {
        let rel = meta.save.as_deref().ok_or("This world backup is missing its save name.")?;
        let root = saves_root.as_deref().ok_or("The game's Saves folder couldn't be found.")?;
        if crate::playmode::save_path(root, rel)?.is_dir() {
            progress("Backing up the current copy first");
            snapshot_save(app, inst, root, rel, &format!("Before restoring: {}", meta.label))?;
        }
    } else {
        create(app, inst, &format!("Before restoring: {}", meta.label), meta.has_mods, meta.has_saves, true, progress)?;
    }

    let stage = app.dir.join("staging").join(format!("restore_{id}"));
    let _ = fs::remove_dir_all(&stage);
    progress("Unpacking backup");
    extract_zip(&dir_for(app, inst).join(format!("{id}.zip")), &stage)?;

    let result = (|| -> R<()> {
        if meta.kind == "save" {
            let rel = meta.save.as_deref().unwrap_or_default();
            let root = saves_root.as_deref().ok_or("The game's Saves folder couldn't be found.")?;
            let staged = rel.split('/').fold(stage.join("saves"), |p, part| p.join(part));
            progress("Restoring world");
            return replace_dir(&staged, &crate::playmode::save_path(root, rel)?).map_err(|e| io_msg("Could not restore world", e));
        }
        if meta.has_mods {
            for root in mods::roots(inst) {
                for (dir, on) in [(&root.enabled, true), (&root.disabled, false)] {
                    let staged = stage.join(section(root.key, on));
                    if meta.kind == "full" {
                        progress(&format!("Restoring {}", dir.display()));
                        replace_or_clear(&staged, dir)?;
                    } else if staged.is_dir() {
                        for child in fs::read_dir(&staged).map_err(|e| io_msg("Could not read backup", e))?.flatten() {
                            let target = dir.join(child.file_name());
                            let other = if on { &root.disabled } else { &root.enabled }.join(child.file_name());
                            // The mod may have been toggled since this backup was taken.
                            if other.is_dir() {
                                fs::remove_dir_all(&other).map_err(|e| io_msg("Could not clear old copy", e))?;
                            }
                            if target.exists() {
                                replace_dir(&child.path(), &target).map_err(|e| io_msg("Could not restore mod", e))?;
                            } else {
                                move_dir(&child.path(), &target).map_err(|e| io_msg("Could not restore mod", e))?;
                            }
                        }
                    }
                }
            }
        }
        if meta.has_saves {
            if let Some(saves) = &saves_root {
                progress("Restoring saves");
                replace_or_clear(&stage.join("saves"), saves)?;
            }
        }
        Ok(())
    })();
    let _ = fs::remove_dir_all(&stage);
    result
}

pub fn open_dir(app: &App, inst: &Instance) -> PathBuf {
    let d = dir_for(app, inst);
    let _ = fs::create_dir_all(&d);
    d
}
