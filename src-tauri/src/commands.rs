//! The commands the interface can call.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::Ordering;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;

use crate::backups::{self, Backup};
use crate::conflicts;
use crate::game::{self, Found, LaunchResult, Status};
use crate::install::{self, CommitResult, Staging};
use crate::logs::{self, LogFile, LogView};
use crate::mods::{self, ModEntry};
use crate::nexus;
use crate::playmode;
use crate::profiles::{self, ApplyResult};
use crate::store::{App, Instance, LaunchPrefs, NexusLink, NexusUser, Profile};
use crate::util::{io_msg, R};

#[derive(Serialize, Clone)]
struct Progress<'a> {
    message: &'a str,
    fraction: Option<f64>,
}

fn notify(handle: &AppHandle, message: &str, fraction: Option<f64>) {
    let _ = handle.emit("progress", Progress { message, fraction });
}

fn scan(app: &App, inst: &Instance) -> Vec<ModEntry> {
    mods::scan(inst, &app.links_for(&inst.id))
}

fn open_in_explorer(path: &Path) -> R<()> {
    Command::new("explorer").arg(path).spawn().map(|_| ()).map_err(|e| io_msg("Could not open folder", e))
}

// ---- setup -----------------------------------------------------------------

#[derive(Serialize)]
pub struct Bootstrap {
    instances: Vec<Instance>,
    active: Option<String>,
    version: String,
    data_dir: String,
    nexus_user: Option<NexusUser>,
    has_key: bool,
}

#[tauri::command(async)]
pub fn bootstrap(app: State<App>) -> Bootstrap {
    let (instances, active, nexus_user) = app.read(|d| (d.instances.clone(), d.active.clone(), d.nexus_user.clone()));
    Bootstrap {
        instances,
        active,
        version: env!("CARGO_PKG_VERSION").to_string(),
        data_dir: app.dir.to_string_lossy().to_string(),
        nexus_user,
        has_key: nexus::get_key().is_some(),
    }
}

#[tauri::command(async)]
pub fn detect_games() -> Vec<Found> {
    game::detect()
}

#[tauri::command(async)]
pub fn add_instance(app: State<App>, path: String, name: Option<String>) -> R<Instance> {
    let dir = PathBuf::from(&path);
    let kind = game::kind_of(&dir).ok_or("That folder doesn't contain 7DaysToDie.exe or 7DaysToDieServer.exe.")?;
    if let Some(existing) = app.read(|d| d.instances.iter().find(|i| i.path.eq_ignore_ascii_case(&path)).cloned()) {
        let id = existing.id.clone();
        app.write(|d| d.active = Some(id))?;
        return Ok(existing);
    }
    let default_name = if kind == "server" { "Dedicated Server" } else { "7 Days to Die" };
    let inst = Instance {
        id: uuid::Uuid::new_v4().to_string(),
        name: name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| default_name.to_string()),
        kind: kind.to_string(),
        path,
    };
    let out = inst.clone();
    app.write(|d| {
        d.active = Some(inst.id.clone());
        d.instances.push(inst);
    })?;
    Ok(out)
}

#[tauri::command(async)]
pub fn pick_game_folder(handle: AppHandle) -> Option<String> {
    handle.dialog().file().set_title("Select your game or dedicated server folder").blocking_pick_folder().map(|p| p.to_string())
}

#[tauri::command(async)]
pub fn remove_instance(app: State<App>, inst: String) -> R<()> {
    app.write(|d| {
        d.instances.retain(|i| i.id != inst);
        if d.active.as_deref() == Some(&inst) {
            d.active = d.instances.first().map(|i| i.id.clone());
        }
    })
}

#[tauri::command(async)]
pub fn set_active(app: State<App>, inst: String) -> R<()> {
    app.write(|d| d.active = Some(inst))
}

// ---- mods ------------------------------------------------------------------

#[tauri::command(async)]
pub fn list_mods(app: State<App>, inst: String) -> R<Vec<ModEntry>> {
    Ok(scan(&app, &app.instance(&inst)?))
}

