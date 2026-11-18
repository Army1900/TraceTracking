use crate::{
    git::{StatusMonitor, Statuses},
    refs,
    store::{self, Entry},
    syntax::{Syntax, terminal_text},
    view::{self, Row, TextPoint, WrappedRows},
    watch,
};
use anyhow::Result;
use crossterm::{
    event::{
        self, DisableMouseCapture, Event, KeyCode as K, KeyEventKind, KeyModifiers, MouseEvent,
        MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Paragraph},
};
use rusqlite::{Connection, params};
use std::{
    io::{self, Stdout},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
const ACC: Color = Color::Rgb(110, 199, 178);
const DIM: Color = Color::DarkGray;
#[derive(Clone, Default)]
struct Position {
    entry: Option<Entry>,
    recent: bool,
    folded: bool,
    only: String,
    query: String,
    mode: usize,
    offset: usize,
    line: TextPoint,
    top: TextPoint,
    body: bool,
    diff: bool,
    revision: Option<String>,
}
enum Panel {
    Main,
    Search,
    Help,
    Projects,
    Ignore,
    Links,
    Git,
    Add,
}
struct QueryTask<T> {
    cancel: Arc<AtomicBool>,
    results: mpsc::Receiver<Result<T>>,
}

impl<T: Send + 'static> QueryTask<T> {
    fn start(query: impl FnOnce(&Connection, &AtomicBool) -> Result<T> + Send + 'static) -> Self {
        let (tx, results) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        std::thread::spawn(move || {
            let result = store::read_only().and_then(|db| {
                let progress = flag.clone();
                db.progress_handler(500, Some(move || progress.load(Ordering::Relaxed)));
                query(&db, &flag)
            });
            let _ = tx.send(result);
        });
        Self { cancel, results }
    }
}

impl<T> Drop for QueryTask<T> {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}
type Groups = std::collections::HashMap<i64, (i64, usize)>;

#[derive(Default)]
struct Preview {
    rows: Vec<Row>,
    wrap: WrappedRows,
    line_digits: usize,
    syntax: Syntax,
    old_syntax: Syntax,
    changes: Vec<usize>,
    comparison_note: Option<&'static str>,
    source: String,
}

#[derive(PartialEq, Eq)]
struct ViewKey {
    recent: bool,
    folded: bool,
    only: String,
    query: String,
    mode: usize,
    diff: bool,
}

impl ViewKey {
    fn of(pos: &Position, recent: bool) -> Self {
        Self {
            recent,
            folded: pos.folded,
            only: pos.only.clone(),
            query: pos.query.clone(),
            mode: pos.mode,
            diff: pos.diff,
        }
    }
}

struct ViewPage {
    pos: Position,
    list: Vec<Entry>,
    groups: Groups,
    raw_count: usize,
    preview: Preview,
}

struct ViewQuery {
    key: ViewKey,
    epoch: u64,
    task: QueryTask<ViewPage>,
}

struct CachedView {
    key: ViewKey,
    epoch: u64,
    page: ViewPage,
}

#[derive(Default)]
struct ViewSwitch {
    preload: Option<ViewQuery>,
    refresh: Option<ViewQuery>,
    cached: Option<CachedView>,
    wanted: bool,
    refresh_due: bool,
    epoch: u64,
    active_epoch: Option<u64>,
    transition: Option<Instant>,
    failed: Option<(ViewKey, u64)>,
}

const TRANSITION: Duration = Duration::from_millis(128);

struct Bookmark {
    epoch: Option<u64>,
    position: Position,
    panel: Panel,
    choice: usize,
    list: Vec<Entry>,
    groups: Groups,
    count: usize,
    options: Vec<String>,
    targets: Vec<refs::Target>,
}
struct App {
    db: Connection,
    root: String,
    worker: watch::Worker,
    git_monitor: StatusMonitor,
    git_status: Option<Statuses>,
    status: String,
    pos: Position,
    list: Vec<Entry>,
    preview: Preview,
    notice: Option<&'static str>,
    views: ViewSwitch,
    stack: Vec<Bookmark>,
    panel: Panel,
    options: Vec<String>,
    choice: usize,
    targets: Vec<refs::Target>,
    groups: Groups,
    raw_count: usize,
    positions: std::collections::HashMap<String, Position>,
    search: Option<QueryTask<Vec<Entry>>>,
    references: Option<QueryTask<Vec<refs::Target>>>,
    reference_error: Option<String>,
    search_after: Option<std::time::Instant>,
    input: String,
    follow: bool,
    pending: bool,
    dirty: bool,
}
impl ViewQuery {
    fn start(root: String, mut pos: Position, recent: bool, epoch: u64, size: Size) -> Self {
        let key = ViewKey::of(&pos, recent);
        pos.recent = recent;
        pos.offset = 0;
        pos.revision = None;
        let area = Rect::new(0, 0, size.width, size.height);
        let width = reader_panes(screen_bands(area, 0)[1], true)[1].width;
        let task = QueryTask::start(move |db, cancelled| {
            let mut list = store::entries(db, &root, recent, &pos.only, &pos.query, pos.mode, 0)?;
            let raw_count = list.len();
            let groups = fold_page(&mut list, &pos);
            pos.entry = list.first().cloned();
            anyhow::ensure!(!cancelled.load(Ordering::Relaxed), "查询已取消");
            let mut preview = Preview::prepare(db, &root, &mut pos, &groups, true)?;
            anyhow::ensure!(!cancelled.load(Ordering::Relaxed), "查询已取消");
            preview.prepare_wrap(width);
            Ok(ViewPage {
                pos,
                list,
                groups,
                raw_count,
                preview,
            })
        });
        Self { key, epoch, task }
    }

    fn take_result(slot: &mut Option<Self>) -> Option<(Self, Result<ViewPage>)> {
        let result = match slot.as_ref()?.task.results.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => Err(anyhow::anyhow!("视图查询已中断")),
        };
        slot.take().map(|job| (job, result))
    }
}

fn fold_page(list: &mut Vec<Entry>, pos: &Position) -> Groups {
    let mut groups = Groups::new();
    if !pos.folded || pos.recent || pos.mode != 0 || !pos.query.is_empty() {
        return groups;
    }
    let mut compact: Vec<Entry> = Vec::new();
    let mut previous = 0;
    for entry in std::mem::take(list) {
        if let Some(last) = compact.last()
            && last.kind == "M"
            && entry.kind == "M"
            && last.path == entry.path
            && previous - entry.stamp <= 60
        {
            let group = groups.entry(last.id).or_insert((last.id, 1));
            group.0 = entry.id;
            group.1 += 1;
        } else {
            previous = entry.stamp;
            compact.push(entry);
            continue;
        }
        previous = entry.stamp;
    }
    *list = compact;
    groups
}

impl Position {
    fn anchor_row(&mut self, row: usize) {
        self.line = TextPoint::at_row(row);
        self.top = TextPoint::at_row(row.saturating_sub(3));
    }
}

