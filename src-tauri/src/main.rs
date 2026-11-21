#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod backups;
mod commands;
mod conflicts;
mod game;
mod install;
mod logs;
mod mods;
mod nexus;
mod playmode;
mod profiles;
mod store;
mod util;

#[cfg(test)]
mod integration_tests;

use tauri::Manager;

fn nxm_arg(args: &[String]) -> Option<String> {
    args.iter().find(|a| a.to_lowercase().starts_with("nxm://")).cloned()
}

fn main() {
    tauri::Builder::default()
        // Must be first so a second launch forwards its nxm link and exits.
        .plugin(tauri_plugin_single_instance::init(|handle, args, _cwd| match nxm_arg(&args) {
            Some(url) => commands::receive_nxm(handle, url),
            None => {
                if let Some(window) = handle.get_webview_window("main") {
                    let _ = window.unminimize();
                    let _ = window.set_focus();
                }
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let state = store::App::load(store::resolve_dir());
            let args: Vec<String> = std::env::args().collect();
            if let Some(url) = nxm_arg(&args) {
                state.pending_nxm.lock().unwrap().push(url);
            }
            app.manage(state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::bootstrap,
            commands::detect_games,
            commands::add_instance,
            commands::pick_game_folder,
            commands::remove_instance,
            commands::set_active,
            commands::list_mods,
            commands::get_status,
            commands::set_play_mode,
            commands::set_save_mode,
            commands::resolve_mismatch,
            commands::set_mods_enabled,
            commands::uninstall_mod,
            commands::fix_nested,
            commands::open_mod_folder,
            commands::open_mods_root,
            commands::open_url,
            commands::pick_mod_files,
            commands::prepare_install,
            commands::commit_install,
            commands::cancel_install,
            commands::check_conflicts,
            commands::list_profiles,
            commands::save_profile,
            commands::update_profile,
            commands::rename_profile,
            commands::delete_profile,
            commands::apply_profile,
            commands::export_profile,
            commands::import_modpack,
            commands::list_backups,
            commands::create_backup,
            commands::restore_backup,
            commands::delete_backup,
            commands::open_backups,
            commands::list_logs,
            commands::read_log,
            commands::get_launch,
            commands::set_launch,
            commands::launch_game,
            commands::disable_server_eac,
            commands::nexus_set_key,
            commands::nexus_clear_key,
            commands::nexus_handler_status,
            commands::nexus_set_handler,
            commands::take_pending_nxm,
            commands::nexus_download,
            commands::nexus_link_mod,
            commands::nexus_unlink_mod,
            commands::nexus_check_updates,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the app");
}
