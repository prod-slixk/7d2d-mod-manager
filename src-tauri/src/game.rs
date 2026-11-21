//! Locating game installs, checking processes and launching.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use regex::Regex;
use serde::Serialize;
use sysinfo::{ProcessesToUpdate, System};
use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

use crate::mods::{user_data_dir, ModEntry};
use crate::store::{Instance, LaunchPrefs};
use crate::util::{find_child_ci, io_msg, write_atomic, R};

const CLIENT_EXE: &str = "7DaysToDie.exe";
const CLIENT_EAC_EXE: &str = "7DaysToDie_EAC.exe";
const SERVER_EXE: &str = "7DaysToDieServer.exe";

#[derive(Serialize, Clone, Debug)]
pub struct Found {
    pub name: String,
    pub kind: String,
    pub path: String,
}

fn steam_root() -> Option<PathBuf> {
    let key = RegKey::predef(HKEY_CURRENT_USER).open_subkey("Software\\Valve\\Steam").ok()?;
    let path: String = key.get_value("SteamPath").ok()?;
    Some(PathBuf::from(path.replace('/', "\\")))
}

pub fn parse_library_paths(vdf: &str) -> Vec<PathBuf> {
    let re = Regex::new(r#"(?i)"path"\s+"([^"]+)""#).unwrap();
    re.captures_iter(vdf).map(|c| PathBuf::from(c[1].replace("\\\\", "\\"))).collect()
}

fn acf_installdir(acf: &str) -> Option<String> {
    let re = Regex::new(r#"(?i)"installdir"\s+"([^"]+)""#).unwrap();
    re.captures(acf).map(|c| c[1].to_string())
}

/// Returns the instance kind if `path` looks like a game folder.
pub fn kind_of(path: &Path) -> Option<&'static str> {
    if find_child_ci(path, SERVER_EXE).is_some() {
        Some("server")
    } else if find_child_ci(path, CLIENT_EXE).is_some() {
        Some("client")
    } else {
        None
    }
}

pub fn detect() -> Vec<Found> {
    let mut libraries: Vec<PathBuf> = Vec::new();
    if let Some(root) = steam_root() {
        if let Ok(vdf) = fs::read_to_string(root.join("steamapps").join("libraryfolders.vdf")) {
            libraries.extend(parse_library_paths(&vdf));
        }
        libraries.push(root);
    }
    libraries.push(PathBuf::from(r"C:\Program Files (x86)\Steam"));
    for drive in 'C'..='K' {
        for sub in ["SteamLibrary", "Steam", "Games\\SteamLibrary", "Games\\Steam"] {
            libraries.push(PathBuf::from(format!("{drive}:\\{sub}")));
        }
    }
    let mut seen = Vec::new();
    let mut out = Vec::new();
    for lib in libraries {
        let apps = lib.join("steamapps");
        for (appid, fallback, name) in
            [("251570", "7 Days To Die", "7 Days to Die"), ("294420", "7 Days to Die Dedicated Server", "Dedicated Server")]
        {
            let dir = fs::read_to_string(apps.join(format!("appmanifest_{appid}.acf")))
                .ok()
                .and_then(|a| acf_installdir(&a))
                .unwrap_or_else(|| fallback.to_string());
            let path = apps.join("common").join(dir);
            let key = path.to_string_lossy().to_lowercase();
            if seen.contains(&key) {
                continue;
            }
            if let Some(kind) = kind_of(&path) {
                seen.push(key);
                out.push(Found { name: name.to_string(), kind: kind.to_string(), path: path.to_string_lossy().to_string() });
            }
        }
    }
    out
}

fn running(names: &[&str]) -> Vec<bool> {
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    let live: Vec<String> = sys.processes().values().map(|p| p.name().to_string_lossy().to_lowercase()).collect();
    names.iter().map(|n| live.iter().any(|l| l == &n.to_lowercase())).collect()
}

fn process_names(kind: &str) -> &'static [&'static str] {
    if kind == "server" {
        &[SERVER_EXE]
    } else {
        &[CLIENT_EXE, CLIENT_EAC_EXE]
    }
}

pub fn is_running(kind: &str) -> bool {
    running(process_names(kind)).into_iter().any(|r| r)
}

pub fn ensure_stopped(kind: &str) -> R<()> {
    if is_running(kind) {
        Err("The game is running. Close it before changing mods.".into())
    } else {
        Ok(())
    }
}

#[derive(Serialize, Debug)]
pub struct Status {
    pub running: bool,
    pub steam_running: bool,
    pub path_ok: bool,
    pub eac_needed: Vec<String>,
    pub server_eac: Option<bool>,
    pub game_version: Option<String>,
}

fn eac_needed(mods: &[ModEntry]) -> Vec<String> {
    mods.iter().filter(|m| m.enabled && m.has_dll && !m.core && m.problem.is_none()).map(|m| m.title.clone()).collect()
}

fn server_config_path(inst: &Instance) -> PathBuf {
    Path::new(&inst.path).join("serverconfig.xml")
}