impl Preview {
    fn prepare(
        db: &Connection,
        root: &str,
        pos: &mut Position,
        groups: &Groups,
        locate: bool,
    ) -> Result<Self> {
        let mut preview = Self::default();
        if locate {
            pos.anchor_row(0);
        }
        if let Some(e) = &pos.entry {
            let (mut a, b, source) = store::bodies(db, e.id)?;
            preview.source = source;
            if let Some((oldest, count)) = groups.get(&e.id) {
                a = store::bodies(db, *oldest)?.0;
                preview.source = format!("{count} 次保存 · 本组开始前至末次保存");
            }
            let kind = if let Some(rev) = &pos.revision {
                a = Some(crate::git::body(root, &e.path, rev)?);
                preview.source = format!("Git {}", &rev[..rev.len().min(8)]);
                "M"
            } else {
                &e.kind
            };
            preview.comparison_note = match (kind, a.as_deref(), b.as_deref()) {
                ("B", _, _) => Some("首次记录，没有上一版本"),
                (_, Some(old), Some(new)) if old == new => Some("两个版本的正文一致"),
                ("A", None, Some(_)) | ("D", Some(_), None) | (_, Some(_), Some(_)) => None,
                _ => Some("缺少可对比的文本快照"),
            };
            pos.diff &= preview.comparison_note.is_none();
            let (before, after, visible_kind) = if !pos.diff && pos.revision.is_some() {
                (None, a.as_deref(), "B")
            } else if !pos.diff && preview.comparison_note.is_some() && matches!(kind, "O" | "R") {
                (None, b.as_deref(), "B")
            } else {
                (a.as_deref(), b.as_deref(), kind)
            };
            preview.rows = view::content(before, after, visible_kind, pos.diff);
            preview.syntax = Syntax::parse(&e.path, after);
            if pos.diff {
                preview.old_syntax = Syntax::parse(&e.path, before);
            }
            preview
                .changes
                .extend(preview.rows.iter().enumerate().filter_map(|(i, r)| {
                    (r.change.is_some() && (i == 0 || r.change != preview.rows[i - 1].change))
                        .then_some(i)
                }));
            if e.kind == "R" && pos.revision.is_none() {
                let prev: String =
                    db.query_row("SELECT previous FROM moves WHERE id=?", [e.id], |r| {
                        r.get(0)
                    })?;
                preview.source = format!("{prev} → {}", e.path);
            }
            if e.kind == "O" && pos.revision.is_none() {
                preview.source = "离线期间变更".into();
            }
            if locate {
                let row = if pos.mode > 0 && !pos.query.is_empty() {
                    preview
                        .rows
                        .iter()
                        .position(|r| r.text.contains(&pos.query))
                        .unwrap_or(0)
                } else {
                    preview.changes.first().copied().unwrap_or(0)
                };
                pos.anchor_row(row);
            }
        }
        let last = preview.rows.len().saturating_sub(1);
        if pos.line.row > last {
            pos.anchor_row(last);
        }
        pos.top.row = pos.top.row.min(last);
        preview.line_digits = preview
            .rows
            .iter()
            .map(|row| row.num)
            .max()
            .unwrap_or(0)
            .to_string()
            .len()
            .max(4);
        Ok(preview)
    }

    fn prepare_wrap(&mut self, pane_width: u16) {
        let width = (pane_width as usize)
            .saturating_sub(self.line_digits + 5)
            .max(1);
        if self.wrap.width != width {
            self.wrap = WrappedRows::new(&self.rows, width);
        }
    }
}

impl App {
    fn apply_page(&mut self, mut page: ViewPage, epoch: u64) {
        page.pos.body = self.pos.body;
        self.pos = page.pos;
        self.list = page.list;
        self.groups = page.groups;
        self.raw_count = page.raw_count;
        self.preview = page.preview;
        self.views.active_epoch = Some(epoch);
        self.dirty = true;
    }

    fn show_alternate(&mut self, target: CachedView) {
        let previous =
            if self.views.active_epoch == Some(self.views.epoch) && self.pos.revision.is_none() {
                Some(CachedView {
                    key: ViewKey::of(&self.pos, self.pos.recent),
                    epoch: self.views.epoch,
                    page: ViewPage {
                        pos: self.pos.clone(),
                        list: std::mem::take(&mut self.list),
                        groups: std::mem::take(&mut self.groups),
                        raw_count: self.raw_count,
                        preview: std::mem::take(&mut self.preview),
                    },
                })
            } else {
                None
            };
        self.pending = target.epoch != self.views.epoch;
        self.apply_page(target.page, target.epoch);
        self.views.cached = previous;
        self.views.wanted = false;
        self.views.refresh = None;
        self.views.refresh_due = false;
        self.views.transition = Some(Instant::now());
    }

    fn toggle_view(&mut self) {
        self.search = None;
        self.search_after = None;
        self.views.wanted = !self.views.wanted;
        if !self.views.wanted {
            return;
        }
        let key = ViewKey::of(&self.pos, !self.pos.recent);
        if self
            .views
            .cached
            .as_ref()
            .is_some_and(|cache| cache.key == key && cache.epoch == self.views.epoch)
        {
            let target = self.views.cached.take().unwrap();
            self.show_alternate(target);
        } else {
            self.views.failed = None;
        }
    }

    fn view_tick(&mut self, size: Size) {
        if let Some(start) = self.views.transition {
            if start.elapsed() >= TRANSITION {
                self.views.transition = None;
            }
            self.dirty = true;
        }
        if !matches!(self.panel, Panel::Main)
            || self.search.is_some()
            || self.search_after.is_some()
        {
            return;
        }

        let key = ViewKey::of(&self.pos, self.pos.recent);
        if let Some((job, result)) = ViewQuery::take_result(&mut self.views.refresh)
            && job.key == key
            && !self.views.wanted
            && (self.follow || self.pos.entry.is_none())
        {
            match result {
                Ok(page) => self.apply_page(page, job.epoch),
                Err(_) => {
                    self.notice = Some("列表加载失败，按 g 重试");
                    self.dirty = true;
                }
            }
        }
        let alternate = ViewKey::of(&self.pos, !self.pos.recent);
        if let Some((job, result)) = ViewQuery::take_result(&mut self.views.preload)
            && job.key == alternate
            && (job.epoch == self.views.epoch || self.views.wanted)
        {
            match result {
                Ok(page) => {
                    let cache = CachedView {
                        key: job.key,
                        epoch: job.epoch,
                        page,
                    };
                    if self.views.wanted {
                        self.show_alternate(cache);
                        return;
                    }
                    self.views.cached = Some(cache);
                }
                Err(_) => {
                    self.views.failed = Some((job.key, job.epoch));
                    if self.views.wanted {
                        self.views.wanted = false;
                        self.notice = Some("视图切换失败，按 t 重试");
                        self.dirty = true;
                    }
                }
            }
        }

        if !self.views.wanted
            && self.views.refresh_due
            && (self.follow || self.pos.entry.is_none())
            && self.views.refresh.is_none()
        {
            self.views.refresh_due = false;
            self.views.refresh = Some(ViewQuery::start(
                self.root.clone(),
                self.pos.clone(),
                self.pos.recent,
                self.views.epoch,
                size,
            ));
        }
        if self
            .views
            .preload
            .as_ref()
            .is_some_and(|job| job.key != alternate)
        {
            self.views.preload = None;
        }
        if self
            .views
            .cached
            .as_ref()
            .is_some_and(|cache| cache.key != alternate || cache.epoch != self.views.epoch)
        {
            self.views.cached = None;
        }
        if (self.views.wanted || self.views.refresh.is_none())
            && self.views.preload.is_none()
            && self.views.cached.is_none()
            && self
                .views
                .failed
                .as_ref()
                .is_none_or(|(key, epoch)| *key != alternate || *epoch != self.views.epoch)
        {
            self.views.preload = Some(ViewQuery::start(
                self.root.clone(),
                self.pos.clone(),
                !self.pos.recent,
                self.views.epoch,
                size,
            ));
        }
    }

