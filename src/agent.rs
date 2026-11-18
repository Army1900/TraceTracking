mod native;

use crate::store;
use anyhow::{Context, Result, ensure};
use notify::{RecursiveMode, Watcher};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashMap},
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

pub const MISSING: &str = "-";
const MAX_INPUT: u64 = 8 * 1024 * 1024;
// The filesystem event can arrive on either side of the tool receipt.
const MATCH_SLOP_MS: i64 = 3_000;
pub const ACTIVITY_MS: i64 = 120_000;

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Phase {
    SessionStart,
    SessionIdle,
    SessionEnd,
    WriteStart,
    WriteFinish,
    WriteFail,
}

#[derive(Deserialize, Serialize)]
struct Receipt {
    version: u32,
    tool: String,
    #[serde(default = "local_instance")]
    instance: String,
    session_id: String,
    cwd: String,
    phase: Phase,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    turn_id: Option<String>,
    #[serde(default)]
    call_id: Option<String>,
    #[serde(default)]
    files: Vec<FileReceipt>,
}

#[derive(Deserialize, Serialize)]
struct FileReceipt {
    path: String,
    #[serde(default)]
    before: Option<String>,
    #[serde(default)]
    expected_after: Option<String>,
    #[serde(default)]
    observed_after: Option<String>,
}

fn local_instance() -> String {
    "local".into()
}

#[derive(Deserialize, Serialize)]
struct Queued {
    id: String,
    at: i64,
    receipt: Receipt,
}

pub fn schema(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS agent_sessions(
            id INTEGER PRIMARY KEY,tool TEXT NOT NULL,instance TEXT NOT NULL,external_id TEXT NOT NULL,
            cwd TEXT NOT NULL,title TEXT,model TEXT,state TEXT NOT NULL,last_seen INTEGER NOT NULL,
            UNIQUE(tool,instance,external_id));
         CREATE INDEX IF NOT EXISTS agent_sessions_recent ON agent_sessions(last_seen DESC);
         CREATE TABLE IF NOT EXISTS agent_actions(
            id INTEGER PRIMARY KEY,session_id INTEGER NOT NULL,call_id TEXT NOT NULL,path TEXT NOT NULL,turn_id TEXT,
            started INTEGER,finished INTEGER,state TEXT NOT NULL,before_hash TEXT,
            expected_hash TEXT,after_hash TEXT,verified INTEGER NOT NULL DEFAULT 0,
            UNIQUE(session_id,call_id,path));
         CREATE INDEX IF NOT EXISTS agent_actions_path ON agent_actions(path,after_hash,finished);
         CREATE TABLE IF NOT EXISTS agent_matches(
            event_id INTEGER NOT NULL,action_id INTEGER NOT NULL,PRIMARY KEY(event_id,action_id));
         CREATE INDEX IF NOT EXISTS agent_matches_action ON agent_matches(action_id);
         CREATE TABLE IF NOT EXISTS agent_receipts(id TEXT PRIMARY KEY);
         CREATE TABLE IF NOT EXISTS agent_focus(root TEXT PRIMARY KEY, settings TEXT NOT NULL);",
    )?;
    Ok(())
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Focus {
    pub tools: BTreeSet<String>,
    pub custom: HashMap<String, Brand>,
}

impl Focus {
    pub fn follows(&self, tool: &str) -> bool {
        self.tools.contains(tool)
    }
}

pub fn focus(db: &Connection, root: &str) -> Result<Option<Focus>> {
    let settings: Option<String> = db
        .query_row(
            "SELECT settings FROM agent_focus WHERE root=?",
            [root],
            |row| row.get(0),
        )
        .optional()?;
    Ok(settings.as_deref().map(serde_json::from_str).transpose()?)
}

pub fn save_focus(db: &Connection, root: &str, focus: &Focus) -> Result<()> {
    db.execute(
        "INSERT INTO agent_focus(root,settings) VALUES(?,?)
         ON CONFLICT(root) DO UPDATE SET settings=excluded.settings",
        params![root, serde_json::to_string(focus)?],
    )?;
    Ok(())
}

