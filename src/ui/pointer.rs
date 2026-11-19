use super::*;
use ratatui::layout::Position as Pointer;
use std::{ops::Range, path::Path};

#[derive(Default)]
pub(super) struct MouseState {
    pub list: Rect,
    pub list_top: usize,
    pub reader: Option<ReaderViewport>,
    click: Option<(Pointer, i64, Instant)>,
    selection: Option<Selection>,
}

#[derive(Clone, Copy)]
pub(super) struct ReaderViewport {
    pub text: Rect,
    pub first_row: usize,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct TextCell {
    start: TextPoint,
    end: TextPoint,
}

impl ReaderViewport {
    fn cell(self, preview: &Preview, pointer: Pointer, clamp: bool) -> Option<TextCell> {
        if self.text.is_empty() || (!clamp && !self.text.contains(pointer)) {
            return None;
        }
        let total = preview.wrap.len(&preview.rows);
        let last_visible = self
            .first_row
            .saturating_add(self.text.height as usize)
            .min(total)
            .checked_sub(1)?;
        let screen_row = self.first_row + pointer.y.saturating_sub(self.text.y) as usize;
        if screen_row > last_visible && !clamp {
            return None;
        }
        let screen_row = screen_row.min(last_visible);
        let point = preview.wrap.point(screen_row);
        let row = &preview.rows[point.row];
        if !clamp && row.num == 0 {
            return None;
        }
        let bytes = preview.wrap.range(screen_row, point, row.text.len());
        let column = pointer.x.saturating_sub(self.text.x) as usize;
        let mut used = 0;
        for (offset, grapheme) in row.text[bytes.clone()].grapheme_indices(true) {
            used += view::cell_width(grapheme);
            if column < used {
                let byte = bytes.start + offset;
                return Some(TextCell {
                    start: TextPoint {
                        row: point.row,
                        byte,
                    },
                    end: TextPoint {
                        row: point.row,
                        byte: byte + grapheme.len(),
                    },
                });
            }
        }
        let end = TextPoint {
            row: point.row,
            byte: bytes.end,
        };
        Some(TextCell { start: end, end })
    }
}

struct Selection {
    anchor: TextCell,
    edge: TextCell,
    dragging: bool,
    moved: bool,
}

impl Selection {
    fn range(&self) -> Option<Range<TextPoint>> {
        if !self.moved {
            return None;
        }
        let start = self.anchor.start.min(self.edge.start);
        let end = self.anchor.end.max(self.edge.end);
        (start < end).then_some(start..end)
    }
}

impl MouseState {
    pub fn reset_click(&mut self) {
        self.click = None;
    }

    pub fn clear_text(&mut self) -> bool {
        self.selection.take().is_some()
    }

    pub fn selecting(&self) -> bool {
        self.selection.is_some()
    }

    pub fn selected(&self) -> Option<Range<TextPoint>> {
        self.selection.as_ref().and_then(Selection::range)
    }
}

pub(super) fn selection_highlight(
    highlights: &mut Vec<(Range<usize>, Style)>,
    selection: &Range<TextPoint>,
    row: usize,
    visible: &Range<usize>,
) {
    if row < selection.start.row || row > selection.end.row {
        return;
    }
    let start = if row == selection.start.row {
        selection.start.byte.max(visible.start)
    } else {
        visible.start
    };
    let end = if row == selection.end.row {
        selection.end.byte.min(visible.end)
    } else {
        visible.end
    };
    if start >= end {
        return;
    }
    let mut tail = None;
    highlights.retain_mut(|(range, style)| {
        if range.end <= start || range.start >= end {
            return true;
        }
        if range.start < start {
            if range.end > end {
                tail = Some((end..range.end, *style));
            }
            range.end = start;
            true
        } else if range.end > end {
            range.start = end;
            true
        } else {
            false
        }
    });
    highlights.extend(tail);
    highlights.push((
        start..end,
        Style::new().bg(Color::Rgb(44, 89, 118)).fg(Color::White),
    ));
    highlights.sort_unstable_by_key(|(range, _)| range.start);
}

impl App {
    pub(super) fn copy_selection(&mut self) -> bool {
        let Some(selection) = self.mouse.selected() else {
            return false;
        };
        let mut text = String::new();
        let mut wrote_line = false;
        for row_index in selection.start.row..=selection.end.row {
            let row = &self.preview.rows[row_index];
            if row.num == 0 {
                continue;
            }
            let from = if row_index == selection.start.row {
                selection.start.byte
            } else {
                0
            };
            let to = if row_index == selection.end.row {
                selection.end.byte
            } else {
                row.text.len()
            };
            if wrote_line {
                text.push('\n');
            }
            text.push_str(&row.text[from..to]);
            wrote_line = true;
        }
        if !text.is_empty() {
            self.copy_text(text);
        }
        true
    }