    fn reload(&mut self, keep: bool) -> Result<()> {
        self.views.refresh = None;
        self.views.refresh_due = false;
        self.views.active_epoch = Some(self.views.epoch);
        self.list = store::entries(
            &self.db,
            &self.root,
            self.pos.recent,
            &self.pos.only,
            &self.pos.query,
            self.pos.mode,
            self.pos.offset,
        )?;
        self.group_page();
        if !keep || self.pos.entry.is_none() {
            self.pos.entry = self.list.first().cloned();
            self.load(true)?;
        }
        Ok(())
    }
    fn group_page(&mut self) {
        self.raw_count = self.list.len();
        self.groups = fold_page(&mut self.list, &self.pos);
    }

    fn load(&mut self, locate: bool) -> Result<()> {
        self.preview = Preview::prepare(&self.db, &self.root, &mut self.pos, &self.groups, locate)?;
        Ok(())
    }

    fn toggle_diff(&mut self) -> Result<()> {
        if self.pos.entry.is_none() {
            self.notice = Some("请先选择一个文件");
            return Ok(());
        }
        if let Some(note) = self.preview.comparison_note {
            self.notice = Some(note);
            return Ok(());
        }
        let anchor = self
            .preview
            .rows
            .get(self.pos.line.row)
            .map(|r| (r.num, r.mark, r.change));
        self.pos.diff = !self.pos.diff;
        self.load(false)?;
        if let Some((num, mark, change)) = anchor {
            let start = change.and_then(|c| self.preview.changes.get(c - 1).copied());
            let exact = || {
                self.preview.rows.iter().position(|r| {
                    num > 0
                        && r.num == num
                        && r.change == change
                        && (mark == '-') == (r.mark == '-')
                })
            };
            let row = if self.pos.diff {
                start.or_else(exact)
            } else {
                exact().or(start)
            }
            .or_else(|| self.preview.changes.first().copied())
            .unwrap_or(0);
            self.pos.anchor_row(row);
        }
        Ok(())
    }

    fn jump_change(&mut self, forward: bool) {
        let target = if forward {
            self.preview.changes.get(
                self.preview
                    .changes
                    .partition_point(|&i| i <= self.pos.line.row),
            )
        } else {
            self.preview.changes[..self
                .preview
                .changes
                .partition_point(|&i| i < self.pos.line.row)]
                .last()
        };
        if let Some(&target) = target {
            self.pos.anchor_row(target);
        } else {
            self.notice = Some(if self.preview.changes.is_empty() {
                "当前版本没有可跳转的变化"
            } else if forward {
                "已到最后一处变化"
            } else {
                "已到第一处变化"
            });
        }
    }

    fn scroll(&mut self, forward: bool, amount: usize, body: Rect) {
        if self.preview.rows.is_empty() {
            return;
        }
        self.preview.prepare_wrap(body.width);
        let rows = self.preview.wrap.len(&self.preview.rows);
        let advance = |index: usize| {
            if forward {
                index.saturating_add(amount).min(rows - 1)
            } else {
                index.saturating_sub(amount)
            }
        };
        let line = advance(self.preview.wrap.index(self.pos.line));
        let mut top = self.preview.wrap.index(self.pos.top);
        if amount > 1 {
            top = advance(top);
        }
        let height = body.height.saturating_sub(2) as usize;
        self.pos.top = self
            .preview
            .wrap
            .point(visible_top(top, line, rows, height));
        self.pos.line = self.preview.wrap.point(line);
    }

    fn push(&mut self) {
        self.stack.push(Bookmark {
            epoch: self.views.active_epoch,
            position: self.pos.clone(),
            panel: std::mem::replace(&mut self.panel, Panel::Main),
            choice: self.choice,
            list: self.list.clone(),
            groups: self.groups.clone(),
            count: self.raw_count,
            options: self.options.clone(),
            targets: self.targets.clone(),
        });
    }
    fn back(&mut self) -> Result<()> {
        self.references = None;
        self.reference_error = None;
        self.search = None;
        self.search_after = None;
        if let Some(bookmark) = self.stack.pop() {
            self.views.active_epoch = bookmark.epoch;
            self.pos = bookmark.position;
            self.panel = bookmark.panel;
            self.choice = bookmark.choice;
            self.list = bookmark.list;
            self.groups = bookmark.groups;
            self.raw_count = bookmark.count;
            self.options = bookmark.options;
            self.targets = bookmark.targets;
            self.load(false)?;
        } else {
            self.panel = Panel::Main;
            self.pos.body = false;
        }
        Ok(())
    }

    fn links(&mut self) {
        let Some(entry) = self.pos.entry.clone() else {
            return;
        };
        let root = self.root.clone();
        let (line, removed) = self
            .preview
            .rows
            .get(self.pos.line.row)
            .map_or((0, false), |row| {
                (row.num.saturating_sub(1), row.mark == '-')
            });
        let git = self
            .pos
            .revision
            .clone()
            .filter(|_| !self.pos.diff || removed);
        let oldest = self.groups.get(&entry.id).map(|&(id, _)| id);
        self.search = None;
        self.search_after = None;
        self.options.clear();
        self.targets.clear();
        self.reference_error = None;
        self.references = Some(QueryTask::start(move |db, flag| {
            let (previous, current, _) = store::bodies(db, entry.id)?;
            let body = if let Some(rev) = git {
                Some(crate::git::body(&root, &entry.path, &rev)?)
            } else if removed && let Some(id) = oldest {
                store::bodies(db, id)?.0
            } else if removed {
                previous
            } else {
                current.or(previous)
            };
            refs::links(
                db,
                &root,
                &entry.path,
                body.as_deref().unwrap_or(""),
                line,
                || flag.load(Ordering::Relaxed),
            )
        }));
    }

