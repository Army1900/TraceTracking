use anyhow::{Context, Result};
use chrono::Local;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

pub const LIMIT: u64 = 1024 * 1024;
pub(crate) fn data_dir() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("TT_DATA_DIR") {
        return Ok(PathBuf::from(p));
    }
    Ok(dirs::data_local_dir()
        .context("无法定位应用数据目录")?
        .join("TraceTracking"))
}
pub fn database() -> Result<Connection> {
    let dir = data_dir()?;
    fs::create_dir_all(&dir)?;
    let db = Connection::open(dir.join("history.sqlite3"))?;
    schema(db)
}
pub fn read_only() -> Result<Connection> {
    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY;
    let db = Connection::open_with_flags(data_dir()?.join("history.sqlite3"), flags)?;
    db.busy_timeout(Duration::from_secs(3))?;
    Ok(db)
}
fn schema(db: Connection) -> Result<Connection> {
    db.busy_timeout(Duration::from_secs(10))?;
    db.execute_batch("PRAGMA journal_mode=WAL;
 CREATE TABLE IF NOT EXISTS projects(root TEXT PRIMARY KEY,used INTEGER);
 CREATE TABLE IF NOT EXISTS blobs(hash TEXT PRIMARY KEY,body TEXT);
 CREATE TABLE IF NOT EXISTS events(id INTEGER PRIMARY KEY,root TEXT,path TEXT,stamp INTEGER,kind TEXT,old TEXT,new TEXT,source TEXT DEFAULT '');
 CREATE INDEX IF NOT EXISTS history ON events(root,path,id);
 CREATE INDEX IF NOT EXISTS timeline ON events(root,stamp DESC,id DESC);
 CREATE TABLE IF NOT EXISTS moves(id INTEGER PRIMARY KEY,previous TEXT);
 CREATE TABLE IF NOT EXISTS files(root TEXT,path TEXT,id INTEGER,PRIMARY KEY(root,path));
 CREATE TABLE IF NOT EXISTS rules(root TEXT,rule TEXT,PRIMARY KEY(root,rule));
 CREATE TABLE IF NOT EXISTS edges(root TEXT,path TEXT,target TEXT,line INTEGER,PRIMARY KEY(root,path,target,line));
 CREATE INDEX IF NOT EXISTS inbound ON edges(root,target);
 CREATE TABLE IF NOT EXISTS parsed(hash TEXT,ext TEXT,PRIMARY KEY(hash,ext));
 CREATE TABLE IF NOT EXISTS symbols(hash TEXT,ext TEXT,kind TEXT,name TEXT,line INTEGER,PRIMARY KEY(hash,ext,kind,name,line));
 CREATE INDEX IF NOT EXISTS symbol_names ON symbols(kind,name);
 CREATE INDEX IF NOT EXISTS revisions ON events(new);
 CREATE TABLE IF NOT EXISTS hooks(root TEXT,path TEXT,hash TEXT,source TEXT,stamp INTEGER);")?;
    crate::agent::schema(&db)?;
    Ok(db)
}
pub fn strings(db: &Connection, sql: &str, args: impl rusqlite::Params) -> Result<Vec<String>> {
    Ok(db
        .prepare(sql)?
        .query_map(args, |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}
pub fn rules(db: &Connection, root: &str) -> Result<Gitignore> {
    let mut b = GitignoreBuilder::new(root);
    for s in [".git", "node_modules/", "target/", "__pycache__/", "dist/"] {
        b.add_line(None, s)?;
    }
    for s in strings(db, "SELECT rule FROM rules WHERE root=?", [root])? {
        b.add_line(None, &s)?;
    }
    if let Ok(data) = data_dir()
        && let Ok(rel) = data.strip_prefix(root)
    {
        b.add_line(
            None,
            &format!(
                "/{}/",
                rel.to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/")
            ),
        )?;
    }
    Ok(b.build()?)
}
pub fn digest(b: &[u8]) -> String {
    format!("{:x}", Sha256::digest(b))
}
pub fn scan(
    db: &mut Connection,
    root: &str,
    path: &Path,
    filter: &Gitignore,
    initial: bool,
) -> Result<()> {
    scan_with(db, root, path, filter, initial, || true)
}
pub fn scan_with(
    db: &mut Connection,
    root: &str,
    path: &Path,
    filter: &Gitignore,
    initial: bool,
    mut progress: impl FnMut() -> bool,
) -> Result<()> {
    let prefix = path
        .strip_prefix(root)?
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/");
    if !prefix.is_empty()
        && filter
            .matched_path_or_any_parents(path, path.is_dir())
            .is_ignore()
    {
        return Ok(());
    }
    let mut walk = ignore::WalkBuilder::new(path);
    walk.hidden(false)
        .ignore(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .parents(false);
    let base = PathBuf::from(root);
    let matcher = filter.clone();
    walk.filter_entry(move |e| {
        e.path() == base
            || !matcher
                .matched(e.path(), e.file_type().is_some_and(|t| t.is_dir()))
                .is_ignore()
    });
    let mut found = std::collections::HashSet::new();
    let mut complete = true;
    if path.try_exists()? {
        for entry in walk.build() {
            if !progress() {
                return Ok(());
            }
            let entry = match entry {
                Ok(e) => e,
                Err(_) => {
                    complete = false;
                    continue;
                }
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let p = entry
                .path()
                .strip_prefix(root)?
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            found.insert(p.clone());
            let Ok(meta) = entry.metadata() else { continue };
            let (hash, body) = if meta.len() <= LIMIT {
                let Ok(bytes) = fs::read(entry.path()) else {
                    continue;
                };
                let hash = digest(&bytes);
                let body = if bytes.contains(&0) {
                    None
                } else {
                    String::from_utf8(bytes).ok()
                };
                (hash, body)
            } else {
                (format!("large:{}:{:?}", meta.len(), meta.modified()?), None)
            };
            save(
                db,
                root,
                &p,
                Some(&hash),
                body.as_deref(),
                initial,
                meta.modified()
                    .ok()
                    .map(chrono::DateTime::<chrono::Utc>::from)
                    .map(|t| t.timestamp()),
            )?;
        }
    }
    if complete && !path.is_file() {
        let paths = strings(
            db,
            "SELECT path FROM files WHERE root=?1 AND (?2='' OR path=?2 OR instr(path,?2||'/')=1)",
            params![root, prefix],
        )?;
        for p in paths {
            if !found.contains(&p)
                && !filter
                    .matched_path_or_any_parents(Path::new(root).join(&p), false)
                    .is_ignore()
            {
                save(db, root, &p, None, None, initial, None)?;
            }
        }
    }
    Ok(())
}
fn save(
    db: &mut Connection,
    root: &str,
    p: &str,
    new: Option<&str>,
    body: Option<&str>,
    initial: bool,
    mtime: Option<i64>,
) -> Result<()> {
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let old:Option<String>=tx.query_row("SELECT new FROM files JOIN events ON events.id=files.id WHERE files.root=? AND files.path=?",params![root,p],|r|r.get(0)).optional()?;
    if new == old.as_deref() {
        return Ok(());
    }
    if let (Some(hash), Some(body)) = (new, body) {
        crate::refs::index(&tx, hash, p, body)?;
        tx.execute(
            "INSERT OR IGNORE INTO blobs VALUES(?,?)",
            params![hash, body],
        )?;
    }
    crate::refs::refresh_edges(&tx, root, p, new)?;
    let kind = match (old.is_some(), new.is_some(), initial) {
        (_, false, _) => "D",
        (false, true, true) => "B",
        (false, true, false) => "A",
        (true, true, true) => "O",
        _ => "M",
    };
    let now = Local::now().timestamp();
    let source:Option<String>=tx.query_row("SELECT source FROM hooks WHERE root=? AND path=? AND hash=? AND stamp>=? ORDER BY stamp DESC LIMIT 1",params![root,p,new,now-15],|r|r.get(0)).optional()?;
    tx.execute(
        "DELETE FROM hooks WHERE (root=? AND path=?) OR stamp<?",
        params![root, p, now - 15],
    )?;
    tx.execute(
        "INSERT INTO events(root,path,stamp,kind,old,new,source) VALUES(?,?,?,?,?,?,?)",
        params![
            root,
            p,
            if kind == "B" {
                mtime.unwrap_or(now)
            } else {
                now
            },
            kind,
            old,
            new,
            source.unwrap_or_default()
        ],
    )?;
    let event_id = tx.last_insert_rowid();
    if matches!(kind, "A" | "M" | "D") {
        crate::agent::match_observed(&tx, root, p, event_id)?;
    }
    if new.is_some() {
        tx.execute(
            "INSERT OR REPLACE INTO files VALUES(?,?,?)",
            params![root, p, event_id],
        )?;
    } else {
        tx.execute(
            "DELETE FROM files WHERE root=? AND path=?",
            params![root, p],
        )?;
    }
    tx.commit()?;
    Ok(())
}
#[derive(Clone, Default)]
pub struct Entry {
    pub id: i64,
    pub path: String,
    pub kind: String,
    pub stamp: i64,
    pub agent: Option<crate::agent::Mark>,
}
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum SearchMode {
    #[default]
    Path,
    Body,
    Changes,
    PathOrBody,
}

#[derive(Default)]
pub struct EntryQuery<'a> {
    pub root: &'a str,
    pub recent: bool,
    pub only: &'a str,
    pub text: &'a str,
    pub mode: SearchMode,
    pub offset: usize,
    pub session: Option<i64>,
    pub git_status: Option<Arc<crate::git::Statuses>>,
}

pub fn entries(db: &Connection, query: EntryQuery<'_>) -> Result<Vec<Entry>> {
    let EntryQuery {
        root,
        recent,
        only,
        text,
        mode,
        offset,
        session,
        git_status,
    } = query;
    if git_status
        .as_ref()
        .is_some_and(|statuses| statuses.is_empty())
    {
        return Ok(Vec::new());
    }
    let git_paths = git_status
        .as_ref()
        .map(|statuses| serde_json::to_string(&statuses.keys().collect::<Vec<_>>()))
        .transpose()?;
    let filter = rules(db, root)?;
    let base = PathBuf::from(root);
    db.create_scalar_function(
        "visible_path",
        1,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8,
        move |ctx| {
            let p = ctx.get_raw(0).as_str()?;
            Ok(!filter
                .matched_path_or_any_parents(base.join(p), false)
                .is_ignore())
        },
    )?;
    db.create_scalar_function(
        "changed_text",
        3,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8,
        |ctx| {
            let a: Option<String> = ctx.get(0)?;
            let b: Option<String> = ctx.get(1)?;
            let q: String = ctx.get(2)?;
            Ok(crate::view::content(
                a.as_deref(),
                b.as_deref(),
                if a.is_none() {
                    "A"
                } else if b.is_none() {
                    "D"
                } else {
                    "M"
                },
                true,
            )
            .iter()
            .any(|r| r.mark != ' ' && r.text.contains(&q)))
        },
    )?;
    let recent = recent && mode != SearchMode::Changes;
    let current_files = ((recent && git_paths.is_none())
        || matches!(mode, SearchMode::Body | SearchMode::PathOrBody))
        && session.is_none();
    let table = if current_files {
        "files f JOIN events e ON f.id=e.id"
    } else {
        "events e"
    };
    // Start from Git's changed paths so clean-file history never has to be scanned.
    let (git_table, git_filter) = match (git_paths.is_some(), current_files, recent, session) {
        (false, ..) => ("", ""),
        (true, true, ..) => (
            "json_each(?6) git CROSS JOIN ",
            "AND f.root=?1 AND f.path=git.value",
        ),
        (true, false, true, None) => (
            "json_each(?6) git CROSS JOIN ",
            "AND e.id=(SELECT max(id) FROM events WHERE root=?1 AND path=git.value)",
        ),
        (true, false, ..) => ("json_each(?6) git CROSS JOIN ", "AND e.path=git.value"),
    };
    let pred = match mode {
        SearchMode::Body => "instr((SELECT body FROM blobs WHERE hash=e.new),?3)>0",
        SearchMode::Changes => {
            "e.kind!='B' AND EXISTS(SELECT 1 FROM blobs WHERE hash IN(e.old,e.new) AND instr(body,?3)>0) AND changed_text((SELECT body FROM blobs WHERE hash=e.old),(SELECT body FROM blobs WHERE hash=e.new),?3)"
        }
        SearchMode::Path => "instr(lower(e.path),lower(?3))>0",
        SearchMode::PathOrBody => {
            "(instr(lower(e.path),lower(?3))>0 OR instr((SELECT body FROM blobs WHERE hash=e.new),?3)>0)"
        }
    };
    let session_filter = crate::agent::session_condition();
    let recent_filter = if recent && session.is_some() {
        format!(
            "AND NOT EXISTS (SELECT 1 FROM events newer
           WHERE newer.root=e.root AND newer.path=e.path AND newer.id>e.id
             AND EXISTS(SELECT 1 FROM events e WHERE e.id=newer.id AND {session_filter}))"
        )
    } else {
        String::new()
    };
    let sql = format!(
        "SELECT e.id,e.path,e.kind,e.stamp FROM {git_table}{table}
         WHERE e.root=?1 AND (?2='' OR e.path=?2 OR (substr(?2,-1)='/' AND instr(e.path,?2)=1))
           AND {pred} AND visible_path(e.path) AND (?5 IS NULL OR {session_filter})
           {git_filter} {recent_filter}
         ORDER BY e.stamp DESC,e.id DESC LIMIT 256 OFFSET ?4"
    );
    let args: [&dyn rusqlite::ToSql; 6] = [&root, &only, &text, &offset, &session, &git_paths];
    let param_count = if git_paths.is_some() { 6 } else { 5 };
    let mut entries = db
        .prepare(&sql)?
        .query_map(&args[..param_count], |r| {
            Ok(Entry {
                id: r.get(0)?,
                path: r.get(1)?,
                kind: r.get(2)?,
                stamp: r.get(3)?,
                agent: None,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let ids: Vec<_> = entries.iter().map(|event| event.id).collect();
    let mut marks = crate::agent::marks(db, &ids)?;
    for event in &mut entries {
        event.agent = marks.remove(&event.id);
    }
    Ok(entries)
}

pub fn bodies(db: &Connection, id: i64) -> Result<(Option<String>, Option<String>, String)> {
    Ok(db.query_row("SELECT a.body,b.body,e.source FROM events e LEFT JOIN blobs a ON a.hash=e.old LEFT JOIN blobs b ON b.hash=e.new WHERE e.id=?",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?)
}

/// Record a move only when the native backend explicitly paired both paths.
pub fn moved(
    db: &mut Connection,
    root: &str,
    from: &Path,
    to: &Path,
    filter: &Gitignore,
) -> Result<()> {
    if !to.is_file()
        || filter.matched_path_or_any_parents(from, false).is_ignore()
        || filter.matched_path_or_any_parents(to, false).is_ignore()
    {
        return Ok(());
    }
    let rel = |p: &Path| -> Result<String> {
        Ok(p.strip_prefix(root)?
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/"))
    };
    let a = rel(from)?;
    let b = rel(to)?;
    let tx = db.transaction()?;
    let hash: Option<String> = tx
        .query_row(
            "SELECT e.new FROM files f JOIN events e ON f.id=e.id WHERE f.root=? AND f.path=?",
            params![root, a],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(hash) = hash {
        crate::refs::refresh_edges(&tx, root, &a, None)?;
        crate::refs::refresh_edges(&tx, root, &b, Some(&hash))?;
        tx.execute(
            "INSERT INTO events(root,path,stamp,kind,old,new) VALUES(?,?,?,'R',?,?)",
            params![root, b, Local::now().timestamp(), hash, hash],
        )?;
        let id = tx.last_insert_rowid();
        tx.execute("INSERT INTO moves VALUES(?,?)", params![id, a])?;
        tx.execute(
            "DELETE FROM files WHERE root=? AND path=?",
            params![root, a],
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO files VALUES(?,?,?)",
            params![root, b, id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub fn update_rule(db: &mut Connection, root: &str, rule: &str, add: bool) -> Result<()> {
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let old = rules(&tx, root)?;
    tx.execute(
        if add {
            "INSERT OR IGNORE INTO rules VALUES(?,?)"
        } else {
            "DELETE FROM rules WHERE root=? AND rule=?"
        },
        params![root, rule],
    )?;
    let new = rules(&tx, root)?;
    // Retain immutable history. Discard live pointers across an observation gap.
    for path in strings(&tx, "SELECT path FROM files WHERE root=?", [root])? {
        let absolute = Path::new(root).join(&path);
        if old
            .matched_path_or_any_parents(&absolute, false)
            .is_ignore()
            != new
                .matched_path_or_any_parents(&absolute, false)
                .is_ignore()
        {
            tx.execute(
                "DELETE FROM files WHERE root=? AND path=?",
                params![root, path],
            )?;
            crate::refs::refresh_edges(&tx, root, &path, None)?;
        }
    }
    tx.commit()?;
    if !add {
        scan(db, root, Path::new(root), &new, true)?;
    }
    Ok(())
}
#[cfg(test)]
pub fn test_database() -> Result<Connection> {
    schema(Connection::open_in_memory()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_filter_queries_changed_paths_before_paging() -> Result<()> {
        use crate::git::{FileStatus, Statuses};

        let db = test_database()?;
        let root = "/project";
        db.execute_batch(
            "INSERT INTO events (root,path,stamp,kind) VALUES
                ('/project','edit.txt',1,'B'),
                ('/project','edit.txt',2,'M'),
                ('/project','gone.txt',3,'D'),
                ('/project','hidden.txt',4,'B');
             INSERT INTO rules VALUES ('/project','hidden.txt');
             WITH RECURSIVE clean(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM clean WHERE n<270)
               INSERT INTO events (root,path,stamp,kind)
               SELECT '/project','clean-'||n||'.txt',n+4,'B' FROM clean;
             INSERT INTO files (root,path,id)
               SELECT root,path,max(id) FROM events WHERE kind!='D' GROUP BY root,path;",
        )?;
        let statuses = Arc::new(Statuses::from([
            ("edit.txt".into(), FileStatus(*b" M")),
            ("gone.txt".into(), FileStatus(*b" D")),
            ("hidden.txt".into(), FileStatus(*b"??")),
        ]));
        let query = |recent, offset| {
            entries(
                &db,
                EntryQuery {
                    root,
                    recent,
                    offset,
                    git_status: Some(Arc::clone(&statuses)),
                    ..Default::default()
                },
            )
        };
        let timeline = query(false, 0)?;
        assert_eq!(
            timeline.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
            ["D", "M", "B"]
        );
        let recent = query(true, 0)?;
        assert_eq!(
            recent.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
            ["gone.txt", "edit.txt"]
        );
        assert_eq!(query(false, 2)?[0].kind, "B");
        assert!(query(true, 2)?.is_empty());
        Ok(())
    }

    #[test]
    fn baseline_update_delete_and_references() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("tt-check-{}", std::process::id()));
        fs::create_dir_all(&dir)?;
        let root = dir.to_str().unwrap();
        let db = schema(Connection::open_in_memory()?)?;
        let path = dir.join("note.md");
        fs::write(&path, "[settings](config.yaml)")?;
        fs::write(dir.join("config.yaml"), "count: 1")?;
        let mut db = db;
        let filter = rules(&db, root)?;
        scan(&mut db, root, &dir, &filter, true)?;
        let links =
            crate::refs::links(&db, root, "note.md", "[settings](config.yaml)", 0, || false)?;
        assert_eq!(links[0].event.path, "config.yaml");
        fs::write(dir.join("config.yaml"), "count: 2")?;
        scan(&mut db, root, &dir.join("config.yaml"), &filter, false)?;
        let e = entries(
            &db,
            EntryQuery {
                root,
                only: "config.yaml",
                ..Default::default()
            },
        )?;
        assert_eq!(e[0].kind, "M");
        let (a, b, _) = bodies(&db, e[0].id)?;
        assert_eq!(a.as_deref(), Some("count: 1"));
        assert_eq!(b.as_deref(), Some("count: 2"));
        fs::remove_file(&path)?;
        scan(&mut db, root, &path, &filter, false)?;
        assert_eq!(
            entries(
                &db,
                EntryQuery {
                    root,
                    only: "note.md",
                    ..Default::default()
                }
            )?[0]
                .kind,
            "D"
        );
        fs::remove_dir_all(dir)?;
        Ok(())
    }
}
