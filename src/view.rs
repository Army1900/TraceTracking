use similar::{ChangeTag, DiffTag, TextDiff};
use std::time::Duration;
#[derive(Clone)]
pub struct Row {
    pub num: usize,
    pub mark: char,
    pub text: String,
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
                text: s.into(),
            })
            .collect();
    }
    if new.is_none() && kind != "D" {
        return vec![Row {
            num: 0,
            mark: ' ',
            text: "二进制或大文件 · 仅记录文件信息".into(),
        }];
    }
    let changes = TextDiff::configure()
        .timeout(Duration::from_millis(80))
        .diff_lines(old.unwrap_or(""), new.unwrap_or(""));
    for op in changes.ops() {
        let replace = op.tag() == DiffTag::Replace;
        if !diff && op.tag() == DiffTag::Delete {
            rows.push(Row {
                num: 0,
                mark: '-',
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
