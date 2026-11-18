use crate::{
    refs,
    store::{self, Entry},
    syntax::Syntax,
    view::{self, Row},
    watch,
};
use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode as K, KeyEventKind, KeyModifiers},
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
    time::Duration,
};
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
    line: usize,
    top: usize,
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
struct SearchTask {
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    results: std::sync::mpsc::Receiver<Result<Vec<Entry>>>,
}
impl Drop for SearchTask {
    fn drop(&mut self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}
struct Bookmark {
    position: Position,
    panel: Panel,
    choice: usize,
    list: Vec<Entry>,
    groups: std::collections::HashMap<i64, (i64, usize)>,
    count: usize,
    options: Vec<String>,
    targets: Vec<refs::Target>,
}
struct App {
    db: Connection,
    root: String,
    worker: watch::Worker,
    status: String,
    pos: Position,
    list: Vec<Entry>,
    rows: Vec<Row>,
    syntax: Syntax,
    old_syntax: Syntax,
    changes: Vec<usize>,
    comparison_note: Option<&'static str>,
    notice: Option<&'static str>,
    source: String,
    stack: Vec<Bookmark>,
    panel: Panel,
    options: Vec<String>,
    choice: usize,
    targets: Vec<refs::Target>,
    groups: std::collections::HashMap<i64, (i64, usize)>,
    raw_count: usize,
    positions: std::collections::HashMap<String, Position>,
    search: Option<SearchTask>,
    search_after: Option<std::time::Instant>,
    input: String,
    follow: bool,
    pending: bool,
    dirty: bool,
}
impl App {
    fn reload(&mut self, keep: bool) -> Result<()> {
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
        self.groups.clear();
        if !self.pos.folded || self.pos.recent || self.pos.mode != 0 || !self.pos.query.is_empty() {
            return;
        }
        let mut compact: Vec<Entry> = Vec::new();
        let mut previous = 0;
        for e in std::mem::take(&mut self.list) {
            if let Some(last) = compact.last()
                && last.kind == "M"
                && e.kind == "M"
                && last.path == e.path
                && previous - e.stamp <= 60
            {
                let group = self.groups.entry(last.id).or_insert((last.id, 1));
                group.0 = e.id;
                group.1 += 1;
            } else {
                compact.push(e.clone());
            }
            previous = e.stamp;
        }
        self.list = compact;
    }
    fn load(&mut self, locate: bool) -> Result<()> {
        self.rows.clear();
        self.changes.clear();
        self.source.clear();
        self.comparison_note = None;
        self.syntax = Syntax::default();
        self.old_syntax = Syntax::default();
        if locate {
            self.pos.line = 0;
            self.pos.top = 0;
        }
        if let Some(e) = &self.pos.entry {
            let (mut a, b, source) = store::bodies(&self.db, e.id)?;
            self.source = source;
            if let Some((oldest, count)) = self.groups.get(&e.id) {
                a = store::bodies(&self.db, *oldest)?.0;
                self.source = format!("{count} 次保存 · 本组开始前至末次保存");
            }
            let kind = if let Some(rev) = &self.pos.revision {
                a = Some(crate::git::body(&self.root, &e.path, rev)?);
                self.source = format!("Git {}", &rev[..rev.len().min(8)]);
                "M"
            } else {
                &e.kind
            };
            self.comparison_note = match (kind, a.as_deref(), b.as_deref()) {
                ("B", _, _) => Some("首次记录，没有上一版本"),
                (_, Some(old), Some(new)) if old == new => Some("两个版本的正文一致"),
                ("A", None, Some(_)) | ("D", Some(_), None) | (_, Some(_), Some(_)) => None,
                _ => Some("缺少可对比的文本快照"),
            };
            self.pos.diff &= self.comparison_note.is_none();
            let (before, after, visible_kind) = if !self.pos.diff && self.pos.revision.is_some() {
                (None, a.as_deref(), "B")
            } else if !self.pos.diff && self.comparison_note.is_some() && matches!(kind, "O" | "R")
            {
                (None, b.as_deref(), "B")
            } else {
                (a.as_deref(), b.as_deref(), kind)
            };
            self.rows = view::content(before, after, visible_kind, self.pos.diff);
            self.syntax = Syntax::parse(&e.path, after);
            if self.pos.diff {
                self.old_syntax = Syntax::parse(&e.path, before);
            }
            self.changes
                .extend(self.rows.iter().enumerate().filter_map(|(i, r)| {
                    (r.change.is_some() && (i == 0 || r.change != self.rows[i - 1].change))
                        .then_some(i)
                }));
            if e.kind == "R" && self.pos.revision.is_none() {
                let prev: String =
                    self.db
                        .query_row("SELECT previous FROM moves WHERE id=?", [e.id], |r| {
                            r.get(0)
                        })?;
                self.source = format!("{prev} → {}", e.path);
            }
            if e.kind == "O" && self.pos.revision.is_none() {
                self.source = "离线期间变更".into();
            }
            if locate {
                self.pos.line = if self.pos.mode > 0 && !self.pos.query.is_empty() {
                    self.rows
                        .iter()
                        .position(|r| r.text.contains(&self.pos.query))
                        .unwrap_or(0)
                } else {
                    self.changes.first().copied().unwrap_or(0)
                };
                self.pos.top = self.pos.line.saturating_sub(3);
            }
        }
        self.pos.line = self.pos.line.min(self.rows.len().saturating_sub(1));
        Ok(())
    }

