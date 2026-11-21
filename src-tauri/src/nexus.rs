//! Nexus Mods: API key storage, nxm:// links, downloads and update checks.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use reqwest::blocking::{Client, Response};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
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
        403 => Err("Nexus refused this download. Free accounts download through the Mod Manager Download button on the mod page, and those links expire, so click it again.".into()),
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

pub const CANCELLED: &str = "Download cancelled.";

/// Downloads the file an nxm:// link points at. Works for free accounts.
pub fn download(data_dir: &Path, raw_link: &str, progress: &dyn Fn(&str, Option<f64>), cancel: &AtomicBool) -> R<Downloaded> {
    let link = parse_nxm(raw_link)?;
    download_file(data_dir, link.mod_id, link.file_id, Some((&link.key, &link.expires)), progress, cancel)
}

/// Downloads one file. Without an nxm key and expiry, Nexus only allows this for Premium accounts.
pub fn download_file(
    data_dir: &Path,
    mod_id: u64,
    file_id: u64,
    nxm: Option<(&str, &str)>,
    progress: &dyn Fn(&str, Option<f64>),
    cancel: &AtomicBool,
) -> R<Downloaded> {
    let key = get_key().ok_or("Add your Nexus API key in Settings first.")?;
    let http = client()?;
    let info: FileInfo = api_get(&http, &key, &format!("/games/{GAME}/mods/{mod_id}/files/{file_id}.json"))?.json().map_err(|e| e.to_string())?;

    let query = match nxm {
        Some((k, e)) => format!(
            "?{}",
            reqwest::Url::parse_with_params("https://x/", [("key", k), ("expires", e)]).map_err(|e| e.to_string())?.query().unwrap_or_default()
        ),
        None => String::new(),
    };
    let mirrors: Vec<DownloadUri> = api_get(&http, &key, &format!("/games/{GAME}/mods/{mod_id}/files/{file_id}/download_link.json{query}"))?
        .json()
        .map_err(|e| e.to_string())?;
    let mirror = mirrors.into_iter().find(|m| m.uri.starts_with("https://")).ok_or("Nexus didn't offer a secure download mirror.")?;
    if cancel.load(Ordering::Relaxed) {
        return Err(CANCELLED.into());
    }

    let mut response = http.get(encode_url(&mirror.uri)).send().map_err(|e| format!("Download failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("Download failed with error {}.", response.status().as_u16()));
    }
    let total = response.content_length();
    let name = sanitize_folder(&info.file_name);
    let dir = data_dir.join("downloads");
    std::fs::create_dir_all(&dir).map_err(|e| io_msg("Could not create folder", e))?;
    let path = dir.join(&name);
    save_stream(&mut response, total, &path, &|got| progress(&format!("Downloading {name}"), total.map(|t| got as f64 / t as f64)), cancel)?;
    Ok(Downloaded {
        path,
        link: NexusLink { mod_id, file_id: Some(file_id), version: info.version.or(info.mod_version), latest: None, title: info.name },
    })
}

/// Writes a download to `path`. On error, cancel or a short read, the partial file is removed.
pub fn save_stream(reader: &mut dyn Read, total: Option<u64>, path: &Path, progress: &dyn Fn(u64), cancel: &AtomicBool) -> R<()> {
    let result = (|| -> R<()> {
        let mut out = File::create(path).map_err(|e| io_msg("Could not save download", e))?;
        let (mut buf, mut got, mut last) = (vec![0u8; 256 * 1024], 0u64, 0u64);
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(CANCELLED.into());
            }
            let n = reader.read(&mut buf).map_err(|e| format!("Download interrupted: {e}"))?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n]).map_err(|e| io_msg("Could not save download", e))?;
            got += n as u64;
            if got - last >= 512 * 1024 {
                last = got;
                progress(got);
            }
        }
        if total.is_some_and(|t| t != got) {
            return Err("The download ended early. Try again.".into());
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result
}

