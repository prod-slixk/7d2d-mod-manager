//! Finds mods whose XML patches or identities collide.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use serde::Serialize;
use walkdir::WalkDir;

use crate::mods::ModEntry;

#[derive(Serialize, Debug, Clone)]
pub struct Part {
    pub title: String,
    pub folder: String,
    pub op: String,
    pub value: String,
}

#[derive(Serialize, Debug, Clone)]
pub struct Issue {
    /// "high", "medium" or "low"
    pub severity: String,
    pub kind: String,
    pub title: String,
    pub detail: String,
    pub file: Option<String>,
    pub xpath: Option<String>,
    pub parts: Vec<Part>,
    pub winner: Option<String>,
}

#[derive(Serialize, Debug, Default)]
pub struct Report {
    pub issues: Vec<Issue>,
    pub mods: usize,
    pub edits: usize,
}

#[derive(Clone, Debug)]
pub struct Edit {
    pub mod_idx: usize,
    pub file: String,
    pub xpath: String,
    pub op: String,
    pub value: String,
}

/// Normalizes spacing and quote style so equal xpaths compare equal.
pub fn normalize_xpath(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut quote: Option<char> = None;
    for c in raw.trim().chars() {
        match quote {
            Some(q) if c == q => {
                quote = None;
                out.push('\'');
            }
            Some(_) => out.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                out.push('\'');
            }
            None if c.is_whitespace() => {}
            None => out.push(c),
        }
    }
    out.trim_end_matches('/').to_string()
}

/// Byte offsets of each `/` that separates path steps, ignoring `/` inside predicates.
fn step_ends(xpath: &str) -> Vec<usize> {
    let (mut depth, mut quoted, mut ends) = (0i32, false, Vec::new());
    for (i, c) in xpath.char_indices() {
        match c {
            '\'' => quoted = !quoted,
            '[' if !quoted => depth += 1,
            ']' if !quoted => depth -= 1,
            '/' if !quoted && depth == 0 && i > 0 => ends.push(i),
            _ => {}
        }
    }
    ends
}

const OPS: [&str; 8] = ["set", "append", "insertafter", "insertbefore", "remove", "setattribute", "removeattribute", "csv"];

fn collect(node: roxmltree::Node, mod_idx: usize, file: &str, out: &mut Vec<Edit>) {
    for child in node.children().filter(|n| n.is_element()) {
        let tag = child.tag_name().name().to_ascii_lowercase();
        if ["conditional", "if", "else"].contains(&tag.as_str()) {
            collect(child, mod_idx, file, out);
            continue;
        }
        let (true, Some(xpath)) = (OPS.contains(&tag.as_str()), child.attribute("xpath")) else { continue };
        let mut xpath = normalize_xpath(xpath);
        // setattribute acts on a named attribute, which is the real target.
        if tag == "setattribute" || tag == "removeattribute" {
            if let Some(name) = child.attribute("name") {
                xpath = format!("{xpath}/@{name}");
            }
        }
        let value = child.text().unwrap_or("").trim().to_string();
        out.push(Edit { mod_idx, file: file.to_string(), xpath, op: tag, value });
    }
}

/// Parses every XML patch under a mod's Config folder. Returns unparseable files too.
pub fn parse_mod_config(mod_dir: &Path, mod_idx: usize) -> (Vec<Edit>, Vec<String>) {
    let (mut edits, mut bad) = (Vec::new(), Vec::new());
    let Some(config) = fs::read_dir(mod_dir).ok().and_then(|rd| {
        rd.flatten().find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case("config")).map(|e| e.path())
    }) else {
        return (edits, bad);
    };
    for entry in WalkDir::new(&config).into_iter().flatten() {
        let is_xml = entry.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("xml"));
        if !entry.file_type().is_file() || !is_xml {
            continue;
        }
        let rel = entry.path().strip_prefix(&config).unwrap_or(entry.path()).to_string_lossy().replace('\\', "/").to_lowercase();
        let bytes = fs::read(entry.path()).unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes);
        match roxmltree::Document::parse(text.trim_start_matches('\u{feff}').trim_start()) {
            Ok(doc) => collect(doc.root_element(), mod_idx, &rel, &mut edits),
            Err(_) => bad.push(rel),
        }
    }
    (edits, bad)
}

fn part(m: &ModEntry, e: &Edit) -> Part {
    let value: String = e.value.chars().take(120).collect();
    Part { title: m.title.clone(), folder: m.folder.clone(), op: e.op.clone(), value }
}

const WRITES: [&str; 5] = ["set", "setattribute", "remove", "removeattribute", "csv"];