    fn toggle_diff(&mut self) -> Result<()> {
        if self.pos.entry.is_none() {
            self.notice = Some("请先选择一个文件");
            return Ok(());
        }
        if let Some(note) = self.comparison_note {
            self.notice = Some(note);
            return Ok(());
        }
        let anchor = self
            .rows
            .get(self.pos.line)
            .map(|r| (r.num, r.mark, r.change));
        self.pos.diff = !self.pos.diff;
        self.load(false)?;
        if let Some((num, mark, change)) = anchor {
            let start = change.and_then(|c| self.changes.get(c - 1).copied());
            let exact = || {
                self.rows.iter().position(|r| {
                    num > 0
                        && r.num == num
                        && r.change == change
                        && (mark == '-') == (r.mark == '-')
                })
            };
            self.pos.line = if self.pos.diff {
                start.or_else(exact)
            } else {
                exact().or(start)
            }
            .or_else(|| self.changes.first().copied())
            .unwrap_or(0);
            self.pos.top = self.pos.line.saturating_sub(3);
        }
        Ok(())
    }

    fn jump_change(&mut self, forward: bool) {
        let target = if forward {
            self.changes
                .get(self.changes.partition_point(|&i| i <= self.pos.line))
        } else {
            self.changes[..self.changes.partition_point(|&i| i < self.pos.line)].last()
        };
        if let Some(&target) = target {
            self.pos.line = target;
            self.pos.top = target.saturating_sub(3);
        } else {
            self.notice = Some(if self.changes.is_empty() {
                "当前版本没有可跳转的变化"
            } else if forward {
                "已到最后一处变化"
            } else {
                "已到第一处变化"
            });
        }
    }

