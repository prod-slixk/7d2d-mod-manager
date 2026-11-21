//! End-to-end backend checks against a simulated game install.

use std::fs;
use std::io::Write;
use std::path::Path;

use crate::playmode::{self, MODDED, VANILLA};
use crate::store::{App, Instance, ModRef};
use crate::{backups, conflicts, install, mods, profiles};

struct Sim {
    _tmp: tempfile::TempDir,
    app: App,
    inst: Instance,
}

fn modinfo(name: &str, version: &str) -> String {
    format!(r#"<?xml version="1.0"?><xml><Name value="{name}"/><DisplayName value="{name} Display"/><Version value="{version}"/><Author value="Tester"/></xml>"#)
}

fn write_mod(dir: &Path, name: &str, version: &str) {
    fs::create_dir_all(dir.join("Config")).unwrap();
    fs::write(dir.join("ModInfo.xml"), modinfo(name, version)).unwrap();
    fs::write(dir.join("Config").join("items.xml"), r#"<configs><set xpath="/items/item[@name='gun']/@v">1</set></configs>"#).unwrap();
}

fn sim() -> Sim {
    let tmp = tempfile::tempdir().unwrap();
    let game = tmp.path().join("game");
    fs::create_dir_all(game.join("Mods")).unwrap();
    fs::write(game.join("7DaysToDieServer.exe"), "x").unwrap();
    write_mod(&game.join("Mods").join("0_TFP_Harmony"), "TFP_Harmony", "1.0");
    fs::write(game.join("Mods").join("0_TFP_Harmony").join("Harmony.dll"), "x").unwrap();
    write_mod(&game.join("Mods").join("Alpha"), "Alpha", "1.0");
    write_mod(&game.join("Mods").join("Bravo"), "Bravo", "2.0");
    let mut app = App::load(tmp.path().join("data"));
    app.saves = Some(tmp.path().join("saves"));
    // A server-kind instance keeps these tests away from the real %APPDATA%.
    let inst = Instance { id: "i1".into(), name: "Sim".into(), kind: "server".into(), path: game.to_string_lossy().into() };
    Sim { _tmp: tmp, app, inst }
}

fn scan(s: &Sim) -> Vec<mods::ModEntry> {
    mods::scan(&s.inst, &s.app.links_for(&s.inst.id))
}

fn by_folder<'a>(list: &'a [mods::ModEntry], f: &str) -> &'a mods::ModEntry {
    list.iter().find(|m| m.folder == f).unwrap()
}

