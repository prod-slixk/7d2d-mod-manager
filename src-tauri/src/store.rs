//! Persistent app data and in-memory session state.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::install::Staging;
use crate::util::{write_atomic, R};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Instance {
    pub id: String,
    pub name: String,
    /// "client" or "server"
    pub kind: String,
    pub path: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct NexusLink {
    pub mod_id: u64,
    pub file_id: Option<u64>,
    pub version: Option<String>,
    pub latest: Option<String>,
    pub title: Option<String>,
    /// Result of the last update check: "current", "update" or "unknown" (which Nexus file is
    /// installed can't be told). None until checked.
    #[serde(default)]
    pub status: Option<String>,
    /// The Nexus file the last check suggests installing.
    #[serde(default)]
    pub latest_file: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ModRef {
    pub folder: String,
    pub name: String,
    pub version: String,
    pub nexus_id: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub created: i64,
    pub mods: Vec<ModRef>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LaunchPrefs {
    /// "auto", "on" or "off"
    pub eac: String,
    pub args: String,
}

impl Default for LaunchPrefs {
    fn default() -> Self {
        Self { eac: "auto".into(), args: String::new() }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct NexusUser {
    pub name: String,
    pub premium: bool,
}

/// Which play mode a world belongs to, and its pre-launch backup.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct SaveTag {
    /// "vanilla" or "modded"
    pub mode: String,
    pub played: i64,
    pub backup: Option<String>,
    /// The save's last-played time when `backup` was taken.
    pub backed: i64,
}

/// A game session in progress, started by this app or noticed running.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Session {
    pub mode: String,
    pub started: i64,
    pub seen_running: bool,
    /// Each save's last-played time when the session began. Changes after that were made in this session.
    #[serde(default)]
    pub baseline: HashMap<String, i64>,
}

/// A world that changed during a session in the other mode.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Mismatch {
    pub save: String,
    pub world: String,
    pub game: String,
    pub expected: String,
    pub played: String,
    pub backup: Option<String>,
    pub when: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Data {
    pub instances: Vec<Instance>,
    pub active: Option<String>,
    pub links: HashMap<String, HashMap<String, NexusLink>>,
    pub profiles: HashMap<String, Vec<Profile>>,
    pub active_profile: HashMap<String, String>,
    pub launch: HashMap<String, LaunchPrefs>,
    pub nexus_user: Option<NexusUser>,
    /// The mods to bring back when switching from vanilla to modded.
    pub modded_set: HashMap<String, Vec<ModRef>>,
    pub save_tags: HashMap<String, HashMap<String, SaveTag>>,
    pub sessions: HashMap<String, Session>,
    pub mismatches: HashMap<String, Vec<Mismatch>>,
    /// Nexus mods opened in Browse, newest first.
    pub recent_nexus: Vec<crate::nexus::RecentMod>,
}

pub struct App {
    pub dir: PathBuf,
    /// The game's Saves folder (%APPDATA%\7DaysToDie\Saves).
    pub saves: Option<PathBuf>,
    pub data: Mutex<Data>,
    pub staging: Mutex<HashMap<String, Staging>>,
    pub pending_nxm: Mutex<Vec<String>>,
    /// Set by the Cancel button to stop the running download.
    pub cancel_download: std::sync::atomic::AtomicBool,
    /// Files downloaded for the install queue, waiting to be staged together.
    pub downloads: Mutex<HashMap<String, crate::nexus::Downloaded>>,
}

/// A `data` folder beside the exe switches the app to fully portable storage.
pub fn resolve_dir() -> PathBuf {
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.join("data"))) {
        if dir.is_dir() {
            return dir;
        }
    }
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("7D2D Mod Manager")
}

impl App {
    pub fn load(dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        let data = std::fs::read(dir.join("state.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        // Staging and queued downloads from an earlier session can never be used.
        let _ = std::fs::remove_dir_all(dir.join("staging"));
        let _ = std::fs::remove_dir_all(dir.join("downloads"));
        Self {
            dir,
            saves: crate::mods::user_data_dir().map(|u| u.join("Saves")),
            data: Mutex::new(data),
            staging: Mutex::new(HashMap::new()),
            pending_nxm: Mutex::new(Vec::new()),
            cancel_download: std::sync::atomic::AtomicBool::new(false),
            downloads: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Data> {
        self.data.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn read<T>(&self, f: impl FnOnce(&Data) -> T) -> T {
        f(&self.lock())
    }

    /// Runs a mutation and saves the result.
    pub fn write<T>(&self, f: impl FnOnce(&mut Data) -> T) -> R<T> {
        let mut guard = self.lock();
        let out = f(&mut guard);
        let bytes = serde_json::to_vec_pretty(&*guard).map_err(|e| e.to_string())?;
        write_atomic(&self.dir.join("state.json"), &bytes)?;
        Ok(out)
    }

    pub fn instance(&self, id: &str) -> R<Instance> {
        self.read(|d| d.instances.iter().find(|i| i.id == id).cloned())
            .ok_or_else(|| "That game folder is no longer configured.".to_string())
    }

    pub fn links_for(&self, id: &str) -> HashMap<String, NexusLink> {
        self.read(|d| d.links.get(id).cloned().unwrap_or_default())
    }
}
