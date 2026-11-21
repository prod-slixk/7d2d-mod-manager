//! Filesystem and archive helpers shared by the other modules.

use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Seek, Write};
use std::path::{Component, Path, PathBuf};

use walkdir::WalkDir;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

pub type R<T> = Result<T, String>;

/// Suffix for a folder parked aside during a swap. Scans ignore it.
pub const OLD_SUFFIX: &str = ".7dmm_old";

pub fn io_msg(context: &str, e: impl std::fmt::Display) -> String {
    let text = e.to_string();
    if text.contains("os error 5") || text.to_lowercase().contains("access is denied") {
        format!("{context}: Windows denied access. Close the game and anything using these files, then try again.")
    } else if text.contains("os error 32") {
        format!("{context}: another program is using these files. Close the game and try again.")
    } else {
        format!("{context}: {text}")
    }
}

/// True if `name` is exactly one plain path component.
pub fn is_safe_name(name: &str) -> bool {
    if name.is_empty() || name.contains([':', '/', '\\']) {
        return false;
    }
    let mut parts = Path::new(name).components();
    matches!(parts.next(), Some(Component::Normal(_))) && parts.next().is_none()
}

/// True if `rel` stays inside the directory it gets joined to.
pub fn is_safe_rel(rel: &Path) -> bool {
    !rel.to_string_lossy().contains(':')
        && rel.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

/// Turns arbitrary text into a valid Windows folder name.
pub fn sanitize_folder(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| if "<>:\"/\\|?*".contains(c) || c.is_control() { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim().trim_end_matches('.').trim().to_string();
    let upper = cleaned.to_uppercase();
    let reserved = ["CON", "PRN", "AUX", "NUL"].contains(&upper.as_str())
        || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && upper.ends_with(|c: char| c.is_ascii_digit()));
    if cleaned.is_empty() || cleaned == ".." || reserved {
        format!("Mod_{}", cleaned.replace('.', "_"))
    } else {
        cleaned
    }
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> R<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| io_msg("Could not create folder", e))?;
    }
    let tmp = path.with_extension("tmp7dmm");
    fs::write(&tmp, bytes).map_err(|e| io_msg("Could not write file", e))?;
    fs::rename(&tmp, path).map_err(|e| io_msg("Could not replace file", e))
}

pub fn copy_dir(src: &Path, dst: &Path) -> io::Result<()> {
    for entry in WalkDir::new(src) {
        let entry = entry.map_err(io::Error::other)?;
        let rel = entry.path().strip_prefix(src).map_err(io::Error::other)?;
        let out = dst.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&out)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = out.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(entry.path(), &out)?;
        }
    }
    Ok(())
}

/// Moves a directory, falling back to copy + delete across drives.
pub fn move_dir(src: &Path, dst: &Path) -> io::Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    if let Err(e) = copy_dir(src, dst) {
        let _ = fs::remove_dir_all(dst);
        return Err(e);
    }
    fs::remove_dir_all(src)
}

fn parked(target: &Path) -> PathBuf {
    let mut name = target.as_os_str().to_os_string();
    name.push(OLD_SUFFIX);
    PathBuf::from(name)
}

/// Replaces `target` with `new_src`, restoring the original if the move fails.
pub fn replace_dir(new_src: &Path, target: &Path) -> io::Result<()> {
    if !target.exists() {
        return move_dir(new_src, target);
    }
    let old = parked(target);
    if old.exists() {
        fs::remove_dir_all(&old)?;
    }
    fs::rename(target, &old)?;
    if let Err(e) = move_dir(new_src, target) {
        let _ = fs::remove_dir_all(target);
        let _ = fs::rename(&old, target);
        return Err(e);
    }
    fs::remove_dir_all(&old)
}

/// Finds a direct child file by name, ignoring case.
pub fn find_child_ci(dir: &Path, name: &str) -> Option<PathBuf> {
    fs::read_dir(dir).ok()?.flatten().find_map(|e| {
        let is_file = e.file_type().map(|t| t.is_file()).unwrap_or(false);
        (is_file && e.file_name().to_string_lossy().eq_ignore_ascii_case(name)).then(|| e.path())
    })
}

pub fn modified_secs(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---- zip ----------------------------------------------------------------

pub fn zip_create(path: &Path) -> R<ZipWriter<BufWriter<File>>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| io_msg("Could not create folder", e))?;
    }
    let file = File::create(path).map_err(|e| io_msg("Could not create archive", e))?;
    Ok(ZipWriter::new(BufWriter::new(file)))
}

/// Adds the contents of `src` under `prefix/` (forward slashes, no trailing slash).
pub fn zip_add_dir<W: Write + Seek>(zip: &mut ZipWriter<W>, src: &Path, prefix: &str) -> R<()> {
    let base = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .compression_level(Some(3));
    zip.add_directory(prefix, base).map_err(|e| io_msg("Could not write archive", e))?;
    for entry in WalkDir::new(src).min_depth(1) {
        let entry = entry.map_err(|e| io_msg("Could not read folder", e))?;
        let rel = entry.path().strip_prefix(src).map_err(|e| e.to_string())?;
        let name = format!("{prefix}/{}", rel.to_string_lossy().replace('\\', "/"));
        if entry.file_type().is_dir() {
            zip.add_directory(name, base).map_err(|e| io_msg("Could not write archive", e))?;
        } else if entry.file_type().is_file() {
            let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
            zip.start_file(name, base.large_file(len >= u32::MAX as u64))
                .map_err(|e| io_msg("Could not write archive", e))?;
            let mut f = File::open(entry.path()).map_err(|e| io_msg("Could not read file", e))?;
            io::copy(&mut f, zip).map_err(|e| io_msg("Could not write archive", e))?;
        }
    }
    Ok(())
}