fn make_zip(path: &Path, entries: &[(&str, &str)]) {
    let mut zip = zip::ZipWriter::new(fs::File::create(path).unwrap());
    for (name, body) in entries {
        zip.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
}

#[test]
fn scan_orders_and_marks_core() {
    let s = sim();
    let list = scan(&s);
    assert_eq!(list.len(), 3);
    let core = by_folder(&list, "0_TFP_Harmony");
    assert!(core.core && core.has_dll);
    assert_eq!(core.load_index, Some(0));
    assert_eq!(by_folder(&list, "Bravo").load_index, Some(2));
}

#[test]
fn disable_then_enable_moves_folder() {
    let s = sim();
    let list = scan(&s);
    mods::set_enabled(&s.inst, by_folder(&list, "Alpha"), false).unwrap();
    assert!(Path::new(&s.inst.path).join("Mods_Disabled").join("Alpha").is_dir());
    assert!(!Path::new(&s.inst.path).join("Mods").join("Alpha").exists());
    let list = scan(&s);
    assert!(!by_folder(&list, "Alpha").enabled);
    mods::set_enabled(&s.inst, by_folder(&list, "Alpha"), true).unwrap();
    assert!(by_folder(&scan(&s), "Alpha").enabled);
}

#[test]
fn core_mods_cannot_be_disabled() {
    let s = sim();
    let list = scan(&s);
    assert!(mods::set_enabled(&s.inst, by_folder(&list, "0_TFP_Harmony"), false).is_err());
}

#[test]
fn install_from_zip_with_wrapper_and_loose_root() {
    let s = sim();
    let src = s._tmp.path().join("src");
    fs::create_dir_all(&src).unwrap();
    let wrapped = src.join("wrapped.zip");
    make_zip(&wrapped, &[("Stuff/Charlie/ModInfo.xml", &modinfo("Charlie", "1.0")), ("Stuff/Charlie/Config/x.xml", "<configs/>")]);
    let loose = src.join("loose.zip");
    make_zip(&loose, &[("ModInfo.xml", &modinfo("Delta", "1.0"))]);

    let staged = install::prepare(&s.app, &scan(&s), &[wrapped.to_string_lossy().into(), loose.to_string_lossy().into()], None, &|_| {}).unwrap();
    assert_eq!(staged.candidates.iter().map(|c| c.folder.as_str()).collect::<Vec<_>>(), vec!["Charlie", "Delta"]);

    let ids: Vec<String> = staged.candidates.iter().map(|c| c.id.clone()).collect();
    let r = install::commit(&s.app, &s.inst, &staged, &ids).unwrap();
    assert_eq!(r.installed.len(), 2);
    let list = scan(&s);
    assert_eq!(by_folder(&list, "Charlie").version, "1.0");
    assert!(Path::new(&by_folder(&list, "Delta").path).join("ModInfo.xml").is_file());
}

#[test]
fn queued_downloads_install_together_and_each_keeps_its_nexus_link() {
    let s = sim();
    let src = s._tmp.path().join("src");
    fs::create_dir_all(&src).unwrap();
    let (a, b) = (src.join("a.zip"), src.join("b.zip"));
    make_zip(&a, &[("Echo/ModInfo.xml", &modinfo("Echo", "1.0"))]);
    make_zip(&b, &[("Foxtrot/ModInfo.xml", &modinfo("Foxtrot", "2.0"))]);
    let link = |mod_id, file_id, v: &str| crate::store::NexusLink { mod_id, file_id: Some(file_id), version: Some(v.into()), ..Default::default() };
    let sources = vec![(a.to_string_lossy().to_string(), Some(link(11, 110, "1.0"))), (b.to_string_lossy().to_string(), Some(link(22, 220, "2.0")))];

    let staged = install::prepare_sources(&s.app, &scan(&s), &sources, &|_| {}).unwrap();
    assert_eq!(staged.candidates.len(), 2, "one review covers every queued file");
    let ids: Vec<String> = staged.candidates.iter().map(|c| c.id.clone()).collect();
    install::commit(&s.app, &s.inst, &staged, &ids).unwrap();

    let links = s.app.links_for(&s.inst.id);
    assert_eq!((links["Echo"].mod_id, links["Echo"].file_id), (11, Some(110)));
    assert_eq!((links["Foxtrot"].mod_id, links["Foxtrot"].file_id), (22, Some(220)));
}

#[test]
fn zip_slip_entries_never_escape_staging() {
    let s = sim();
    let z = s._tmp.path().join("evil.zip");
    make_zip(&z, &[("../../../escaped.txt", "pwn"), ("Good/ModInfo.xml", &modinfo("Good", "1"))]);
    let staged = install::prepare(&s.app, &scan(&s), &[z.to_string_lossy().into()], None, &|_| {}).unwrap();
    assert_eq!(staged.candidates.len(), 1);
    let mut stray = walkdir::WalkDir::new(s._tmp.path()).into_iter().flatten().filter(|e| e.file_name() == "escaped.txt");
    assert!(stray.next().is_none());
}

#[test]
fn rejects_archives_without_a_mod() {
    let s = sim();
    let z = s._tmp.path().join("nope.zip");
    make_zip(&z, &[("readme.txt", "hi")]);
    let err = install::prepare(&s.app, &scan(&s), &[z.to_string_lossy().into()], None, &|_| {}).unwrap_err();
    assert!(err.contains("ModInfo.xml"));
}

#[test]
fn update_keeps_disabled_state_and_backs_up_old_version() {
    let s = sim();
    let list = scan(&s);
    mods::set_enabled(&s.inst, by_folder(&list, "Alpha"), false).unwrap();
    let z = s._tmp.path().join("alpha2.zip");
    make_zip(&z, &[("Alpha/ModInfo.xml", &modinfo("Alpha", "9.9"))]);
    let staged = install::prepare(&s.app, &scan(&s), &[z.to_string_lossy().into()], None, &|_| {}).unwrap();
    assert!(staged.candidates[0].exists);
    let ids = vec![staged.candidates[0].id.clone()];
    install::commit(&s.app, &s.inst, &staged, &ids).unwrap();

    let list = scan(&s);
    let alpha = by_folder(&list, "Alpha");
    assert_eq!(alpha.version, "9.9");
    assert!(!alpha.enabled);
    let auto = backups::list(&s.app, &s.inst);
    assert_eq!(auto.len(), 1);
    assert!(auto[0].auto && auto[0].kind == "mod");

    backups::restore(&s.app, &s.inst, &auto[0].id, &|_| {}).unwrap();
    let list = scan(&s);
    assert_eq!(by_folder(&list, "Alpha").version, "1.0");
}

#[test]
fn full_backup_restores_removed_and_added_mods() {
    let s = sim();
    let b = backups::create(&s.app, &s.inst, "t", true, false, false, &|_| {}).unwrap();
    let game = Path::new(&s.inst.path).to_path_buf();
    fs::remove_dir_all(game.join("Mods").join("Alpha")).unwrap();
    write_mod(&game.join("Mods").join("Newcomer"), "Newcomer", "1");
    backups::restore(&s.app, &s.inst, &b.id, &|_| {}).unwrap();
    let list = scan(&s);
    assert!(list.iter().any(|m| m.folder == "Alpha"));
    assert!(!list.iter().any(|m| m.folder == "Newcomer"));
    // The restore itself left an undo point.
    assert!(backups::list(&s.app, &s.inst).iter().any(|x| x.label.starts_with("Before restoring")));
}

#[test]
fn profile_apply_switches_mods_and_reports_missing() {
    let s = sim();
    let list = scan(&s);
    let refs = vec![
        ModRef { folder: "Bravo".into(), name: "Bravo".into(), version: "2".into(), nexus_id: None },
        ModRef { folder: "Ghost".into(), name: "Ghost".into(), version: "1".into(), nexus_id: Some(5) },
    ];
    let p = profiles::new_profile("P", refs);
    let r = profiles::apply(&s.inst, &list, &p).unwrap();
    assert_eq!((r.enabled, r.disabled), (0, 1));
    assert_eq!(r.missing.len(), 1);
    let list = scan(&s);
    assert!(!by_folder(&list, "Alpha").enabled);
    assert!(by_folder(&list, "Bravo").enabled);
    assert!(by_folder(&list, "0_TFP_Harmony").enabled);
}

fn write_save(s: &Sim, rel: &str, body: &str) {
    let dir = playmode::save_path(s.app.saves.as_ref().unwrap(), rel).unwrap();
    fs::create_dir_all(dir.join("Region")).unwrap();
    fs::write(dir.join("main.ttw"), body).unwrap();
    fs::write(dir.join("Region").join("r.0.0.7rg"), body).unwrap();
}

fn read_save(s: &Sim, rel: &str) -> String {
    fs::read_to_string(playmode::save_path(s.app.saves.as_ref().unwrap(), rel).unwrap().join("main.ttw")).unwrap()
}

/// One play session as the status poll sees it: game appears, game closes.
fn play_session(s: &Sim, mode: &str, touch: &[(&str, &str)]) {
    playmode::observe(&s.app, &s.inst, true, mode).unwrap();
    for (rel, body) in touch {
        write_save(s, rel, body);
    }
    playmode::observe(&s.app, &s.inst, false, mode).unwrap();
}

#[test]
fn switching_to_vanilla_and_back_restores_the_same_mods() {
    let s = sim();
    let r = playmode::switch(&s.app, &s.inst, &scan(&s), VANILLA).unwrap();
    assert_eq!(r.disabled, 2);
    let list = scan(&s);
    assert_eq!(playmode::current_mode(&list), VANILLA);
    assert!(by_folder(&list, "0_TFP_Harmony").enabled, "the game's own mod stays on");

    let r = playmode::switch(&s.app, &s.inst, &list, MODDED).unwrap();
    assert_eq!(r.enabled, 2);
    let list = scan(&s);
    assert!(by_folder(&list, "Alpha").enabled && by_folder(&list, "Bravo").enabled);
}

#[test]
fn modded_without_a_remembered_setup_explains_itself() {
    let s = sim();
    let err = playmode::switch(&s.app, &s.inst, &scan(&s), MODDED).unwrap_err();
    assert!(err.contains("Turn your mods on"));
}

#[test]
fn wrong_mode_session_is_caught_and_restorable() {
    let s = sim();
    write_save(&s, "Navezgane/Crossplay", "vanilla-v1");
    write_save(&s, "Pregen/Overhaul", "modded-v1");
    playmode::set_tag(&s.app, &s.inst, "Navezgane/Crossplay", Some(VANILLA)).unwrap();
    playmode::set_tag(&s.app, &s.inst, "Pregen/Overhaul", Some(MODDED)).unwrap();

    // Launching modded backs up the vanilla world, once.
    assert_eq!(playmode::protect(&s.app, &s.inst, MODDED, &|_| {}).unwrap(), 1);
    assert_eq!(playmode::protect(&s.app, &s.inst, MODDED, &|_| {}).unwrap(), 0);

    // A normal modded session leaves no warning.
    play_session(&s, MODDED, &[("Pregen/Overhaul", "modded-v2")]);
    assert!(playmode::info(&s.app, &s.inst, &scan(&s)).mismatches.is_empty());

    // Then the vanilla world gets opened with mods on.
    play_session(&s, MODDED, &[("Navezgane/Crossplay", "vanilla-v1 + modded blocks")]);
    let info = playmode::info(&s.app, &s.inst, &scan(&s));
    assert_eq!(info.mismatches.len(), 1);
    assert_eq!(info.mismatches[0].expected, VANILLA);
    assert!(info.saves.iter().any(|v| v.entry.rel == "Navezgane/Crossplay" && v.mismatch));

    // The damaged copy must not replace the good backup on the next launch.
    assert_eq!(playmode::protect(&s.app, &s.inst, MODDED, &|_| {}).unwrap(), 0);

    playmode::resolve(&s.app, &s.inst, "Navezgane/Crossplay", "restore", &|_| {}).unwrap();
    assert_eq!(read_save(&s, "Navezgane/Crossplay"), "vanilla-v1");
    assert_eq!(read_save(&s, "Pregen/Overhaul"), "modded-v2", "other worlds are untouched");
    assert!(playmode::info(&s.app, &s.inst, &scan(&s)).mismatches.is_empty());
    // The damaged copy was kept too, in case it was wanted after all.
    assert!(backups::list(&s.app, &s.inst).iter().any(|b| b.label.starts_with("Before restoring")));
}

#[test]
fn relabel_accepts_the_world_as_modded() {
    let s = sim();
    write_save(&s, "W/G", "v1");
    playmode::set_tag(&s.app, &s.inst, "W/G", Some(VANILLA)).unwrap();
    play_session(&s, MODDED, &[("W/G", "v2")]);
    playmode::resolve(&s.app, &s.inst, "W/G", "relabel", &|_| {}).unwrap();
    let info = playmode::info(&s.app, &s.inst, &scan(&s));
    assert!(info.mismatches.is_empty());
    assert_eq!(info.saves[0].mode.as_deref(), Some(MODDED));
}

#[test]
fn first_session_tags_untagged_worlds() {
    let s = sim();
    play_session(&s, VANILLA, &[("W/Fresh", "v1")]);
    let info = playmode::info(&s.app, &s.inst, &scan(&s));
    assert_eq!(info.saves[0].mode.as_deref(), Some(VANILLA));
}

#[test]
fn failed_profile_apply_undoes_earlier_moves() {
    let s = sim();
    // A stray copy makes disabling Bravo collide, after Alpha has already moved.
    write_mod(&Path::new(&s.inst.path).join("Mods_Disabled").join("Bravo"), "Bravo", "1");
    let list = scan(&s);
    let p = profiles::new_profile("Empty", Vec::new());
    assert!(profiles::apply(&s.inst, &list, &p).is_err());
    let list = scan(&s);
    assert!(by_folder(&list, "Alpha").enabled);
}

#[test]
fn modpack_round_trip_with_files() {
    let s = sim();
    s.app
        .write(|d| {
            let link = crate::store::NexusLink { mod_id: 7, file_id: Some(70), version: Some("1.0".into()), status: Some("update".into()), ..Default::default() };
            d.links.entry(s.inst.id.clone()).or_default().insert("Alpha".into(), link);
        })
        .unwrap();
    let list = scan(&s);
    let p = profiles::new_profile("Pack", profiles::snapshot(&list));
    assert_eq!(p.mods.len(), 2, "core mods stay out of profiles");
    let dest = s._tmp.path().join("pack.7dmp");
    profiles::export(&p, &list, &dest, true, &|_| {}).unwrap();

    let other = sim();
    fs::remove_dir_all(Path::new(&other.inst.path).join("Mods").join("Alpha")).unwrap();
    let staged = profiles::import(&other.app, &scan(&other), &dest, &|_| {}).unwrap();
    assert_eq!(staged.import.as_ref().unwrap().name, "Pack");
    assert_eq!(staged.candidates.len(), 2);
    let ids: Vec<String> = staged.candidates.iter().map(|c| c.id.clone()).collect();
    install::commit(&other.app, &other.inst, &staged, &ids).unwrap();
    let alpha = scan(&other).into_iter().find(|m| m.folder == "Alpha").expect("Alpha installed");
    let link = alpha.nexus.expect("Alpha keeps its Nexus link");
    assert_eq!((link.mod_id, link.file_id), (7, Some(70)), "the Nexus file travels with the modpack");
    assert!(!alpha.update, "the old check result doesn't carry over to the new copy");
}

#[test]
fn nested_mod_is_detected_and_fixed() {
    let s = sim();
    let wrapper = Path::new(&s.inst.path).join("Mods").join("Download_v1");
    write_mod(&wrapper.join("RealMod"), "RealMod", "1");
    let list = scan(&s);
    let bad = by_folder(&list, "Download_v1");
    assert_eq!(bad.problem.as_deref(), Some("nested"));
    mods::fix_nested(bad).unwrap();
    let list = scan(&s);
    assert!(by_folder(&list, "RealMod").problem.is_none());
    assert!(!list.iter().any(|m| m.folder == "Download_v1"));
}

#[test]
fn conflicts_reflect_only_enabled_mods() {
    let s = sim();
    let game = Path::new(&s.inst.path).to_path_buf();
    fs::write(game.join("Mods").join("Bravo").join("Config").join("items.xml"), r#"<configs><set xpath="/items/item[@name='gun']/@v">2</set></configs>"#).unwrap();
    let list = scan(&s);
    let r = conflicts::analyze(&list);
    assert_eq!(r.issues.iter().filter(|i| i.kind == "override").count(), 1);
    assert_eq!(r.issues.iter().find(|i| i.kind == "override").unwrap().winner.as_deref(), Some("Bravo Display"));
    mods::set_enabled(&s.inst, by_folder(&list, "Bravo"), false).unwrap();
    assert!(conflicts::analyze(&scan(&s)).issues.iter().all(|i| i.kind != "override"));
}
