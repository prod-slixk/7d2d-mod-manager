//! Two-step install: unpack into staging, then commit only what the user approves.

use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::Serialize;

use crate::backups;
use crate::mods::{self, is_core, parse_modinfo, ModEntry};
use crate::store::{App, Instance, NexusLink};
use crate::util::{copy_dir, extract_archive, find_child_ci, io_msg, replace_dir, sanitize_folder, R};

#[derive(Serialize, Clone, Debug)]
pub struct Candidate {
    pub id: String,
    pub src: PathBuf,
    pub folder: String,
    pub title: String,
    pub version: String,
    pub author: String,
    pub description: String,
    pub has_dll: bool,
    pub exists: bool,
    pub existing_version: Option<String>,
    pub origin: String,
    pub nexus_id: Option<u64>,
}

#[derive(Serialize, Clone, Debug)]
pub struct Manifest {
    pub name: String,
    pub mods: Vec<crate::store::ModRef>,
}

#[derive(Serialize, Clone, Debug)]
pub struct Staging {
    pub id: String,
    pub dir: PathBuf,
    pub candidates: Vec<Candidate>,
    pub errors: Vec<String>,
    pub nexus: Option<NexusLink>,
    pub import: Option<Manifest>,
}

/// Finds folders holding a ModInfo.xml, without looking inside a mod once found.
pub fn find_mod_dirs(dir: &Path, out: &mut Vec<PathBuf>) {
    if find_child_ci(dir, "ModInfo.xml").is_some() {
        out.push(dir.to_path_buf());
        return;
    }
    let Ok(children) = fs::read_dir(dir) else { return };
    let mut subdirs: Vec<PathBuf> =
        children.flatten().filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false)).map(|e| e.path()).collect();
    subdirs.sort();
    for sub in subdirs {
        find_mod_dirs(&sub, out);
    }
}

/// Reads the Nexus mod id from names like `Cool Mod-1234-1-0-1700000000.zip`.
pub fn nexus_id_from_filename(name: &str) -> Option<u64> {
    let re = Regex::new(r"-(\d+)-[\dv._-]*?-?\d{9,}\.[A-Za-z0-9]+$").unwrap();
    re.captures(name).and_then(|c| c[1].parse().ok())
}

pub fn candidate_from(dir: &Path, unpack_root: &Path, origin: &str, stem: &str, existing: &[ModEntry], nexus_id: Option<u64>) -> Candidate {
    let info = find_child_ci(dir, "ModInfo.xml").map(|p| parse_modinfo(&fs::read(p).unwrap_or_default())).unwrap_or_default();
    let raw_folder = if dir == unpack_root {
        // Files sit at the archive root, so the folder name has to be invented.
        if info.name.is_empty() { stem.to_string() } else { info.name.clone() }
    } else {
        dir.file_name().unwrap_or_default().to_string_lossy().to_string()
    };
    let folder = sanitize_folder(&raw_folder);
    let old = existing.iter().find(|m| m.folder.eq_ignore_ascii_case(&folder));
    let has_dll = walkdir::WalkDir::new(dir).into_iter().flatten().any(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("dll")));
    let title = [&info.display_name, &info.name, &folder].iter().find(|s| !s.is_empty()).map(|s| s.to_string()).unwrap();
    Candidate {
        id: uuid::Uuid::new_v4().to_string(),
        src: dir.to_path_buf(),
        exists: old.is_some(),
        existing_version: old.map(|m| m.version.clone()),
        folder,
        title,
        version: info.version,
        author: info.author,
        description: info.description,
        has_dll,
        origin: origin.to_string(),
        nexus_id,
    }
}

/// Gives repeated folder names a numeric suffix so one doesn't overwrite another.
pub fn dedupe(candidates: &mut [Candidate]) {
    let mut seen: Vec<String> = Vec::new();
    for c in candidates.iter_mut() {
        let base = c.folder.clone();
        let mut n = 2;
        while seen.contains(&c.folder.to_lowercase()) {
            c.folder = format!("{base}_{n}");
            n += 1;
        }
        seen.push(c.folder.to_lowercase());
    }
}

