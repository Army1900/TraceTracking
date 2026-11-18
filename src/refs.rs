use crate::store::Entry;
use anyhow::Result;
use regex::Regex;
use rusqlite::{Connection, params};
use std::{
    path::{Component, Path},
    sync::LazyLock,
};
#[derive(Clone)]
pub struct Target {
    pub label: String,
    pub event: Entry,
    pub line: usize,
}
static TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?:["'(\x60])([^"'()\s\x60]+)(?:["')\x60])"#).unwrap());
static CONFIG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*[\w.-]+\s*[:=]\s*([\w./-]+)\s*(?:#.*)?$").unwrap());
static NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[A-Za-z_]\w*").unwrap());
fn normalize(path: &Path) -> Option<String> {
    let mut v = Vec::new();
    for c in path.components() {
        match c {
            Component::Normal(x) => v.push(x.to_string_lossy()),
            Component::ParentDir => {
                v.pop()?;
            }
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(v.join("/"))
}
pub fn index(db: &Connection, hash: &str, path: &str, body: &str) -> Result<()> {
    let ext = Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if db.execute(
        "INSERT OR IGNORE INTO parsed VALUES(?,?)",
        params![hash, ext],
    )? == 0
    {
        return Ok(());
    }
    let mut stmt = db.prepare_cached("INSERT OR IGNORE INTO symbols VALUES(?,?,?,?,?)")?;
    if ext == "xml" {
        let mut reader = quick_xml::Reader::from_str(body);
        let mut offset = 0;
        let mut line = 0;
        loop {
            let start = reader.buffer_position() as usize;
            line += body.as_bytes()[offset..start]
                .iter()
                .filter(|&&c| c == b'\n')
                .count();
            offset = start;
            match reader.read_event() {
                Ok(quick_xml::events::Event::Start(tag) | quick_xml::events::Event::Empty(tag)) => {
                    for attr in tag.attributes().flatten() {
                        if matches!(
                            attr.key.local_name().as_ref(),
                            b"href"
                                | b"src"
                                | b"file"
                                | b"resource"
                                | b"schemaLocation"
                                | b"noNamespaceSchemaLocation"
                        ) && let Ok(value) = attr.unescape_value()
                        {
                            for value in value.split_whitespace().filter(|v| !v.contains("://")) {
                                stmt.execute(params![
                                    hash,
                                    ext,
                                    "link",
                                    value.trim_start_matches("classpath:"),
                                    line
                                ])?;
                            }
                        }
                    }
                }
                Ok(quick_xml::events::Event::Eof) | Err(_) => break,
                _ => {}
            }
        }
        return Ok(());
    }
    for (n, s) in body.lines().enumerate() {
        let mut record = |raw: &str| -> Result<()> {
            let value = raw.split('#').next().unwrap_or("");
            if !value.is_empty() && value.len() < 512 && !value.contains("://") {
                stmt.execute(params![hash, ext, "link", value, n])?;
            }
            Ok(())
        };
        if ext == "rs"
            && let Some(raw) = s
                .trim()
                .trim_start_matches("pub ")
                .strip_prefix("mod ")
                .and_then(|s| s.strip_suffix(';'))
        {
            record(&format!("{raw}.rs"))?;
            record(&format!("{raw}/mod.rs"))?;
        }
        if ext == "py"
            && let Some(raw) = s
                .trim()
                .strip_prefix("from ")
                .or_else(|| s.trim().strip_prefix("import "))
                .and_then(|s| s.split_whitespace().next())
        {
            let dots = raw.len() - raw.trim_start_matches('.').len();
            let raw = format!(
                "{}{}",
                "../".repeat(dots.saturating_sub(1)),
                raw[dots..].replace('.', "/")
            );
            if !raw.is_empty() {
                record(&format!("{raw}.py"))?;
                record(&format!("{raw}/__init__.py"))?;
            }
        }
        if matches!(ext, "md" | "markdown" | "json" | "yaml" | "yml" | "toml")
            || s.contains("import")
            || s.contains("require(")
            || s.contains("from ")
            || s.contains("include")
        {
            for cap in TOKEN.captures_iter(s) {
                record(&cap[1])?;
            }
        }
        if matches!(ext, "yaml" | "yml" | "toml")
            && let Some(cap) = CONFIG.captures(s)
        {
            record(&cap[1])?;
        }
    }
    let language = match ext {
        "java" => tree_sitter_java::LANGUAGE,
        "rs" => tree_sitter_rust::LANGUAGE,
        "ts" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX,
        "js" | "jsx" | "mjs" => tree_sitter_javascript::LANGUAGE,
        "py" => tree_sitter_python::LANGUAGE,
        _ => return Ok(()),
    };
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language.into())?;
    if let Some(tree) = parser.parse(body, None) {
        let mut package = String::new();
        let mut cursor = tree.walk();
        loop {
            let node = cursor.node();
            if ext == "java" {
                match node.kind() {
                    "package_declaration" => {
                        package = node
                            .utf8_text(body.as_bytes())?
                            .trim_start_matches("package")
                            .trim_end_matches(';')
                            .trim()
                            .to_string()
                    }
                    "import_declaration" => {
                        let text = node
                            .utf8_text(body.as_bytes())?
                            .trim_start_matches("import")
                            .trim_end_matches(';')
                            .trim();
                        let name = if let Some(member) = text.strip_prefix("static ") {
                            member
                                .rsplit_once('.')
                                .map(|(name, _)| name)
                                .unwrap_or(member)
                        } else {
                            text
                        };
                        stmt.execute(params![
                            hash,
                            ext,
                            "link",
                            format!("java:{name}"),
                            node.start_position().row
                        ])?;
                    }
                    "class_declaration"
                    | "interface_declaration"
                    | "enum_declaration"
                    | "record_declaration"
                        if node.parent().is_some_and(|p| p.kind() == "program") =>
                    {
                        if let Some(n) = node.child_by_field_name("name") {
                            let name = n.utf8_text(body.as_bytes())?;
                            let qualified = if package.is_empty() {
                                name.into()
                            } else {
                                format!("{package}.{name}")
                            };
                            stmt.execute(params![
                                hash,
                                ext,
                                "qualified",
                                format!("java:{qualified}"),
                                n.start_position().row
                            ])?;
                        }
                    }
                    _ => {}
                }
            }
            if matches!(
                node.kind(),
                "function_definition"
                    | "class_definition"
                    | "function_item"
                    | "struct_item"
                    | "enum_item"
                    | "trait_item"
                    | "type_item"
                    | "class_declaration"
                    | "function_declaration"
                    | "interface_declaration"
                    | "type_alias_declaration"
                    | "enum_declaration"
                    | "record_declaration"
                    | "method_declaration"
                    | "constructor_declaration"
                    | "method_definition"
            ) && let Some(name) = node.child_by_field_name("name")
            {
                stmt.execute(params![
                    hash,
                    ext,
                    "def",
                    name.utf8_text(body.as_bytes())?,
                    name.start_position().row
                ])?;
            }
            if cursor.goto_first_child() {
                continue;
            }
            while !cursor.goto_next_sibling() {
                if !cursor.goto_parent() {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}
const SUFFIXES: &[&str] = &[
    "",
    ".ts",
    ".tsx",
    ".js",
    ".py",
    ".rs",
    "/index.ts",
    "/index.js",
];
fn candidates(path: &str, raw: &str) -> Vec<String> {
    if raw.starts_with("java:") {
        return vec![raw.into()];
    }
    let mut out = vec![];
    for base in [
        Path::new(path).parent().unwrap_or(Path::new("")).join(raw),
        raw.into(),
    ] {
        if let Some(p) = normalize(&base) {
            for end in SUFFIXES {
                out.push(format!("{p}{end}"));
            }
        }
    }
    out
}
pub fn links(
    db: &Connection,
    root: &str,
    path: &str,
    body: &str,
    line: usize,
) -> Result<Vec<Target>> {
    let filter = crate::store::rules(db, root)?;
    let mut out = vec![];
    let mut seen = std::collections::HashSet::new();
    let mut add = |p: &str, n: usize, label: &str| -> Result<()> {
        if out.len() >= 200
            || filter
                .matched_path_or_any_parents(Path::new(root).join(p), false)
                .is_ignore()
        {
            return Ok(());
        }
        let mut st=db.prepare_cached("SELECT e.id,e.kind,e.stamp FROM files f JOIN events e ON e.id=f.id WHERE f.root=? AND f.path=?")?;
        let mut rows = st.query(params![root, p])?;
        if let Some(r) = rows.next()?
            && seen.insert((p.to_string(), n, label.to_string()))
        {
            out.push(Target {
                event: Entry {
                    id: r.get(0)?,
                    path: p.into(),
                    kind: r.get(1)?,
                    stamp: r.get(2)?,
                },
                line: n,
                label: format!("{label}  {p}:{}", n + 1),
            });
        }
        Ok(())
    };
    let ext = Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let mut st = db.prepare(
        "SELECT name FROM symbols WHERE hash=? AND ext=? AND kind='link' ORDER BY abs(line-?)",
    )?;
    for raw in st.query_map(
        params![crate::store::digest(body.as_bytes()), ext, line],
        |r| r.get::<_, String>(0),
    )? {
        for target in candidates(path, &raw?) {
            if target.starts_with("java:") {
                let mut q=db.prepare_cached("SELECT f.path,s.line FROM symbols s JOIN events e ON s.hash=e.new JOIN files f ON f.id=e.id WHERE f.root=? AND s.kind='qualified' AND s.name=?")?;
                for row in q.query_map(params![root, target], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, usize>(1)?))
                })? {
                    let (p, n) = row?;
                    add(&p, n, "引用")?;
                }
            } else {
                add(&target, 0, "引用")?;
            }
        }
    }
    let mut defs=db.prepare("SELECT f.path,s.line FROM symbols s JOIN events e ON e.new=s.hash JOIN files f ON f.id=e.id WHERE f.root=? AND s.kind='def' AND s.name=? LIMIT 200")?;
    for name in NAME
        .find_iter(body.lines().nth(line).unwrap_or(""))
        .take(32)
    {
        for row in defs.query_map(params![root, name.as_str()], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, usize>(1)?))
        })? {
            let (p, n) = row?;
            add(&p, n, "定义候选")?;
        }
    }
    let mut rev = db.prepare("SELECT path,line FROM edges WHERE root=? AND target=? LIMIT 200")?;
    let mut reverse_keys = vec![path.to_string()];
    if ext == "java" {
        reverse_keys.extend(crate::store::strings(
            db,
            "SELECT name FROM symbols WHERE hash=? AND kind='qualified'",
            [crate::store::digest(body.as_bytes())],
        )?);
    }
    for key in reverse_keys {
        for row in rev.query_map(params![root, key], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, usize>(1)?))
        })? {
            let (p, n) = row?;
            add(&p, n, "被引用")?;
        }
    }
    Ok(out)
}

pub fn refresh_edges(db: &Connection, root: &str, path: &str, hash: Option<&str>) -> Result<()> {
    db.execute(
        "DELETE FROM edges WHERE root=? AND path=?",
        params![root, path],
    )?;
    let Some(hash) = hash else { return Ok(()) };
    let ext = Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let mut st =
        db.prepare_cached("SELECT name,line FROM symbols WHERE hash=? AND ext=? AND kind='link'")?;
    let mut insert = db.prepare_cached("INSERT OR IGNORE INTO edges VALUES(?,?,?,?)")?;
    for row in st.query_map(params![hash, ext], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, usize>(1)?))
    })? {
        let (raw, n) = row?;
        for target in candidates(path, &raw) {
            if Path::new(&raw).extension().is_some()
                && !target.ends_with(&raw.rsplit('/').next().unwrap_or(""))
            {
                continue;
            }
            insert.execute(params![root, path, target, n])?;
        }
    }
    Ok(())
}
