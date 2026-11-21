//! Finding mods on disk, reading ModInfo.xml, enabling and disabling.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::Serialize;
use walkdir::WalkDir;

use crate::store::{Instance, NexusLink};
use crate::util::{find_child_ci, io_msg, is_safe_name, modified_secs, OLD_SUFFIX, R};

pub struct Root {
    pub key: &'static str,
    pub enabled: PathBuf,
    pub disabled: PathBuf,
}

/// The game's per-user data folder (%APPDATA%\7DaysToDie).
pub fn user_data_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("7DaysToDie"))
}

/// Disabled mods sit in a sibling folder the game never reads.
pub fn roots(inst: &Instance) -> Vec<Root> {
    let base = PathBuf::from(&inst.path);
    let mut out = vec![Root { key: "game", enabled: base.join("Mods"), disabled: base.join("Mods_Disabled") }];
    if inst.kind == "client" {
        if let Some(user) = user_data_dir() {
            out.push(Root { key: "user", enabled: user.join("Mods"), disabled: user.join("Mods_Disabled") });
        }
    }
    out
}

#[derive(Default, Debug, Clone, PartialEq)]
pub struct ModInfo {
    pub name: String,
    pub display_name: String,
    pub version: String,
    pub author: String,
    pub description: String,
    pub website: String,
    pub legacy: bool,
    pub malformed: bool,
}