pub fn prepare(app: &App, existing: &[ModEntry], paths: &[String], nexus: Option<NexusLink>, progress: &dyn Fn(&str)) -> R<Staging> {
    let id = uuid::Uuid::new_v4().to_string();
    let dir = app.dir.join("staging").join(&id);
    let (mut candidates, mut errors) = (Vec::new(), Vec::new());

    for (i, raw) in paths.iter().enumerate() {
        let path = Path::new(raw);
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        let stem = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
        progress(&format!("Unpacking {name}"));
        let out = dir.join(i.to_string());
        let unpacked = if path.is_dir() {
            copy_dir(path, &out).map_err(|e| io_msg("Could not read folder", e))
        } else {
            extract_archive(path, &out)
        };
        if let Err(e) = unpacked {
            errors.push(format!("{name}: {e}"));
            continue;
        }
        let mut found = Vec::new();
        find_mod_dirs(&out, &mut found);
        if found.is_empty() {
            errors.push(format!("{name}: no ModInfo.xml inside, so this isn't an installable mod."));
        }
        let nexus_id = nexus.as_ref().map(|n| n.mod_id).or_else(|| nexus_id_from_filename(&name));
        for m in found {
            // A folder dropped directly is its own root, but already has a good name.
            let root = if path.is_dir() && m == out { PathBuf::new() } else { out.clone() };
            let mut c = candidate_from(&m, &root, &name, &stem, existing, nexus_id);
            if path.is_dir() && m == out {
                c.folder = sanitize_folder(&name);
                let old = existing.iter().find(|e| e.folder.eq_ignore_ascii_case(&c.folder));
                c.exists = old.is_some();
                c.existing_version = old.map(|o| o.version.clone());
            }
            candidates.push(c);
        }
    }
    dedupe(&mut candidates);
    if candidates.is_empty() {
        let _ = fs::remove_dir_all(&dir);
        return Err(if errors.is_empty() { "Nothing to install.".to_string() } else { errors.join("\n") });
    }
    Ok(Staging { id, dir, candidates, errors, nexus, import: None })
}

#[derive(Serialize, Debug, Default)]
pub struct CommitResult {
    pub installed: Vec<String>,
    pub failed: Vec<String>,
    pub profile: Option<String>,
    pub missing: Vec<crate::store::ModRef>,
}

pub fn commit(app: &App, inst: &Instance, staging: &Staging, selected: &[String]) -> R<CommitResult> {
    let mut result = CommitResult::default();
    let links = app.links_for(&inst.id);
    let existing = mods::scan(inst, &links);
    let game_mods = mods::roots(inst)[0].enabled.clone();

    for c in staging.candidates.iter().filter(|c| selected.contains(&c.id)) {
        if is_core(&c.folder) {
            result.failed.push(format!("{}: can't replace a mod that ships with the game.", c.title));
            continue;
        }
        let old = existing.iter().find(|m| m.folder.eq_ignore_ascii_case(&c.folder));
        // Updates stay where the old copy was, including if it was disabled.
        let target = old.map(|m| PathBuf::from(&m.path)).unwrap_or_else(|| game_mods.join(&c.folder));
        if let Some(m) = old {
            if let Err(e) = backups::snapshot_mod(app, inst, m) {
                result.failed.push(format!("{}: backup failed, so nothing was replaced. {e}", c.title));
                continue;
            }
        }
        match replace_dir(&c.src, &target) {
            Ok(()) => {
                result.installed.push(c.title.clone());
                if let Some(mod_id) = c.nexus_id {
                    let from_nexus = staging.nexus.clone().filter(|n| n.mod_id == mod_id);
                    let link = NexusLink {
                        mod_id,
                        file_id: from_nexus.as_ref().and_then(|n| n.file_id),
                        version: from_nexus.as_ref().and_then(|n| n.version.clone()).or_else(|| Some(c.version.clone())),
                        latest: from_nexus.as_ref().and_then(|n| n.latest.clone()),
                        title: from_nexus.and_then(|n| n.title),
                    };
                    let (inst_id, folder) = (inst.id.clone(), c.folder.clone());
                    app.write(|d| {
                        d.links.entry(inst_id).or_default().insert(folder, link);
                    })?;
                }
            }
            Err(e) => result.failed.push(io_msg(&c.title, e)),
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_every_mod_but_not_inside_one() {
        let t = tempfile::tempdir().unwrap();
        for p in ["Pack/ModA", "Pack/Optional/ModB", "Pack/ModA/Inner"] {
            fs::create_dir_all(t.path().join(p)).unwrap();
            fs::write(t.path().join(p).join("modinfo.XML"), "<xml/>").unwrap();
        }
        let mut found = Vec::new();
        find_mod_dirs(t.path(), &mut found);
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn reads_nexus_id_from_filenames() {
        assert_eq!(nexus_id_from_filename("Cool Mod-1234-1-0-1700000000.zip"), Some(1234));
        assert_eq!(nexus_id_from_filename("Cool-77-v2-1-1700000000.7z"), Some(77));
        assert_eq!(nexus_id_from_filename("MyMod.zip"), None);
        assert_eq!(nexus_id_from_filename("Mod-v1-2.zip"), None);
    }
}