pub fn session_tool(db: &Connection, id: i64) -> Result<Option<String>> {
    Ok(db
        .query_row("SELECT tool FROM agent_sessions WHERE id=?", [id], |row| {
            row.get(0)
        })
        .optional()?)
}

pub fn capture(tool: Option<&str>) -> Result<()> {
    let mut input = Vec::new();
    io::stdin()
        .lock()
        .take(MAX_INPUT + 1)
        .read_to_end(&mut input)?;
    ensure!(input.len() as u64 <= MAX_INPUT, "AI 事件超出大小限制");
    let receipt = match tool {
        Some(tool) => native::translate(tool, serde_json::from_slice(&input)?)?,
        None => Some(serde_json::from_slice(&input)?),
    };
    if let Some(receipt) = receipt {
        enqueue(receipt)?;
    }
    Ok(())
}

pub fn hook_config(tool: &str) -> Result<()> {
    ensure!(
        matches!(tool, "claude-code" | "codex"),
        "未知的原生 AI 适配器"
    );
    let mut hooks = serde_json::Map::new();
    let command = serde_json::json!([{"type":"command","command":format!("tt ai-hook {tool}")}]);
    let matcher = if tool == "codex" {
        "apply_patch"
    } else {
        "Edit|Write"
    };
    for event in ["SessionStart", "UserPromptSubmit", "Stop", "SessionEnd"] {
        hooks.insert(event.into(), serde_json::json!([{"hooks":command}]));
    }
    if tool == "codex" {
        hooks.insert("Interrupt".into(), serde_json::json!([{"hooks":command}]));
    }
    for event in ["PreToolUse", "PostToolUse"] {
        hooks.insert(
            event.into(),
            serde_json::json!([{"matcher":matcher,"hooks":command}]),
        );
    }
    if tool == "claude-code" {
        hooks.insert(
            "PostToolUseFailure".into(),
            serde_json::json!([{"matcher":matcher,"hooks":command}]),
        );
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({"hooks":hooks}))?
    );
    Ok(())
}

fn inbox() -> Result<PathBuf> {
    let dir = store::data_dir()?.join("agent-inbox");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn normalized(path: &Path) -> Result<String> {
    Ok(path
        .to_str()
        .context("AI 事件路径需要 UTF-8")?
        .replace(std::path::MAIN_SEPARATOR, "/"))
}

fn absolute(cwd: &Path, path: &Path) -> Result<PathBuf> {
    let target = cwd.join(path);
    match target.canonicalize() {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = target.parent().context("缺少父目录")?;
            // Creation and removal receipts may refer to a path that is absent.
            Ok(absolute(cwd, parent)?.join(target.file_name().context("缺少文件名")?))
        }
        Err(error) => Err(error.into()),
    }
}

fn fingerprint(path: &Path) -> Option<String> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Some(MISSING.into()),
        Err(_) => return None,
    };
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(store::LIMIT + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= store::LIMIT).then(|| store::digest(&bytes))
}