#[derive(Serialize)]
pub struct StatusView {
    #[serde(flatten)]
    status: Status,
    play: playmode::PlayInfo,
}

#[tauri::command(async)]
pub fn get_status(app: State<App>, inst: String) -> R<StatusView> {
    let i = app.instance(&inst)?;
    let list = scan(&app, &i);
    let status = game::status(&i, &list, logs::game_version(&i));
    playmode::observe(&app, &i, status.running, playmode::current_mode(&list))?;
    Ok(StatusView { status, play: playmode::info(&app, &i, &list) })
}

#[tauri::command(async)]
pub fn set_play_mode(app: State<App>, inst: String, mode: String) -> R<playmode::SwitchResult> {
    let i = app.instance(&inst)?;
    game::ensure_stopped(&i.kind)?;
    playmode::switch(&app, &i, &scan(&app, &i), &mode)
}

#[tauri::command(async)]
pub fn set_save_mode(app: State<App>, inst: String, save: String, mode: Option<String>) -> R<()> {
    playmode::set_tag(&app, &app.instance(&inst)?, &save, mode.as_deref())
}

#[tauri::command(async)]
pub fn resolve_mismatch(handle: AppHandle, app: State<App>, inst: String, save: String, action: String) -> R<()> {
    let i = app.instance(&inst)?;
    game::ensure_stopped(&i.kind)?;
    playmode::resolve(&app, &i, &save, &action, &|m| notify(&handle, m, None))
}

#[tauri::command(async)]
pub fn set_mods_enabled(app: State<App>, inst: String, ids: Vec<String>, enabled: bool) -> R<()> {
    let i = app.instance(&inst)?;
    game::ensure_stopped(&i.kind)?;
    let list = scan(&app, &i);
    for id in ids {
        mods::set_enabled(&i, mods::find(&list, &id)?, enabled)?;
    }
    Ok(())
}

#[tauri::command(async)]
pub fn uninstall_mod(app: State<App>, inst: String, id: String) -> R<()> {
    let i = app.instance(&inst)?;
    game::ensure_stopped(&i.kind)?;
    let list = scan(&app, &i);
    let m = mods::find(&list, &id)?;
    if m.core {
        return Err(format!("{} ships with the game and can't be removed.", m.title));
    }
    backups::snapshot_mod(&app, &i, m)?;
    std::fs::remove_dir_all(&m.path).map_err(|e| io_msg(&format!("Could not remove {}", m.title), e))?;
    let folder = m.folder.clone();
    app.write(|d| {
        if let Some(l) = d.links.get_mut(&inst) {
            l.remove(&folder);
        }
    })
}

#[tauri::command(async)]
pub fn fix_nested(app: State<App>, inst: String, id: String) -> R<()> {
    let i = app.instance(&inst)?;
    game::ensure_stopped(&i.kind)?;
    let list = scan(&app, &i);
    mods::fix_nested(mods::find(&list, &id)?)
}

#[tauri::command(async)]
pub fn open_mod_folder(app: State<App>, inst: String, id: String) -> R<()> {
    let i = app.instance(&inst)?;
    let list = scan(&app, &i);
    open_in_explorer(Path::new(&mods::find(&list, &id)?.path))
}

#[tauri::command(async)]
pub fn open_mods_root(app: State<App>, inst: String) -> R<()> {
    let i = app.instance(&inst)?;
    let dir = mods::roots(&i).remove(0).enabled;
    std::fs::create_dir_all(&dir).map_err(|e| io_msg("Could not create folder", e))?;
    open_in_explorer(&dir)
}

#[tauri::command(async)]
pub fn open_url(handle: AppHandle, url: String) -> R<()> {
    let parsed = reqwest::Url::parse(url.trim()).map_err(|_| "That isn't a valid web address.".to_string())?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("Only web links can be opened.".into());
    }
    handle.opener().open_url(parsed.as_str(), None::<&str>).map_err(|e| e.to_string())
}

// ---- install ---------------------------------------------------------------

