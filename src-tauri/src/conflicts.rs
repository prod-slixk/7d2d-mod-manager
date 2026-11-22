//! Finds mods whose XML patches or identities collide.

use std::collections::{HashMap, HashSet};
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
    /// For csv edits: "add" or "remove", and the separator between entries.
    pub list_op: String,
    pub delim: String,
    /// Inside a condition that depends on a setting, so it may not apply.
    pub maybe: bool,
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

/// Result of a patch condition when only the set of loaded mods is known.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Tri {
    Yes,
    No,
    /// Depends on something unknown here, such as a Gears setting.
    Maybe,
}

/// Evaluates a `cond` attribute such as `mod_loaded('A') and not mod_loaded('B')`.
/// `mod_loaded` is answered from `loaded` (lowercase ModInfo names); anything else is Maybe.
pub fn eval_cond(cond: &str, loaded: &HashSet<String>) -> Tri {
    let tokens = tokenize(cond);
    let mut p = CondParser { t: &tokens, i: 0, loaded };
    match p.or() {
        Some(v) if p.i == tokens.len() => v,
        _ => Tri::Maybe,
    }
}

#[derive(Debug, PartialEq)]
enum Tok {
    Word(String),
    Str(String),
    Sym(&'static str),
}

fn tokenize(s: &str) -> Vec<Tok> {
    let (mut out, chars): (Vec<Tok>, Vec<char>) = (Vec::new(), s.chars().collect());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '\'' || c == '"' {
            let end = chars[i + 1..].iter().position(|&x| x == c).map(|p| i + 1 + p).unwrap_or(chars.len());
            out.push(Tok::Str(chars[i + 1..end.min(chars.len())].iter().collect()));
            i = end + 1;
        } else if chars[i..].starts_with(&['=', '=']) || chars[i..].starts_with(&['!', '=']) {
            out.push(Tok::Sym(if c == '=' { "==" } else { "!=" }));
            i += 2;
        } else if "(),".contains(c) {
            out.push(Tok::Sym(match c { '(' => "(", ')' => ")", _ => "," }));
            i += 1;
        } else {
            let start = i;
            while i < chars.len() && !chars[i].is_whitespace() && !"(),'\"=!".contains(chars[i]) {
                i += 1;
            }
            if i == start {
                // A lone '=' or '!' that isn't part of an operator: unreadable, so give up.
                return vec![Tok::Sym("?")];
            }
            out.push(Tok::Word(chars[start..i].iter().collect()));
        }
    }
    out
}

struct CondParser<'a> {
    t: &'a [Tok],
    i: usize,
    loaded: &'a HashSet<String>,
}