    pub(super) fn file_mouse(&mut self, kind: MouseEventKind, pointer: Pointer) -> Result<bool> {
        if !matches!(self.panel, Panel::Main | Panel::Search) {
            return Ok(false);
        }
        match kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.follow = false;
                self.views.wanted = false;
                let cleared = self.mouse.clear_text();
                let previous = self.mouse.click.take();
                if !self.views.refilter && !self.searching() {
                    let now = Instant::now();
                    if let Some((position, id, at)) = previous
                        && position.y == pointer.y
                        && position.x.abs_diff(pointer.x) <= 1
                        && (self.mouse.list.x..self.mouse.list.right()).contains(&pointer.x)
                        && now.saturating_duration_since(at) <= Duration::from_millis(500)
                        && let Some(entry) = &self.pos.entry
                        && entry.id == id
                    {
                        self.open_file(Path::new(&self.root).join(&entry.path));
                        return Ok(true);
                    }
                    if self.mouse.list.contains(pointer) {
                        let index = self.mouse.list_top + (pointer.y - self.mouse.list.y) as usize;
                        if let Some(entry) = self.list.get(index) {
                            let id = entry.id;
                            if self
                                .pos
                                .entry
                                .as_ref()
                                .is_none_or(|current| current.id != id)
                            {
                                self.pos.entry = Some(entry.clone());
                                self.pos.revision = None;
                                self.load(true)?;
                            }
                            self.mouse.click = Some((pointer, id, now));
                            self.notice = None;
                            return Ok(true);
                        }
                    }
                    if let Some(cell) = self
                        .mouse
                        .reader
                        .and_then(|view| view.cell(&self.preview, pointer, false))
                    {
                        self.mouse.selection = Some(Selection {
                            anchor: cell,
                            edge: cell,
                            dragging: true,
                            moved: false,
                        });
                        self.notice = None;
                        return Ok(true);
                    }
                }
                Ok(cleared)
            }
            MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left) => {
                let released = kind == MouseEventKind::Up(MouseButton::Left);
                if !released {
                    self.mouse.reset_click();
                }
                let Some(selection) = self
                    .mouse
                    .selection
                    .as_mut()
                    .filter(|selection| selection.dragging)
                else {
                    return Ok(false);
                };
                if let Some(cell) = self
                    .mouse
                    .reader
                    .and_then(|view| view.cell(&self.preview, pointer, true))
                {
                    selection.moved |= cell != selection.anchor || !released;
                    selection.edge = cell;
                }
                if released {
                    selection.dragging = false;
                    if selection.range().is_none() {
                        self.mouse.clear_text();
                    }
                }
                Ok(true)
            }
            MouseEventKind::Down(MouseButton::Right) => {
                self.mouse.reset_click();
                if self
                    .mouse
                    .reader
                    .is_some_and(|view| view.text.contains(pointer))
                {
                    return Ok(self.copy_selection());
                }
                Ok(false)
            }
            _ => Ok(false),
        }
    }
}