pub fn zip_add_bytes<W: Write + Seek>(zip: &mut ZipWriter<W>, name: &str, bytes: &[u8]) -> R<()> {
    zip.start_file(name, SimpleFileOptions::default()).map_err(|e| io_msg("Could not write archive", e))?;
    zip.write_all(bytes).map_err(|e| io_msg("Could not write archive", e))
}

// ---- extraction ---------------------------------------------------------

pub fn extract_zip(path: &Path, dest: &Path) -> R<()> {
    let file = File::open(path).map_err(|e| io_msg("Could not open archive", e))?;
    let mut archive = ZipArchive::new(BufReader::new(file)).map_err(|e| format!("Not a readable zip file: {e}"))?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| format!("Could not read zip entry: {e}"))?;
        // enclosed_name() returns None for paths that would escape `dest`.
        let Some(rel) = entry.enclosed_name() else { continue };
        if !is_safe_rel(&rel) {
            continue;
        }
        let out = dest.join(rel);
        if entry.is_dir() {
            fs::create_dir_all(&out).map_err(|e| io_msg("Could not extract", e))?;
        } else {
            if let Some(parent) = out.parent() {
                fs::create_dir_all(parent).map_err(|e| io_msg("Could not extract", e))?;
            }
            let mut w = File::create(&out).map_err(|e| io_msg("Could not extract", e))?;
            io::copy(&mut entry, &mut w).map_err(|e| io_msg("Could not extract", e))?;
        }
    }
    Ok(())
}

fn extract_7z(path: &Path, dest: &Path) -> R<()> {
    sevenz_rust2::decompress_file_with_extract_fn(path, dest, |entry, reader, out| {
        if !is_safe_rel(Path::new(entry.name())) {
            io::copy(reader, &mut io::sink())?;
            return Ok(true);
        }
        sevenz_rust2::default_entry_extract_fn(entry, reader, out)
    })
    .map_err(|e| format!("Could not extract 7z archive: {e}"))
}

fn extract_rar(path: &Path, dest: &Path) -> R<()> {
    let mut archive = unrar::Archive::new(path)
        .open_for_processing()
        .map_err(|e| format!("Could not open rar archive: {e}"))?;
    loop {
        let Some(header) = archive.read_header().map_err(|e| format!("Could not read rar archive: {e}"))? else {
            return Ok(());
        };
        let wanted = header.entry().is_file() && is_safe_rel(&header.entry().filename);
        archive = if wanted {
            header.extract_with_base(dest).map_err(|e| format!("Could not extract rar archive: {e}"))?
        } else {
            header.skip().map_err(|e| format!("Could not read rar archive: {e}"))?
        };
    }
}

/// Extracts a zip, 7z or rar file, picking the format from its header bytes.
pub fn extract_archive(path: &Path, dest: &Path) -> R<()> {
    let mut magic = [0u8; 6];
    let mut f = File::open(path).map_err(|e| io_msg("Could not open file", e))?;
    let n = f.read(&mut magic).map_err(|e| io_msg("Could not read file", e))?;
    drop(f);
    fs::create_dir_all(dest).map_err(|e| io_msg("Could not create folder", e))?;
    match &magic[..n] {
        [0x50, 0x4B, ..] => extract_zip(path, dest),
        [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C] => extract_7z(path, dest),
        [0x52, 0x61, 0x72, 0x21, ..] => extract_rar(path, dest),
        _ => Err(format!(
            "{} is not a zip, 7z or rar archive.",
            path.file_name().unwrap_or_default().to_string_lossy()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_paths_that_escape() {
        assert!(!is_safe_rel(Path::new("../evil.dll")));
        assert!(!is_safe_rel(Path::new("a/../../evil.dll")));
        assert!(!is_safe_rel(Path::new("C:\\Windows\\evil.dll")));
        assert!(!is_safe_rel(Path::new("\\evil.dll")));
        assert!(!is_safe_rel(Path::new("ok.txt:stream")));
        assert!(is_safe_rel(Path::new("Mod/Config/items.xml")));
        assert!(!is_safe_name(".."));
        assert!(!is_safe_name("a\\b"));
        assert!(is_safe_name("0-Mod Name"));
    }

    #[test]
    fn sanitizes_folder_names() {
        assert_eq!(sanitize_folder("My: Mod?"), "My_ Mod_");
        assert_eq!(sanitize_folder("CON"), "Mod_CON");
        assert_eq!(sanitize_folder("Name..."), "Name");
    }

    #[test]
    fn replace_dir_swaps_contents() {
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (t.path().join("new"), t.path().join("target"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("n.txt"), "n").unwrap();
        fs::write(b.join("o.txt"), "o").unwrap();
        replace_dir(&a, &b).unwrap();
        assert!(b.join("n.txt").exists());
        assert!(!b.join("o.txt").exists());
        assert!(!parked(&b).exists());
    }
}