pub fn parse_modinfo(bytes: &[u8]) -> ModInfo {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim_start_matches('\u{feff}').trim_start();
    let mut info = ModInfo::default();
    match roxmltree::Document::parse(text) {
        Ok(doc) => {
            for node in doc.descendants().filter(|n| n.is_element()) {
                let tag = node.tag_name().name().to_ascii_lowercase();
                if tag == "modinfo" {
                    info.legacy = true;
                }
                let value = node.attribute("value").unwrap_or("").to_string();
                match tag.as_str() {
                    "name" if info.name.is_empty() => info.name = value,
                    "displayname" if info.display_name.is_empty() => info.display_name = value,
                    "version" if info.version.is_empty() => info.version = value,
                    "author" if info.author.is_empty() => info.author = value,
                    "description" if info.description.is_empty() => info.description = value,
                    "website" if info.website.is_empty() => info.website = value,
                    _ => {}
                }
            }
        }
        Err(_) => {
            // Broken XML is common in hand-edited mods; salvage what we can.
            info.malformed = true;
            let grab = |tag: &str| {
                Regex::new(&format!(r#"(?i)<{tag}\s+value\s*=\s*"([^"]*)""#))
                    .ok()
                    .and_then(|re| re.captures(text).map(|c| c[1].to_string()))
                    .unwrap_or_default()
            };
            info.name = grab("Name");
            info.display_name = grab("DisplayName");
            info.version = grab("Version");
            info.author = grab("Author");
            info.description = grab("Description");
            info.website = grab("Website");
        }
    }
    info
}

#[derive(Serialize, Clone, Debug)]
pub struct ModEntry {
    pub id: String,
    pub root: String,
    pub folder: String,
    pub enabled: bool,
    pub path: String,
    pub name: String,
    pub title: String,
    pub version: String,
    pub author: String,
    pub description: String,
    pub website: String,
    pub legacy: bool,
    pub malformed: bool,
    pub has_dll: bool,
    pub core: bool,
    pub size: u64,
    pub files: u64,
    pub modified: i64,
    /// None, "missing" (no ModInfo.xml) or "nested" (mod is one folder too deep).
    pub problem: Option<String>,
    pub nested: Option<String>,
    pub load_index: Option<usize>,
    pub nexus: Option<NexusLink>,
    pub update: bool,
}

/// The vanilla mods the game ships with, such as 0_TFP_Harmony.
pub fn is_core(folder: &str) -> bool {
    folder.to_ascii_lowercase().starts_with("0_tfp_")
}

pub fn version_newer(latest: &str, current: &str) -> bool {
    fn nums(v: &str) -> Vec<u64> {
        v.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty()).filter_map(|s| s.parse().ok()).collect()
    }
    let (a, b) = (nums(latest), nums(current));
    if a.is_empty() || b.is_empty() {
        return latest.trim() != current.trim();
    }
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

/// "1.0" and "1.0.0" are the same version.
pub fn same_version(a: &str, b: &str) -> bool {
    !version_newer(a, b) && !version_newer(b, a)
}

fn nested_mod(dir: &Path) -> Option<String> {
    fs::read_dir(dir).ok()?.flatten().find_map(|e| {
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        (is_dir && find_child_ci(&e.path(), "ModInfo.xml").is_some())
            .then(|| e.file_name().to_string_lossy().to_string())
    })
}

fn read_entry(root: &str, enabled: bool, path: &Path, links: &HashMap<String, NexusLink>) -> ModEntry {
    let folder = path.file_name().unwrap_or_default().to_string_lossy().to_string();
    let (info, problem, nested) = match find_child_ci(path, "ModInfo.xml") {
        Some(file) => (parse_modinfo(&fs::read(file).unwrap_or_default()), None, None),
        None => match nested_mod(path) {
            Some(child) => (ModInfo::default(), Some("nested".to_string()), Some(child)),
            None => (ModInfo::default(), Some("missing".to_string()), None),
        },
    };
    let (mut size, mut files, mut has_dll) = (0u64, 0u64, false);
    for entry in WalkDir::new(path).into_iter().flatten() {
        if entry.file_type().is_file() {
            files += 1;
            size += entry.metadata().map(|m| m.len()).unwrap_or(0);
            has_dll |= entry.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("dll"));
        }
    }
    let modified = fs::metadata(path).map(|m| modified_secs(&m)).unwrap_or(0);
    let title = [&info.display_name, &info.name, &folder].iter().find(|s| !s.is_empty()).map(|s| s.to_string()).unwrap();
    let nexus = links.get(&folder).cloned();
    // Set by the update check, which works from the installed Nexus file, not ModInfo's version.
    let update = nexus.as_ref().is_some_and(|l| l.status.as_deref() == Some("update"));
    ModEntry {
        id: format!("{root}:{}:{folder}", if enabled { "on" } else { "off" }),
        root: root.to_string(),
        enabled,
        path: path.to_string_lossy().to_string(),
        name: if info.name.is_empty() { folder.clone() } else { info.name },
        title,
        version: info.version,
        author: info.author,
        description: info.description,
        website: info.website,
        legacy: info.legacy,
        malformed: info.malformed,
        has_dll,
        core: is_core(&folder),
        size,
        files,
        modified,
        problem,
        nested,
        load_index: None,
        nexus,
        update,
        folder,
    }
}

pub fn scan(inst: &Instance, links: &HashMap<String, NexusLink>) -> Vec<ModEntry> {
    let mut out = Vec::new();
    for root in roots(inst) {
        for (dir, enabled) in [(&root.enabled, true), (&root.disabled, false)] {
            let Ok(children) = fs::read_dir(dir) else { continue };
            for child in children.flatten() {
                let name = child.file_name().to_string_lossy().to_string();
                if child.file_type().map(|t| t.is_dir()).unwrap_or(false) && !name.ends_with(OLD_SUFFIX) {
                    out.push(read_entry(root.key, enabled, &child.path(), links));
                }
            }
        }
    }
    out.sort_by_key(|m| m.folder.to_lowercase());
    // The game loads mods in folder-name order.
    let mut position = 0;
    for m in out.iter_mut().filter(|m| m.enabled && m.problem.is_none()) {
        m.load_index = Some(position);
        position += 1;
    }
    out
}

pub fn find<'a>(mods: &'a [ModEntry], id: &str) -> R<&'a ModEntry> {
    mods.iter().find(|m| m.id == id).ok_or_else(|| "That mod is no longer on disk. Refresh the list.".to_string())
}

