//! Nexus Mods: API key storage, nxm:// links, downloads and update checks.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use reqwest::blocking::{Client, Response};
use serde::{Deserialize, Serialize};
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};
use winreg::RegKey;

use crate::store::{NexusLink, NexusUser};
use crate::util::{io_msg, sanitize_folder, R};

const API: &str = "https://api.nexusmods.com/v1";
const GAME: &str = "7daystodie";
const APP_NAME: &str = "7D2D Mod Manager";
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn key_entry() -> R<keyring::Entry> {
    keyring::Entry::new(APP_NAME, "nexus_api_key").map_err(|e| format!("Windows Credential Manager is unavailable: {e}"))
}

pub fn get_key() -> Option<String> {
    key_entry().ok()?.get_password().ok().filter(|k| !k.is_empty())
}

pub fn store_key(key: &str) -> R<()> {
    key_entry()?.set_password(key).map_err(|e| format!("Could not save the key: {e}"))
}

pub fn clear_key() {
    if let Ok(entry) = key_entry() {
        let _ = entry.delete_credential();
    }
}

fn client() -> R<Client> {
    Client::builder()
        .user_agent(format!("{APP_NAME}/{VERSION}"))
        .connect_timeout(std::time::Duration::from_secs(20))
        .timeout(None)
        .build()
        .map_err(|e| e.to_string())
}

fn api_get(http: &Client, key: &str, path: &str) -> R<Response> {
    let response = http
        .get(format!("{API}{path}"))
        .header("apikey", key)
        .header("Application-Name", APP_NAME)
        .header("Application-Version", VERSION)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .map_err(|e| format!("Couldn't reach Nexus Mods: {e}"))?;
    match response.status().as_u16() {
        200..=299 => Ok(response),
        401 => Err("Nexus rejected the API key. Check it in Settings.".into()),
        403 => Err("Nexus refused this request. The download link may have expired, so click Mod Manager Download again.".into()),
        404 => Err("Nexus couldn't find that mod or file.".into()),
        429 => Err("Nexus rate limit reached. Try again in an hour.".into()),
        code => Err(format!("Nexus returned error {code}.")),
    }
}

#[derive(Deserialize)]
struct ValidateResp {
    name: String,
    is_premium: Option<bool>,
}

pub fn validate(key: &str) -> R<NexusUser> {
    let r: ValidateResp = api_get(&client()?, key, "/users/validate.json")?.json().map_err(|e| e.to_string())?;
    Ok(NexusUser { name: r.name, premium: r.is_premium.unwrap_or(false) })
}

#[derive(Debug, PartialEq, Clone)]
pub struct NxmLink {
    pub mod_id: u64,
    pub file_id: u64,
    pub key: String,
    pub expires: String,
}

pub fn parse_nxm(raw: &str) -> R<NxmLink> {
    let url = reqwest::Url::parse(raw.trim()).map_err(|_| "That isn't a valid nxm:// link.".to_string())?;
    if url.scheme() != "nxm" {
        return Err("That isn't an nxm:// link.".into());
    }
    if !url.host_str().is_some_and(|h| h.eq_ignore_ascii_case(GAME)) {
        return Err("That Nexus link is for a different game.".into());
    }
    let parts: Vec<&str> = url.path().trim_matches('/').split('/').collect();
    let [ "mods", mod_id, "files", file_id ] = parts[..] else {
        return Err("That Nexus link has an unexpected format.".into());
    };
    let param = |name: &str| url.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.to_string()).unwrap_or_default();
    let (key, expires) = (param("key"), param("expires"));
    if key.is_empty() || expires.is_empty() {
        return Err("That Nexus link is missing its download key.".into());
    }
    Ok(NxmLink {
        mod_id: mod_id.parse().map_err(|_| "Bad mod id in link.")?,
        file_id: file_id.parse().map_err(|_| "Bad file id in link.")?,
        key,
        expires,
    })
}

/// Accepts a nexusmods.com mod URL or a bare mod id.
pub fn parse_mod_ref(raw: &str) -> R<u64> {
    let t = raw.trim();
    if let Ok(id) = t.parse::<u64>() {
        return Ok(id);
    }
    let re = regex::Regex::new(r"(?i)nexusmods\.com/7daystodie/mods/(\d+)").unwrap();
    re.captures(t).and_then(|c| c[1].parse().ok()).ok_or_else(|| "Paste a Nexus mod page URL for 7 Days to Die, or its mod number.".to_string())
}

#[derive(Deserialize)]
struct FileInfo {
    file_name: String,
    version: Option<String>,
    mod_version: Option<String>,
    name: Option<String>,
}

#[derive(Deserialize)]
struct DownloadUri {
    #[serde(rename = "URI")]
    uri: String,
}

pub struct Downloaded {
    pub path: PathBuf,
    pub link: NexusLink,
}