/// Percent-encodes what an HTTP request line can't carry. Nexus CDN addresses contain raw
/// file names with spaces, brackets and accented letters. Existing %XX escapes are kept.
pub fn encode_url(url: &str) -> String {
    let bytes = url.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    for (i, &b) in bytes.iter().enumerate() {
        let is_escape = b == b'%' && bytes.get(i + 1).is_some_and(u8::is_ascii_hexdigit) && bytes.get(i + 2).is_some_and(u8::is_ascii_hexdigit);
        let keep = b.is_ascii_alphanumeric()
            || is_escape
            || matches!(b, b'-' | b'.' | b'_' | b'~' | b':' | b'/' | b'?' | b'#' | b'&' | b'=' | b'+' | b';' | b',' | b'@' | b'!' | b'$' | b'\'' | b'(' | b')' | b'*');
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// ---- browsing (GraphQL v2, no key needed) ---------------------------------

const GRAPHQL_URL: &str = "https://api.nexusmods.com/v2/graphql";
const GAME_ID: &str = "1059";
const THUMBNAIL_HOST: &str = "https://staticdelivery.nexusmods.com/";

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct NexusMod {
    pub id: u64,
    pub name: String,
    pub summary: String,
    pub version: String,
    pub author: String,
    pub downloads: u64,
    pub endorsements: u64,
    /// Only ever a Nexus image server address, so the page can show it directly.
    pub picture: Option<String>,
    pub updated: String,
}

#[derive(Serialize, Debug)]
pub struct SearchPage {
    pub mods: Vec<NexusMod>,
    pub total: u64,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct NexusFile {
    pub id: u64,
    pub name: String,
    pub version: String,
    pub category: String,
    pub size: u64,
    pub date: i64,
    pub description: String,
}

fn graphql(query: &str, variables: Value) -> R<Value> {
    let response = client()?
        .post(GRAPHQL_URL)
        .header("Application-Name", APP_NAME)
        .header("Application-Version", VERSION)
        .timeout(std::time::Duration::from_secs(30))
        .json(&json!({ "query": query, "variables": variables }))
        .send()
        .map_err(|e| format!("Couldn't reach Nexus Mods: {e}"))?;
    let status = response.status().as_u16();
    if status >= 400 {
        return Err(if status == 429 { "Nexus rate limit reached. Try again in a while.".into() } else { format!("Nexus returned error {status}.") });
    }
    let body: Value = response.json().map_err(|e| format!("Unexpected reply from Nexus: {e}"))?;
    if let Some(message) = body["errors"][0]["message"].as_str() {
        return Err(format!("Nexus error: {message}"));
    }
    Ok(body["data"].clone())
}

/// Nexus sends some numbers as strings, and some fields as null.
fn number(v: &Value) -> u64 {
    v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())).unwrap_or(0)
}

fn text(v: &Value) -> String {
    v.as_str().unwrap_or("").to_string()
}

pub fn sort_field(sort: &str) -> R<&'static str> {
    match sort {
        "endorsements" => Ok("endorsements"),
        "downloads" => Ok("downloads"),
        "updated" => Ok("updatedAt"),
        "newest" => Ok("createdAt"),
        _ => Err("Unknown sort order.".into()),
    }
}

pub fn parse_search(data: &Value) -> SearchPage {
    let mods = data["mods"]["nodes"]
        .as_array()
        .map(|nodes| {
            nodes
                .iter()
                .map(|n| NexusMod {
                    id: number(&n["modId"]),
                    name: text(&n["name"]),
                    summary: text(&n["summary"]),
                    version: text(&n["version"]),
                    author: text(&n["author"]),
                    downloads: number(&n["downloads"]),
                    endorsements: number(&n["endorsements"]),
                    picture: n["pictureUrl"].as_str().filter(|u| u.starts_with(THUMBNAIL_HOST)).map(str::to_string),
                    updated: text(&n["updatedAt"]),
                })
                .filter(|m| m.id > 0)
                .collect()
        })
        .unwrap_or_default();
    SearchPage { mods, total: number(&data["mods"]["totalCount"]) }
}

pub fn search(query: &str, sort: &str, offset: u64) -> R<SearchPage> {
    const GQL: &str = "query Q($filter: ModsFilter, $sort: [ModsSort!], $offset: Int, $count: Int) {
        mods(filter: $filter, sort: $sort, offset: $offset, count: $count) {
            totalCount
            nodes { modId name summary version author downloads endorsements pictureUrl updatedAt }
        }
    }";
    let mut filter = json!({
        "gameDomainName": [{ "value": GAME, "op": "EQUALS" }],
        "adultContent": [{ "value": false, "op": "EQUALS" }],
    });
    let query = query.trim();
    if !query.is_empty() {
        filter["name"] = json!([{ "value": query, "op": "WILDCARD" }]);
    }
    let mut sort_entry = serde_json::Map::new();
    sort_entry.insert(sort_field(sort)?.to_string(), json!({ "direction": "DESC" }));
    let data = graphql(GQL, json!({ "filter": filter, "sort": [sort_entry], "offset": offset, "count": 24 }))?;
    Ok(parse_search(&data))
}

/// Current files only, main files first. Archived and removed versions are dropped.
pub fn parse_files(data: &Value) -> Vec<NexusFile> {
    let rank = |c: &str| match c {
        "MAIN" => 0,
        "UPDATE" => 1,
        "OPTIONAL" => 2,
        _ => 3,
    };
    let mut files: Vec<NexusFile> = data["modFiles"]
        .as_array()
        .map(|list| {
            list.iter()
                .map(|f| NexusFile {
                    id: number(&f["fileId"]),
                    name: text(&f["name"]),
                    version: text(&f["version"]),
                    category: text(&f["category"]),
                    size: number(&f["sizeInBytes"]),
                    date: number(&f["date"]) as i64,
                    description: text(&f["description"]),
                })
                .filter(|f| matches!(f.category.as_str(), "MAIN" | "UPDATE" | "OPTIONAL" | "MISCELLANEOUS"))
                .collect()
        })
        .unwrap_or_default();
    files.sort_by_key(|f| (rank(&f.category), std::cmp::Reverse(f.date), std::cmp::Reverse(f.id)));
    files
}

