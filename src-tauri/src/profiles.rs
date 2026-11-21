//! Named mod sets: applying them, and exporting or importing modpacks.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::install::{self, Manifest, Staging};
use crate::mods::{self, ModEntry};
use crate::store::{App, Instance, ModRef, NexusLink, Profile};
use crate::util::{extract_zip, io_msg, zip_add_bytes, zip_add_dir, zip_create, R};

pub fn snapshot(mods: &[ModEntry]) -> Vec<ModRef> {
    mods.iter()
        .filter(|m| m.enabled && !m.core && m.problem.is_none())
        .map(|m| ModRef { folder: m.folder.clone(), name: m.title.clone(), version: m.version.clone(), nexus_id: m.nexus.as_ref().map(|n| n.mod_id) })
        .collect()
}

pub fn new_profile(name: &str, mods: Vec<ModRef>) -> Profile {
    Profile { id: uuid::Uuid::new_v4().to_string(), name: name.to_string(), created: chrono::Utc::now().timestamp(), mods }
}

#[derive(Serialize, Debug, Default)]
pub struct ApplyResult {
    pub enabled: usize,
    pub disabled: usize,
    pub missing: Vec<ModRef>,
}

/// Enables exactly the profile's mods. All moves are undone if any one fails.
pub fn apply(inst: &Instance, mods: &[ModEntry], profile: &Profile) -> R<ApplyResult> {
    let wanted = |m: &ModEntry| profile.mods.iter().any(|r| r.folder.eq_ignore_ascii_case(&m.folder));
    let missing: Vec<ModRef> = profile.mods.iter().filter(|r| !mods.iter().any(|m| m.folder.eq_ignore_ascii_case(&r.folder))).cloned().collect();

    let changes: Vec<(&ModEntry, bool)> = mods
        .iter()
        .filter(|m| !m.core && m.problem.is_none() && wanted(m) != m.enabled)
        .map(|m| (m, !m.enabled))
        .collect();

    let mut done: Vec<(ModEntry, bool)> = Vec::new();
    for (m, enable) in &changes {
        if let Err(e) = mods::set_enabled(inst, m, *enable) {
            let roots = mods::roots(inst);
            for (moved, now_enabled) in done.iter().rev() {
                if let Some(root) = roots.iter().find(|r| r.key == moved.root) {
                    let current = if *now_enabled { &root.enabled } else { &root.disabled }.join(&moved.folder);
                    // `moved.path` still records where the folder was before this run.
                    let _ = fs::rename(current, &moved.path);
                }
            }
            return Err(format!("Profile not applied, and changes were undone. {e}"));
        }
        done.push(((*m).clone(), *enable));
    }
    Ok(ApplyResult {
        enabled: changes.iter().filter(|(_, on)| *on).count(),
        disabled: changes.iter().filter(|(_, on)| !*on).count(),
        missing,
    })
}

#[derive(Serialize, Deserialize, Debug)]
struct PackFile {
    format: u32,
    name: String,
    created: i64,
    mods: Vec<PackMod>,
}

#[derive(Serialize, Deserialize, Debug)]
struct PackMod {
    folder: String,
    name: String,
    version: String,
    nexus_id: Option<u64>,
    /// The Nexus file the mod was installed from, so update checks work after import.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file_id: Option<u64>,
}

pub fn export(profile: &Profile, mods: &[ModEntry], dest: &Path, with_files: bool, progress: &dyn Fn(&str)) -> R<()> {
    let pack = PackFile {
        format: 1,
        name: profile.name.clone(),
        created: chrono::Utc::now().timestamp(),
        mods: profile
            .mods
            .iter()
            .map(|m| {
                let link = mods.iter().find(|e| e.folder.eq_ignore_ascii_case(&m.folder)).and_then(|e| e.nexus.as_ref());
                let file_id = link.filter(|l| Some(l.mod_id) == m.nexus_id).and_then(|l| l.file_id);
                PackMod { folder: m.folder.clone(), name: m.name.clone(), version: m.version.clone(), nexus_id: m.nexus_id, file_id }
            })
            .collect(),
    };
    let tmp = dest.with_extension("partial");
    let build = || -> R<()> {
        let mut zip = zip_create(&tmp)?;
        zip_add_bytes(&mut zip, "manifest.json", &serde_json::to_vec_pretty(&pack).map_err(|e| e.to_string())?)?;
        if with_files {
            for r in &profile.mods {
                if let Some(m) = mods.iter().find(|m| m.folder.eq_ignore_ascii_case(&r.folder)) {
                    progress(&format!("Packing {}", m.title));
                    zip_add_dir(&mut zip, Path::new(&m.path), &format!("mods/{}", m.folder))?;
                }
            }
        }
        zip.finish().map_err(|e| io_msg("Could not finish modpack", e))?;
        Ok(())
    };
    if let Err(e) = build() {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::rename(&tmp, dest).map_err(|e| io_msg("Could not save modpack", e))
}

/// Unpacks a modpack into staging so the user can review it like any install.
pub fn import(app: &App, existing: &[ModEntry], path: &Path, progress: &dyn Fn(&str)) -> R<Staging> {
    let id = uuid::Uuid::new_v4().to_string();
    let dir = app.dir.join("staging").join(&id);
    progress("Unpacking modpack");
    extract_zip(path, &dir)?;
    let inner = || -> R<Staging> {
        let bytes = fs::read(dir.join("manifest.json")).map_err(|_| "This file isn't a 7D2D modpack (no manifest.json).".to_string())?;
        let pack: PackFile = serde_json::from_slice(&bytes).map_err(|e| format!("The modpack manifest is damaged: {e}"))?;
        if pack.format != 1 {
            return Err("This modpack was made by a newer version of the manager.".into());
        }
        let origin = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        let mods_dir = dir.join("mods");
        let mut candidates = Vec::new();
        for child in fs::read_dir(&mods_dir).into_iter().flatten().flatten() {
            let mut found = Vec::new();
            install::find_mod_dirs(&child.path(), &mut found);
            for m in found {
                let entry = pack.mods.iter().find(|p| p.folder.eq_ignore_ascii_case(&m.file_name().unwrap_or_default().to_string_lossy()));
                let nexus_id = entry.and_then(|p| p.nexus_id);
                let mut c = install::candidate_from(&m, &mods_dir, &origin, "Mod", existing, nexus_id);
                if let (Some(mod_id), Some(file_id)) = (nexus_id, entry.and_then(|p| p.file_id)) {
                    c.nexus = Some(NexusLink { mod_id, file_id: Some(file_id), ..Default::default() });
                }
                candidates.push(c);
            }
        }
        install::dedupe(&mut candidates);
        Ok(Staging {
            id: id.clone(),
            dir: dir.clone(),
            candidates,
            errors: Vec::new(),
            import: Some(Manifest {
                name: pack.name,
                mods: pack.mods.into_iter().map(|m| ModRef { folder: m.folder, name: m.name, version: m.version, nexus_id: m.nexus_id }).collect(),
            }),
        })
    };
    let out = inner();
    if out.is_err() {
        let _ = fs::remove_dir_all(&dir);
    }
    out
}
