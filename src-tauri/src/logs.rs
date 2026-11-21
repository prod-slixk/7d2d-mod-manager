//! Finding and reading game logs.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::Serialize;

use crate::mods::{user_data_dir, ModEntry};
use crate::store::Instance;
use crate::util::{io_msg, modified_secs, R};

const MAX_TAIL: u64 = 4 * 1024 * 1024;
const MAX_LINE: usize = 1500;

#[derive(Serialize, Clone, Debug)]
pub struct LogFile {
    pub path: String,
    pub name: String,
    pub size: u64,
    pub modified: i64,
}

fn search_dirs(inst: &Instance) -> Vec<PathBuf> {
    let game = Path::new(&inst.path);
    let mut dirs = vec![game.to_path_buf(), game.join("logs")];
    if inst.kind == "server" {
        dirs.push(game.join("7DaysToDieServer_Data"));
    } else {
        dirs.push(game.join("7DaysToDie_Data"));
        if let Some(u) = user_data_dir() {
            dirs.push(u.join("logs"));
        }
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            dirs.push(PathBuf::from(profile).join("AppData").join("LocalLow").join("The Fun Pimps").join("7 Days To Die"));
        }
    }
    dirs
}

fn is_log_name(name: &str) -> bool {
    let n = name.to_lowercase();
    (n.starts_with("output_log") || n.starts_with("player")) && (n.ends_with(".txt") || n.ends_with(".log"))
}

pub fn list(inst: &Instance) -> Vec<LogFile> {
    let mut out = Vec::new();
    for dir in search_dirs(inst) {
        for e in fs::read_dir(dir).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_file() && is_log_name(&name) {
                out.push(LogFile { path: e.path().to_string_lossy().to_string(), name, size: meta.len(), modified: modified_secs(&meta) });
            }
        }
    }
    out.sort_by(|a, b| b.modified.cmp(&a.modified));
    out
}

/// Pulls the game's version from the newest log, if there is one.
pub fn game_version(inst: &Instance) -> Option<String> {
    let re = Regex::new(r"Version:\s+((?:V|Alpha|A)\s*[\d.]+\s*(?:\(b\d+\))?)").unwrap();
    for log in list(inst).into_iter().take(3) {
        let mut buf = vec![0u8; 256 * 1024];
        let n = File::open(&log.path).and_then(|mut f| f.read(&mut buf)).unwrap_or(0);
        if let Some(c) = re.captures(&String::from_utf8_lossy(&buf[..n])) {
            return Some(c[1].trim().to_string());
        }
    }
    None
}

#[derive(Serialize, Debug)]
pub struct Implicated {
    pub title: String,
    pub count: usize,
}

#[derive(Serialize, Debug)]
pub struct LogView {
    /// (level, mod-related, text). Level: 0 info, 1 warning, 2 error.
    pub lines: Vec<(u8, bool, String)>,
    pub size: u64,
    pub truncated: bool,
    pub errors: usize,
    pub warnings: usize,
    pub implicated: Vec<Implicated>,
}

pub fn classify(line: &str) -> u8 {
    if line.contains(" ERR ") || line.contains(" EXC ") || line.contains("Exception:") || line.starts_with("ERR ") || line.starts_with("EXC ") {
        2
    } else if line.contains(" WRN ") || line.starts_with("WRN ") {
        1
    } else {
        0
    }
}

pub fn analyze(text: &str, mods: &[ModEntry]) -> (Vec<(u8, bool, String)>, usize, usize, Vec<Implicated>) {
    // Short names like "Mod" would match nearly every line.
    let needles: Vec<(String, String)> = mods
        .iter()
        .filter(|m| m.enabled && !m.core)
        .flat_map(|m| {
            let mut v = vec![(m.folder.to_lowercase(), m.title.clone())];
            if m.name != m.folder {
                v.push((m.name.to_lowercase(), m.title.clone()));
            }
            v
        })
        .filter(|(n, _)| n.len() >= 4)
        .collect();
    let mut counts: Vec<Implicated> = Vec::new();
    let (mut errors, mut warnings) = (0, 0);
    let mut lines = Vec::new();
    for raw in text.lines() {
        let level = classify(raw);
        errors += (level == 2) as usize;
        warnings += (level == 1) as usize;
        let lower = raw.to_lowercase();
        let hit = needles.iter().find(|(n, _)| lower.contains(n.as_str()));
        if let (Some((_, title)), 2) = (hit, level) {
            match counts.iter_mut().find(|c| &c.title == title) {
                Some(c) => c.count += 1,
                None => counts.push(Implicated { title: title.clone(), count: 1 }),
            }
        }
        let related = hit.is_some() || lower.contains("[mods]") || lower.contains("xml patch") || lower.contains("xpath");
        let text: String = raw.chars().take(MAX_LINE).collect();
        lines.push((level, related, text));
    }
    counts.sort_by(|a, b| b.count.cmp(&a.count));
    (lines, errors, warnings, counts)
}

/// Reads the end of a log. Returns None when the size matches `known_size`.
pub fn read(inst: &Instance, path: &str, mods: &[ModEntry], known_size: Option<u64>) -> R<Option<LogView>> {
    // Only ever read files this module discovered itself.
    if !list(inst).iter().any(|l| l.path == path) {
        return Err("That log file is no longer available.".into());
    }
    let mut f = File::open(path).map_err(|e| io_msg("Could not open log", e))?;
    let size = f.metadata().map(|m| m.len()).unwrap_or(0);
    if known_size == Some(size) {
        return Ok(None);
    }
    let truncated = size > MAX_TAIL;
    if truncated {
        f.seek(SeekFrom::End(-(MAX_TAIL as i64))).map_err(|e| io_msg("Could not read log", e))?;
    }
    let mut bytes = Vec::with_capacity(size.min(MAX_TAIL) as usize);
    f.read_to_end(&mut bytes).map_err(|e| io_msg("Could not read log", e))?;
    let text = String::from_utf8_lossy(&bytes);
    let body = if truncated { text.split_once('\n').map(|x| x.1).unwrap_or(&text) } else { &text };
    let (lines, errors, warnings, implicated) = analyze(body, mods);
    Ok(Some(LogView { lines, size, truncated, errors, warnings, implicated }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_game_log_lines() {
        assert_eq!(classify("2024-07-30T20:31:10 12.3 ERR Something failed"), 2);
        assert_eq!(classify("2024-07-30T20:31:10 12.3 EXC Null ref"), 2);
        assert_eq!(classify("2024-07-30T20:31:10 12.3 WRN Slow"), 1);
        assert_eq!(classify("2024-07-30T20:31:10 12.3 INF Hello"), 0);
    }
}