#[tauri::command(async)]
pub fn pick_mod_files(handle: AppHandle) -> Vec<String> {
    handle
        .dialog()
        .file()
        .set_title("Choose mod archives")
        .add_filter("Mod archives", &["zip", "7z", "rar"])
        .blocking_pick_files()
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.to_string())
        .collect()
}

fn keep(app: &App, s: Staging) -> Staging {
    app.staging.lock().unwrap_or_else(|p| p.into_inner()).insert(s.id.clone(), s.clone());
    s
}

#[tauri::command(async)]
pub fn prepare_install(handle: AppHandle, app: State<App>, inst: String, paths: Vec<String>) -> R<Staging> {
    let i = app.instance(&inst)?;
    let list = scan(&app, &i);
    let s = install::prepare(&app, &list, &paths, None, &|m| notify(&handle, m, None))?;
    Ok(keep(&app, s))
}

#[tauri::command(async)]
pub fn commit_install(app: State<App>, inst: String, staging: String, selected: Vec<String>) -> R<CommitResult> {
    let i = app.instance(&inst)?;
    game::ensure_stopped(&i.kind)?;
    let s = app.staging.lock().unwrap_or_else(|p| p.into_inner()).remove(&staging).ok_or("This install has expired. Start it again.")?;
    let mut result = install::commit(&app, &i, &s, &selected);
    if let (Ok(r), Some(pack)) = (&mut result, &s.import) {
        let list = scan(&app, &i);
        r.missing = pack.mods.iter().filter(|m| !list.iter().any(|e| e.folder.eq_ignore_ascii_case(&m.folder))).cloned().collect();
        let profile = profiles::new_profile(&pack.name, pack.mods.clone());
        r.profile = Some(profile.name.clone());
        let id = inst.clone();
        app.write(|d| d.profiles.entry(id).or_default().push(profile))?;
    }
    let _ = std::fs::remove_dir_all(&s.dir);
    result
}

#[tauri::command(async)]
pub fn cancel_install(app: State<App>, staging: String) {
    if let Some(s) = app.staging.lock().unwrap_or_else(|p| p.into_inner()).remove(&staging) {
        let _ = std::fs::remove_dir_all(s.dir);
    }
}

// ---- conflicts -------------------------------------------------------------

#[tauri::command(async)]
pub fn check_conflicts(app: State<App>, inst: String) -> R<conflicts::Report> {
    let i = app.instance(&inst)?;
    Ok(conflicts::analyze(&scan(&app, &i)))
}

// ---- profiles --------------------------------------------------------------

#[derive(Serialize)]
pub struct ProfilesView {
    profiles: Vec<Profile>,
    active: Option<String>,
}

#[tauri::command(async)]
pub fn list_profiles(app: State<App>, inst: String) -> ProfilesView {
    app.read(|d| ProfilesView { profiles: d.profiles.get(&inst).cloned().unwrap_or_default(), active: d.active_profile.get(&inst).cloned() })
}

#[tauri::command(async)]
pub fn save_profile(app: State<App>, inst: String, name: String) -> R<()> {
    let i = app.instance(&inst)?;
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("Give the profile a name.".into());
    }
    let profile = profiles::new_profile(&name, profiles::snapshot(&scan(&app, &i)));
    app.write(|d| {
        d.active_profile.insert(inst.clone(), profile.id.clone());
        d.profiles.entry(inst).or_default().push(profile);
    })
}

#[tauri::command(async)]
pub fn update_profile(app: State<App>, inst: String, id: String) -> R<()> {
    let i = app.instance(&inst)?;
    let snapshot = profiles::snapshot(&scan(&app, &i));
    app.write(|d| {
        if let Some(p) = d.profiles.get_mut(&inst).and_then(|l| l.iter_mut().find(|p| p.id == id)) {
            p.mods = snapshot;
        }
    })
}

#[tauri::command(async)]
pub fn rename_profile(app: State<App>, inst: String, id: String, name: String) -> R<()> {
    app.write(|d| {
        if let Some(p) = d.profiles.get_mut(&inst).and_then(|l| l.iter_mut().find(|p| p.id == id)) {
            p.name = name.trim().to_string();
        }
    })
}

