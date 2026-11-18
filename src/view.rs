use similar::{ChangeTag, DiffTag, TextDiff};
use std::{ops::Range, time::Duration};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
#[derive(Clone)]
pub struct Row {
    pub num: usize,
    pub mark: char,
    pub text: String,
    pub change: Option<usize>,
}
pub fn content(old: Option<&str>, new: Option<&str>, kind: &str, diff: bool) -> Vec<Row> {
    let mut rows = Vec::new();
    if kind == "B" || (old.is_none() && kind == "M") {
        return new
            .unwrap_or("仅记录文件信息")
            .lines()
            .enumerate()
            .map(|(i, s)| Row {
                num: i + 1,
                mark: ' ',
                change: None,
                text: s.into(),
            })
            .collect();
    }
    if new.is_none() && kind != "D" {
        return vec![Row {
            num: 0,
            mark: ' ',
            change: None,
            text: "二进制或大文件 · 仅记录文件信息".into(),
        }];
    }
    let changes = TextDiff::configure()
        .timeout(Duration::from_millis(80))
        .diff_lines(old.unwrap_or(""), new.unwrap_or(""));
    let mut group = 0;
    let mut in_change = false;
    for op in changes.ops() {
        let changed = op.tag() != DiffTag::Equal;
        if changed && !in_change {
            group += 1;
        }
        in_change = changed;
        let change = changed.then_some(group);
        let replace = op.tag() == DiffTag::Replace;
        if !diff && op.tag() == DiffTag::Delete {
            rows.push(Row {
                num: 0,
                mark: '-',
                change,
                text: format!("删除 {} 行", op.old_range().len()),
            });
        }
        for ch in changes.iter_changes(op) {
            let mark = match ch.tag() {
                ChangeTag::Equal => ' ',
                ChangeTag::Insert if replace && !diff => '~',
                ChangeTag::Insert => '+',
                ChangeTag::Delete if diff => '-',
                _ => continue,
            };
            rows.push(Row {
                num: ch.new_index().or(ch.old_index()).unwrap_or(0) + 1,
                mark,
                change,
                text: ch.value().trim_end_matches(['\r', '\n']).into(),
            });
        }
    }
    rows
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextPoint {
    pub row: usize,
    pub byte: usize,
}

impl TextPoint {
    pub fn at_row(row: usize) -> Self {
        Self { row, byte: 0 }
    }
}

struct Continuation {
    screen_row: usize,
    point: TextPoint,
}

#[derive(Default)]
pub struct WrappedRows {
    pub width: usize,
    continuations: Vec<Continuation>,
}

impl WrappedRows {
    pub fn new(rows: &[Row], width: usize) -> Self {
        let width = width.max(1);
        let mut wrapped = Self {
            width,
            continuations: Vec::new(),
        };
        for (row, line) in rows.iter().enumerate() {
            let tabs = line.text.as_bytes().contains(&b'\t');
            if !tabs && (line.text.len() <= width || line.text.is_ascii()) {
                for byte in (width..line.text.len()).step_by(width) {
                    wrapped.push(row, byte);
                }
                continue;
            }
            let mut used = 0;
            for (byte, cluster) in line.text.grapheme_indices(true) {
                let columns = if cluster == "\t" {
                    4
                } else if cluster.chars().any(char::is_control) {
                    crate::syntax::terminal_text(cluster).width()
                } else {
                    cluster.width()
                };
                if used != 0 && used + columns > width {
                    wrapped.push(row, byte);
                    used = 0;
                }
                used += columns;
            }
        }
        wrapped
    }

    fn push(&mut self, row: usize, byte: usize) {
        self.continuations.push(Continuation {
            screen_row: row + self.continuations.len() + 1,
            point: TextPoint { row, byte },
        });
    }

    pub fn len(&self, rows: &[Row]) -> usize {
        rows.len() + self.continuations.len()
    }

    pub fn index(&self, point: TextPoint) -> usize {
        point.row
            + self.continuations.partition_point(|item| {
                (item.point.row, item.point.byte) <= (point.row, point.byte)
            })
    }

    pub fn point(&self, screen_row: usize) -> TextPoint {
        let before = self
            .continuations
            .partition_point(|item| item.screen_row <= screen_row);
        if before > 0 && self.continuations[before - 1].screen_row == screen_row {
            self.continuations[before - 1].point
        } else {
            TextPoint::at_row(screen_row - before)
        }
    }

    pub fn range(&self, screen_row: usize, point: TextPoint, line_bytes: usize) -> Range<usize> {
        let next = self.point(screen_row + 1);
        let end = if next.row == point.row {
            next.byte
        } else {
            line_bytes
        };
        point.byte..end
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wrapping_preserves_source_rows_and_graphemes() {
        let rows = content(None, Some("1234567\nA汉e\u{301}👩‍💻B\n\nlast"), "B", false);
        let wrapped = WrappedRows::new(&rows, 3);
        let segments: Vec<_> = (0..wrapped.len(&rows))
            .map(|i| {
                let point = wrapped.point(i);
                assert_eq!(wrapped.index(point), i);
                let row = &rows[point.row];
                (row.num, &row.text[wrapped.range(i, point, row.text.len())])
            })
            .collect();
        assert_eq!(
            segments,
            [
                (1, "123"),
                (1, "456"),
                (1, "7"),
                (2, "A汉"),
                (2, "e\u{301}👩‍💻"),
                (2, "B"),
                (3, ""),
                (4, "las"),
                (4, "t"),
            ]
        );
        let resized = WrappedRows::new(&rows, 6);
        let anchor = wrapped.point(4);
        assert_eq!(resized.point(resized.index(anchor)).row, anchor.row);
    }

    #[test]
    fn separate_changes_have_stable_ids_in_text_and_diff() {
        let old = "before\nremoved\nkept\nold value\nafter\n";
        let new = "before\nkept\nnew value\nafter\n";
        let text = content(Some(old), Some(new), "M", false);
        let diff = content(Some(old), Some(new), "M", true);
        assert_eq!(text[1].change, Some(1));
        assert_eq!(text[3].change, Some(2));
        assert_eq!(diff[1].change, Some(1));
        assert_eq!(diff[3].change, Some(2));
        assert_eq!(diff[4].change, Some(2));
    }

    #[test]
    fn replacement_markers_and_deletion_anchor() {
        let rows = content(
            Some("port: 4\ntimeout: 30\n"),
            Some("port: 4\ntimeout: 60\n"),
            "M",
            false,
        );
        assert_eq!(rows[1].mark, '~');
        assert_eq!(rows[1].text, "timeout: 60");
        let rows = content(Some("a\nb\nc\n"), Some("a\nc\n"), "M", false);
        assert_eq!(rows[1].mark, '-');
        assert_eq!(rows[2].num, 2);
    }
}