fn valid_hash(hash: &str) -> bool {
    hash == MISSING || (hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn enqueue(mut receipt: Receipt) -> Result<()> {
    ensure!(receipt.version == 1, "仅支持第 1 版 AI 事件");
    for id in [&receipt.tool, &receipt.instance, &receipt.session_id] {
        ensure!(
            !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control),
            "无效的 AI 工具或会话 ID"
        );
    }
    let cwd = Path::new(&receipt.cwd).canonicalize()?;
    ensure!(cwd.is_dir(), "AI 会话工作目录无效");
    receipt.cwd = normalized(&cwd)?;
    let is_file = matches!(
        receipt.phase,
        Phase::WriteStart | Phase::WriteFinish | Phase::WriteFail
    );
    if is_file {
        ensure!(
            receipt
                .call_id
                .as_ref()
                .is_some_and(|id| !id.is_empty() && id.len() <= 512),
            "写入事件需要 call_id"
        );
        ensure!(
            !receipt.files.is_empty() && receipt.files.len() <= 256,
            "写入事件需要 1 到 256 个文件"
        );
    } else {
        receipt.files.clear();
    }
    for file in &mut receipt.files {
        ensure!(!file.path.is_empty(), "写入事件缺少文件路径");
        let target = absolute(&cwd, Path::new(&file.path))?;
        if receipt.phase == Phase::WriteFinish && file.observed_after.is_none() {
            file.observed_after = fingerprint(&target);
        }
        file.path = normalized(&target)?;
        for hash in [
            &mut file.before,
            &mut file.expected_after,
            &mut file.observed_after,
        ]
        .into_iter()
        .flatten()
        {
            ensure!(valid_hash(hash), "无效的 SHA-256");
            hash.make_ascii_lowercase();
        }
    }
    let now = chrono::Utc::now();
    let id = format!(
        "{}-{}",
        now.timestamp_nanos_opt().context("无效时间")?,
        std::process::id()
    );
    let item = Queued {
        id: id.clone(),
        at: now.timestamp_millis(),
        receipt,
    };
    let dir = inbox()?;
    let staged = dir.join(format!("{id}.tmp"));
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&staged)?;
    serde_json::to_writer(&mut file, &item)?;
    file.flush()?;
    drop(file);
    fs::rename(staged, dir.join(format!("{id}.json")))?;
    Ok(())
}

fn receive(db: &mut Connection, queued: &Queued) -> Result<()> {
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if tx.execute(
        "INSERT OR IGNORE INTO agent_receipts VALUES(?)",
        [&queued.id],
    )? == 0
    {
        return Ok(());
    }
    let r = &queued.receipt;
    let state = match r.phase {
        Phase::SessionIdle => "idle",
        Phase::SessionEnd => "ended",
        _ => "active",
    };
    let session: i64 = tx.query_row(
        "INSERT INTO agent_sessions(tool,instance,external_id,cwd,title,model,state,last_seen)
         VALUES(?,?,?,?,?,?,?,?)
         ON CONFLICT(tool,instance,external_id) DO UPDATE SET
           cwd=CASE WHEN excluded.last_seen>=last_seen THEN excluded.cwd ELSE cwd END,
           state=CASE WHEN excluded.last_seen>=last_seen THEN excluded.state ELSE state END,
           title=COALESCE(excluded.title,title),model=COALESCE(excluded.model,model),
           last_seen=max(last_seen,excluded.last_seen)
         RETURNING id",
        params![
            r.tool,
            r.instance,
            r.session_id,
            r.cwd,
            r.title,
            r.model,
            state,
            queued.at
        ],
        |row| row.get(0),
    )?;
    for file in &r.files {
        let call = r.call_id.as_deref().context("写入事件缺少 call_id")?;
        match r.phase {
            Phase::WriteStart => {
                let id: i64 = tx.query_row(
                    "INSERT INTO agent_actions(session_id,call_id,path,turn_id,started,state,before_hash,expected_hash)
                     VALUES(?,?,?,?,?,'writing',?,?)
                     ON CONFLICT(session_id,call_id,path) DO UPDATE SET
                       started=COALESCE(started,excluded.started),
                       before_hash=COALESCE(before_hash,excluded.before_hash),
                       expected_hash=COALESCE(expected_hash,excluded.expected_hash)
                     RETURNING id",
                    params![session,call,file.path,r.turn_id,queued.at,file.before,file.expected_after],
                    |row| row.get(0),
                )?;
                reconcile(&tx, id, &file.path)?;
            }
            Phase::WriteFail => {
                tx.execute(
                    "UPDATE agent_actions SET finished=?,state='failed'
                     WHERE session_id=? AND call_id=? AND path=? AND state='writing'",
                    params![queued.at, session, call, file.path],
                )?;
            }
            Phase::WriteFinish => {
                let id: i64 = tx.query_row(
                    "INSERT INTO agent_actions(session_id,call_id,path,turn_id,finished,state,before_hash,expected_hash,after_hash)
                     VALUES(?,?,?,?,?,'reported',?,?,?)
                     ON CONFLICT(session_id,call_id,path) DO UPDATE SET
                         finished=COALESCE(finished,excluded.finished),
                         state=CASE WHEN state='writing' THEN 'reported' ELSE state END,
                         before_hash=COALESCE(before_hash,excluded.before_hash),
                         expected_hash=COALESCE(expected_hash,excluded.expected_hash),
                         after_hash=COALESCE(after_hash,excluded.after_hash)
                     RETURNING id",
                    params![session,call,file.path,r.turn_id,queued.at,file.before,file.expected_after,file.observed_after],
                    |row| row.get(0),
                )?;
                reconcile(&tx, id, &file.path)?;
            }
            _ => {}
        }
    }
    tx.commit()?;
    Ok(())
}