    fn references_tick(&mut self) {
        let Some(result) =
            self.references
                .as_ref()
                .and_then(|task| match task.results.try_recv() {
                    Ok(result) => Some(result),
                    Err(mpsc::TryRecvError::Empty) => None,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        Some(Err(anyhow::anyhow!("查询已中断")))
                    }
                })
        else {
            return;
        };
        self.references = None;
        match result {
            Ok(targets) => {
                self.options = targets
                    .iter()
                    .map(|target| target.label.to_string())
                    .collect();
                self.targets = targets;
            }
            Err(error) => self.reference_error = Some(format!("引用查询失败 · {error}")),
        }
        self.dirty = true;
    }

    fn switch(&mut self, r: String) -> Result<()> {
        self.positions.insert(self.root.clone(), self.pos.clone());
        self.root = r;
        self.views = ViewSwitch::default();
        self.db.execute(
            "INSERT OR REPLACE INTO projects VALUES(?,?)",
            params![self.root, chrono::Local::now().timestamp_millis()],
        )?;
        self.worker = watch::start(self.root.clone());
        self.git_monitor = StatusMonitor::start(self.root.clone());
        self.git_status = None;
        self.status = "建立基线".into();
        self.pos = self.positions.get(&self.root).cloned().unwrap_or_default();
        self.panel = Panel::Main;
        self.stack.clear();
        self.follow = self.pos.entry.is_none();
        self.pending = false;
        self.reload(true)?;
        self.load(false)
    }
    fn queue_search(&mut self) {
        self.search = None;
        self.search_after = Some(std::time::Instant::now() + Duration::from_millis(100));
        self.pos.offset = 0;
    }
    fn search_tick(&mut self) -> Result<()> {
        if self
            .search_after
            .is_some_and(|when| std::time::Instant::now() >= when)
        {
            self.search_after = None;
            let root = self.root.clone();
            let pos = self.pos.clone();
            self.search = Some(QueryTask::start(move |db, _| {
                store::entries(
                    db, &root, pos.recent, &pos.only, &pos.query, pos.mode, pos.offset,
                )
            }));
        }
        if let Some(result) = self.search.as_ref().and_then(|s| s.results.try_recv().ok()) {
            self.search = None;
            self.list = result?;
            self.views.active_epoch = Some(self.views.epoch);
            self.group_page();
            self.pos.entry = self.list.first().cloned();
            self.load(true)?;
            self.dirty = true;
        }
        Ok(())
    }
    fn move_entry(&mut self, forward: bool) -> Result<bool> {
        if self.list.is_empty() {
            return Ok(false);
        }
        let current = self.pos.entry.as_ref().map(|e| e.id);
        let index = self
            .list
            .iter()
            .position(|e| Some(e.id) == current)
            .unwrap_or(0);
        let old_offset = self.pos.offset;
        let next = if forward && index + 1 == self.list.len() && self.raw_count == 256 {
            self.pos.offset += 255;
            self.reload(true)?;
            usize::from(self.list.len() > 1)
        } else if !forward && index == 0 && self.pos.offset > 0 {
            self.pos.offset = self.pos.offset.saturating_sub(255);
            self.reload(true)?;
            self.list.len().saturating_sub(2)
        } else if forward {
            (index + 1).min(self.list.len() - 1)
        } else {
            index.saturating_sub(1)
        };
        let entry = self.list.get(next);
        if old_offset == self.pos.offset && entry.map(|e| e.id) == current {
            return Ok(false);
        }
        self.pos.entry = entry.cloned();
        self.pos.revision = None;
        self.load(true)?;
        Ok(true)
    }

    fn move_choice(&mut self, forward: bool) -> bool {
        let next = if forward {
            (self.choice + 1).min(self.options.len().saturating_sub(1))
        } else {
            self.choice.saturating_sub(1)
        };
        let moved = next != self.choice;
        self.choice = next;
        moved
    }

    fn mouse(&mut self, event: MouseEvent, area: Rect) -> Result<bool> {
        let forward = match event.kind {
            MouseEventKind::ScrollDown => true,
            MouseEventKind::ScrollUp => false,
            _ => return Ok(false),
        };
        let pointer = ratatui::layout::Position::new(event.column, event.row);
        let [_, content, _] = screen_bands(area, self.path_lines(area).len());
        let moved = match self.panel {
            Panel::Main | Panel::Search => {
                let [events, body] = reader_panes(content, self.pos.body);
                if body.contains(pointer) {
                    let before = (self.pos.line, self.pos.top);
                    self.scroll(forward, 3, body);
                    before != (self.pos.line, self.pos.top)
                } else if events.contains(pointer) {
                    self.move_entry(forward)?
                } else {
                    false
                }
            }
            Panel::Add => false,
            _ => {
                let list = if matches!(self.panel, Panel::Links) && !self.targets.is_empty() {
                    link_panes(content)[0]
                } else {
                    content
                };
                list.contains(pointer) && self.move_choice(forward)
            }
        };
        if moved {
            self.views.wanted = false;
            self.notice = None;
            self.follow = false;
        }
        Ok(moved)
    }

    fn key(&mut self, k: K, size: Size) -> Result<bool> {
        if k != K::Char('t') {
            self.views.wanted = false;
        }
        self.notice = None;
        let area = Rect::new(0, 0, size.width, size.height);
        let [_, content, _] = screen_bands(area, self.path_lines(area).len());
        let reader = reader_panes(content, true)[1];
        let page = reader.height.saturating_sub(2).max(1) as usize;
        let narrow = size.width < 80;
        self.follow = false;
        if matches!(self.panel, Panel::Add) {
            match k {
                K::Esc => self.back()?,
                K::Char(c) => self.input.push(c),
                K::Backspace => {
                    self.input.pop();
                }
                K::Enter => match crate::root(&self.input) {
                    Ok(r) => self.switch(r)?,
                    Err(e) => self.status = e.to_string(),
                },
                _ => {}
            }
            return Ok(false);
        }
        if matches!(self.panel, Panel::Search) {
            match k {
                K::Esc => {
                    self.search = None;
                    self.search_after = None;
                    self.back()?
                }
                K::Enter => self.panel = Panel::Main,
                K::Char(c) => {
                    self.pos.query.push(c);
                    self.queue_search()
                }
                K::Backspace => {
                    self.pos.query.pop();
                    self.queue_search()
                }
                K::Tab => {
                    self.pos.mode = (self.pos.mode + 1) % 3;
                    self.pos.diff = self.pos.mode == 2;
                    self.queue_search()
                }
                _ => {}
            }
            return Ok(false);
        }
        if !matches!(self.panel, Panel::Main) {
            match k {
                K::Esc | K::Char('q') => self.back()?,
                K::Up | K::Down => {
                    self.move_choice(k == K::Down);
                }
                K::Enter if self.choice < self.options.len() => match self.panel {
                    Panel::Projects => {
                        if self.choice == 0 {
                            self.input.clear();
                            self.panel = Panel::Add;
                        } else {
                            self.switch(self.options[self.choice].clone())?;
                        }
                    }
                    Panel::Git => {
                        let rev = self.options[self.choice]
                            .split_whitespace()
                            .next()
                            .unwrap()
                            .to_string();
                        let e = self.pos.entry.as_ref().unwrap();
                        match crate::git::body(&self.root, &e.path, &rev) {
                            Ok(_) => {
                                self.push();
                                self.pos.revision = Some(rev);
                                self.pos.body = true;
                                self.pos.diff = false;
                                self.load(true)?
                            }
                            Err(e) => self.status = e.to_string(),
                        }
                    }
                    Panel::Links => {
                        let e = self.targets[self.choice].event.clone();
                        let n = self.targets[self.choice].line;
                        self.push();
                        self.pos.entry = Some(e);
                        self.pos.revision = None;
                        self.pos.body = true;
                        self.pos.diff = false;
                        self.load(false)?;
                        let row = self
                            .preview
                            .rows
                            .iter()
                            .position(|r| r.num == n + 1)
                            .unwrap_or(0);
                        self.pos.anchor_row(row);
                    }
                    Panel::Ignore => {
                        store::update_rule(
                            &mut self.db,
                            &self.root,
                            &self.options[self.choice],
                            true,
                        )?;
                        self.back()?;
                        self.views.epoch = self.views.epoch.wrapping_add(1);
                        self.reload(false)?
                    }
                    _ => {}
                },
                _ => {}
            }
            return Ok(false);
        }
        match k {
            K::Char('q') => return Ok(true),
            K::Esc => self.back()?,
            K::Char('?') => {
                self.push();
                self.panel = Panel::Help;
                self.options = vec![
                    "↑↓  选择事件或正文行".into(),
                    "[ / ]  上一处 / 下一处变化".into(),
                    "PgUp / PgDn  正文翻页".into(),
                    "滚轮  滚动鼠标所在的列表或正文".into(),
                    "z  折叠连续保存     f  当前目录     H  回到起点".into(),
                    "d  切换对比     h  文件历史".into(),
                    "r  查引用与定义 · 正文里先用 ↑↓ 选一行".into(),
                    "i  忽略规则".into(),
                    "t  时间轴 / 最近文件     g  最新".into(),
                    "G  Git 历史     /  搜索     p  项目     q  退出".into(),
                    "Git 两格状态  暂存区 / 工作区，?? 未跟踪".into(),
                ];
                self.choice = 0
            }
            K::Char('/') => {
                self.push();
                self.panel = Panel::Search;
                self.pos.query.clear();
                self.pos.offset = 0;
                self.pos.body = false;
                self.reload(false)?
            }
            K::Char('p') => {
                self.push();
                self.panel = Panel::Projects;
                self.choice = 0;
                self.options =
                    store::strings(&self.db, "SELECT root FROM projects ORDER BY used DESC", [])?;
                self.options.insert(0, "＋ 添加目录".into());
            }
            K::Char('G') if self.pos.entry.is_some() => {
                let e = self.pos.entry.as_ref().unwrap();
                match crate::git::history(&self.root, &e.path) {
                    Ok(v) => {
                        self.push();
                        self.panel = Panel::Git;
                        self.options = v;
                        self.choice = 0;
                    }
                    Err(_) => self.status = "当前路径没有可用的 Git 历史".into(),
                }
            }
            K::Char('r') if self.pos.entry.is_some() => {
                self.push();
                self.panel = Panel::Links;
                self.choice = 0;
                self.links()
            }
            K::Char('i') if self.pos.entry.is_some() => {
                let p = self.pos.entry.as_ref().unwrap().path.clone();
                self.push();
                self.panel = Panel::Ignore;
                self.choice = 0;
                self.options = ignore_options(&p);
            }
            K::Char('h') if self.pos.entry.is_some() => {
                self.push();
                self.pos.only = self.pos.entry.as_ref().unwrap().path.clone();
                self.pos.recent = false;
                self.pos.body = false;
                self.pos.offset = 0;
                self.reload(false)?
            }
            K::Char('z') => {
                self.pos.folded = !self.pos.folded;
                self.reload(false)?
            }
            K::Char('f') if self.pos.entry.is_some() => {
                let path = &self.pos.entry.as_ref().unwrap().path;
                if let Some((dir, _)) = path.rsplit_once('/') {
                    let dir = format!("{dir}/");
                    self.push();
                    self.pos.only = dir;
                    self.pos.offset = 0;
                    self.pos.body = false;
                    self.reload(false)?;
                }
            }
            K::Char('H') => {
                while !self.stack.is_empty() {
                    self.back()?;
                }
            }
            K::Char('t') => self.toggle_view(),
            K::Char('g') => {
                self.git_monitor.refresh();
                self.views.epoch = self.views.epoch.wrapping_add(1);
                self.pos.offset = 0;
                self.pending = false;
                self.follow = true;
                self.reload(false)?
            }
            K::Char('d') => {
                if narrow
                    && !self.pos.body
                    && self.pos.entry.is_some()
                    && self.preview.comparison_note.is_none()
                {
                    self.push();
                    self.pos.body = true;
                }
                self.toggle_diff()?;
            }
            K::Char('[') | K::Char(']') => {
                if narrow && !self.pos.body && !self.preview.changes.is_empty() {
                    self.push();
                    self.pos.body = true;
                }
                self.jump_change(k == K::Char(']'));
            }
            K::Enter if !self.pos.body => {
                self.push();
                self.pos.body = true;
            }
            K::PageUp | K::PageDown if self.pos.entry.is_some() => {
                if !self.pos.body {
                    self.push();
                    self.pos.body = true;
                }
                self.scroll(k == K::PageDown, page.saturating_sub(1).max(1), reader);
            }
            K::Up | K::Down if self.pos.body => self.scroll(k == K::Down, 1, reader),
            K::Up | K::Down => {
                self.move_entry(k == K::Down)?;
            }
            _ => {}
        }
        Ok(false)
    }
    fn path_lines(&self, area: Rect) -> Vec<String> {
        if !matches!(self.panel, Panel::Main | Panel::Search | Panel::Links) {
            return Vec::new();
        }
        self.pos.entry.as_ref().map_or_else(Vec::new, |entry| {
            wrap_path(
                &terminal_text(&entry.path),
                area.width.saturating_sub(2) as usize,
                area.height.saturating_sub(6).max(1) as usize,
            )
        })
    }

    fn draw(&mut self, f: &mut Frame) {
        let path = self.path_lines(f.area());
        let bands = screen_bands(f.area(), path.len());
        let name = std::path::Path::new(&self.root)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        let mut heading = vec![Line::from(vec![
            Span::styled("TraceTracking  ", Style::new().fg(ACC).bold()),
            Span::raw(name),
            Span::styled(
                format!(
                    "   ·   {}{}",
                    self.status,
                    if self.pending {
                        "   ↑ 有新记录  g"
                    } else {
                        ""
                    }
                ),
                Style::new().fg(DIM),
            ),
        ])];
        if self.views.wanted {
            let target = if self.pos.recent {
                "时间轴"
            } else {
                "最近文件"
            };
            heading[0].push_span(Span::styled(
                format!("   → {target}…"),
                Style::new().fg(ACC),
            ));
        }
        heading.extend(path.into_iter().map(Line::raw));
        f.render_widget(Paragraph::new(heading), bands[0]);
        let main = matches!(self.panel, Panel::Main | Panel::Search);
        if main {
            let [events, body] = reader_panes(bands[1], self.pos.body);
            if !events.is_empty() {
                let a = events;
                let h = a.height.saturating_sub(1) as usize;
                let index = self
                    .pos
                    .entry
                    .as_ref()
                    .and_then(|e| self.list.iter().position(|x| x.id == e.id))
                    .unwrap_or(0);
                let start = index
                    .saturating_sub(h / 2)
                    .min(self.list.len().saturating_sub(h));
                let today = chrono::Local::now().date_naive();
                let lines = self
                    .list
                    .iter()
                    .enumerate()
                    .skip(start)
                    .take(h)
                    .map(|(i, e)| {
                        let local = chrono::DateTime::from_timestamp(e.stamp, 0)
                            .map(|t| t.with_timezone(&chrono::Local));
                        let time = local
                            .map(|t| {
                                t.format(if t.date_naive() == today {
                                    "%H:%M"
                                } else {
                                    "%m-%d"
                                })
                                .to_string()
                            })
                            .unwrap_or_default();
                        let count = self
                            .groups
                            .get(&e.id)
                            .map(|(_, n)| format!(" ×{n}"))
                            .unwrap_or_default();
                        let mark = if i == index { "▸" } else { "│" };
                        let trace = if e.kind == "B" { " " } else { e.kind.as_str() };
                        let prefix = format!("{mark} {time} {trace} ");
                        let git = self
                            .git_status
                            .as_ref()
                            .and_then(|statuses| statuses.get(&e.path))
                            .copied();
                        let status_width = if git.is_some() { 3 } else { 0 };
                        let width = (a.width as usize)
                            .saturating_sub(prefix.width() + count.width() + status_width);
                        let display = compact_path(&terminal_text(&e.path), width);
                        let mut spans = vec![Span::raw(prefix)];
                        let mut path_style = Style::default();
                        if let Some(status) = git {
                            let [staged, worktree] = status.0;
                            let conflict = status.conflicted();
                            for (code, in_index) in [(staged, true), (worktree, false)] {
                                let glyph = if code == b' ' { '·' } else { char::from(code) };
                                spans.push(Span::styled(
                                    glyph.to_string(),
                                    git_style(code, in_index, conflict),
                                ));
                            }
                            spans.push(Span::raw(" "));
                            path_style = if worktree == b' ' {
                                git_style(staged, true, conflict)
                            } else {
                                git_style(worktree, false, conflict)
                            };
                        }
                        spans.push(Span::styled(display, path_style));
                        spans.push(Span::raw(count));
                        let selected = if i == index {
                            Style::new().fg(ACC).bold()
                        } else {
                            Style::default()
                        };
                        Line::from(spans).style(selected)
                    })
                    .collect::<Vec<_>>();
                let mut title = if !self.pos.query.is_empty() {
                    format!("搜索 · {}", self.pos.query)
                } else if !self.pos.only.is_empty() {
                    format!("历史 · {}", self.pos.only)
                } else if self.pos.recent {
                    "最近文件".into()
                } else {
                    "时间轴".into()
                };
                if self.git_status.is_some() {
                    title.push_str(" · Git");
                }
                let title_width = title.width() as u16;
                f.render_widget(
                    Paragraph::new(lines).block(
                        Block::default()
                            .title(title)
                            .borders(Borders::TOP)
                            .border_style(Style::new().fg(DIM)),
                    ),
                    a,
                );
                draw_transition(f, a, title_width, self.views.transition);
            }
            if !body.is_empty() {
                let a = body;
                let h = a.height.saturating_sub(2) as usize;
                self.preview.prepare_wrap(a.width);
                let total = self.preview.wrap.len(&self.preview.rows);
                let current = self.preview.wrap.index(self.pos.line);
                let start = visible_top(self.preview.wrap.index(self.pos.top), current, total, h);
                let previous = self
                    .preview
                    .changes
                    .partition_point(|&row| row <= self.pos.line.row);
                let mut summary = if self.pos.diff {
                    let base = if self.pos.revision.is_some() {
                        "Git"
                    } else {
                        match self.pos.entry.as_ref().map(|e| e.kind.as_str()) {
                            Some("A") => "空文件",
                            _ => "上一版",
                        }
                    };
                    format!(" 对比 · {base} → 本次")
                } else {
                    " 正文".to_string()
                };
                if !self.preview.changes.is_empty() {
                    summary.push_str(&format!(
                        "  [{} / {}] 变化",
                        previous,
                        self.preview.changes.len()
                    ));
                } else if let Some(note) = self.preview.comparison_note {
                    summary.push_str(&format!("  ·  {note}"));
                }
                if let Some(local) = self
                    .pos
                    .entry
                    .as_ref()
                    .and_then(|e| chrono::DateTime::from_timestamp(e.stamp, 0))
                {
                    summary.push_str(&format!(
                        "  ·  {}",
                        local.with_timezone(&chrono::Local).format("%m-%d %H:%M:%S")
                    ));
                }
                if !self.preview.source.is_empty() {
                    summary.push_str(&format!("  ·  {}", self.preview.source));
                }
                let mut lines = vec![Line::styled(summary, Style::new().fg(ACC))];
                for screen_row in start..start.saturating_add(h).min(total) {
                    let point = self.preview.wrap.point(screen_row);
                    let row = &self.preview.rows[point.row];
                    let first = point.byte == 0;
                    let selected = screen_row == current;
                    let color = match row.mark {
                        '~' => Color::Yellow,
                        '+' => Color::Green,
                        '-' => Color::Red,
                        _ => DIM,
                    };
                    let gutter = if first {
                        let anchor =
                            selected && (self.pos.body || !self.preview.changes.is_empty());
                        let number = if row.num == 0 {
                            String::new()
                        } else {
                            row.num.to_string()
                        };
                        let width = self.preview.line_digits;
                        format!(
                            "{} {} {number:>width$}  ",
                            if anchor { "›" } else { " " },
                            row.mark
                        )
                    } else {
                        " ".repeat(self.preview.line_digits + 5)
                    };
                    let syntax = if self.pos.diff && row.mark == '-' {
                        &self.preview.old_syntax
                    } else {
                        &self.preview.syntax
                    };
                    let bytes = self.preview.wrap.range(screen_row, point, row.text.len());
                    let mut spans = vec![Span::styled(gutter, Style::new().fg(color))];
                    spans.extend(syntax.spans_in(row.num, &row.text, bytes));
                    let mut line = Line::from(spans);
                    let mut style = Style::new();
                    if let Some(bg) = diff_background(self.pos.diff, row.mark) {
                        style = style.bg(bg);
                        let padding = (a.width as usize).saturating_sub(line.width());
                        line.push_span(Span::raw(" ".repeat(padding)));
                    }
                    if self.pos.body && selected {
                        style = style.add_modifier(Modifier::BOLD);
                    }
                    lines.push(line.style(style));
                }
                f.render_widget(
                    Paragraph::new(lines).block(
                        Block::default()
                            .title("文件")
                            .borders(Borders::TOP)
                            .border_style(Style::new().fg(DIM)),
                    ),
                    a,
                );
                if events.is_empty() {
                    draw_transition(f, a, "文件".width() as u16, self.views.transition);
                }
            }
        } else {
            let title = match self.panel {
                Panel::Git => "Git 提交 · d 与所选快照对比",
                Panel::Add => "添加项目目录",
                Panel::Help => "快捷键",
                Panel::Projects => "项目",
                Panel::Ignore => "选择忽略规则",
                Panel::Links => "引用与定义 · 当前索引",
                _ => "",
            };
            let target = if matches!(self.panel, Panel::Links) {
                self.targets.get(self.choice)
            } else {
                None
            };
            let [list_area, preview_area] = if target.is_some() {
                link_panes(bands[1])
            } else {
                [bands[1], Rect::default()]
            };
            let mut lines = Vec::new();
            for (i, s) in self
                .options
                .iter()
                .enumerate()
                .skip(
                    self.choice
                        .saturating_sub(list_area.height.saturating_sub(1) as usize / 2),
                )
                .take(list_area.height.saturating_sub(1) as usize)
            {
                let text = if matches!(self.panel, Panel::Links)
                    && let Some(candidate) = self.targets.get(i)
                {
                    let path = terminal_text(&candidate.event.path);
                    let suffix = format!(":{}", candidate.line + 1);
                    let width =
                        (list_area.width as usize).saturating_sub(s.width() + suffix.width() + 4);
                    let display = if path.width() <= width {
                        path
                    } else {
                        compact_path(&path, width)
                    };
                    format!("{s}  {display}{suffix}")
                } else {
                    s.clone()
                };
                lines.push(Line::styled(
                    format!("{} {text}", if i == self.choice { "›" } else { " " }),
                    if i == self.choice {
                        Style::new().fg(ACC)
                    } else {
                        Style::default()
                    },
                ));
            }
            if matches!(self.panel, Panel::Add) {
                lines = vec![Line::from(format!("{}▏", self.input))];
            }
            if lines.is_empty() {
                let message = if matches!(self.panel, Panel::Links) {
                    if self.references.is_some() {
                        "正在查找引用…"
                    } else {
                        self.reference_error
                            .as_deref()
                            .unwrap_or("没有找到引用或定义候选")
                    }
                } else {
                    "暂无匹配"
                };
                lines.push(Line::styled(message, Style::new().fg(DIM)));
            }
            if let Some(target) = target
                && let Ok((_, Some(body), _)) = store::bodies(&self.db, target.event.id)
            {
                let start = target.line.saturating_sub(2);
                let lines = body
                    .lines()
                    .enumerate()
                    .skip(start)
                    .take(preview_area.height.saturating_sub(1) as usize)
                    .map(|(i, s)| {
                        Line::styled(
                            format!("{:>4}  {}", i + 1, s),
                            if i == target.line {
                                Style::new().fg(ACC)
                            } else {
                                Style::default()
                            },
                        )
                    })
                    .collect::<Vec<_>>();
                let path = terminal_text(&target.event.path);
                let width = (preview_area.width as usize).saturating_sub("当前版本 · ".width());
                let caption = if path.width() <= width {
                    path
                } else {
                    compact_path(&path, width)
                };
                f.render_widget(
                    Paragraph::new(lines).block(
                        Block::default()
                            .title(format!("当前版本 · {caption}"))
                            .borders(Borders::TOP),
                    ),
                    preview_area,
                );
            }
            f.render_widget(
                Paragraph::new(lines).block(Block::default().title(title).borders(Borders::TOP)),
                list_area,
            );
        }
        let hint = if matches!(self.panel, Panel::Search) {
            format!(
                "/ {}  {}▏   Tab 范围 · Enter 确定",
                ["路径", "正文", "变更"][self.pos.mode],
                self.pos.query
            )
        } else if matches!(self.panel, Panel::Links) {
            if self.references.is_some() {
                "Esc 取消查找".into()
            } else {
                "↑↓ 预览   Enter 跳转   Esc 返回".into()
            }
        } else if !main {
            "↑↓ 选择   Enter 确定   Esc 返回".into()
        } else if let Some(notice) = self.notice {
            notice.into()
        } else if self.pos.body {
            format!(
                "↑↓  PgUp/PgDn 翻页  [ / ] 变化  d {}  r 引用  Esc 返回",
                if self.pos.diff { "正文" } else { "对比" }
            )
        } else {
            "↑↓ 事件  Enter 阅读  r 引用  [ / ] 变化  d 对比  / 搜索".into()
        };
        f.render_widget(Paragraph::new(hint).style(Style::new().fg(ACC)), bands[2]);
    }
}
fn draw_transition(f: &mut Frame, pane: Rect, title_width: u16, started: Option<Instant>) {
    let Some(started) = started else { return };
    let elapsed = started.elapsed();
    if elapsed >= TRANSITION {
        return;
    }
    let available = pane.width.saturating_sub(title_width);
    let width = (available as u128 * (elapsed.as_millis() + 24) / TRANSITION.as_millis())
        .min(available as u128) as u16;
    if width > 0 {
        let border = Rect::new(pane.x + title_width, pane.y, width, 1);
        f.render_widget(
            Paragraph::new("─".repeat(width as usize)).style(Style::new().fg(ACC)),
            border,
        );
    }
}