impl CondParser<'_> {
    fn word(&self, w: &str) -> bool {
        matches!(self.t.get(self.i), Some(Tok::Word(x)) if x.eq_ignore_ascii_case(w))
    }
    fn sym(&self, s: &str) -> bool {
        matches!(self.t.get(self.i), Some(Tok::Sym(x)) if *x == s)
    }
    fn or(&mut self) -> Option<Tri> {
        let mut v = self.and()?;
        while self.word("or") {
            self.i += 1;
            let r = self.and()?;
            v = match (v, r) {
                (Tri::Yes, _) | (_, Tri::Yes) => Tri::Yes,
                (Tri::No, Tri::No) => Tri::No,
                _ => Tri::Maybe,
            };
        }
        Some(v)
    }
    fn and(&mut self) -> Option<Tri> {
        let mut v = self.unary()?;
        while self.word("and") {
            self.i += 1;
            let r = self.unary()?;
            v = match (v, r) {
                (Tri::No, _) | (_, Tri::No) => Tri::No,
                (Tri::Yes, Tri::Yes) => Tri::Yes,
                _ => Tri::Maybe,
            };
        }
        Some(v)
    }
    fn unary(&mut self) -> Option<Tri> {
        if self.word("not") {
            self.i += 1;
            return Some(match self.unary()? {
                Tri::Yes => Tri::No,
                Tri::No => Tri::Yes,
                Tri::Maybe => Tri::Maybe,
            });
        }
        self.primary()
    }
    fn primary(&mut self) -> Option<Tri> {
        if self.sym("(") {
            self.i += 1;
            let v = self.or()?;
            if !self.sym(")") {
                return None;
            }
            self.i += 1;
            return Some(v);
        }
        let v = match self.t.get(self.i)? {
            Tok::Word(name) if matches!(self.t.get(self.i + 1), Some(Tok::Sym("("))) => {
                let name = name.to_ascii_lowercase();
                self.i += 2;
                let mut args = Vec::new();
                while !self.sym(")") {
                    match self.t.get(self.i)? {
                        Tok::Str(s) | Tok::Word(s) => args.push(s.clone()),
                        Tok::Sym(",") => {}
                        Tok::Sym(_) => return None,
                    }
                    self.i += 1;
                }
                self.i += 1;
                match (name.as_str(), args.as_slice()) {
                    ("mod_loaded", [m]) => if self.loaded.contains(&m.to_lowercase()) { Tri::Yes } else { Tri::No },
                    _ => Tri::Maybe,
                }
            }
            Tok::Word(_) | Tok::Str(_) => {
                self.i += 1;
                Tri::Maybe
            }
            Tok::Sym(_) => return None,
        };
        // Comparisons such as modsetting(...) == 'Vanilla' depend on values not known here.
        if self.sym("==") || self.sym("!=") {
            self.i += 1;
            self.primary()?;
            return Some(Tri::Maybe);
        }
        Some(v)
    }
}

/// What `collect` needs to follow conditions and `<include>` files.
pub struct Scope<'a> {
    pub loaded: &'a HashSet<String>,
    pub dir: &'a Path,
    pub depth: usize,
    /// Under a branch whose condition depends on a setting.
    pub maybe: bool,
}

fn collect(node: roxmltree::Node, mod_idx: usize, file: &str, scope: &Scope, out: &mut Vec<Edit>) {
    for child in node.children().filter(|n| n.is_element()) {
        let tag = child.tag_name().name().to_ascii_lowercase();
        if tag == "conditional" {
            // The game applies the first branch whose condition holds, else the <else>.
            let mut settled = false;
            for branch in child.children().filter(|n| n.is_element()) {
                if settled {
                    break;
                }
                let maybe = match branch.tag_name().name().to_ascii_lowercase().as_str() {
                    "if" => match eval_cond(branch.attribute("cond").unwrap_or(""), scope.loaded) {
                        Tri::Yes => {
                            settled = true;
                            false
                        }
                        Tri::No => continue,
                        Tri::Maybe => true,
                    },
                    "else" => {
                        settled = true;
                        false
                    }
                    _ => continue,
                };
                let inner = Scope { maybe: scope.maybe || maybe, ..*scope };
                collect(branch, mod_idx, file, &inner, out);
            }
            continue;
        }
        if tag == "if" || tag == "else" {
            collect(child, mod_idx, file, &Scope { maybe: true, ..*scope }, out);
            continue;
        }
        if tag == "include" {
            // Included patches act on the including file, the same way the game reads them.
            let Some(target) = child.attribute("filename").map(|f| scope.dir.join(f)) else { continue };
            if scope.depth >= 8 {
                continue;
            }
            let text = fs::read(&target).map(|b| String::from_utf8_lossy(&b).to_string()).unwrap_or_default();
            if let Ok(doc) = roxmltree::Document::parse(text.trim_start_matches('\u{feff}').trim_start()) {
                let dir = target.parent().unwrap_or(scope.dir);
                collect(doc.root_element(), mod_idx, file, &Scope { dir, depth: scope.depth + 1, ..*scope }, out);
            }
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
        let list_op = child.attribute("op").unwrap_or("add").trim().to_ascii_lowercase();
        let delim = child.attribute("delim").filter(|d| !d.is_empty()).unwrap_or(",").to_string();
        out.push(Edit { mod_idx, file: file.to_string(), xpath, op: tag, value, list_op, delim, maybe: scope.maybe });
    }
}

/// Parses every XML patch under a mod's Config folder. Returns unparseable files too.
/// `loaded` holds the lowercase ModInfo names of every enabled mod, for `mod_loaded()` conditions.
pub fn parse_mod_config(mod_dir: &Path, mod_idx: usize, loaded: &HashSet<String>) -> (Vec<Edit>, Vec<String>) {
    let (mut edits, mut bad) = (Vec::new(), Vec::new());
    let Some(config) = fs::read_dir(mod_dir).ok().and_then(|rd| {
        rd.flatten().find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case("config")).map(|e| e.path())
    }) else {
        return (edits, bad);
    };
    let files: Vec<(std::path::PathBuf, String)> = WalkDir::new(&config)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("xml")))
        .map(|e| (e.path().to_path_buf(), String::from_utf8_lossy(&fs::read(e.path()).unwrap_or_default()).to_string()))
        .collect();
    // Files pulled in through <include> are read as part of the file that includes them.
    let include = regex::Regex::new(r#"<include\s+filename\s*=\s*"([^"]+)""#).unwrap();
    let key = |p: &Path| p.to_string_lossy().replace('\\', "/").to_lowercase();
    let mut included: HashSet<String> = HashSet::new();
    for (path, text) in &files {
        let dir = path.parent().unwrap_or(&config);
        for c in include.captures_iter(text) {
            included.insert(key(&normalize_path(&dir.join(&c[1]))));
        }
    }
    for (path, text) in &files {
        if included.contains(&key(path)) {
            continue;
        }
        let rel = path.strip_prefix(&config).unwrap_or(path).to_string_lossy().replace('\\', "/").to_lowercase();
        match roxmltree::Document::parse(text.trim_start_matches('\u{feff}').trim_start()) {
            Ok(doc) => {
                let scope = Scope { loaded, dir: path.parent().unwrap_or(&config), depth: 0, maybe: false };
                collect(doc.root_element(), mod_idx, &rel, &scope, &mut edits);
            }
            Err(_) => bad.push(rel),
        }
    }
    (edits, bad)
}