fn state_dir(inst: &Instance, root: &str, enabled: bool) -> R<PathBuf> {
    let r = roots(inst).into_iter().find(|r| r.key == root).ok_or("Unknown mod location.")?;
    Ok(if enabled { r.enabled } else { r.disabled })
}

pub fn set_enabled(inst: &Instance, m: &ModEntry, enabled: bool) -> R<()> {
    if m.enabled == enabled {
        return Ok(());
    }
    if m.core && !enabled {
        return Err(format!("{} ships with the game and has to stay enabled.", m.title));
    }
    if !is_safe_name(&m.folder) {
        return Err("Invalid mod folder name.".into());
    }
    let dir = state_dir(inst, &m.root, enabled)?;
    let dst = dir.join(&m.folder);
    if dst.exists() {
        return Err(format!("A folder named {} already exists in {}.", m.folder, dir.display()));
    }
    fs::create_dir_all(&dir).map_err(|e| io_msg("Could not create folder", e))?;
    fs::rename(&m.path, &dst).map_err(|e| io_msg(&format!("Could not move {}", m.title), e))
}

/// Lifts a mod out of an extra wrapper folder, removing the wrapper if empty.
pub fn fix_nested(m: &ModEntry) -> R<()> {
    let child = m.nested.as_ref().ok_or("This mod is not nested.")?;
    let wrapper = PathBuf::from(&m.path);
    let dst = wrapper.parent().ok_or("Invalid path.")?.join(child);
    if dst.exists() && !dst.eq_ignore_ascii_case_path(&wrapper) {
        return Err(format!("A folder named {child} already exists."));
    }
    let source = wrapper.join(child);
    if dst.eq_ignore_ascii_case_path(&wrapper) {
        // Same name inside and out: step through a temporary name.
        let temp = wrapper.with_extension("7dmm_fix");
        fs::rename(&source, &temp).map_err(|e| io_msg("Could not move mod", e))?;
        fs::remove_dir_all(&wrapper).map_err(|e| io_msg("Could not remove wrapper folder", e))?;
        return fs::rename(&temp, &wrapper).map_err(|e| io_msg("Could not move mod", e));
    }
    fs::rename(&source, &dst).map_err(|e| io_msg("Could not move mod", e))?;
    let _ = fs::remove_dir(&wrapper);
    Ok(())
}

trait PathEqCi {
    fn eq_ignore_ascii_case_path(&self, other: &Path) -> bool;
}

impl PathEqCi for PathBuf {
    fn eq_ignore_ascii_case_path(&self, other: &Path) -> bool {
        self.to_string_lossy().eq_ignore_ascii_case(&other.to_string_lossy())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_v2_modinfo() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8" ?>
        <xml><Name value="Fast" /><DisplayName value="Fast Thing" /><Version value="1.2.0" /><Author value="Me" /></xml>"#;
        let i = parse_modinfo(xml);
        assert_eq!(i.name, "Fast");
        assert_eq!(i.display_name, "Fast Thing");
        assert_eq!(i.version, "1.2.0");
        assert!(!i.legacy && !i.malformed);
    }

    #[test]
    fn flags_legacy_format_and_handles_bom() {
        let xml = "\u{feff}<xml><ModInfo><Name value=\"Old\"/><Version value=\"2\"/></ModInfo></xml>";
        let i = parse_modinfo(xml.as_bytes());
        assert_eq!(i.name, "Old");
        assert!(i.legacy);
    }

    #[test]
    fn salvages_broken_xml() {
        let i = parse_modinfo(b"<xml><Name value=\"Bad\"><Version value=\"3\"/>");
        assert!(i.malformed);
        assert_eq!(i.name, "Bad");
        assert_eq!(i.version, "3");
    }

    #[test]
    fn compares_versions() {
        assert!(version_newer("1.10.0", "1.9"));
        assert!(!version_newer("1.0", "1.0.0"));
        assert!(!version_newer("1.0", "1.2"));
        assert!(version_newer("beta", "alpha"));
    }
}