#[tauri::command(async)]
pub fn delete_profile(app: State<App>, inst: String, id: String) -> R<()> {
    app.write(|d| {
        if let Some(l) = d.profiles.get_mut(&inst) {
            l.retain(|p| p.id != id);
        }
        if d.active_profile.get(&inst) == Some(&id) {
            d.active_profile.remove(&inst);
        }
    })
}

fn profile_of(app: &App, inst: &str, id: &str) -> R<Profile> {
    app.read(|d| d.profiles.get(inst).and_then(|l| l.iter().find(|p| p.id == id)).cloned()).ok_or_else(|| "That profile no longer exists.".to_string())
}

#[tauri::command(async)]
pub fn apply_profile(app: State<App>, inst: String, id: String) -> R<ApplyResult> {
    let i = app.instance(&inst)?;
    game::ensure_stopped(&i.kind)?;
    let profile = profile_of(&app, &inst, &id)?;
    let result = profiles::apply(&i, &scan(&app, &i), &profile)?;
    app.write(|d| d.active_profile.insert(inst, id))?;
    Ok(result)
}

#[tauri::command(async)]
pub fn export_profile(handle: AppHandle, app: State<App>, inst: String, id: String, files: bool) -> R<bool> {
    let i = app.instance(&inst)?;
    let profile = profile_of(&app, &inst, &id)?;
    let Some(dest) = handle
        .dialog()
        .file()
        .set_title("Export modpack")
        .set_file_name(format!("{}.7dmp", crate::util::sanitize_folder(&profile.name)))
        .add_filter("7D2D modpack", &["7dmp"])
        .blocking_save_file()
    else {
        return Ok(false);
    };
    let dest = PathBuf::from(dest.to_string());
    profiles::export(&profile, &scan(&app, &i), &dest, files, &|m| notify(&handle, m, None))?;
    Ok(true)
}

#[tauri::command(async)]
pub fn import_modpack(handle: AppHandle, app: State<App>, inst: String) -> R<Option<Staging>> {
    let i = app.instance(&inst)?;
    let Some(path) = handle.dialog().file().set_title("Import modpack").add_filter("7D2D modpack", &["7dmp"]).blocking_pick_file() else {
        return Ok(None);
    };
    let path = PathBuf::from(path.to_string());
    let s = profiles::import(&app, &scan(&app, &i), &path, &|m| notify(&handle, m, None))?;
    Ok(Some(keep(&app, s)))
}

// ---- backups ---------------------------------------------------------------

#[tauri::command(async)]
pub fn list_backups(app: State<App>, inst: String) -> R<Vec<Backup>> {
    Ok(backups::list(&app, &app.instance(&inst)?))
}

#[tauri::command(async)]
pub fn create_backup(handle: AppHandle, app: State<App>, inst: String, label: String, mods: bool, saves: bool) -> R<Backup> {
    let i = app.instance(&inst)?;
    if !mods && !saves {
        return Err("Choose mods, saves, or both.".into());
    }
    let label = if label.trim().is_empty() { "Manual backup" } else { label.trim() };
    backups::create(&app, &i, label, mods, saves, false, &|m| notify(&handle, m, None))
}

#[tauri::command(async)]
pub fn restore_backup(handle: AppHandle, app: State<App>, inst: String, id: String) -> R<()> {
    let i = app.instance(&inst)?;
    game::ensure_stopped(&i.kind)?;
    backups::restore(&app, &i, &id, &|m| notify(&handle, m, None))
}

#[tauri::command(async)]
pub fn delete_backup(app: State<App>, inst: String, id: String) -> R<()> {
    backups::delete(&app, &app.instance(&inst)?, &id)
}

#[tauri::command(async)]
pub fn open_backups(app: State<App>, inst: String) -> R<()> {
    open_in_explorer(&backups::open_dir(&app, &app.instance(&inst)?))
}