fn screen_bands(area: Rect, path_rows: usize) -> [Rect; 3] {
    Layout::vertical([
        Constraint::Length(1 + path_rows.max(1) as u16),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(area.inner(Margin {
        horizontal: 1,
        vertical: 0,
    }))
}

fn reader_panes(content: Rect, reading: bool) -> [Rect; 2] {
    if content.width >= 78 {
        let sidebar = (content.width / 4).clamp(24, 34);
        Layout::horizontal([Constraint::Length(sidebar), Constraint::Min(1)]).areas(content)
    } else if reading {
        [Rect::default(), content]
    } else {
        [content, Rect::default()]
    }
}

fn compact_path(path: &str, width: usize) -> String {
    let Some((first, rest)) = path.split_once('/') else {
        return ellipsis_tail(path, width);
    };
    let name = rest.rsplit('/').next().unwrap_or(rest);
    if !rest.contains('/') && path.width() <= width {
        return path.into();
    }
    if width < 9 {
        return ellipsis_tail(name, width);
    }
    let separator = "/…/";
    let available = width - separator.width();
    let head = first
        .width()
        .min(available / 2)
        .min(available.saturating_sub(name.width()).max(available / 4));
    let first = take_prefix(first, head);
    format!(
        "{first}{separator}{}",
        ellipsis_tail(name, available - first.width())
    )
}

fn take_prefix(text: &str, width: usize) -> &str {
    let mut used = 0;
    for (i, ch) in text.char_indices() {
        used += ch.width().unwrap_or(0);
        if used > width {
            return &text[..i];
        }
    }
    text
}

fn ellipsis_tail(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if text.width() <= width {
        return text.into();
    }
    let mut used = 1;
    for (i, ch) in text.char_indices().rev() {
        used += ch.width().unwrap_or(0);
        if used > width {
            return format!("…{}", &text[i + ch.len_utf8()..]);
        }
    }
    text.into()
}

fn wrap_path(mut path: &str, width: usize, max_lines: usize) -> Vec<String> {
    let mut lines = Vec::new();
    while !path.is_empty() && lines.len() < max_lines {
        let head = take_prefix(path, width);
        if head.is_empty() {
            break;
        }
        if lines.len() + 1 == max_lines {
            lines.push(ellipsis_tail(path, width));
            break;
        }
        let head = if head.len() < path.len()
            && let Some(end) = head.rfind('/')
        {
            &head[..=end]
        } else {
            head
        };
        lines.push(head.to_string());
        path = &path[head.len()..];
    }
    lines
}

fn link_panes(content: Rect) -> [Rect; 2] {
    Layout::vertical([Constraint::Percentage(57), Constraint::Percentage(43)]).areas(content)
}

fn git_style(code: u8, staged: bool, conflict: bool) -> Style {
    let color = if conflict {
        Color::LightRed
    } else {
        match code {
            b' ' => DIM,
            b'?' => Color::LightCyan,
            b'D' => Color::LightRed,
            b'R' | b'C' | b'T' => Color::LightMagenta,
            b'A' => Color::LightGreen,
            b'M' if staged => Color::LightGreen,
            _ => Color::LightYellow,
        }
    };
    Style::new().fg(color)
}

fn diff_background(diff: bool, mark: char) -> Option<Color> {
    match (diff, mark) {
        (true, '+') => Some(Color::Rgb(22, 53, 36)),
        (true, '-') => Some(Color::Rgb(62, 28, 32)),
        _ => None,
    }
}

fn visible_top(top: usize, line: usize, rows: usize, height: usize) -> usize {
    let height = height.max(1);
    top.clamp(line.saturating_add(1).saturating_sub(height), line)
        .min(rows.saturating_sub(height))
}

fn ignore_options(path: &str) -> Vec<String> {
    fn escape_glob(path: &str) -> String {
        let mut escaped = String::with_capacity(path.len());
        for ch in path.chars() {
            if matches!(ch, '\\' | '*' | '?' | '[' | ']' | '{' | '}' | ' ') {
                escaped.push('\\');
            }
            escaped.push(ch);
        }
        escaped
    }
    let mut options = vec![format!("/{}", escape_glob(path))];
    let name = path.rsplit('/').next().unwrap_or(path);
    if let Some((stem, ext)) = name.rsplit_once('.')
        && !stem.is_empty()
        && !ext.is_empty()
    {
        options.push(format!("*.{}", escape_glob(ext)));
    }
    for (end, _) in path.rmatch_indices('/') {
        options.push(format!("/{}/", escape_glob(&path[..end])));
    }
    options
}
struct Restore;
impl Drop for Restore {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableMouseCapture);
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
    }
}
pub fn run(db: Connection, root: String) -> Result<()> {
    enable_raw_mode()?;
    let _restore = Restore;
    execute!(io::stdout(), EnterAlternateScreen)?;
    #[cfg(windows)]
    execute!(io::stdout(), crossterm::event::EnableMouseCapture)?;
    // Track buttons and wheel coordinates on ANSI terminals without requesting pointer motion.
    #[cfg(not(windows))]
    execute!(
        io::stdout(),
        crossterm::style::Print("\x1b[?1000h\x1b[?1006h")
    )?;
    let mut term: Terminal<CrosstermBackend<Stdout>> =
        Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let worker = watch::start(root.clone());
    let git_monitor = StatusMonitor::start(root.clone());
    let mut a = App {
        db,
        root,
        worker,
        git_monitor,
        git_status: None,
        status: "建立基线".into(),
        pos: Position::default(),
        list: vec![],
        preview: Preview::default(),
        notice: None,
        views: ViewSwitch {
            refresh_due: true,
            ..Default::default()
        },
        stack: vec![],
        panel: Panel::Main,
        options: vec![],
        choice: 0,
        targets: vec![],
        groups: Default::default(),
        raw_count: 0,
        positions: Default::default(),
        search: None,
        references: None,
        reference_error: None,
        search_after: None,
        input: String::new(),
        follow: true,
        pending: false,
        dirty: true,
    };
    let mut size = term.size()?;
    loop {
        a.search_tick()?;
        a.references_tick();
        while let Ok(statuses) = a.git_monitor.updates.try_recv() {
            if a.git_status.as_ref() != Some(&statuses) {
                a.git_status = Some(statuses);
                a.dirty = true;
            }
        }
        while let Ok(s) = a.worker.updates.try_recv() {
            if s.starts_with("监听中") {
                a.git_monitor.refresh();
            }
            a.status = s;
            a.views.epoch = a.views.epoch.wrapping_add(1);
            a.views.failed = None;
            if a.follow || a.pos.entry.is_none() {
                a.views.refresh_due = true;
            } else {
                a.pending = true;
            }
            a.dirty = true;
        }
        a.view_tick(size);
        if a.dirty {
            term.draw(|f| a.draw(f))?;
            a.dirty = false;
        }
        let poll_ms = if a.views.transition.is_some() || a.views.wanted {
            24
        } else if a.references.is_some() || a.views.refresh.is_some() {
            35
        } else {
            250
        };
        if event::poll(Duration::from_millis(poll_ms))? {
            match event::read()? {
                Event::Key(k) if k.kind != KeyEventKind::Release => {
                    if (k.code == K::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL))
                        || a.key(k.code, size)?
                    {
                        break;
                    }
                    a.dirty = true
                }
                Event::Mouse(event)
                    if matches!(
                        event.kind,
                        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                    ) =>
                {
                    a.dirty |= a.mouse(event, Rect::new(0, 0, size.width, size.height))?;
                }
                Event::Resize(width, height) => {
                    size = Size::new(width, height);
                    a.dirty = true;
                }
                _ => {}
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignore_menu_lists_every_parent_and_preserves_literal_path() -> Result<()> {
        let rules = ignore_options("apps/SDK [Beta]/src/internal/config.java");
        assert_eq!(
            rules,
            [
                "/apps/SDK\\ \\[Beta\\]/src/internal/config.java",
                "*.java",
                "/apps/SDK\\ \\[Beta\\]/src/internal/",
                "/apps/SDK\\ \\[Beta\\]/src/",
                "/apps/SDK\\ \\[Beta\\]/",
                "/apps/",
            ]
        );
        let mut builder = ignore::gitignore::GitignoreBuilder::new(".");
        builder.add_line(None, &rules[4])?;
        let matcher = builder.build()?;
        assert!(
            matcher
                .matched_path_or_any_parents("apps/SDK [Beta]/sibling.txt", false)
                .is_ignore()
        );
        assert!(
            !matcher
                .matched_path_or_any_parents("apps/SDK B/sibling.txt", false)
                .is_ignore()
        );
        Ok(())
    }

    #[test]
    fn path_labels_preserve_the_filename_and_wrap_at_slashes() {
        assert_eq!(
            compact_path("服务/config/production/app.yml", 20),
            "服务/…/app.yml"
        );
        assert_eq!(compact_path("config/app.yml", 20), "config/app.yml");
        let path = "服务/config/production/app.yml";
        let heading = wrap_path(path, 12, 4);
        assert_eq!(heading.concat(), path);
        assert_eq!(heading.last().map(String::as_str), Some("app.yml"));
        assert!(heading.iter().all(|line| line.width() <= 12));
    }

    #[test]
    fn viewport_keeps_cursor_visible_after_paging_or_resize() {
        assert_eq!(visible_top(23, 26, 100, 24), 23);
        assert_eq!(visible_top(23, 50, 100, 24), 27);
        assert_eq!(visible_top(23, 2, 100, 24), 2);
        assert_eq!(visible_top(97, 99, 100, 24), 76);
        assert_eq!(visible_top(5, 3, 4, 24), 0);
    }
}
