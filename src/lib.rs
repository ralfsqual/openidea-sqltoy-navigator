//! SqlToy Navigator for OpenIDEA.
//!
//! Java code references SqlToy SQL statements by string id —
//! `sqlToyLazyDao.findBySql("app_order_list", …)` — while the SQL itself
//! lives in `*.sql.xml` files as `<sql id="app_order_list">`. This plugin
//! indexes both sides so Go to Definition, Ctrl/⌘-Click, gutter markers,
//! Find Usages and Rename resolve across the Java/XML boundary, completes
//! sqlIds inside the DAO-call string, and reports ids that match no XML
//! definition as errors.

use openidea_plugin::indexing::{Anchor, DocumentIndexer, Document as IndexDoc, Edges};
use openidea_plugin::providers::{CompletionItem, CompletionKind, Document, LineMarker, Providers};
use openidea_plugin::text::{offset, position};
use openidea_plugin::{
    Diagnostic, Error, Invocation, Location, LogLevel, NotificationLevel, Plugin, Position, Project, Range, Severity,
    TextQuery, host, register_plugin,
};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// Every anchor key this plugin publishes starts with this prefix.
const KEY_PREFIX: &str = "sqltoy#";
/// How long a search-derived lookup is trusted.
const CACHE_TTL: Duration = Duration::from_secs(30);
/// Upper bounds for host text searches (kept well under indexer/provider limits).
const MAX_XML_HITS: u32 = 64;
const MAX_JAVA_HITS: u32 = 4000;
const MAX_COMPLETIONS: u32 = 500;

fn range_of(text: &str, start: usize, end: usize) -> Range {
    Range::new(position(text, start), position(text, end))
}

fn ident_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
}

// ---------------------------------------------------------------------------
// XML side: `<sql id="…">` definitions in SqlToy sql files.
// ---------------------------------------------------------------------------

/// One `<sql id="…">` definition: the id and where the id value is written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqlDef {
    pub name: String,
    pub range: Range,
}

/// Parsed SqlToy XML.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SqlToyXml {
    pub defs: Vec<SqlDef>,
}

/// A file is treated as SqlToy SQL definitions when its name follows the
/// framework convention (`*.sql.xml`) or its text has the `<sqltoy>` root tag.
pub fn is_sqltoy_xml(path: &str, text: &str) -> bool {
    path.rsplit(['/', '\\']).next().is_some_and(|f| f.ends_with(".sql.xml")) || text.contains("<sqltoy")
}

/// Extracts `attr="value"` ranges (byte offsets of the value, quotes excluded)
/// inside the tag starting at `tag_start` (just past `<`).
fn tag_attr(text: &str, tag_start: usize, tag_end: usize, attr: &str) -> Option<(usize, usize)> {
    let tag = &text[tag_start..tag_end];
    let needle = format!("{attr}=\"");
    let i = tag.find(&needle)? + attr.len() + 2;
    let j = tag[i..].find('"')? + i;
    Some((tag_start + i, tag_start + j))
}

pub fn parse_xml(text: &str) -> SqlToyXml {
    let mut out = SqlToyXml::default();
    let bytes = text.as_bytes();
    let mut at = 0;
    while let Some(i) = text[at..].find('<') {
        let start = at + i + 1;
        at = start;
        if start >= bytes.len() {
            break;
        }
        let name: String = text[start..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .collect();
        // Only `<sql …>` defines a statement (`<sqltoy>`, `<sql-extract>` and
        // friends are different tags; the identifier scan stops at `-`).
        if name != "sql" {
            continue;
        }
        // The tag's attribute area ends at the closing `>` of the open tag.
        let tag_end = text[start..]
            .find('>')
            .map_or(text.len(), |k| start + k);
        let Some((s, e)) = tag_attr(text, start, tag_end, "id") else {
            continue;
        };
        let id = &text[s..e];
        if !id.is_empty() {
            out.defs.push(SqlDef { name: id.to_owned(), range: range_of(text, s, e) });
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Java side: first string argument of a `…BySql(…)` DAO call.
// ---------------------------------------------------------------------------

/// One sqlId reference in Java: the id and where the id text is (quotes
/// excluded — anchors must cover exactly the name).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqlIdRef {
    pub name: String,
    pub range: Range,
}

/// The index just past the string/char literal opening at `bytes[from]`.
fn skip_literal(bytes: &[u8], from: usize) -> usize {
    let quote = bytes[from];
    let mut i = from + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            c if c == quote => return i + 1,
            _ => i += 1,
        }
    }
    i
}

/// The index just past a `//…` or `/*…*/` comment opening at `bytes[from]`.
fn skip_comment(bytes: &[u8], from: usize) -> usize {
    if bytes.get(from + 1) == Some(&b'/') {
        let mut i = from;
        while i < bytes.len() && bytes[i] != b'\n' {
            i += 1;
        }
        i
    } else {
        let mut i = from + 2;
        while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
            i += 1;
        }
        i + 2
    }
}