// ---- logs ------------------------------------------------------------------

#[tauri::command(async)]
pub fn list_logs(app: State<App>, inst: String) -> R<Vec<LogFile>> {
    Ok(logs::list(&app.instance(&inst)?))
}

#[tauri::command(async)]
pub fn read_log(app: State<App>, inst: String, path: String, known: Option<u64>) -> R<Option<LogView>> {
    let i = app.instance(&inst)?;
    logs::read(&i, &path, &scan(&app, &i), known)
}

// ---- launch ----------------------------------------------------------------

#[tauri::command(async)]
pub fn get_launch(app: State<App>, inst: String) -> LaunchPrefs {
    app.read(|d| d.launch.get(&inst).cloned().unwrap_or_default())
}

#[tauri::command(async)]
pub fn set_launch(app: State<App>, inst: String, prefs: LaunchPrefs) -> R<()> {
    app.write(|d| d.launch.insert(inst, prefs)).map(|_| ())
}

#[tauri::command(async)]
pub fn launch_game(handle: AppHandle, app: State<App>, inst: String) -> R<LaunchResult> {
    let i = app.instance(&inst)?;
    game::ensure_stopped(&i.kind)?;
    let prefs = app.read(|d| d.launch.get(&inst).cloned().unwrap_or_default());
    let list = scan(&app, &i);
    let mode = playmode::current_mode(&list);
    let protected = playmode::protect(&app, &i, mode, &|m| notify(&handle, m, None))?;
    let mut result = game::launch(&i, &list, &prefs, mode == playmode::VANILLA)?;
    result.protected = protected;
    let session = playmode::new_session(&app, mode, false);
    app.write(|d| d.sessions.insert(inst, session))?;
    Ok(result)
}

#[tauri::command(async)]
pub fn disable_server_eac(app: State<App>, inst: String) -> R<()> {
    let i = app.instance(&inst)?;
    game::ensure_stopped(&i.kind)?;
    game::disable_server_eac(&i)
}

// ---- nexus -----------------------------------------------------------------

#[tauri::command(async)]
pub fn nexus_set_key(app: State<App>, key: String) -> R<NexusUser> {
    let key = key.trim().to_string();
    let user = nexus::validate(&key)?;
    nexus::store_key(&key)?;
    let out = user.clone();
    app.write(|d| d.nexus_user = Some(user))?;
    Ok(out)
}

#[tauri::command(async)]
pub fn nexus_clear_key(app: State<App>) -> R<()> {
    nexus::clear_key();
    app.write(|d| d.nexus_user = None)
}

#[tauri::command(async)]
pub fn nexus_handler_status() -> nexus::HandlerStatus {
    nexus::handler_status()
}

#[tauri::command(async)]
pub fn nexus_set_handler(on: bool) -> R<()> {
    if on {
        nexus::register_handler()
    } else {
        nexus::unregister_handler()
    }
}

#[tauri::command(async)]
pub fn take_pending_nxm(app: State<App>) -> Vec<String> {
    std::mem::take(&mut *app.pending_nxm.lock().unwrap_or_else(|p| p.into_inner()))
}

fn stage_download(handle: &AppHandle, app: &App, i: &Instance, d: nexus::Downloaded) -> R<Staging> {
    let list = scan(app, i);
    let paths = [d.path.to_string_lossy().to_string()];
    let staged = install::prepare(app, &list, &paths, Some(d.link), &|m| notify(handle, m, None));
    let _ = std::fs::remove_file(&d.path);
    Ok(keep(app, staged?))
}

/// Downloads an nxm:// link's file, then stages it. Nothing installs until the user confirms.
#[tauri::command(async)]
pub fn nexus_download(handle: AppHandle, app: State<App>, inst: String, url: String) -> R<Staging> {
    let i = app.instance(&inst)?;
    app.cancel_download.store(false, Ordering::Relaxed);
    let d = nexus::download(&app.dir, &url, &|m, f| notify(&handle, m, f), &app.cancel_download)?;
    stage_download(&handle, &app, &i, d)
}