fn reconcile(db: &Connection, id: i64, path: &str) -> Result<()> {
    if db.execute(
        "UPDATE agent_actions SET verified=COALESCE(expected_hash=after_hash,0)
         WHERE id=? AND state='reported'",
        [id],
    )? != 0
    {
        db.execute("DELETE FROM agent_matches WHERE action_id=?", [id])?;
        match_report(db, id, path)?;
    }
    Ok(())
}

// Use the same conditions whether the watcher or the tool finishes first.
const MATCH: &str = "a.state='reported' AND COALESCE(e.new,'-')=a.after_hash
     AND COALESCE(e.old,'-')<>a.after_hash
     AND e.kind IN ('A','M','D','R')";

fn match_report(db: &Connection, action: i64, path: &str) -> Result<()> {
    let native = path.replace('/', std::path::MAIN_SEPARATOR_STR);
    let path = Path::new(&native);
    // Probe the existing history index for each possible registered ancestor.
    for root in path.ancestors().skip(1) {
        let Some(root_str) = root.to_str() else {
            continue;
        };
        let rel = normalized(path.strip_prefix(root)?)?;
        db.execute(
            &format!(
                "INSERT OR IGNORE INTO agent_matches
              SELECT e.id,a.id FROM agent_actions a JOIN events e ON e.root=? AND e.path=?
              WHERE a.id=? AND e.stamp BETWEEN
                (COALESCE(a.started,a.finished)-?)/1000 AND (a.finished+?)/1000 AND {MATCH}"
            ),
            params![root_str, rel, action, MATCH_SLOP_MS, MATCH_SLOP_MS],
        )?;
    }
    Ok(())
}