fn server_eac_regex() -> Regex {
    Regex::new(r#"(?i)(<property\s+name\s*=\s*"EACEnabled"\s+value\s*=\s*")(true|false)(")"#).unwrap()
}

pub fn parse_server_eac(xml: &str) -> Option<bool> {
    server_eac_regex().captures(xml).map(|c| c[2].eq_ignore_ascii_case("true"))
}

pub fn disable_server_eac(inst: &Instance) -> R<()> {
    let path = server_config_path(inst);
    let xml = fs::read_to_string(&path).map_err(|e| io_msg("Could not read serverconfig.xml", e))?;
    if parse_server_eac(&xml).is_none() {
        return Err("serverconfig.xml has no EACEnabled setting to change.".into());
    }
    let updated = server_eac_regex().replace(&xml, "${1}false${3}");
    write_atomic(&path, updated.as_bytes())
}

pub fn status(inst: &Instance, mods: &[ModEntry], version: Option<String>) -> Status {
    let names = process_names(&inst.kind);
    let mut wanted: Vec<&str> = names.to_vec();
    wanted.push("steam.exe");
    let found = running(&wanted);
    Status {
        running: found[..names.len()].iter().any(|r| *r),
        steam_running: found[names.len()],
        path_ok: kind_of(Path::new(&inst.path)).is_some(),
        eac_needed: eac_needed(mods),
        server_eac: (inst.kind == "server")
            .then(|| fs::read_to_string(server_config_path(inst)).ok().and_then(|x| parse_server_eac(&x)))
            .flatten(),
        game_version: version,
    }
}

#[derive(Serialize, Debug)]
pub struct LaunchResult {
    pub target: String,
    pub eac: bool,
    pub warnings: Vec<String>,
    /// Worlds backed up before launch.
    pub protected: usize,
}

/// Splits launch options on whitespace, honoring double quotes.
pub fn split_args(raw: &str) -> Vec<String> {
    let (mut out, mut cur, mut quoted) = (Vec::new(), String::new(), false);
    for c in raw.chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Pure decision: EAC on or off for a client launch.
pub fn wants_eac(mode: &str, dll_mods: usize) -> bool {
    match mode {
        "on" => true,
        "off" => false,
        _ => dll_mods == 0,
    }
}

/// `force_eac` overrides the preference. Vanilla play uses it, because crossplay needs EAC.
pub fn launch(inst: &Instance, mods: &[ModEntry], prefs: &LaunchPrefs, force_eac: bool) -> R<LaunchResult> {
    if is_running(&inst.kind) {
        return Err("The game is already running.".into());
    }
    let dir = Path::new(&inst.path);
    let needed = eac_needed(mods);
    let mut warnings = Vec::new();

    if inst.kind == "server" {
        if needed.len() > 0 && fs::read_to_string(server_config_path(inst)).ok().and_then(|x| parse_server_eac(&x)) == Some(true) {
            warnings.push("EAC is on in serverconfig.xml, so DLL mods may not load on this server.".to_string());
        }
        if find_child_ci(dir, "startdedicated.bat").is_none() {
            return Err("startdedicated.bat was not found in the server folder.".into());
        }
        Command::new("cmd")
            .args(["/C", "start", "7D2D Dedicated Server", "/D"])
            .arg(dir)
            .arg("startdedicated.bat")
            .spawn()
            .map_err(|e| io_msg("Could not start the server", e))?;
        return Ok(LaunchResult { target: "startdedicated.bat".into(), eac: false, warnings, protected: 0 });
    }

    if !running(&["steam.exe"])[0] {
        warnings.push("Steam isn't running, so the game may fail to start. Open Steam and try again if it does.".to_string());
    }
    let mut eac = force_eac || wants_eac(&prefs.eac, needed.len());
    if eac && !needed.is_empty() {
        warnings.push(format!("EAC is forced on, so {} DLL mod(s) won't load.", needed.len()));
    }
    let eac_exe = find_child_ci(dir, CLIENT_EAC_EXE);
    if eac && eac_exe.is_none() {
        warnings.push("7DaysToDie_EAC.exe was not found, so the game started without EAC.".to_string());
        eac = false;
    }
    let exe = if eac { eac_exe.unwrap() } else { find_child_ci(dir, CLIENT_EXE).ok_or("7DaysToDie.exe was not found in the game folder.")? };

    let mut cmd = Command::new(&exe);
    cmd.current_dir(dir);
    // Without -logfile a direct launch writes elsewhere and the log viewer loses it.
    if let Some(user) = user_data_dir() {
        let logs = user.join("logs");
        let _ = fs::create_dir_all(&logs);
        let stamp = chrono::Local::now().format("%Y-%m-%d__%H-%M-%S");
        cmd.arg("-logfile").arg(logs.join(format!("output_log_client__{stamp}.txt")));
    }
    cmd.args(split_args(&prefs.args));
    cmd.spawn().map_err(|e| io_msg("Could not start the game", e))?;
    Ok(LaunchResult { target: exe.file_name().unwrap_or_default().to_string_lossy().to_string(), eac, warnings, protected: 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_steam_libraries() {
        let vdf = r#""libraryfolders" { "0" { "path" "C:\\Program Files (x86)\\Steam" } "1" { "path" "D:\\SteamLibrary" } }"#;
        let libs = parse_library_paths(vdf);
        assert_eq!(libs, vec![PathBuf::from(r"C:\Program Files (x86)\Steam"), PathBuf::from(r"D:\SteamLibrary")]);
    }

    #[test]
    fn auto_mode_drops_eac_only_for_dll_mods() {
        assert!(wants_eac("auto", 0));
        assert!(!wants_eac("auto", 2));
        assert!(wants_eac("on", 2));
        assert!(!wants_eac("off", 0));
    }

    #[test]
    fn splits_quoted_args() {
        assert_eq!(split_args(r#"-a "b c"  -d"#), vec!["-a", "b c", "-d"]);
    }

    #[test]
    fn reads_and_flips_server_eac() {
        let t = tempfile::tempdir().unwrap();
        let cfg = t.path().join("serverconfig.xml");
        fs::write(&cfg, r#"<ServerSettings><property name="EACEnabled" value="true" /></ServerSettings>"#).unwrap();
        let inst = Instance { id: "s".into(), name: "s".into(), kind: "server".into(), path: t.path().to_string_lossy().into() };
        disable_server_eac(&inst).unwrap();
        assert_eq!(parse_server_eac(&fs::read_to_string(cfg).unwrap()), Some(false));
    }
}