/// Compares patch edits. `mods` must be in load order.
pub fn compare(mods: &[ModEntry], edits: &[Edit]) -> Vec<Issue> {
    let mut groups: HashMap<(&str, &str), Vec<&Edit>> = HashMap::new();
    for e in edits {
        groups.entry((&e.file, &e.xpath)).or_default().push(e);
    }
    let mut issues = Vec::new();

    for ((file, xpath), group) in &groups {
        let writers: Vec<&&Edit> = group.iter().filter(|e| WRITES.contains(&e.op.as_str())).collect();
        let mut owners: Vec<usize> = writers.iter().map(|e| e.mod_idx).collect();
        owners.sort_unstable();
        owners.dedup();
        if owners.len() < 2 {
            continue;
        }
        let removes = writers.iter().any(|e| e.op == "remove" || e.op == "removeattribute");
        let csv = writers.iter().any(|e| e.op == "csv");
        // Mods that write the same value don't actually disagree.
        if !removes && !csv && writers.iter().all(|e| e.value == writers[0].value) {
            continue;
        }
        let winner = mods[*owners.last().unwrap()].title.clone();
        let (severity, kind, title, detail) = if removes {
            ("high", "remove", "One mod removes what another changes", format!("Whichever runs later may fail. {winner} loads last."))
        } else if csv {
            ("medium", "csv", "Mods edit the same list", "Results depend on load order. Check that the combination does what you expect.".to_string())
        } else {
            ("medium", "override", "Mods set the same value", format!("Only one value can apply. {winner} loads last, so its value wins."))
        };
        issues.push(Issue {
            severity: severity.into(),
            kind: kind.into(),
            title: title.into(),
            detail,
            file: Some(file.to_string()),
            xpath: Some(xpath.to_string()),
            parts: writers.iter().map(|e| part(&mods[e.mod_idx], e)).collect(),
            winner: (!removes).then_some(winner),
        });
    }

    // A removed node takes every edit beneath it down too.
    let mut by_prefix: HashMap<(&str, &str), Vec<&Edit>> = HashMap::new();
    for e in edits {
        for end in step_ends(&e.xpath) {
            by_prefix.entry((&e.file, &e.xpath[..end])).or_default().push(e);
        }
    }
    for rem in edits.iter().filter(|e| e.op == "remove") {
        let Some(children) = by_prefix.get(&(rem.file.as_str(), rem.xpath.as_str())) else { continue };
        let others: Vec<&&Edit> = children.iter().filter(|c| c.mod_idx != rem.mod_idx).collect();
        if others.is_empty() {
            continue;
        }
        let mut parts = vec![part(&mods[rem.mod_idx], rem)];
        parts.extend(others.iter().map(|e| part(&mods[e.mod_idx], e)));
        issues.push(Issue {
            severity: "high".into(),
            kind: "remove-parent".into(),
            title: "One mod removes something another edits inside".into(),
            detail: format!("{} removes this node, so edits to anything inside it can fail.", mods[rem.mod_idx].title),
            file: Some(rem.file.clone()),
            xpath: Some(rem.xpath.clone()),
            parts,
            winner: None,
        });
    }
    issues
}

fn simple(severity: &str, kind: &str, title: String, detail: String, parts: Vec<Part>) -> Issue {
    Issue { severity: severity.into(), kind: kind.into(), title, detail, file: None, xpath: None, parts, winner: None }
}

fn plain_part(m: &ModEntry) -> Part {
    Part { title: m.title.clone(), folder: m.folder.clone(), op: String::new(), value: String::new() }
}