pub fn match_observed(db: &Connection, root: &str, path: &str, event: i64) -> Result<()> {
    let path = normalized(&Path::new(root).join(path))?;
    db.execute(
        &format!("INSERT OR IGNORE INTO agent_matches
          SELECT e.id,a.id FROM events e JOIN agent_actions a ON a.path=? AND a.after_hash=COALESCE(e.new,'-')
          WHERE e.id=? AND e.stamp BETWEEN
            (COALESCE(a.started,a.finished)-?)/1000 AND (a.finished+?)/1000 AND {MATCH}"),
        params![path,event,MATCH_SLOP_MS,MATCH_SLOP_MS],
    )?;
    Ok(())
}

const UNIQUE_MATCH: &str = "
    a.verified=1
    AND (a.before_hash IS NULL OR COALESCE(e.old,'-')=a.before_hash)
    AND (SELECT count(*) FROM agent_matches competing WHERE competing.event_id=m.event_id)=1
    AND NOT EXISTS (
        SELECT 1 FROM agent_matches repeated JOIN events other ON other.id=repeated.event_id
        WHERE repeated.action_id=a.id AND other.root=e.root AND other.id<>e.id
          AND (a.before_hash IS NULL OR COALESCE(other.old,'-')=a.before_hash)
    )
    AND NOT EXISTS (
        SELECT 1 FROM agent_actions parallel
        WHERE parallel.path=a.path AND parallel.session_id<>a.session_id
          AND parallel.started<=a.finished AND COALESCE(parallel.finished,parallel.started+120000)>=COALESCE(a.started,a.finished)
          AND parallel.state IN ('writing','reported')
    )";

pub fn session_condition() -> String {
    format!(
        "EXISTS(SELECT 1 FROM agent_matches m JOIN agent_actions a ON a.id=m.action_id
                      WHERE m.event_id=e.id AND a.session_id=?5 AND ({UNIQUE_MATCH}))"
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub id: i64,
    pub tool: String,
    pub external_id: String,
}

impl Identity {
    pub fn short(&self) -> String {
        self.external_id.chars().take(6).collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mark {
    Linked(Identity),
    Possible(Identity),
    Ambiguous,
}

pub fn marks(db: &Connection, ids: &[i64]) -> Result<HashMap<i64, Mark>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = std::iter::repeat_n("?", ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let mut query = db.prepare(&format!(
        "SELECT m.event_id,s.id,s.tool,s.external_id,({UNIQUE_MATCH})
         FROM agent_matches m JOIN agent_actions a ON a.id=m.action_id
         JOIN agent_sessions s ON s.id=a.session_id JOIN events e ON e.id=m.event_id
         WHERE m.event_id IN ({placeholders})"
    ))?;
    let mut result = HashMap::new();
    let mut rows = query.query(rusqlite::params_from_iter(ids))?;
    while let Some(row) = rows.next()? {
        let event = row.get::<_, i64>(0)?;
        let identity = Identity {
            id: row.get(1)?,
            tool: row.get(2)?,
            external_id: row.get(3)?,
        };
        let mark = if row.get::<_, bool>(4)? {
            Mark::Linked(identity)
        } else {
            Mark::Possible(identity)
        };
        result
            .entry(event)
            .and_modify(|old| *old = Mark::Ambiguous)
            .or_insert(mark);
    }
    Ok(result)
}

#[derive(Clone)]
pub struct Session {
    pub identity: Identity,
    pub title: Option<String>,
    pub model: Option<String>,
    pub state: String,
    pub last_seen: i64,
    pub last_write: Option<i64>,
    pub files: usize,
    pub writing: Option<(String, i64)>,
}

impl Session {
    pub fn status(&self, now: i64) -> &'static str {
        if self.state == "ended" {
            return "会话已结束";
        }
        if self.current_write(now).is_some() {
            return "准备写入";
        }
        if now.saturating_sub(self.last_seen) > ACTIVITY_MS {
            return "最近活动";
        }
        if self.state == "idle" {
            return "等待下一轮";
        }
        "会话活动中"
    }
    pub fn current_write(&self, now: i64) -> Option<&str> {
        let (path, since) = self.writing.as_ref()?;
        (self.state == "active" && now.saturating_sub(*since) < ACTIVITY_MS)
            .then_some(path.as_str())
    }
}

pub fn sessions(db: &Connection, root: &str) -> Result<Vec<Session>> {
    let db_root = root;
    let root = normalized(Path::new(root))?;
    let prefix = format!("{}/", root.trim_end_matches('/'));
    let upper = format!("{}0", root.trim_end_matches('/'));
    let now = chrono::Utc::now().timestamp_millis();
    let filter = store::rules(db, db_root)?;
    let base = PathBuf::from(db_root);
    let path_prefix = prefix.clone();
    db.create_scalar_function(
        "visible_agent_path",
        1,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8,
        move |ctx| {
            let path: String = ctx.get(0)?;
            Ok(path.strip_prefix(&path_prefix).is_some_and(|rel| {
                !filter
                    .matched_path_or_any_parents(base.join(rel), false)
                    .is_ignore()
            }))
        },
    )?;
    let focus = focus(db, db_root)?;
    db.create_scalar_function(
        "followed_agent",
        1,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8,
        move |ctx| {
            let tool: String = ctx.get(0)?;
            Ok(focus.as_ref().is_none_or(|focus| focus.follows(&tool)))
        },
    )?;
    let scope = format!(
        "FROM agent_actions a
             WHERE a.session_id=s.id AND a.path>=?1 AND a.path<?2 AND a.state='reported'
               AND visible_agent_path(a.path)
               AND EXISTS(SELECT 1 FROM agent_matches m JOIN events e ON e.id=m.event_id
                          WHERE m.action_id=a.id AND e.root=?5 AND ({UNIQUE_MATCH}))"
    );
    let mut query = db.prepare(&format!(
        "SELECT s.id,s.tool,s.external_id,s.title,s.model,s.state,s.last_seen,
           (SELECT count(DISTINCT a.path) {scope}), live.path,live.started,
           (SELECT max(a.finished) {scope})
         FROM agent_sessions s
         LEFT JOIN agent_actions live ON live.id=(
             SELECT a.id FROM agent_actions a WHERE a.session_id=s.id AND a.path>=?1 AND a.path<?2
               AND a.state='writing' AND a.started>=?3 AND visible_agent_path(a.path)
               ORDER BY a.started DESC LIMIT 1
         )
         WHERE followed_agent(s.tool) AND (
             s.cwd=?4 OR (s.cwd>=?1 AND s.cwd<?2)
             OR EXISTS(SELECT 1 FROM agent_actions a WHERE a.session_id=s.id AND a.path>=?1 AND a.path<?2
                       AND visible_agent_path(a.path)))
         ORDER BY s.last_seen DESC LIMIT 60"
    ))?;
    let items = query
        .query_map(
            params![prefix, upper, now - ACTIVITY_MS, root, db_root],
            |row| {
                let writing: Option<String> = row.get(8)?;
                let since: Option<i64> = row.get(9)?;
                Ok(Session {
                    identity: Identity {
                        id: row.get(0)?,
                        tool: row.get(1)?,
                        external_id: row.get(2)?,
                    },
                    title: row.get(3)?,
                    model: row.get(4)?,
                    state: row.get(5)?,
                    last_seen: row.get(6)?,
                    files: row.get(7)?,
                    last_write: row.get(10)?,
                    writing: writing
                        .as_deref()
                        .and_then(|path| path.strip_prefix(&prefix))
                        .zip(since)
                        .map(|(path, at)| (path.to_string(), at)),
                })
            },
        )?
        .collect::<rusqlite::Result<_>>()?;
    Ok(items)
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Brand {
    pub name: String,
    pub icon: String,
    pub color: String,
    #[serde(default)]
    pub ascii: Option<String>,
}

pub fn brands() -> HashMap<String, Brand> {
    let mut brands = HashMap::from([
        (
            "claude-code".into(),
            Brand {
                name: "Claude Code".into(),
                icon: "✳".into(),
                color: "#FFA071".into(),
                ascii: Some("C".into()),
            },
        ),
        (
            "codex".into(),
            Brand {
                name: "Codex".into(),
                icon: "◆".into(),
                color: "#7FB4FF".into(),
                ascii: Some("X".into()),
            },
        ),
    ]);
    if let Ok(dir) = store::data_dir()
        && let Ok(bytes) = fs::read(dir.join("agents.json"))
        && let Ok(custom) = serde_json::from_slice::<HashMap<String, Brand>>(&bytes)
    {
        brands.extend(custom);
    }
    brands
}

pub struct InboxWorker {
    stop: Arc<AtomicBool>,
    pub updates: mpsc::Receiver<()>,
}

impl InboxWorker {
    pub fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let quit = stop.clone();
        let (send, updates) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let Ok(dir) = inbox() else { return };
            let (notify, changes) = mpsc::sync_channel(1);
            let watcher =
                notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                    if event.is_err()
                        || event
                            .as_ref()
                            .is_ok_and(|e| !matches!(e.kind, notify::EventKind::Access(_)))
                    {
                        let _ = notify.try_send(());
                    }
                });
            let mut watcher = watcher.ok();
            let listening = watcher
                .as_mut()
                .is_some_and(|w| w.watch(&dir, RecursiveMode::NonRecursive).is_ok());
            let Ok(mut db) = store::database() else {
                return;
            };
            let mut wake = true;
            while !quit.load(Ordering::Relaxed) {
                if wake {
                    // The same receipt may be observed by two viewers. SQLite deduplicates it.
                    let mut paths: Vec<_> = fs::read_dir(&dir)
                        .into_iter()
                        .flatten()
                        .filter_map(Result::ok)
                        .map(|entry| entry.path())
                        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
                        .collect();
                    paths.sort_unstable();
                    let mut retry = false;
                    for path in paths {
                        if quit.load(Ordering::Relaxed) {
                            return;
                        }
                        let Ok(bytes) = fs::read(&path) else { continue };
                        let Ok(receipt) = serde_json::from_slice::<Queued>(&bytes) else {
                            continue;
                        };
                        match receive(&mut db, &receipt) {
                            Ok(()) => {
                                let _ = fs::remove_file(path);
                            }
                            Err(error) => {
                                retry |= matches!(
                                    error.downcast_ref::<rusqlite::Error>(),
                                    Some(rusqlite::Error::SqliteFailure(code, _))
                                        if matches!(code.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
                                );
                            }
                        }
                    }
                    let _ = send.try_send(());
                    if retry {
                        std::thread::sleep(Duration::from_millis(300));
                        continue;
                    }
                }
                // Timeout only lets the worker observe Stop; a healthy watcher needs no scans.
                let delay = if listening { 500 } else { 1000 };
                wake = if listening {
                    changes.recv_timeout(Duration::from_millis(delay)).is_ok()
                } else {
                    std::thread::sleep(Duration::from_millis(delay));
                    true
                };
            }
        });
        Self { stop, updates }
    }
}