    fn scroll(&mut self, forward: bool, amount: usize, height: usize) {
        let last = self.rows.len().saturating_sub(1);
        self.pos.line = if forward {
            self.pos.line.saturating_add(amount).min(last)
        } else {
            self.pos.line.saturating_sub(amount)
        };
        if amount > 1 {
            self.pos.top = if forward {
                self.pos.top.saturating_add(amount)
            } else {
                self.pos.top.saturating_sub(amount)
            };
        }
        self.pos.top = visible_top(self.pos.top, self.pos.line, self.rows.len(), height);
    }
    fn push(&mut self) {
        self.stack.push(Bookmark {
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
        self.search = None;
        self.search_after = None;
        if let Some(bookmark) = self.stack.pop() {
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

    fn links(&mut self) -> Result<()> {
        if let Some(e) = &self.pos.entry {
            let (_, mut b, _) = store::bodies(&self.db, e.id)?;
            if let Some(rev) = &self.pos.revision {
                b = Some(crate::git::body(&self.root, &e.path, rev)?);
            }
            let n = self
                .rows
                .get(self.pos.line)
                .map_or(0, |r| r.num.saturating_sub(1));
            self.targets =
                refs::links(&self.db, &self.root, &e.path, b.as_deref().unwrap_or(""), n)?;
            self.options = self.targets.iter().map(|t| t.label.clone()).collect();
        }
        Ok(())
    }
    fn switch(&mut self, r: String) -> Result<()> {
        self.positions.insert(self.root.clone(), self.pos.clone());
        self.root = r;
        self.db.execute(
            "INSERT OR REPLACE INTO projects VALUES(?,?)",
            params![self.root, chrono::Local::now().timestamp_millis()],
        )?;
        self.worker = watch::start(self.root.clone());
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
            let (tx, results) = std::sync::mpsc::channel();
            let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let flag = cancel.clone();
            std::thread::spawn(move || {
                let result = (|| {
                    let db = store::database()?;
                    db.progress_handler(
                        500,
                        Some(move || flag.load(std::sync::atomic::Ordering::Relaxed)),
                    );
                    store::entries(
                        &db, &root, pos.recent, &pos.only, &pos.query, pos.mode, pos.offset,
                    )
                })();
                let _ = tx.send(result);
            });
            self.search = Some(SearchTask { cancel, results });
        }
        if let Some(result) = self.search.as_ref().and_then(|s| s.results.try_recv().ok()) {
            self.search = None;
            self.list = result?;
            self.group_page();
            self.pos.entry = self.list.first().cloned();
            self.load(true)?;
            self.dirty = true;
        }
        Ok(())
    }
    fn key(&mut self, k: K, size: Size) -> Result<bool> {
        self.notice = None;
        let page = size.height.saturating_sub(5).max(1) as usize;
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
                K::Up => self.choice = self.choice.saturating_sub(1),
                K::Down => {
                    self.choice = (self.choice + 1).min(self.options.len().saturating_sub(1))
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
                        self.pos.line = self.rows.iter().position(|r| r.num == n + 1).unwrap_or(0)
                    }
                    Panel::Ignore => {
                        store::update_rule(
                            &mut self.db,
                            &self.root,
                            &self.options[self.choice],
                            true,
                        )?;
                        self.back()?;
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
                    "z  折叠连续保存     f  当前目录     H  回到起点".into(),
                    "d  切换对比     h  文件历史".into(),
                    "r  引用与定义     i  忽略规则".into(),
                    "t  时间轴 / 最近文件     g  最新".into(),
                    "G  Git 历史     /  搜索     p  项目     q  退出".into(),
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
                self.links()?
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
            K::Char('t') => {
                self.pos.recent = !self.pos.recent;
                self.pos.offset = 0;
                self.reload(false)?
            }
            K::Char('g') => {
                self.pos.offset = 0;
                self.pending = false;
                self.follow = true;
                self.reload(false)?
            }
            K::Char('d') => {
                if narrow
                    && !self.pos.body
                    && self.pos.entry.is_some()
                    && self.comparison_note.is_none()
                {
                    self.push();
                    self.pos.body = true;
                }
                self.toggle_diff()?;
            }
            K::Char('[') | K::Char(']') => {
                if narrow && !self.pos.body && !self.changes.is_empty() {
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
                self.scroll(k == K::PageDown, page.saturating_sub(1).max(1), page);
            }
            K::Up | K::Down if self.pos.body => self.scroll(k == K::Down, 1, page),
            K::Up | K::Down => {
                let i = self
                    .pos
                    .entry
                    .as_ref()
                    .and_then(|e| self.list.iter().position(|v| v.id == e.id))
                    .unwrap_or(0);
                let j = if k == K::Down && i + 1 == self.list.len() && self.raw_count == 256 {
                    self.pos.offset += 255;
                    self.reload(false)?;
                    usize::from(self.list.len() > 1)
                } else if k == K::Up && i == 0 && self.pos.offset > 0 {
                    self.pos.offset = self.pos.offset.saturating_sub(255);
                    self.reload(false)?;
                    self.list.len().saturating_sub(2)
                } else if k == K::Down {
                    (i + 1).min(self.list.len().saturating_sub(1))
                } else {
                    i.saturating_sub(1)
                };
                self.pos.entry = self.list.get(j).cloned();
                self.load(true)?
            }
            _ => {}
        }
        Ok(false)
    }
    fn draw(&self, f: &mut Frame) {
        let area = f.area().inner(Margin {
            horizontal: 1,
            vertical: 0,
        });
        let bands = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);
        let name = std::path::Path::new(&self.root)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        f.render_widget(
            Paragraph::new(Line::from(vec![
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
            ])),
            bands[0],
        );
        let main = matches!(self.panel, Panel::Main | Panel::Search);
        if main {
            let wide = area.width >= 78;
            let cols = Layout::horizontal([Constraint::Percentage(38), Constraint::Percentage(62)])
                .split(bands[1]);
            if wide || !self.pos.body {
                let a = if wide { cols[0] } else { bands[1] };
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
                let lines = self
                    .list
                    .iter()
                    .enumerate()
                    .skip(start)
                    .take(h)
                    .map(|(i, e)| {
                        let time = chrono::DateTime::from_timestamp(e.stamp, 0)
                            .map(|t| {
                                t.with_timezone(&chrono::Local)
                                    .format("%m-%d %H:%M")
                                    .to_string()
                            })
                            .unwrap_or_default();
                        let count = self
                            .groups
                            .get(&e.id)
                            .map(|(_, n)| format!(" ×{n}"))
                            .unwrap_or_default();
                        let mark = if i == index { "▸" } else { "│" };
                        let name = e.path.rsplit('/').next().unwrap_or(&e.path);
                        let display =
                            if e.path.chars().count() + 19 > a.width as usize && name != e.path {
                                format!("…/{name}")
                            } else {
                                e.path.clone()
                            };
                        Line::from(format!("{mark} {time} {} {display}{count}", e.kind)).style(
                            if i == index {
                                Style::new().fg(ACC).bold()
                            } else {
                                Style::default()
                            },
                        )
                    })
                    .collect::<Vec<_>>();
                let title = if !self.pos.query.is_empty() {
                    format!("搜索 · {}", self.pos.query)
                } else if !self.pos.only.is_empty() {
                    format!("历史 · {}", self.pos.only)
                } else if self.pos.recent {
                    "最近文件".into()
                } else {
                    "时间轴".into()
                };
                f.render_widget(
                    Paragraph::new(lines).block(
                        Block::default()
                            .title(title)
                            .borders(Borders::TOP)
                            .border_style(Style::new().fg(DIM)),
                    ),
                    a,
                );
            }
            if wide || self.pos.body {
                let a = if wide { cols[1] } else { bands[1] };
                let h = a.height.saturating_sub(2) as usize;
                let start = visible_top(self.pos.top, self.pos.line, self.rows.len(), h);
                let previous = self.changes.partition_point(|&row| row <= self.pos.line);
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
                if !self.changes.is_empty() {
                    summary.push_str(&format!("  [{} / {}] 变化", previous, self.changes.len()));
                } else if let Some(note) = self.comparison_note {
                    summary.push_str(&format!("  ·  {note}"));
                }
                if !self.source.is_empty() {
                    summary.push_str(&format!("  ·  {}", self.source));
                }
                let mut lines = vec![Line::styled(summary, Style::new().fg(ACC))];
                for (i, r) in self.rows.iter().enumerate().skip(start).take(h) {
                    let color = match r.mark {
                        '~' => Color::Yellow,
                        '+' => Color::Green,
                        '-' => Color::Red,
                        _ => DIM,
                    };
                    let anchored =
                        i == self.pos.line && (self.pos.body || !self.changes.is_empty());
                    let gutter = format!(
                        "{} {} {:>4}  ",
                        if anchored { "›" } else { " " },
                        r.mark,
                        if r.num == 0 {
                            String::new()
                        } else {
                            r.num.to_string()
                        }
                    );
                    let syntax = if self.pos.diff && r.mark == '-' {
                        &self.old_syntax
                    } else {
                        &self.syntax
                    };
                    let mut spans = vec![Span::styled(gutter, Style::new().fg(color))];
                    spans.extend(syntax.spans(r.num, &r.text));
                    let line = Line::from(spans);
                    lines.push(if self.pos.body && anchored {
                        line.style(Style::new().add_modifier(Modifier::BOLD))
                    } else {
                        line
                    });
                }
                f.render_widget(
                    Paragraph::new(lines).block(
                        Block::default()
                            .title(self.pos.entry.as_ref().map_or("正文", |e| e.path.as_str()))
                            .borders(Borders::TOP)
                            .border_style(Style::new().fg(DIM)),
                    ),
                    a,
                );
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
            let mut lines = Vec::new();
            for (i, s) in self
                .options
                .iter()
                .enumerate()
                .skip(self.choice.saturating_sub(bands[1].height as usize / 2))
            {
                lines.push(Line::styled(
                    format!("{} {s}", if i == self.choice { "›" } else { " " }),
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
                lines.push(Line::styled("暂无匹配", Style::new().fg(DIM)));
            }
            let mut list_area = bands[1];
            if matches!(self.panel, Panel::Links)
                && let Some(target) = self.targets.get(self.choice)
            {
                let areas =
                    Layout::vertical([Constraint::Percentage(57), Constraint::Percentage(43)])
                        .split(bands[1]);
                list_area = areas[0];
                if let Ok((_, Some(body), _)) = store::bodies(&self.db, target.event.id) {
                    let start = target.line.saturating_sub(2);
                    let lines = body
                        .lines()
                        .enumerate()
                        .skip(start)
                        .take(areas[1].height as usize)
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
                    f.render_widget(
                        Paragraph::new(lines).block(
                            Block::default()
                                .title(format!("当前版本 · {}", target.event.path))
                                .borders(Borders::TOP),
                        ),
                        areas[1],
                    );
                }
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
            "↑↓ 事件  Enter 阅读  [ / ] 变化  d 对比  / 搜索  ? 帮助".into()
        };
        f.render_widget(Paragraph::new(hint).style(Style::new().fg(ACC)), bands[2]);
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
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
    }
}
pub fn run(db: Connection, root: String) -> Result<()> {
    enable_raw_mode()?;
    let _restore = Restore;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let mut term: Terminal<CrosstermBackend<Stdout>> =
        Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let worker = watch::start(root.clone());
    let mut a = App {
        db,
        root,
        worker,
        status: "建立基线".into(),
        pos: Position::default(),
        list: vec![],
        rows: vec![],
        syntax: Syntax::default(),
        old_syntax: Syntax::default(),
        changes: vec![],
        comparison_note: None,
        notice: None,
        source: String::new(),
        stack: vec![],
        panel: Panel::Main,
        options: vec![],
        choice: 0,
        targets: vec![],
        groups: Default::default(),
        raw_count: 0,
        positions: Default::default(),
        search: None,
        search_after: None,
        input: String::new(),
        follow: true,
        pending: false,
        dirty: true,
    };
    a.reload(false)?;
    loop {
        a.search_tick()?;
        while let Ok(s) = a.worker.updates.try_recv() {
            a.status = s;
            if a.follow || a.pos.entry.is_none() {
                a.reload(false)?
            } else {
                a.pending = true
            }
            a.dirty = true;
        }
        if a.dirty {
            term.draw(|f| a.draw(f))?;
            a.dirty = false;
        }
        if event::poll(Duration::from_millis(250))? {
            match event::read()? {
                Event::Key(k) if k.kind != KeyEventKind::Release => {
                    if (k.code == K::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL))
                        || a.key(k.code, term.size()?)?
                    {
                        break;
                    }
                    a.dirty = true
                }
                Event::Resize(..) => a.dirty = true,
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
    fn viewport_keeps_cursor_visible_after_paging_or_resize() {
        assert_eq!(visible_top(23, 26, 100, 24), 23);
        assert_eq!(visible_top(23, 50, 100, 24), 27);
        assert_eq!(visible_top(23, 2, 100, 24), 2);
        assert_eq!(visible_top(97, 99, 100, 24), 76);
        assert_eq!(visible_top(5, 3, 4, 24), 0);
    }
}
