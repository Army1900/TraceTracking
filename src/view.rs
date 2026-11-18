use similar::{ChangeTag, DiffTag, TextDiff};
use std::time::Duration;
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

#[cfg(test)]
mod tests {
    use super::*;
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