/// Finds every `…BySql("id"` call argument: a method whose name ends in
/// `BySql` (matching the SqlToy DAO API family: `findBySql`, `getBySql`,
/// `findTopBySql`, `updateBySql`, …) followed by a string literal first
/// argument. Strings and comments are skipped, so matches inside them never
/// count.
pub fn parse_java(text: &str) -> Vec<SqlIdRef> {
    let bytes = text.as_bytes();
    let needle = b"Sql";
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' | b'\'' => {
                i = skip_literal(bytes, i);
                continue;
            }
            b'/' => {
                if matches!(bytes.get(i + 1), Some(b'/') | Some(b'*')) {
                    i = skip_comment(bytes, i);
                    continue;
                }
                i += 1;
                continue;
            }
            _ => {}
        }
        if bytes[i..].starts_with(needle)
            && (i == 0 || ident_byte(bytes[i - 1]))
            && bytes.get(i + needle.len()).is_some_and(|&c| c == b'(')
        {
            let mut j = i + needle.len();
            // j now points at '('
            let mut k = j + 1;
            // Scan arguments for the first string literal.
            let mut found: Option<(usize, usize)> = None;
            let mut depth = 1;
            while k < bytes.len() && depth > 0 {
                // Skip whitespace
                while k < bytes.len() && (bytes[k] as char).is_whitespace() {
                    k += 1;
                }
                if k >= bytes.len() {
                    break;
                }
                let c = bytes[k];
                if c == b')' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    k += 1;
                } else if c == b'(' {
                    depth += 1;
                    k += 1;
                } else if c == b',' {
                    k += 1;
                } else if c == b'"' || c == b'\'' {
                    let quote = c;
                    let start = k + 1;
                    let mut e = start;
                    while e < bytes.len() && bytes[e] != quote && bytes[e] != b'\n' {
                        e += 1;
                    }
                    if e < bytes.len() && bytes[e] == quote {
                        found = Some((start, e));
                    }
                    break;
                } else {
                    // Skip non-string argument: advance until comma or matching paren.
                    let mut arg_depth = 0;
                    while k < bytes.len() {
                        let ch = bytes[k];
                        if ch == b'(' {
                            arg_depth += 1;
                        } else if ch == b')' {
                            if arg_depth == 0 {
                                break;
                            }
                            arg_depth -= 1;
                        } else if ch == b',' && arg_depth == 0 {
                            break;
                        } else if ch == b'"' || ch == b'\'' {
                            // Skip string literal inside argument.
                            let q = ch;
                            k += 1;
                            while k < bytes.len() && bytes[k] != q && bytes[k] != b'\n' {
                                k += 1;
                            }
                            if k < bytes.len() {
                                k += 1;
                            }
                            continue;
                        }
                        k += 1;
                    }
                }
            }
            if let Some((start, e)) = found {
                let id = &text[start..e];
                let plausible =
                    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
                if plausible {
                    out.push(SqlIdRef { name: id.to_owned(), range: range_of(text, start, e) });
                }
                i = e + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn contains(r: Range, p: Position) -> bool {
    r.start <= p && p <= r.end
}

/// Escapes regex metacharacters in a literal id.
fn regex_escape(s: &str) -> String {
    s.chars()
        .flat_map(|c| matches!(c, '.' | '$' | '(' | ')' | '[' | ']' | '{' | '}' | '*' | '+' | '?' | '|' | '^' | '\\' | '"')
            .then_some('\\')
            .into_iter()
            .chain([c]))
        .collect()
}

fn project_of(path: &str) -> Option<Project> {
    host::projects()
        .ok()?
        .into_iter()
        .filter(|p| p.root().is_ok_and(|r| path.starts_with(&r)))
        .max_by_key(|p| p.root().map(|r| r.len()).unwrap_or(0))
}

fn sql_def_query(id: &str) -> TextQuery {
    TextQuery {
        pattern: format!(r#"<sql\s+id\s*=\s*["']{}["']"#, regex_escape(id)),
        regex: true,
        case_sensitive: true,
        include: vec!["**/*.sql.xml".into()],
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// The plugin.
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct Navigator {
    /// project root → sqlId → (when looked up, its XML definition, if any).
    def_index: BTreeMap<String, BTreeMap<String, (Instant, Option<Location>)>>,
    /// project root → (when looked up, id → Java use site for every `BySql` call).
    uses_index: BTreeMap<String, (Instant, Vec<(String, Location)>)>,
    /// project root → sqlId → (when looked up, whether a definition exists).
    exists_index: BTreeMap<String, BTreeMap<String, (Instant, bool)>>,
    /// file path → sqlId references discovered during indexing (avoids search_text in line_markers).
    java_refs: BTreeMap<String, Vec<(String, Location)>>,
}

impl Navigator {
    /// The XML definition of `id`, via a host text search (cached per project).
    fn xml_def_for(&mut self, project: &Project, id: &str) -> Option<Location> {
        let root = project.root().ok()?;
        let cache = self.def_index.entry(root.clone()).or_default();
        if let Some((at, hit)) = cache.get(id)
            && at.elapsed() < CACHE_TTL
        {
            return hit.clone();
        }
        let found = project.search_text(&sql_def_query(id), MAX_XML_HITS).ok();
        let hit = found.and_then(|f| {
            host::log(
                LogLevel::Info,
                &format!("sqltoy-navigator: def search for {id:?} → {} matches", f.matches.len()),
            );
            f.matches.into_iter().find_map(|m| {
                let text = project.read_file(&m.location.path).ok()?;
                let xml = parse_xml(&text);
                xml.defs
                    .iter()
                    .find(|d| d.name == id)
                    .map(|d| Location { path: m.location.path.clone(), range: d.range })
            })
        });
        host::log(
            LogLevel::Info,
            &format!("sqltoy-navigator: def for {id:?} → {:?}", hit.as_ref().map(|l| &l.path)),
        );
        cache.insert(id.to_owned(), (Instant::now(), hit.clone()));
        hit
    }

    /// Every Java `…BySql("…")` use site in the project, one host search
    /// (cached per project). The location covers the match; the id is parsed
    /// back out of the matching line.
    fn java_uses(&mut self, project: &Project) -> Vec<(String, Location)> {
        let Ok(root) = project.root() else {
            host::log(LogLevel::Warn, "sqltoy-navigator: java_uses project.root() failed");
            return Vec::new();
        };
        if let Some((at, uses)) = self.uses_index.get(&root)
            && at.elapsed() < CACHE_TTL
        {
            host::log(LogLevel::Info, &format!("sqltoy-navigator: java_uses cache hit → {} uses", uses.len()));
            return uses.clone();
        }
        host::log(LogLevel::Info, "sqltoy-navigator: java_uses cache miss, searching...");
        let query = TextQuery {
            pattern: r#"BySql\(\s*["'][^"']+["']"#.to_owned(),
            regex: true,
            case_sensitive: true,
            include: vec!["**/*.java".into()],
            ..Default::default()
        };
        let mut uses = Vec::new();
        match project.search_text(&query, MAX_JAVA_HITS) {
            Ok(found) => {
                host::log(
                    LogLevel::Info,
                    &format!(
                        "sqltoy-navigator: java uses search → {} matches (limit_hit: {})",
                        found.matches.len(),
                        found.limit_hit
                    ),
                );
                for m in found.matches {
                    let Some(id) = extract_by_sql_id(&m.line) else { continue };
                    uses.push((id, m.location.clone()));
                }
            }
            Err(e) => host::log(LogLevel::Warn, &format!("sqltoy-navigator: java uses search failed: {e:?}")),
        }
        self.uses_index.insert(root, (Instant::now(), uses.clone()));
        uses
    }

    /// Whether `id` has an XML definition (cached per project).
    fn def_exists(&mut self, project: &Project, id: &str) -> bool {
        let Ok(root) = project.root() else { return true };
        if let Some((at, ok)) = self.exists_index.get(&root).and_then(|c| c.get(id))
            && at.elapsed() < CACHE_TTL
        {
            return *ok;
        }
        let ok = self.xml_def_for(project, id).is_some();
        self.exists_index
            .entry(root)
            .or_default()
            .insert(id.to_owned(), (Instant::now(), ok));
        ok
    }

    /// The reference under the caret, if any.
    fn ref_at(doc: &Document, at: Position) -> Option<SqlIdRef> {
        doc.language
            .eq_ignore_ascii_case("java")
            .then(|| parse_java(&doc.text).into_iter().find(|r| contains(r.range, at)))
            .flatten()
    }

    /// The typed text before the caret inside a `…BySql("…` call: the prefix
    /// of the string literal still being typed (no closing quote yet).
    fn typed_in_call(doc: &Document, at: Position) -> Option<String> {
        let cursor = offset(&doc.text, at)?;
        let refs = parse_java_open(&doc.text, cursor);
        let r = refs.into_iter().find(|r| offset(&doc.text, r.range.start).is_some_and(|s| s <= cursor))?;
        let start = offset(&doc.text, r.range.start)?;
        if cursor < start {
            return None;
        }
        Some(doc.text[start..cursor].to_owned())
    }
}

/// Like [`parse_java`], but also reports the still-unclosed string literal of
/// a call being typed (`findBySql("app_`): the id spans to end-of-line and is
/// not checked for the closing quote.
fn parse_java_open(text: &str, _cursor: usize) -> Vec<SqlIdRef> {
    let bytes = text.as_bytes();
    let needle = b"Sql";
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' | b'\'' => {
                i = skip_literal(bytes, i);
                continue;
            }
            b'/' => {
                if matches!(bytes.get(i + 1), Some(b'/') | Some(b'*')) {
                    i = skip_comment(bytes, i);
                    continue;
                }
                i += 1;
                continue;
            }
            _ => {}
        }
        if bytes[i..].starts_with(needle)
            && (i == 0 || ident_byte(bytes[i - 1]))
            && bytes.get(i + needle.len()).is_some_and(|&c| c == b'(')
        {
            let mut j = i + needle.len();
            let mut k = j + 1;
            let mut found: Option<(usize, usize, bool)> = None;
            let mut depth = 1;
            while k < bytes.len() && depth > 0 {
                while k < bytes.len() && (bytes[k] as char).is_whitespace() {
                    k += 1;
                }
                if k >= bytes.len() {
                    break;
                }
                let c = bytes[k];
                if c == b')' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    k += 1;
                } else if c == b'(' {
                    depth += 1;
                    k += 1;
                } else if c == b',' {
                    k += 1;
                } else if c == b'"' || c == b'\'' {
                    let quote = c;
                    let start = k + 1;
                    let mut e = start;
                    while e < bytes.len() && bytes[e] != quote && bytes[e] != b'\n' {
                        e += 1;
                    }
                    let closed = e < bytes.len() && bytes[e] == quote;
                    found = Some((start, e, closed));
                    break;
                } else {
                    let mut arg_depth = 0;
                    while k < bytes.len() {
                        let ch = bytes[k];
                        if ch == b'(' {
                            arg_depth += 1;
                        } else if ch == b')' {
                            if arg_depth == 0 {
                                break;
                            }
                            arg_depth -= 1;
                        } else if ch == b',' && arg_depth == 0 {
                            break;
                        } else if ch == b'"' || ch == b'\'' {
                            let q = ch;
                            k += 1;
                            while k < bytes.len() && bytes[k] != q && bytes[k] != b'\n' {
                                k += 1;
                            }
                            if k < bytes.len() {
                                k += 1;
                            }
                            continue;
                        }
                        k += 1;
                    }
                }
            }
            if let Some((start, e, closed)) = found {
                let id = &text[start..e];
                let plausible =
                    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
                if plausible && !closed {
                    out.push(SqlIdRef { name: id.to_owned(), range: range_of(text, start, e) });
                }
                i = e.max(start) + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Returns a glob that limits Java searches to the Maven/Gradle module
/// containing `path`. Falls back to all Java files when the module root
/// cannot be inferred.
fn module_java_glob(path: &str) -> String {
    let parts: Vec<&str> = path.split(['/', '\\']).collect();
    for i in 0..parts.len() {
        if i + 3 <= parts.len()
            && parts[i] == "src"
            && parts[i + 1] == "main"
            && (parts[i + 2] == "java" || parts[i + 2] == "resources")
        {
            let module_root = parts[..i].join("/");
            return format!("{}/src/main/java/**/*.java", module_root);
        }
    }
    "**/*.java".into()
}

fn extract_by_sql_id(line: &str) -> Option<String> {
    let i = line.find("BySql(")? + "BySql(".len();
    let rest = &line[i..];
    let start = rest.find(|c| c == '"' || c == '\'')? + 1;
    let rest = &rest[start..];
    let end = rest.find(|c| c == '"' || c == '\'')?;
    let id = &rest[..end];
    (!id.is_empty()).then(|| id.to_owned())
}

fn extract_id_from_match(line: &str) -> Option<String> {
    // Find the first quoted string in the line.
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' || bytes[i] == b'\'' {
            let quote = bytes[i];
            let start = i + 1;
            let mut e = start;
            while e < bytes.len() && bytes[e] != quote {
                e += 1;
            }
            if e < bytes.len() {
                let id = &line[start..e];
                if !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')) {
                    return Some(id.to_owned());
                }
                i = e + 1;
                continue;
            }
        }
        i += 1;
    }
    None
}

impl DocumentIndexer for Navigator {
    /// Publishes the cross-file graph: `<sql id>` definitions from SqlToy XML
    /// files, `…BySql("…")` references (and gutter markers on their lines)
    /// from Java files. The host answers Go to Definition, Ctrl/⌘-Click,
    /// Find Usages and Rename from this graph without calling the plugin.
    fn index(&mut self, doc: &IndexDoc) -> Edges {
        let mut edges = Edges::default();
        if doc.path.ends_with(".java") {
            let refs: Vec<(String, Location)> = parse_java(&doc.text)
                .into_iter()
                .map(|r| {
                    let key = format!("{KEY_PREFIX}{}", r.name);
                    let loc = Location { path: doc.path.clone(), range: r.range.clone() };
                    (r.name, loc)
                })
                .collect();
            for (name, loc) in &refs {
                let key = format!("{KEY_PREFIX}{}", name);
                edges.references.push(Anchor { key, range: loc.range.clone() });
            }
            let count = refs.len();
            if !refs.is_empty() {
                let sample: Vec<_> = refs.iter().take(3).map(|(id, _)| id.clone()).collect();
                host::log(LogLevel::Info, &format!("sqltoy-navigator: sample refs from {}: {:?}", doc.path, sample));
            }
            self.java_refs.insert(doc.path.clone(), refs);
            host::log(
                LogLevel::Info,
                &format!(
                    "sqltoy-navigator: cached {} refs from {} (total files: {}, total refs: {})",
                    count,
                    doc.path,
                    self.java_refs.len(),
                    self.java_refs.values().map(|v| v.len()).sum::<usize>()
                ),
            );
        } else if is_sqltoy_xml(&doc.path, &doc.text) {
            for d in parse_xml(&doc.text).defs {
                edges.definitions.push(Anchor { key: format!("{KEY_PREFIX}{}", d.name), range: d.range });
            }
        }
        if !edges.definitions.is_empty() || !edges.references.is_empty() {
            host::log(
                LogLevel::Info,
                &format!(
                    "sqltoy-navigator: indexed {} → {} defs, {} refs ({} markers)",
                    doc.path,
                    edges.definitions.len(),
                    edges.references.len(),
                    edges.markers.len()
                ),
            );
        }
        edges
    }
}

impl Providers for Navigator {
    /// Go to Definition on a sqlId string: its `<sql id>` in the XML.
    /// When the cursor is on an XML `<sql id>`, jump to a Java use site instead.
    fn definition(&mut self, doc: &Document, at: Position) -> Vec<Location> {
        host::log(LogLevel::Info, &format!("sqltoy-navigator: definition called for {} at {:?}", doc.path, at));
        let Some(project) = project_of(&doc.path) else { return Vec::new() };
        if doc.path.ends_with(".sql.xml") && doc.text.contains("<sqltoy") {
            let defs = parse_xml(&doc.text).defs;
            let Some(d) = defs.iter().find(|d| contains(d.range, at)) else { return Vec::new() };
            // Module first (fast), then project-wide as a fallback.
            for include in [vec![module_java_glob(&doc.path)], vec!["**/*.java".to_owned()]] {
                let query = TextQuery {
                    pattern: format!(r#"["']{}["']"#, regex_escape(&d.name)),
                    regex: true,
                    case_sensitive: true,
                    include,
                    ..Default::default()
                };
                if let Ok(found) = project.search_text(&query, 1)
                    && let Some(m) = found.matches.first()
                {
                    return vec![m.location.clone()];
                }
            }
            return Vec::new();
        }
        let Some(r) = Self::ref_at(doc, at) else { return Vec::new() };
        self.xml_def_for(&project, &r.name).into_iter().collect()
    }

    /// Gutter arrows: on a Java reference, to its XML definition; on an XML
    /// `<sql id>` line, to a Java use site.
    fn line_markers(&mut self, doc: &Document) -> Vec<LineMarker> {
        let Some(project) = project_of(&doc.path) else { return Vec::new() };
        host::log(
            LogLevel::Info,
            &format!(
                "sqltoy-navigator: line_markers called for {} (language: {}, len: {}, sqltoy: {}, ends: {})",
                doc.path,
                doc.language,
                doc.text.len(),
                doc.text.contains("<sqltoy"),
                doc.path.ends_with(".sql.xml")
            ),
        );
        let mut out: Vec<LineMarker> = Vec::new();
        if doc.path.ends_with(".sql.xml") && doc.text.contains("<sqltoy") {
            let defs = parse_xml(&doc.text).defs;
            host::log(
                LogLevel::Info,
                &format!("sqltoy-navigator: XML markers for {} → {} defs", doc.path, defs.len()),
            );
            // One scoped search for all ids of this file; the module scope keeps
            // it inside the 500 ms provider budget.
            for include in [vec![module_java_glob(&doc.path)], vec!["**/*.java".to_owned()]] {
                if !out.is_empty() {
                    break;
                }
                let ids: Vec<String> = defs.iter().map(|d| regex_escape(&d.name)).collect();
                if ids.is_empty() {
                    break;
                }
                let query = TextQuery {
                    pattern: format!(r#"["']({})["']"#, ids.join("|")),
                    regex: true,
                    case_sensitive: true,
                    include,
                    ..Default::default()
                };
                let Ok(found) = project.search_text(&query, 200) else { continue };
                for m in found.matches {
                    let Some(id) = extract_id_from_match(&m.line) else { continue };
                    if let Some(d) = defs.iter().find(|d| d.name == id)
                        && !out.iter().any(|x: &LineMarker| x.line == d.range.start.line)
                    {
                        out.push(LineMarker {
                            line: d.range.start.line,
                            icon: Some("implemented".into()),
                            tooltip: format!("Go to a SqlToy call using \"{}\"", id),
                            target: Some(m.location.clone()),
                        });
                    }
                }
            }
            host::log(LogLevel::Info, &format!("sqltoy-navigator: returning {} markers for {}", out.len(), doc.path));
        }
        out.sort_by_key(|m| m.line);
        out
    }

    /// sqlId completion inside a `…BySql("` call: every id defined in the
    /// project's SqlToy XML files.
    fn completion(&mut self, doc: &Document, at: Position, _trigger: Option<&str>) -> Vec<CompletionItem> {
        if !doc.language.eq_ignore_ascii_case("java") {
            return Vec::new();
        }
        let Some(typed) = Self::typed_in_call(doc, at) else { return Vec::new() };
        let Some(project) = project_of(&doc.path) else { return Vec::new() };
        let query = TextQuery {
            pattern: r#"<sql\s+id\s*=\s*["'][^"']+["']"#.to_owned(),
            regex: true,
            case_sensitive: true,
            include: vec!["**/*.sql.xml".into()],
            ..Default::default()
        };
        let Ok(found) = project.search_text(&query, MAX_COMPLETIONS) else { return Vec::new() };
        let mut ids: Vec<String> = found
            .matches
            .iter()
            .filter_map(|m| extract_xml_id(&m.line))
            .filter(|id| id.starts_with(&typed))
            .collect();
        ids.sort();
        ids.dedup();
        let start = Position::new(at.line, at.character - typed.encode_utf16().count() as u32);
        ids.into_iter()
            .map(|id| CompletionItem {
                replace: Some(Range::new(start, at)),
                ..CompletionItem::new(&id, CompletionKind::Value)
            })
            .collect()
    }
}

/// `<sql id="…">` on a search-result line → the id value.
fn extract_xml_id(line: &str) -> Option<String> {
    let i = line.find("id=")? + 3;
    let rest = &line[i..];
    let start = rest.find(|c| c == '"' || c == '\'')? + 1;
    let rest = &rest[start..];
    let end = rest.find(|c| c == '"' || c == '\'')?;
    let id = &rest[..end];
    (!id.is_empty()).then(|| id.to_owned())
}

/// Diagnostics for the open Java file: a sqlId that matches no XML definition
/// is an error. (A document over the provider budget reports nothing.)
impl openidea_plugin::diagnostics::DiagnosticsProvider for Navigator {
    fn diagnostics(&mut self, doc: &Document) -> Vec<Diagnostic> {
        if !doc.language.eq_ignore_ascii_case("java") {
            return Vec::new();
        }
        let Some(project) = project_of(&doc.path) else { return Vec::new() };
        let mut seen = std::collections::BTreeSet::new();
        let mut out = Vec::new();
        for r in parse_java(&doc.text) {
            if !seen.insert(r.name.clone()) {
                continue;
            }
            if !self.def_exists(&project, &r.name) {
                out.push(Diagnostic::new(
                    r.range,
                    Severity::Error,
                    format!(
                        "SqlToy sqlId \"{}\" is not defined in any *.sql.xml file",
                        r.name
                    ),
                ));
            }
        }
        out
    }
}

impl Plugin for Navigator {
    fn run_command(&mut self, id: &str, ctx: &Invocation, _arg: Option<&str>) -> Result<Option<String>, Error> {
        host::log(LogLevel::Info, &format!("sqltoy-navigator: command {id} invoked"));
        let result = if id == "goto-xml" {
            self.goto_xml(ctx)
        } else if id == "find-usages" {
            self.find_usages(ctx)
        } else {
            Err(Error::NotFound(format!("command {id}")))
        };
        host::log(LogLevel::Info, &format!("sqltoy-navigator: command {id} → {:?}", result.as_ref().map(|r| r.is_some())));
        result
    }
}

impl Navigator {
    /// Assembles a provider-style `Document` from the focused editor.
    fn editor_doc(editor: &openidea_plugin::host::TextEditor) -> Result<Document, Error> {
        let d = editor.document().map_err(|e| Error::Failed(format!("No document: {e:?}")))?;
        Ok(Document {
            path: editor.path(),
            language: editor.language(),
            version: d.version,
            text: d.text,
        })
    }

    /// Go to XML SQL Definition: the `<sql id>` under the caret.
    fn goto_xml(&mut self, ctx: &Invocation) -> Result<Option<String>, Error> {
        let Some(editor) = ctx.editor() else { return Err(Error::Failed("No focused editor".into())) };
        let doc = Self::editor_doc(&editor)?;
        let Some(sel) = editor.selections().ok().and_then(|s| s.first().cloned()) else {
            return Err(Error::Failed("No caret".into()));
        };
        let Some(r) = Self::ref_at(&doc, sel.start) else {
            return Err(Error::Failed("The caret is not on a sqlId string".into()));
        };
        let Some(project) = project_of(&doc.path) else { return Err(Error::Failed("No project".into())) };
        match self.xml_def_for(&project, &r.name) {
            Some(loc) => {
                host::open_location(&loc.path, loc.range.start.line, loc.range.start.character)
                    .map_err(|e| Error::Failed(format!("Open failed: {e:?}")))?;
                Ok(None)
            }
            None => Err(Error::Failed(format!("No XML definition found for \"{}\"", r.name))),
        }
    }

    /// Find Java Usages: every `…Sql("id", …)` call of the `<sql id>` under
    /// the caret. One hit jumps straight there; several are listed.
    fn find_usages(&mut self, ctx: &Invocation) -> Result<Option<String>, Error> {
        let Some(editor) = ctx.editor() else { return Err(Error::Failed("No focused editor".into())) };
        let doc = Self::editor_doc(&editor)?;
        let Some(sel) = editor.selections().ok().and_then(|s| s.first().cloned()) else {
            return Err(Error::Failed("No caret".into()));
        };
        if doc.path.ends_with(".java") {
            // Reverse direction: from the Java sqlId to its XML — reuse goto logic.
            let Some(r) = Self::ref_at(&doc, sel.start) else {
                return Err(Error::Failed("The caret is not on a sqlId string".into()));
            };
            let Some(project) = project_of(&doc.path) else { return Err(Error::Failed("No project".into())) };
            match self.xml_def_for(&project, &r.name) {
                Some(loc) => {
                    host::open_location(&loc.path, loc.range.start.line, loc.range.start.character)
                        .map_err(|e| Error::Failed(format!("Open failed: {e:?}")))?;
                    Ok(None)
                }
                None => Err(Error::Failed(format!("No XML definition found for \"{}\"", r.name))),
            }
        } else {
            // Forward-reverse: from the XML `<sql id>` to Java use sites.
            let Some(project) = project_of(&doc.path) else { return Err(Error::Failed("No project".into())) };
            let defs = parse_xml(&doc.text).defs;
            let Some(d) = defs.iter().find(|d| contains(d.range, sel.start)) else {
                return Err(Error::Failed("The caret is not on a <sql id> attribute".into()));
            };
            let query = TextQuery {
                pattern: format!(r#"["']{}["']"#, regex_escape(&d.name)),
                regex: true,
                case_sensitive: true,
                include: vec![module_java_glob(&doc.path)],
                ..Default::default()
            };
            let found = project.search_text(&query, 100).map_err(|e| Error::Failed(format!("Search failed: {e:?}")))?;
            let mut hits: Vec<_> = found
                .matches
                .into_iter()
                .filter(|m| extract_id_from_match(&m.line).as_deref() == Some(d.name.as_str()))
                .collect();
            // Fall back to a project-wide search when the module has no hits.
            if hits.is_empty() {
                let query = TextQuery {
                    pattern: format!(r#"["']{}["']"#, regex_escape(&d.name)),
                    regex: true,
                    case_sensitive: true,
                    include: vec!["**/*.java".into()],
                    ..Default::default()
                };
                if let Ok(found) = project.search_text(&query, 100) {
                    hits = found
                        .matches
                        .into_iter()
                        .filter(|m| extract_id_from_match(&m.line).as_deref() == Some(d.name.as_str()))
                        .collect();
                }
            }
            match hits.len() {
                0 => Err(Error::Failed(format!("No Java usage found for \"{}\"", d.name))),
                1 => {
                    let m = &hits[0];
                    host::open_location(&m.location.path, m.location.range.start.line, m.location.range.start.character)
                        .map_err(|e| Error::Failed(format!("Open failed: {e:?}")))?;
                    Ok(None)
                }
                _ => {
                    let m = &hits[0];
                    host::open_location(&m.location.path, m.location.range.start.line, m.location.range.start.character)
                        .map_err(|e| Error::Failed(format!("Open failed: {e:?}")))?;
                    let other: Vec<String> = hits[1..]
                        .iter()
                        .map(|m| {
                            let name = m.location.path.rsplit(['/', '\\']).next().unwrap_or(&m.location.path);
                            format!("{}:{} ({})", name, m.location.range.start.line + 1, m.location.path)
                        })
                        .collect();
                    host::notify(
                        NotificationLevel::Info,
                        "SqlToy Navigator",
                        &format!(
                            "{} total usages of \"{}\"; jumped to the first. Others:\n{}",
                            hits.len(),
                            d.name,
                            other.join("\n")
                        ),
                    );
                    Ok(None)
                }
            }
        }
    }
}

register_plugin!(Navigator, providers, indexing, diagnostics);

#[cfg(test)]
mod tests {
    use super::*;
    use openidea_plugin::indexing::Document as IndexDoc;
    use openidea_plugin::mock::MockHost;

    const XML: &str = concat!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n",
        "<sqltoy xmlns=\"http://www.sagframe.com/schema/sqltoy\">\n",
        "    <sql id=\"app_order_list\">\n",
        "        <value><![CDATA[select * from orders where id = :orderId]]></value>\n",
        "    </sql>\n",
        "    <sql id=\"\"></sql>\n",
        "</sqltoy>\n"
    );

    const JAVA: &str = concat!(
        "package com.acme;\n",
        "\n",
        "public class OrderService {\n",
        "    // findBySql(\"in_a_comment\") must not count\n",
        "    private static final String S = \"also_a_string_BySql(\";\n",
        "\n",
        "    public void load() {\n",
        "        List<Order> orders = sqlToyLazyDao.findBySql(\"app_order_list\", param);\n",
        "        Order o = sqlToyLazyDao.getBySql(\"app_order_list\", param);\n",
        "        sqlToyLazyDao.updateBySql(\"missing_id\", param);\n",
        "        sqlToyLazyDao.executeSql(\"app_order_delete\", param);\n",
        "    }\n",
        "}\n"
    );

    #[test]
    fn parses_xml_defs() {
        let xml = parse_xml(XML);
        assert_eq!(xml.defs.len(), 1);
        assert_eq!(xml.defs[0].name, "app_order_list");
        assert_eq!(xml.defs[0].range, Range::new(Position::new(2, 13), Position::new(2, 27)));
    }

    #[test]
    fn parses_java_refs_and_skips_strings_and_comments() {
        let refs = parse_java(JAVA);
        assert_eq!(
            refs.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["app_order_list", "app_order_list", "missing_id", "app_order_delete"]
        );
        // The first reference covers exactly the id text (quotes excluded).
        assert_eq!(refs[0].range, Range::new(Position::new(7, 54), Position::new(7, 68)));
    }

    #[test]
    fn indexes_both_sides() {
        let _mock = MockHost::install();
        let mut nav = Navigator::default();
        let edges = nav.index(&IndexDoc {
            path: "/p/com/acme/sqltoy-order.sql.xml".into(),
            text: XML.into(),
            symbols: vec![],
        });
        assert_eq!(edges.definitions.len(), 1);
        assert_eq!(edges.definitions[0].key, "sqltoy#app_order_list");
        assert!(edges.references.is_empty());

        let edges = nav.index(&IndexDoc {
            path: "/p/com/acme/OrderService.java".into(),
            text: JAVA.into(),
            symbols: vec![],
        });
        assert_eq!(edges.references.len(), 4);
        assert!(edges.markers.is_empty());
        assert_eq!(edges.references[0].key, "sqltoy#app_order_list");
    }

    #[test]
    fn extracts_ids_from_search_lines() {
        assert_eq!(
            extract_by_sql_id("        List<Order> orders = sqlToyLazyDao.findBySql(\"app_order_list\", param);"),
            Some("app_order_list".into())
        );
        assert_eq!(extract_xml_id("    <sql id=\"app_order_list\">"), Some("app_order_list".into()));
        assert_eq!(extract_xml_id("    <value>no id here</value>"), None);
    }
}