/// Premium only: downloads a file picked in Browse, then stages it.
#[tauri::command(async)]
pub fn nexus_download_file(handle: AppHandle, app: State<App>, inst: String, mod_id: u64, file_id: u64) -> R<Staging> {
    let i = app.instance(&inst)?;
    app.cancel_download.store(false, Ordering::Relaxed);
    let d = nexus::download_file(&app.dir, mod_id, file_id, None, &|m, f| notify(&handle, m, f), &app.cancel_download)?;
    stage_download(&handle, &app, &i, d)
}

#[tauri::command(async)]
pub fn cancel_download(app: State<App>) {
    app.cancel_download.store(true, Ordering::Relaxed);
}

#[tauri::command(async)]
pub fn nexus_search(query: String, sort: String, offset: u64) -> R<nexus::SearchPage> {
    nexus::search(&query, &sort, offset)
}

#[tauri::command(async)]
pub fn nexus_files(mod_id: u64) -> R<Vec<nexus::NexusFile>> {
    nexus::files(mod_id)
}

#[tauri::command(async)]
pub fn nexus_link_mod(app: State<App>, inst: String, folder: String, reference: String) -> R<()> {
    let mod_id = nexus::parse_mod_ref(&reference)?;
    let i = app.instance(&inst)?;
    let list = scan(&app, &i);
    let version = list.iter().find(|m| m.folder == folder).map(|m| m.version.clone());
    let mut link = NexusLink { mod_id, version, ..Default::default() };
    if let Some(key) = nexus::get_key() {
        if let Ok((latest, title)) = nexus::latest_version(&nexus::http_client()?, &key, mod_id) {
            link.latest = latest;
            link.title = title;
        }
    }
    app.write(|d| {
        d.links.entry(inst).or_default().insert(folder, link);
    })
}

#[tauri::command(async)]
pub fn nexus_unlink_mod(app: State<App>, inst: String, folder: String) -> R<()> {
    app.write(|d| {
        if let Some(l) = d.links.get_mut(&inst) {
            l.remove(&folder);
        }
    })
}

#[derive(Serialize)]
pub struct UpdateCheck {
    checked: usize,
    updates: Vec<String>,
    errors: Vec<String>,
}

#[tauri::command(async)]
pub fn nexus_check_updates(handle: AppHandle, app: State<App>, inst: String) -> R<UpdateCheck> {
    let key = nexus::get_key().ok_or("Add your Nexus API key in Settings first.")?;
    let i = app.instance(&inst)?;
    let links = app.links_for(&inst);
    let http = nexus::http_client()?;
    let mut result = UpdateCheck { checked: 0, updates: Vec::new(), errors: Vec::new() };
    let total = links.len().max(1) as f64;
    for (n, (folder, link)) in links.iter().enumerate() {
        notify(&handle, &format!("Checking {folder}"), Some(n as f64 / total));
        match nexus::latest_version(&http, &key, link.mod_id) {
            Ok((latest, title)) => {
                result.checked += 1;
                let (id, folder) = (inst.clone(), folder.clone());
                app.write(|d| {
                    if let Some(l) = d.links.get_mut(&id).and_then(|m| m.get_mut(&folder)) {
                        l.latest = latest;
                        l.title = title;
                    }
                })?;
            }
            Err(e) => {
                result.errors.push(format!("{folder}: {e}"));
                if e.contains("rate limit") || e.contains("API key") {
                    break;
                }
            }
        }
    }
    result.updates = scan(&app, &i).into_iter().filter(|m| m.update).map(|m| m.title).collect();
    Ok(result)
}

/// Hands an nxm:// link to the interface, or holds it until the interface is ready.
pub fn receive_nxm(handle: &AppHandle, url: String) {
    if let Some(app) = handle.try_state::<App>() {
        app.pending_nxm.lock().unwrap_or_else(|p| p.into_inner()).push(url);
    }
    let _ = handle.emit("nxm-ready", ());
    if let Some(window) = handle.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}
