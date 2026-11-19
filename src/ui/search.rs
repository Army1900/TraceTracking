use super::*;
use crate::syntax::append_highlights;
use std::ops::Range;

const HIT: Style = Style::new()
    .bg(Color::Rgb(92, 65, 29))
    .fg(Color::Rgb(255, 223, 157));
const ACTIVE_HIT: Style = Style::new().bg(Color::Rgb(239, 177, 84)).fg(Color::Black);

#[derive(Default)]
pub(super) struct SearchHits {
    pub path: bool,
    // Tracked text snapshots are capped at 1 MiB. Keep match coordinates compact.
    body: Vec<(u32, u32)>,
}

impl SearchHits {
    pub fn collect(pos: &Position, path: &str, rows: &[Row]) -> Self {
        if pos.query.is_empty() {
            return Self::default();
        }
        let path = pos.searches_paths()
            && path
                .to_ascii_lowercase()
                .contains(&pos.query.to_ascii_lowercase());
        let mut body = Vec::new();
        if pos.mode != SearchMode::Path {
            for (i, row) in rows.iter().enumerate().filter(|(_, row)| {
                row.num > 0
                    && if pos.mode == SearchMode::Changes {
                        row.mark != ' '
                    } else {
                        row.mark != '-'
                    }
            }) {
                body.extend(
                    row.text
                        .match_indices(&pos.query)
                        .map(|(byte, _)| (i as u32, byte as u32)),
                );
            }
        }
        Self { path, body }
    }

    fn point(&(row, byte): &(u32, u32)) -> TextPoint {
        TextPoint {
            row: row as usize,
            byte: byte as usize,
        }
    }

    pub fn first(&self) -> Option<TextPoint> {
        self.body.first().map(Self::point)
    }

    pub fn last(&self) -> Option<TextPoint> {
        self.body.last().map(Self::point)
    }

    fn adjacent(&self, pos: &Position, forward: bool) -> Option<TextPoint> {
        if pos.path_hit {
            return forward.then(|| self.first()).flatten();
        }
        let point = (pos.line.row as u32, pos.line.byte as u32);
        if forward {
            self.body
                .get(self.body.partition_point(|hit| *hit <= point))
        } else {
            self.body[..self.body.partition_point(|hit| *hit < point)].last()
        }
        .map(Self::point)
    }

    pub fn highlights(
        &self,
        pos: &Position,
        row: usize,
        bytes: &Range<usize>,
    ) -> Vec<(Range<usize>, Style)> {
        let len = pos.query.len();
        let first = self
            .body
            .partition_point(|&(r, b)| (r as usize, b as usize + len) <= (row, bytes.start));
        self.body[first..]
            .iter()
            .take_while(|&&(r, b)| (r as usize) == row && (b as usize) < bytes.end)
            .map(|hit| {
                let point = Self::point(hit);
                (
                    point.byte..point.byte + len,
                    if !pos.path_hit && pos.line == point {
                        ACTIVE_HIT
                    } else {
                        HIT
                    },
                )
            })
            .collect()
    }
}

pub(super) fn path_spans(
    spans: &mut Vec<Span<'static>>,
    text: &str,
    bytes: Range<usize>,
    style: Style,
    query: &str,
    active: bool,
) {
    let mut highlights = Vec::new();
    if !query.is_empty() {
        highlights.extend(
            text.to_ascii_lowercase()
                .match_indices(query)
                .map(|(byte, _)| {
                    (
                        byte..byte + query.len(),
                        if active { ACTIVE_HIT } else { HIT },
                    )
                }),
        );
    }
    append_highlights(spans, text, bytes, style, &highlights);
}

impl App {
    pub(super) fn jump_search(&mut self, forward: bool) -> Result<()> {
        if self.pos.mode == SearchMode::Changes
            && !self.pos.diff
            && self.preview.comparison_note.is_none()
            && self.pos.entry.is_some()
        {
            self.pos.diff = true;
            return self.load(true);
        }
        if let Some(point) = self.preview.hits.adjacent(&self.pos, forward) {
            self.pos.anchor_point(point);
        } else if !forward && !self.pos.path_hit && self.preview.hits.path {
            self.pos.anchor_path();
        } else if self.move_entry(forward)? {
            if !forward && let Some(last) = self.preview.hits.last() {
                self.pos.anchor_point(last);
            }
        } else {
            self.notice = Some(if forward {
                "已到最后一处搜索命中"
            } else {
                "已到第一处搜索命中"
            });
        }
        Ok(())
    }
}