impl Drop for InboxWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queued(
        root: &str,
        session: &str,
        token: &str,
        at: i64,
        phase: Phase,
        before: &str,
        after: &str,
    ) -> Queued {
        Queued {
            id: format!("{session}-{token}-{at}"),
            at,
            receipt: Receipt {
                version: 1,
                tool: "test-agent".into(),
                instance: local_instance(),
                session_id: session.into(),
                cwd: normalized(Path::new(root)).unwrap(),
                phase,
                title: None,
                model: None,
                turn_id: None,
                call_id: Some(token.into()),
                files: vec![FileReceipt {
                    path: normalized(&Path::new(root).join("config.yaml")).unwrap(),
                    before: Some(before.into()),
                    expected_after: Some(after.into()),
                    observed_after: (phase == Phase::WriteFinish).then(|| after.into()),
                }],
            },
        }
    }

    fn change(db: &Connection, root: &str, at: i64, before: &str, after: &str) -> Result<i64> {
        db.execute(
            "INSERT INTO events(root,path,stamp,kind,old,new) VALUES(?,'config.yaml',?,'M',?,?)",
            params![root, at / 1000, before, after],
        )?;
        let id = db.last_insert_rowid();
        match_observed(db, root, "config.yaml", id)?;
        Ok(id)
    }

    #[test]
    fn follows_selected_tools_per_directory_including_an_explicit_empty_selection() -> Result<()> {
        let mut db = store::test_database()?;
        let tmp = std::env::temp_dir().canonicalize()?;
        let project = tmp.join("focus-a");
        let other = tmp.join("focus-b");
        let root = project.to_str().unwrap();
        let other = other.to_str().unwrap();
        assert!(focus(&db, root)?.is_none());
        let mut first = queued(
            root,
            "claude",
            "start",
            1000,
            Phase::SessionStart,
            MISSING,
            MISSING,
        );
        first.receipt.tool = "claude-code".into();
        first.receipt.files.clear();
        let mut second = queued(
            root,
            "codex",
            "start",
            1001,
            Phase::SessionStart,
            MISSING,
            MISSING,
        );
        second.receipt.tool = "codex".into();
        second.receipt.files.clear();
        receive(&mut db, &first)?;
        receive(&mut db, &second)?;
        assert_eq!(sessions(&db, root)?.len(), 2);
        let mut settings = Focus::default();
        settings.tools.insert("claude-code".into());
        settings.custom.insert(
            "mine".into(),
            Brand {
                name: "My tool".into(),
                icon: "S".into(),
                color: "#2255AA".into(),
                ascii: Some("S".into()),
            },
        );
        save_focus(&db, root, &settings)?;
        let reloaded = focus(&db, root)?.unwrap();
        assert_eq!(reloaded.custom["mine"].color, "#2255AA");
        let visible = sessions(&db, root)?;
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].identity.tool, "claude-code");
        save_focus(&db, root, &Focus::default())?;
        assert!(focus(&db, root)?.is_some());
        assert!(sessions(&db, root)?.is_empty());
        assert!(focus(&db, other)?.is_none());
        Ok(())
    }

    #[test]
    fn matches_both_receipt_orders_and_keeps_collisions_ambiguous() -> Result<()> {
        let root = std::env::temp_dir().canonicalize()?;
        let root = root.to_str().unwrap();
        let mut db = store::test_database()?;
        let now = chrono::Utc::now().timestamp_millis();
        let a = store::digest(b"before");
        let b = store::digest(b"first");
        let c = store::digest(b"second");
        let d = store::digest(b"ambiguous");
        receive(
            &mut db,
            &queued(
                root,
                "old",
                "expired",
                now - ACTIVITY_MS - 10_000,
                Phase::WriteStart,
                &a,
                &b,
            ),
        )?;
        receive(
            &mut db,
            &queued(root, "alice", "1", now, Phase::WriteStart, &a, &b),
        )?;
        let first = change(&db, root, now + 300, &a, &b)?;
        let finished = queued(root, "alice", "1", now + 500, Phase::WriteFinish, &a, &b);
        receive(&mut db, &finished)?;
        receive(&mut db, &finished)?;

        receive(
            &mut db,
            &queued(root, "bob", "2", now + 1000, Phase::WriteStart, &b, &c),
        )?;
        receive(
            &mut db,
            &queued(root, "bob", "2", now + 1100, Phase::WriteFinish, &b, &c),
        )?;
        let second = change(&db, root, now + 1500, &b, &c)?;

        let attribution = marks(&db, &[first, second])?;
        let Some(Mark::Linked(alice)) = attribution.get(&first) else {
            anyhow::bail!("late receipt was not linked");
        };
        assert_eq!(alice.external_id, "alice");
        assert!(
            matches!(attribution.get(&second),Some(Mark::Linked(identity)) if identity.external_id=="bob")
        );

        for (session, offset) in [("alice", 2000), ("bob", 2100)] {
            receive(
                &mut db,
                &queued(root, session, "3", now + offset, Phase::WriteStart, &c, &d),
            )?;
        }
        let third = change(&db, root, now + 2200, &c, &d)?;
        for (session, offset) in [("alice", 2400), ("bob", 2300)] {
            receive(
                &mut db,
                &queued(root, session, "3", now + offset, Phase::WriteFinish, &c, &d),
            )?;
        }
        assert_eq!(marks(&db, &[third])?.get(&third), Some(&Mark::Ambiguous));
        let timeline = store::entries(
            &db,
            store::EntryQuery {
                root,
                session: Some(alice.id),
                ..Default::default()
            },
        )?;
        assert_eq!(
            timeline.iter().map(|row| row.id).collect::<Vec<_>>(),
            [first]
        );
        let activity = sessions(&db, root)?;
        assert_eq!(activity.len(), 3);
        let old = activity
            .iter()
            .find(|session| session.identity.external_id == "old")
            .unwrap();
        assert_eq!(old.files, 0);
        assert!(old.current_write(now).is_none());
        assert!(
            activity
                .iter()
                .filter(|session| session.identity.external_id != "old")
                .all(|session| session.files == 1)
        );
        // The watcher coalesced the intermediate version, so this is a lead, not an attribution.
        let final_hash = store::digest(b"coalesced final");
        receive(
            &mut db,
            &queued(
                root,
                "bob",
                "4",
                now + 6_000,
                Phase::WriteStart,
                &c,
                &final_hash,
            ),
        )?;
        let coalesced = change(&db, root, now + 6_100, &d, &final_hash)?;
        receive(
            &mut db,
            &queued(
                root,
                "bob",
                "4",
                now + 6_200,
                Phase::WriteFinish,
                &c,
                &final_hash,
            ),
        )?;
        assert!(
            matches!(marks(&db,&[coalesced])?.get(&coalesced),Some(Mark::Possible(identity)) if identity.external_id=="bob")
        );
        Ok(())
    }
}