pub fn download(data_dir: &Path, raw_link: &str, progress: &dyn Fn(&str, Option<f64>)) -> R<Downloaded> {
    let link = parse_nxm(raw_link)?;
    let key = get_key().ok_or("Add your Nexus API key in Settings first.")?;
    let http = client()?;
    let (mod_id, file_id) = (link.mod_id, link.file_id);
    let info: FileInfo = api_get(&http, &key, &format!("/games/{GAME}/mods/{mod_id}/files/{file_id}.json"))?.json().map_err(|e| e.to_string())?;

    let query: String = reqwest::Url::parse_with_params("https://x/", [("key", &link.key), ("expires", &link.expires)])
        .map_err(|e| e.to_string())?
        .query()
        .unwrap_or_default()
        .to_string();
    let mirrors: Vec<DownloadUri> = api_get(&http, &key, &format!("/games/{GAME}/mods/{mod_id}/files/{file_id}/download_link.json?{query}"))?
        .json()
        .map_err(|e| e.to_string())?;
    let mirror = mirrors.into_iter().find(|m| m.uri.starts_with("https://")).ok_or("Nexus didn't offer a secure download mirror.")?;

    let mut response = http.get(&mirror.uri).send().map_err(|e| format!("Download failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("Download failed with error {}.", response.status().as_u16()));
    }
    let total = response.content_length();
    let name = sanitize_folder(&info.file_name);
    let dir = data_dir.join("downloads");
    std::fs::create_dir_all(&dir).map_err(|e| io_msg("Could not create folder", e))?;
    let path = dir.join(&name);
    let mut out = File::create(&path).map_err(|e| io_msg("Could not save download", e))?;

    let (mut buf, mut got, mut last) = (vec![0u8; 256 * 1024], 0u64, 0u64);
    loop {
        let n = response.read(&mut buf).map_err(|e| format!("Download interrupted: {e}"))?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).map_err(|e| io_msg("Could not save download", e))?;
        got += n as u64;
        if got - last >= 1024 * 1024 {
            last = got;
            progress(&format!("Downloading {name}"), total.map(|t| got as f64 / t as f64));
        }
    }
    drop(out);
    if total.is_some_and(|t| t != got) {
        let _ = std::fs::remove_file(&path);
        return Err("The download ended early. Try again.".into());
    }
    Ok(Downloaded {
        path,
        link: NexusLink { mod_id, file_id: Some(file_id), version: info.version.or(info.mod_version), latest: None, title: info.name },
    })
}

#[derive(Deserialize)]
struct ModInfoResp {
    version: Option<String>,
    name: Option<String>,
}

pub fn latest_version(http: &Client, key: &str, mod_id: u64) -> R<(Option<String>, Option<String>)> {
    let r: ModInfoResp = api_get(http, key, &format!("/games/{GAME}/mods/{mod_id}.json"))?.json().map_err(|e| e.to_string())?;
    Ok((r.version, r.name))
}

pub fn http_client() -> R<Client> {
    client()
}

// ---- nxm:// protocol handler --------------------------------------------

const NXM_KEY: &str = "Software\\Classes\\nxm";

fn this_exe() -> R<String> {
    std::env::current_exe().map(|p| p.to_string_lossy().to_string()).map_err(|e| e.to_string())
}

#[derive(Serialize, Debug)]
pub struct HandlerStatus {
    /// "none", "this" or "other"
    pub state: String,
}

pub fn handler_status() -> HandlerStatus {
    let command: Option<String> = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(format!("{NXM_KEY}\\shell\\open\\command"), KEY_READ)
        .ok()
        .and_then(|k| k.get_value("").ok());
    let exe = this_exe().unwrap_or_default().to_lowercase();
    let state = match command {
        None => "none",
        Some(c) if c.to_lowercase().contains(&exe) => "this",
        Some(_) => "other",
    };
    HandlerStatus { state: state.into() }
}

pub fn register_handler() -> R<()> {
    let exe = this_exe()?;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (root, _) = hkcu.create_subkey(NXM_KEY).map_err(|e| io_msg("Could not register links", e))?;
    root.set_value("", &"URL:NXM Protocol").map_err(|e| e.to_string())?;
    root.set_value("URL Protocol", &"").map_err(|e| e.to_string())?;
    let (cmd, _) = root.create_subkey("shell\\open\\command").map_err(|e| io_msg("Could not register links", e))?;
    cmd.set_value("", &format!("\"{exe}\" \"%1\"")).map_err(|e| e.to_string())
}

/// Removes the handler only if this copy of the app owns it.
pub fn unregister_handler() -> R<()> {
    if handler_status().state == "this" {
        RegKey::predef(HKEY_CURRENT_USER).delete_subkey_all(NXM_KEY).map_err(|e| io_msg("Could not unregister links", e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_download_link() {
        let l = parse_nxm("nxm://7daystodie/mods/123/files/456?key=abc&expires=1700000000&user_id=9").unwrap();
        assert_eq!((l.mod_id, l.file_id, l.key.as_str(), l.expires.as_str()), (123, 456, "abc", "1700000000"));
    }

    #[test]
    fn rejects_other_games_and_bad_links() {
        assert!(parse_nxm("nxm://skyrim/mods/1/files/2?key=a&expires=1").is_err());
        assert!(parse_nxm("https://7daystodie/mods/1/files/2?key=a&expires=1").is_err());
        assert!(parse_nxm("nxm://7daystodie/mods/1/files/2").is_err());
        assert!(parse_nxm("nxm://7daystodie/mods/x/files/2?key=a&expires=1").is_err());
    }

    #[test]
    fn parses_mod_page_refs() {
        assert_eq!(parse_mod_ref("https://www.nexusmods.com/7daystodie/mods/5541?tab=files").unwrap(), 5541);
        assert_eq!(parse_mod_ref(" 77 ").unwrap(), 77);
        assert!(parse_mod_ref("https://www.nexusmods.com/skyrim/mods/5").is_err());
    }
}