pub fn analyze(mods: &[ModEntry]) -> Report {
    let mut enabled: Vec<ModEntry> = mods.iter().filter(|m| m.enabled && m.problem.is_none()).cloned().collect();
    enabled.sort_by_key(|m| m.load_index);

    let mut edits = Vec::new();
    let mut issues = Vec::new();

    for (i, m) in enabled.iter().enumerate() {
        let (found, bad) = parse_mod_config(Path::new(&m.path), i);
        edits.extend(found);
        for file in bad {
            issues.push(simple(
                "high",
                "bad-xml",
                format!("{}: broken XML in {file}", m.title),
                "The game can't apply patches from this file until it's fixed.".into(),
                vec![plain_part(m)],
            ));
        }
        if m.malformed {
            issues.push(simple("high", "bad-modinfo", format!("{}: ModInfo.xml is malformed", m.title), "The game may refuse to load this mod.".into(), vec![plain_part(m)]));
        }
        if m.legacy {
            issues.push(simple(
                "low",
                "legacy",
                format!("{}: old ModInfo format", m.title),
                "This mod uses the old ModInfo layout, so it was likely built for an older game version and may not work.".into(),
                vec![plain_part(m)],
            ));
        }
    }

    let mut names: HashMap<String, Vec<&ModEntry>> = HashMap::new();
    for m in &enabled {
        names.entry(m.name.to_lowercase()).or_default().push(m);
    }
    for dup in names.values().filter(|v| v.len() > 1) {
        issues.push(simple(
            "high",
            "duplicate",
            format!("Duplicate mod: {}", dup[0].name),
            "Two enabled folders use the same internal name. The game will only load one. Disable the extra copy.".into(),
            dup.iter().map(|m| plain_part(m)).collect(),
        ));
    }

    for m in mods.iter().filter(|m| m.enabled && m.problem.is_some()) {
        let (title, detail) = if m.problem.as_deref() == Some("nested") {
            (format!("{}: mod is nested one folder too deep", m.folder), "The game won't see it. Use Fix folder in the Mods list.".to_string())
        } else {
            (format!("{}: no ModInfo.xml", m.folder), "The game ignores folders without a ModInfo.xml.".to_string())
        };
        issues.push(simple("high", "invalid", title, detail, vec![plain_part(m)]));
    }

    issues.extend(compare(&enabled, &edits));
    let rank = |s: &str| match s {
        "high" => 0,
        "medium" => 1,
        _ => 2,
    };
    issues.sort_by(|a, b| (rank(&a.severity), &a.title, &a.xpath).cmp(&(rank(&b.severity), &b.title, &b.xpath)));
    Report { issues, mods: enabled.len(), edits: edits.len() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &Path, folder: &str, idx: usize) -> ModEntry {
        ModEntry {
            id: folder.into(),
            root: "game".into(),
            folder: folder.into(),
            enabled: true,
            path: path.to_string_lossy().into(),
            name: folder.into(),
            title: folder.into(),
            version: "1".into(),
            author: String::new(),
            description: String::new(),
            website: String::new(),
            legacy: false,
            malformed: false,
            has_dll: false,
            core: false,
            size: 0,
            files: 0,
            modified: 0,
            problem: None,
            nested: None,
            load_index: Some(idx),
            nexus: None,
            update: false,
        }
    }

    fn make(root: &Path, folder: &str, idx: usize, xml: &str) -> ModEntry {
        let dir = root.join(folder);
        fs::create_dir_all(dir.join("Config")).unwrap();
        fs::write(dir.join("Config").join("items.xml"), xml).unwrap();
        entry(&dir, folder, idx)
    }

    #[test]
    fn normalizes_quotes_and_space() {
        assert_eq!(normalize_xpath(r#"/items/item[ @name="a b" ]/"#), "/items/item[@name='a b']");
    }

    #[test]
    fn ignores_slashes_inside_predicates() {
        let p = "/items/item[@name='a/b']/property";
        assert_eq!(step_ends(p).iter().map(|e| &p[..*e]).collect::<Vec<_>>(), vec!["/items", "/items/item[@name='a/b']"]);
    }

    #[test]
    fn last_loaded_mod_wins_a_set_conflict() {
        let t = tempfile::tempdir().unwrap();
        let a = make(t.path(), "A", 0, r#"<configs><set xpath="/items/item[@name='gun']/@value">1</set></configs>"#);
        let b = make(t.path(), "B", 1, r#"<configs><set xpath='/items/item[@name="gun"]/@value'>2</set></configs>"#);
        let r = analyze(&[a, b]);
        assert_eq!(r.issues.len(), 1);
        assert_eq!(r.issues[0].kind, "override");
        assert_eq!(r.issues[0].winner.as_deref(), Some("B"));
    }

    #[test]
    fn identical_values_and_appends_are_not_conflicts() {
        let t = tempfile::tempdir().unwrap();
        let a = make(t.path(), "A", 0, r#"<configs><set xpath="/a/b">1</set><append xpath="/a/c"><x/></append></configs>"#);
        let b = make(t.path(), "B", 1, r#"<configs><set xpath="/a/b">1</set><append xpath="/a/c"><y/></append></configs>"#);
        assert!(analyze(&[a, b]).issues.is_empty());
    }

    #[test]
    fn remove_of_parent_flags_child_edit() {
        let t = tempfile::tempdir().unwrap();
        let a = make(t.path(), "A", 0, r#"<configs><remove xpath="/items/item[@name='x']"/></configs>"#);
        let b = make(t.path(), "B", 1, r#"<configs><set xpath="/items/item[@name='x']/property/@value">5</set></configs>"#);
        let r = analyze(&[a, b]);
        assert_eq!(r.issues.len(), 1);
        assert_eq!(r.issues[0].kind, "remove-parent");
        assert_eq!(r.issues[0].severity, "high");
    }

    #[test]
    fn looks_inside_conditionals() {
        let t = tempfile::tempdir().unwrap();
        let a = make(t.path(), "A", 0, r#"<configs><conditional><if cond="x"><set xpath="/a">1</set></if></conditional></configs>"#);
        let b = make(t.path(), "B", 1, r#"<configs><set xpath="/a">2</set></configs>"#);
        assert_eq!(analyze(&[a, b]).issues.len(), 1);
    }

    #[test]
    fn reports_broken_xml_and_duplicates() {
        let t = tempfile::tempdir().unwrap();
        let a = make(t.path(), "A", 0, "<configs><set");
        let mut b = make(t.path(), "B", 1, "<configs/>");
        b.name = "A".into();
        let kinds: Vec<String> = analyze(&[a, b]).issues.into_iter().map(|i| i.kind).collect();
        assert!(kinds.contains(&"bad-xml".to_string()));
        assert!(kinds.contains(&"duplicate".to_string()));
    }
}