pub fn files(mod_id: u64) -> R<Vec<NexusFile>> {
    const GQL: &str = "query F($modId: ID!, $gameId: ID!) {
        modFiles(modId: $modId, gameId: $gameId) { fileId name version category sizeInBytes date description }
    }";
    Ok(parse_files(&graphql(GQL, json!({ "modId": mod_id.to_string(), "gameId": GAME_ID }))?))
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
    fn cdn_addresses_with_raw_file_names_become_valid() {
        let raw = "https://cf-files.nexus-cdn.com/cdn/1059/22/SMX [A21] {v2} é-22-1-0-1700000000.zip?md5=a+b&expires=1";
        let url = encode_url(raw);
        assert_eq!(url, "https://cf-files.nexus-cdn.com/cdn/1059/22/SMX%20%5BA21%5D%20%7Bv2%7D%20%C3%A9-22-1-0-1700000000.zip?md5=a+b&expires=1");
        assert!(reqwest::Url::parse(&url).is_ok());
        assert_eq!(encode_url("https://x/a%20b.zip"), "https://x/a%20b.zip");
    }

    /// Hands out data forever, and asks to cancel once the first chunk is read.
    struct Endless<'a>(&'a AtomicBool);
    impl Read for Endless<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.0.store(true, Ordering::Relaxed);
            buf.fill(7);
            Ok(buf.len())
        }
    }

    #[test]
    fn cancelling_mid_download_removes_the_partial_file() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("mod.zip");
        let cancel = AtomicBool::new(false);
        let err = save_stream(&mut Endless(&cancel), None, &path, &|_| {}, &cancel).unwrap_err();
        assert_eq!(err, CANCELLED);
        assert!(!path.exists());
    }

    #[test]
    fn short_download_is_rejected_and_cleaned_up() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("mod.zip");
        let err = save_stream(&mut &b"abc"[..], Some(10), &path, &|_| {}, &AtomicBool::new(false)).unwrap_err();
        assert!(err.contains("ended early"));
        assert!(!path.exists());
    }

    #[test]
    fn finished_download_lands_complete() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("mod.zip");
        save_stream(&mut &b"PK\x03\x04data"[..], Some(8), &path, &|_| {}, &AtomicBool::new(false)).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"PK\x03\x04data");
    }

    /// Shape copied from a real reply for 7 Days to Die (game id 1059).
    #[test]
    fn parses_real_search_reply_and_drops_foreign_images() {
        let data = json!({"mods": {"totalCount": 9811, "nodes": [
            {"modId": 22, "name": "SMX - By Sirillion", "summary": "UI", "version": "3.4.10.532", "author": "Sirillion",
             "downloads": 1426675, "endorsements": "10704",
             "pictureUrl": "https://staticdelivery.nexusmods.com/mods/1059/images/22/22-1794079930-1899389423.jpg",
             "updatedAt": "2026-11-07T19:35:54Z"},
            {"modId": 5, "name": "X", "summary": null, "version": null, "author": "A", "downloads": 1, "endorsements": 0,
             "pictureUrl": "https://evil.example/track.png", "updatedAt": "2020-01-01T00:00:00Z"}
        ]}});
        let page = parse_search(&data);
        assert_eq!(page.total, 9811);
        assert_eq!(page.mods[0].endorsements, 10704);
        assert!(page.mods[0].picture.is_some());
        assert_eq!(page.mods[1].picture, None);
        assert_eq!(page.mods[1].summary, "");
    }

    #[test]
    fn file_list_keeps_current_files_main_first() {
        let data = json!({"modFiles": [
            {"fileId": 207, "name": "Old", "version": "A14", "category": "ARCHIVED", "sizeInBytes": "574238", "date": 1472210041},
            {"fileId": 900, "name": "Extra", "version": "1", "category": "OPTIONAL", "sizeInBytes": "10", "date": 1700000002},
            {"fileId": 901, "name": "Core", "version": "3.4", "category": "MAIN", "sizeInBytes": "5000", "date": 1700000001},
            {"fileId": 456, "name": "Gone", "version": "A16", "category": "REMOVED", "sizeInBytes": "1", "date": 1501545543}
        ]});
        let files = parse_files(&data);
        assert_eq!(files.iter().map(|f| f.id).collect::<Vec<_>>(), vec![901, 900]);
        assert_eq!(files[0].size, 5000);
    }

    #[test]
    fn only_known_sort_orders_are_sent() {
        assert_eq!(sort_field("updated").unwrap(), "updatedAt");
        assert!(sort_field("name; drop").is_err());
    }

    #[test]
    fn parses_mod_page_refs() {
        assert_eq!(parse_mod_ref("https://www.nexusmods.com/7daystodie/mods/5541?tab=files").unwrap(), 5541);
        assert_eq!(parse_mod_ref(" 77 ").unwrap(), 77);
        assert!(parse_mod_ref("https://www.nexusmods.com/skyrim/mods/5").is_err());
    }
}