/// Resolves `..` and `.` without touching the disk.
fn normalize_path(p: &Path) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
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
        // List edits that only add entries stack up, so load order doesn't matter. They only
        // disagree when one mod removes an entry another mod adds.
        let mut list_clash = Vec::new();
        if writers.iter().all(|e| e.op == "csv") {
            let entries = |e: &Edit| -> Vec<String> { e.value.split(e.delim.as_str()).map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect() };
            for r in writers.iter().filter(|e| e.list_op == "remove") {
                for a in writers.iter().filter(|e| e.list_op != "remove" && e.mod_idx != r.mod_idx) {
                    let added = entries(a);
                    list_clash.extend(entries(r).into_iter().filter(|x| added.contains(x)));
                }
            }
            list_clash.sort();
            list_clash.dedup();
            if list_clash.is_empty() {
                continue;
            }
        }
        // Mods that write the same value don't actually disagree.
        if !removes && !csv && writers.iter().all(|e| e.value == writers[0].value) {
            continue;
        }
        let winner = mods[*owners.last().unwrap()].title.clone();
        let (severity, kind, title, detail) = if removes {
            let maybe = if writers.iter().any(|e| e.maybe) { " Part of this depends on a mod setting, so it may not happen in your game." } else { "" };
            ("high", "remove", "One mod removes what another changes", format!("Whichever runs later may fail. {winner} loads last.{maybe}"))
        } else if csv {
            let detail = if list_clash.is_empty() {
                "Results depend on load order. Check that the combination does what you expect.".to_string()
            } else {
                let shown: Vec<&str> = list_clash.iter().take(5).map(String::as_str).collect();
                format!("One mod removes entries another adds ({}). {winner} loads last, so its edit decides.", shown.join(", "))
            };
            ("medium", "csv", "Mods edit the same list", detail)
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
        // Mods that load before the remover lose their edits outright. Mods that load after it
        // edit whatever replaced the node, which only works if the new version is built the same way.
        let names = |list: Vec<usize>| {
            let mut list = list;
            list.sort_unstable();
            list.dedup();
            list.iter().map(|i| mods[*i].title.clone()).collect::<Vec<_>>().join(", ")
        };
        let before = names(others.iter().filter(|e| e.mod_idx < rem.mod_idx).map(|e| e.mod_idx).collect());
        let after = names(others.iter().filter(|e| e.mod_idx > rem.mod_idx).map(|e| e.mod_idx).collect());
        let remover = &mods[rem.mod_idx].title;
        let mut detail = format!("{remover} removes this node.");
        if !before.is_empty() {
            detail.push_str(&format!(" {before} loads earlier, so its changes here are wiped out."));
        }
        if !after.is_empty() {
            detail.push_str(&format!(" {after} loads later and edits {remover}'s version, which only works if it's built the same way."));
        }
        if rem.maybe {
            detail.push_str(&format!(" This depends on a mod setting (for example in Gears), so it may not happen in your game."));
        }
        issues.push(Issue {
            severity: if rem.maybe { "medium" } else { "high" }.into(),
            kind: "remove-parent".into(),
            title: "One mod removes something another edits inside".into(),
            detail,
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
    let loaded: HashSet<String> = enabled.iter().map(|m| m.name.to_lowercase()).collect();

    for (i, m) in enabled.iter().enumerate() {
        let (found, bad) = parse_mod_config(Path::new(&m.path), i, &loaded);
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
    fn evaluates_patch_conditions_from_loaded_mods() {
        let loaded: HashSet<String> = ["zsmxhud", "quartz"].iter().map(|s| s.to_string()).collect();
        let e = |c: &str| eval_cond(c, &loaded);
        assert_eq!(e("mod_loaded('Quartz')"), Tri::Yes, "names compare case-insensitively");
        assert_eq!(e("mod_loaded('SMX26LegacyUi')"), Tri::No);
        assert_eq!(e("not mod_loaded('SMX26LegacyUi')"), Tri::Yes);
        assert_eq!(e("mod_loaded('Quartz') and not mod_loaded(\"Gears\")"), Tri::Yes);
        assert_eq!(e("mod_loaded('A') or mod_loaded('B')"), Tri::No);
        // SMX's real conditions: without Gears the setting can't apply, so the branch is certain.
        let smx = "not mod_loaded('Gears') or modsetting('zSMXui', 'global', 'Windows.Inventory.Inventory') != 'Vanilla'";
        assert_eq!(e(smx), Tri::Yes);
        let with_gears: HashSet<String> = ["gears".to_string()].into_iter().collect();
        assert_eq!(eval_cond(smx, &with_gears), Tri::Maybe, "with Gears it depends on the setting");
        assert_eq!(e("mod_loaded('zSMXhud') and (not mod_loaded('Gears') or modsetting('SMXcore', 'global', 'X') == 'True')"), Tri::Yes);
        assert_eq!(e("mod_loaded('Nope') and modsetting('a', 'b', 'c') == 'x'"), Tri::No, "a false side settles an and");
        assert_eq!(e("gamepref('x') == 1"), Tri::Maybe);
        assert_eq!(e("((("), Tri::Maybe, "unreadable conditions are treated as unknown");
        assert_eq!(e(""), Tri::Maybe);
    }

    fn write(root: &Path, folder: &str, rel: &str, xml: &str) {
        let file = root.join(folder).join("Config").join(rel);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, xml).unwrap();
    }

    #[test]
    fn branches_for_mods_that_are_not_installed_are_ignored() {
        let t = tempfile::tempdir().unwrap();
        let a = make(t.path(), "A", 0, r#"<configs><set xpath="/items/item[@name='gun']/property[@name='x']/@value">1</set></configs>"#);
        let b = make(
            t.path(),
            "B",
            1,
            r#"<configs><conditional><if cond="mod_loaded('Legacy')"><remove xpath="/items/item[@name='gun']"/></if></conditional></configs>"#,
        );
        assert!(analyze(&[a.clone(), b.clone()]).issues.is_empty(), "the remove only runs with Legacy installed");
        let legacy = make(t.path(), "Legacy", 2, "<configs/>");
        let r = analyze(&[a, b, legacy]);
        assert_eq!(r.issues.len(), 1);
        assert_eq!(r.issues[0].kind, "remove-parent");
        assert!(r.issues[0].detail.contains("A loads earlier"), "{}", r.issues[0].detail);
    }

    #[test]
    fn included_files_count_as_part_of_the_file_that_includes_them() {
        let t = tempfile::tempdir().unwrap();
        // Like SMXcore: windows.xml pulls the real change in from a sub-folder, behind a setting.
        write(t.path(), "Core", "XUi_InGame/windows.xml", r#"<windows><conditional><if cond="not mod_loaded('Gears') or modsetting('Core', 'global', 'Look') != 'Vanilla'"><include filename="sub/inv.xml"/></if></conditional></windows>"#);
        write(t.path(), "Core", "XUi_InGame/sub/inv.xml", r#"<windows><remove xpath="/windows/window[@name='backpack']"/></windows>"#);
        // A compatibility file that is only included for a mod that isn't installed.
        write(t.path(), "Core", "XUi_InGame/windows_extra.xml", r#"<windows><conditional><if cond="mod_loaded('MapMod')"><include filename="compat/map.xml"/></if></conditional></windows>"#);
        write(t.path(), "Core", "XUi_InGame/compat/map.xml", r#"<windows><remove xpath="/windows/window[@name='map']"/></windows>"#);
        write(t.path(), "Before", "XUi_InGame/windows.xml", r#"<windows><append xpath="/windows/window[@name='backpack']/rect"><button name="b"/></append><set xpath="/windows/window[@name='map']/@pos">1</set></windows>"#);
        let before = entry(&t.path().join("Before"), "Before", 0);
        let core = entry(&t.path().join("Core"), "Core", 1);

        let r = analyze(&[before.clone(), core.clone()]);
        assert_eq!(r.issues.len(), 1, "only the backpack: the map compat file isn't active; {:?}", r.issues.iter().map(|i| &i.xpath).collect::<Vec<_>>());
        let i = &r.issues[0];
        assert_eq!(i.file.as_deref(), Some("xui_ingame/windows.xml"), "included edits act on the including file");
        assert_eq!(i.severity, "high", "without Gears the setting can't keep the vanilla window");

        let gears = make(t.path(), "Gears", 2, "<configs/>");
        let r = analyze(&[before, core, gears]);
        assert_eq!(r.issues[0].severity, "medium");
        assert!(r.issues[0].detail.contains("depends on a mod setting"), "{}", r.issues[0].detail);
    }

    #[test]
    fn list_additions_from_several_mods_are_not_a_conflict() {
        let t = tempfile::tempdir().unwrap();
        let xp = "/items/item[@name='book']/property[@name='Unlocks']/@value";
        let a = make(t.path(), "A", 0, &format!(r#"<configs><csv xpath="{xp}" delim="," op="add">modA1,modA2</csv></configs>"#));
        let b = make(t.path(), "B", 1, &format!(r#"<configs><csv xpath="{xp}" delim="," op="add">modB1</csv></configs>"#));
        assert!(analyze(&[a.clone(), b.clone()]).issues.is_empty(), "both lists of additions apply");

        let c = make(t.path(), "C", 2, &format!(r#"<configs><csv xpath="{xp}" delim="," op="remove">modZ</csv></configs>"#));
        assert!(analyze(&[a.clone(), c]).issues.is_empty(), "removing an entry nobody adds is harmless");

        let d = make(t.path(), "D", 3, &format!(r#"<configs><csv xpath="{xp}" delim="," op="remove">modA2</csv></configs>"#));
        let r = analyze(&[a, b, d]);
        assert_eq!(r.issues.len(), 1, "removing another mod's entry depends on load order");
        assert!(r.issues[0].detail.contains("modA2"));
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
